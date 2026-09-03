//! Minimal HTTP/1.1 REST API server — Crestron Pro3 integration.
//!
//! See docs/crestron-rest-api-plan.md.  Design constraints:
//! - std only: `TcpListener` + one short-lived thread per connection, no async.
//! - Plain HTTP, `Connection: close` — one transaction per connection.
//! - GET/POST only, 8 KiB request cap, 2 s socket timeouts.
//! - At most [`MAX_IN_FLIGHT`] concurrent connections; excess gets a 503.
//! - Reads are served from a published [`StateSnapshot`] and never touch the
//!   render loop; mutations go over an `mpsc` command channel to the main
//!   loop, which validates against live state and replies on a oneshot
//!   (that is how "input N rejected when only M inputs exist" is enforced).

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender},
        Arc, RwLock, RwLockReadGuard,
    },
    time::Duration,
};

use crate::{
    config::Config,
    state::{
        parse_hex_color, parse_palette_index, parse_usize, ApiCommand, ApiCommandKind,
        ApiResult, StateSnapshot,
    },
    tools::Tool,
};

/// Simple session token -> username mapping
type SessionStore = Arc<RwLock<HashMap<String, String>>>;

/// Snapshot handle shared between the main loop (writer) and the API threads.
pub type SharedSnapshot = Arc<RwLock<StateSnapshot>>;

/// Receiving end of the mutation channel, owned by the main loop.
pub type ApiCommandRx = Receiver<ApiCommand>;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_IN_FLIGHT:     usize = 4;
const IO_TIMEOUT:        Duration = Duration::from_secs(2);
/// How long a connection thread waits for the main loop to apply a mutation.
const REPLY_TIMEOUT:     Duration = Duration::from_millis(500);
/// Channel depth — a Crestron sends one command at a time; 16 is generous.
const COMMAND_QUEUE:     usize = 16;

// ── Server ────────────────────────────────────────────────────────────────────

/// Bind the listener and serve on a detached thread.  Returns the command
/// receiver the main loop drains each frame.  Bind failure logs and disables
/// the API — a kiosk box must keep running even without it.
pub fn serve(bind: &str, port: u16, snapshot: SharedSnapshot, cfg: Config) -> ApiCommandRx {
    let (cmd_tx, cmd_rx) = sync_channel::<ApiCommand>(COMMAND_QUEUE);
    let addr = format!("{bind}:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[api] cannot bind {addr}: {e} — REST API disabled");
            return cmd_rx; // drained but never fed — harmless
        }
    };
    let sessions: SessionStore = Arc::new(RwLock::new(HashMap::new()));
    if let Err(e) = std::thread::Builder::new()
        .name("api-accept".into())
        .spawn(move || accept_loop(listener, snapshot, cmd_tx, cfg, sessions))
    {
        eprintln!("[api] cannot spawn accept thread: {e} — REST API disabled");
    } else {
        eprintln!("[api] listening on http://{addr}/api/v1");
    }
    cmd_rx
}

fn accept_loop(listener: TcpListener, snapshot: SharedSnapshot, cmd_tx: SyncSender<ApiCommand>, cfg: Config, sessions: SessionStore) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[api] accept failed: {e}");
                continue;
            }
        };
        // Hard cap on concurrent connections — a Crestron opens one socket
        // per command, so anything above a handful is abuse.
        if in_flight.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
            let _ = write_response(&mut stream, 503, &err_body("busy — too many connections"), "application/json");
            continue;
        }
        in_flight.fetch_add(1, Ordering::SeqCst);
        let snap     = snapshot.clone();
        let tx       = cmd_tx.clone();
        let count    = in_flight.clone();
        let cfg_clone = cfg.clone();
        let sess_clone = sessions.clone();
        let spawned = std::thread::Builder::new().spawn(move || {
            handle_connection(stream, &snap, &tx, &cfg_clone, &sess_clone);
            count.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn handle_connection(mut stream: TcpStream, snapshot: &SharedSnapshot, cmd_tx: &SyncSender<ApiCommand>, cfg: &Config, sessions: &SessionStore) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let (status, body, ctype) = match read_request(&mut stream) {
        Ok(Some(req))            => route(&req, snapshot, cmd_tx, cfg, sessions),
        Ok(None)                 => return,   // peer connected, sent nothing
        Err(ReadError::TooLarge) => (400, err_body("request too large"), "application/json"),
        Err(ReadError::Malformed) => (400, err_body("malformed request"), "application/json"),
        Err(ReadError::Io(_))    => return,   // timeout / reset — peer is gone
    };
    let _ = write_response(&mut stream, status, &body, ctype);
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str, content_type: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _   => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        content_type,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

// ── Request parsing ───────────────────────────────────────────────────────────

#[derive(Debug)]
enum ReadError {
    // Payload kept for future diagnostics; currently only the variant matters.
    #[allow(dead_code)]
    Io(std::io::Error),
    Malformed,
    TooLarge,
}

struct Request {
    method: String,
    path:   String,
    /// Decoded query-string params (merged with the POSTed form on control calls).
    query:  HashMap<String, String>,
    /// Raw request body, form-decoded by the control endpoints.
    body:   Vec<u8>,
}

/// Read one request.  `Ok(None)` = clean EOF before any bytes arrived.
fn read_request<R: Read>(r: &mut R) -> Result<Option<Request>, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];

    // ── Header block (terminated by a blank line) ────────────────────────
    let header_len = loop {
        match r.read(&mut chunk) {
            Ok(0) if buf.is_empty() => return Ok(None),
            Ok(0) => return Err(ReadError::Malformed),  // EOF mid-headers
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_REQUEST_BYTES {
                    return Err(ReadError::TooLarge);
                }
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(e) => return Err(ReadError::Io(e)),
        }
    };

    let head = std::str::from_utf8(&buf[..header_len - 4]).map_err(|_| ReadError::Malformed)?;
    let mut lines = head.split("\r\n");

    // Request line: "METHOD SP TARGET SP HTTP/x.y"
    let request_line = lines.next().ok_or(ReadError::Malformed)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(ReadError::Malformed)?.to_ascii_uppercase();
    let target = parts.next().ok_or(ReadError::Malformed)?.to_string();
    if parts.next().is_none() {
        return Err(ReadError::Malformed);
    }

    // Only header we act on (phase 2 needs the body for POSTed forms).
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().map_err(|_| ReadError::Malformed)?;
            }
        }
    }
    if header_len + content_length > MAX_REQUEST_BYTES {
        return Err(ReadError::TooLarge);
    }

    // ── Body ─────────────────────────────────────────────────────────────
    while buf.len() < header_len + content_length {
        match r.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Malformed),  // EOF mid-body
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_REQUEST_BYTES {
                    return Err(ReadError::TooLarge);
                }
            }
            Err(e) => return Err(ReadError::Io(e)),
        }
    }

    let (path, query) = split_target(&target);
    Ok(Some(Request {
        method,
        path,
        query,
        body: buf[header_len..header_len + content_length].to_vec(),
    }))
}

/// Split `/api/v1/state?fmt=text` into decoded path + query params.
fn split_target(target: &str) -> (String, HashMap<String, String>) {
    match target.split_once('?') {
        Some((p, q)) => (percent_decode(p), parse_query(q)),
        None => (percent_decode(target), HashMap::new()),
    }
}

/// Parse `application/x-www-form-urlencoded` data (query string or POST body).
fn parse_query(qs: &str) -> HashMap<String, String> {
    qs.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if b[i] == b'%' && i + 2 < b.len() {
            match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── Router ────────────────────────────────────────────────────────────────────

fn route(req: &Request, snapshot: &SharedSnapshot, cmd_tx: &SyncSender<ApiCommand>, cfg: &Config, sessions: &SessionStore) -> (u16, String, &'static str) {
    // ── Portal routes ────────────────────────────────────────────────────────────
    match req.path.as_str() {
        "/portal/login" if req.method == "POST" => {
            let p = params(req);
            let username = p.get("username").cloned().unwrap_or_default();
            let password = p.get("password").cloned().unwrap_or_default();
            if let Some(user) = cfg.verify_credentials(&username, &password) {
                let token = generate_token();
                sessions.write().unwrap().insert(token.clone(), user.username.clone());
                return (200, serde_json::json!({ "ok": true, "token": token, "username": user.username }).to_string(), "application/json");
            } else {
                return (401, err_body("invalid credentials"), "application/json");
            }
        }
        "/portal/logout" if req.method == "POST" => {
            let p = params(req);
            if let Some(token) = p.get("token") {
                sessions.write().unwrap().remove(token);
            }
            return (200, serde_json::json!({ "ok": true }).to_string(), "application/json");
        }
        "/portal/check" if req.method == "GET" => {
            let p = &req.query;
            if let Some(token) = p.get("token") {
                if let Some(username) = sessions.read().unwrap().get(token) {
                    return (200, serde_json::json!({ "ok": true, "username": username }).to_string(), "application/json");
                }
            }
            return (401, err_body("not authenticated"), "application/json");
        }
        "/" | "/index.html" | "/portal" => {
            if req.method != "GET" {
                return (405, err_body("method not allowed"), "application/json");
            }
            return (200, PORTAL_HTML.to_string(), "text/html");
        }
        _ => {}
    }

    // Check authentication for /api/v1/* routes
    let p = &req.query;
    let token = p.get("token").or_else(|| p.get("X-Auth-Token"));
    let authenticated = token.and_then(|t| sessions.read().unwrap().get(t).cloned());

    // ── API routes ─────────────────────────────────────────────────────────
    match req.path.as_str() {
        // ── Read endpoints (GET only) ─────────────────────────────────────
        "/api/v1/health" | "/api/v1/state" | "/api/v1/inputs" if req.method != "GET" => {
            return (405, err_body("method not allowed — use GET"), "application/json");
        }
        "/api/v1/health" => {
            return (200, serde_json::json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
            }).to_string(), "application/json");
        }
        "/api/v1/state" => {
            let snap = read_snapshot(snapshot);
            let mut v = serde_json::to_value(&*snap)
                .unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".into(), true.into());
                if let Some(ref user) = authenticated {
                    obj.insert("authenticated_as".into(), user.clone().into());
                }
            }
            return (200, v.to_string(), "application/json");
        }
        "/api/v1/inputs" => {
            let snap = read_snapshot(snapshot);
            return (200, serde_json::json!({
                "ok": true,
                "active": snap.input,
                "inputs": snap.inputs,
            }).to_string(), "application/json");
        }

        // ── Control endpoints (GET with query string or POST with form body) ──
        "/api/v1/tool" | "/api/v1/color" | "/api/v1/brush_size" | "/api/v1/input" => {
            if req.method != "GET" && req.method != "POST" {
                return (405, err_body("method not allowed — use GET or POST"), "application/json");
            }
            let kind = match control_command(req) {
                Ok(k) => k,
                Err(msg) => return (400, err_body(&msg), "application/json"),
            };
            let (s, b) = dispatch(kind, cmd_tx);
            return (s, b, "application/json");
        }

        _ => return (404, err_body("not found"), "application/json"),
    }
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(now.as_nanos().to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// Merged parameters: the POSTed urlencoded form overlays the query string.
fn params(req: &Request) -> HashMap<String, String> {
    let mut p = req.query.clone();
    if req.method == "POST" && !req.body.is_empty() {
        if let Ok(form) = std::str::from_utf8(&req.body) {
            p.extend(parse_query(form));
        }
    }
    p
}

/// Turn a control request into a validated command.  Anything detectable
/// without live state (missing/unknown params) is rejected here with a 400;
/// live-state validation (device list bounds) happens in the main loop.
fn control_command(req: &Request) -> Result<ApiCommandKind, String> {
    let p = params(req);
    match req.path.as_str() {
        "/api/v1/tool" => {
            let name = p.get("name").ok_or("missing param 'name'")?;
            let tool = Tool::from_name(name)
                .ok_or_else(|| format!("unknown tool '{name}'"))?;
            Ok(ApiCommandKind::SetTool(tool))
        }
        "/api/v1/color" => {
            match (p.get("hex"), p.get("palette")) {
                (Some(h), _) => {
                    let (r, g, b) = parse_hex_color(h)?;
                    Ok(ApiCommandKind::SetColor(r, g, b))
                }
                (None, Some(pal)) => {
                    let (r, g, b) = parse_palette_index(pal)?;
                    Ok(ApiCommandKind::SetColor(r, g, b))
                }
                (None, None) => Err("missing param 'hex' or 'palette'".into()),
            }
        }
        "/api/v1/brush_size" => {
            match (p.get("px"), p.get("delta")) {
                (Some(px), _) => Ok(ApiCommandKind::SetBrushSize(
                    parse_usize(px, "brush size 'px'")? as u32,
                )),
                (None, Some(d)) => {
                    // Delta needs the current size → resolved by the main loop
                    // against the live state (encoded as a dedicated value).
                    let d: i64 = d
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad brush size delta '{d}'"))?;
                    if !(-50..=50).contains(&d) {
                        return Err(format!("delta {d} out of range (-50..=50)"));
                    }
                    Ok(ApiCommandKind::SetBrushSizeDelta(d))
                }
                (None, None) => Err("missing param 'px' or 'delta'".into()),
            }
        }
        "/api/v1/input" => {
            let index = parse_usize(
                p.get("index").ok_or("missing param 'index'")?,
                "input index",
            )?;
            let mode = match p.get("mode") {
                Some(m) => parse_usize(m, "input mode")?,
                None => 0,
            };
            Ok(ApiCommandKind::SetInput { index, mode })
        }
        _ => Err("unknown control endpoint".into()),
    }
}

/// Hand a validated command to the main loop and wait for its verdict.
fn dispatch(kind: ApiCommandKind, cmd_tx: &SyncSender<ApiCommand>) -> (u16, String) {
    let (reply_tx, reply_rx) = sync_channel::<ApiResult>(1);
    if cmd_tx.send(ApiCommand { kind, reply: reply_tx }).is_err() {
        return (503, err_body("main loop unavailable"));
    }
    let result = match reply_rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => ApiResult::NoReply,
        Err(RecvTimeoutError::Disconnected) => ApiResult::NoReply,
    };
    result.status_and_body()
}

/// Read the snapshot even if a panicking writer poisoned the lock — the data
/// is plain values, so a stale-but-intact copy is always safe to serve.
fn read_snapshot(snapshot: &SharedSnapshot) -> RwLockReadGuard<'_, StateSnapshot> {
    snapshot.read().unwrap_or_else(|p| p.into_inner())
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InputInfo;
    use crate::tools::Tool;

    fn get(target: &str) -> Request {
        let (path, query) = split_target(target);
        Request { method: "GET".into(), path, query, body: Vec::new() }
    }

    #[test]
    fn percent_decode_handles_plus_hex_and_literals() {
        assert_eq!(percent_decode("a+b%20c%2fD"), "a b c/D");
        assert_eq!(percent_decode("%C3%BF"), "\u{FF}"); // valid UTF-8 for ÿ
        assert_eq!(percent_decode("%FF"), "\u{FFFD}");  // lone byte → replacement char
        assert_eq!(percent_decode("%zz"), "%zz");   // bad hex kept literal
        assert_eq!(percent_decode("100%"), "100%"); // trailing % kept literal
    }

    #[test]
    fn query_parsing_decodes_pairs_and_flags() {
        let q = parse_query("a=1&b=two+words&flag&c=%3D");
        assert_eq!(q.get("a").unwrap(), "1");
        assert_eq!(q.get("b").unwrap(), "two words");
        assert_eq!(q.get("flag").unwrap(), "");
        assert_eq!(q.get("c").unwrap(), "=");
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn parses_post_with_form_body() {
        let raw: &[u8] = b"POST /api/v1/brush_size?fmt=text HTTP/1.1\r\n\
                           Host: anno\r\nContent-Length: 5\r\n\r\npx=12";
        let mut cur = std::io::Cursor::new(raw);
        let r = read_request(&mut cur).unwrap().unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/api/v1/brush_size");
        assert_eq!(r.query.get("fmt").unwrap(), "text");
        assert_eq!(r.body, b"px=12");
    }

    #[test]
    fn rejects_malformed_and_oversized() {
        let mut garbage = std::io::Cursor::new(&b"not an http request"[..]);
        assert!(matches!(read_request(&mut garbage), Err(ReadError::Malformed)));

        let big = vec![b'X'; MAX_REQUEST_BYTES + 16];
        let mut cur = std::io::Cursor::new(big);
        assert!(matches!(read_request(&mut cur), Err(ReadError::TooLarge)));
    }

    #[test]
    fn tool_name_mapping_round_trips() {
        for t in [
            Tool::Pen, Tool::Line, Tool::Rect, Tool::Circle, Tool::Arrow,
            Tool::Text, Tool::Select, Tool::Eraser, Tool::Highlight, Tool::Laser,
        ] {
            assert_eq!(Tool::from_name(t.name()), Some(t));
        }
        assert_eq!(Tool::from_name("PEN"), Some(Tool::Pen)); // case-insensitive
        assert_eq!(Tool::from_name("brush"), None);
    }

    #[test]
    fn route_health_404_and_method_gate() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = Config::default();
        let sessions: SessionStore = Arc::new(RwLock::new(HashMap::new()));

        let (status, body) = route(&get("/api/v1/health"), &snap, &tx, &cfg, &sessions);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

        let (status, body) = route(&get("/nope"), &snap, &tx, &cfg, &sessions);
        assert_eq!(status, 404);
        assert!(body.contains("\"ok\":false"));

        let mut post = get("/api/v1/state");
        post.method = "POST".into();
        let (status, _) = route(&post, &snap, &tx, &cfg, &sessions);
        assert_eq!(status, 405);
    }

    #[test]
    fn route_state_and_inputs_come_from_snapshot() {
        let mut s = StateSnapshot::default();
        s.tool = "pen";
        s.input = 1;
        s.inputs.push(InputInfo {
            index: 0,
            name: "No input (black screen)".into(),
            path: None,
            modes: vec![],
        });
        let snap: SharedSnapshot = Arc::new(RwLock::new(s));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = Config::default();
        let sessions: SessionStore = Arc::new(RwLock::new(HashMap::new()));

        let (status, body) = route(&get("/api/v1/state?fmt=json"), &snap, &tx, &cfg, &sessions);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["tool"], "pen");
        assert_eq!(v["input"], 1);

        let (status, body) = route(&get("/api/v1/inputs"), &snap, &tx, &cfg, &sessions);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["active"], 1);
        assert_eq!(v["inputs"][0]["index"], 0);
        assert_eq!(v["inputs"][0]["name"], "No input (black screen)");
    }

    // ── Phase 2: control-endpoint parsing ────────────────────────────────

    #[test]
    fn tool_endpoint_validates_names() {
        let r = control_command(&get("/api/v1/tool?name=pen")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetTool(Tool::Pen)));

        assert!(control_command(&get("/api/v1/tool?name=LASER")).is_ok());
        assert!(control_command(&get("/api/v1/tool")).is_err());            // missing
        assert!(control_command(&get("/api/v1/tool?name=brush")).is_err()); // unknown
    }

    #[test]
    fn color_endpoint_hex_and_palette() {
        let r = control_command(&get("/api/v1/color?hex=ff3333")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetColor(255, 51, 51)));

        let r = control_command(&get("/api/v1/color?palette=1")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetColor(255, 255, 255)));

        assert!(control_command(&get("/api/v1/color?hex=zzzzzz")).is_err());
        assert!(control_command(&get("/api/v1/color?hex=fff")).is_err());
        assert!(control_command(&get("/api/v1/color?palette=9")).is_err());
        assert!(control_command(&get("/api/v1/color?palette=0")).is_err());
        assert!(control_command(&get("/api/v1/color")).is_err());
    }

    #[test]
    fn brush_size_endpoint_px_and_delta() {
        let r = control_command(&get("/api/v1/brush_size?px=12")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetBrushSize(12)));

        let r = control_command(&get("/api/v1/brush_size?delta=-4")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetBrushSizeDelta(-4)));

        assert!(control_command(&get("/api/v1/brush_size?delta=99")).is_err()); // range
        assert!(control_command(&get("/api/v1/brush_size?px=abc")).is_err());
        assert!(control_command(&get("/api/v1/brush_size")).is_err());
    }

    #[test]
    fn input_endpoint_index_and_optional_mode() {
        let r = control_command(&get("/api/v1/input?index=2")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetInput { index: 2, mode: 0 }));

        let r = control_command(&get("/api/v1/input?index=1&mode=3")).unwrap();
        assert!(matches!(r, ApiCommandKind::SetInput { index: 1, mode: 3 }));

        assert!(control_command(&get("/api/v1/input")).is_err());
        assert!(control_command(&get("/api/v1/input?index=-1")).is_err());
    }

    #[test]
    fn post_form_body_overlays_query_string() {
        let mut r = get("/api/v1/tool?name=pen");
        r.method = "POST".into();
        r.body = b"name=eraser".to_vec();
        let cmd = control_command(&r).unwrap();
        assert!(matches!(cmd, ApiCommandKind::SetTool(Tool::Eraser))); // body wins
    }

    #[test]
    fn dispatch_reports_missing_main_loop() {
        // Channel exists but nobody drains and the receiver is dropped.
        let (tx, rx) = sync_channel::<ApiCommand>(1);
        drop(rx);
        let (status, body) = dispatch(ApiCommandKind::SetTool(Tool::Pen), &tx);
        assert_eq!(status, 503);
        assert!(body.contains("\"ok\":false"));
    }
}

// ── Web Portal HTML ───────────────────────────────────────────────────────────

const PORTAL_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Court Annotator</title>
    <script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4"></script>
    <style>
        .hidden { display: none; }
        .shake { animation: shake 0.3s; }
        @keyframes shake {
            0%, 100% { transform: translateX(0); }
            25% { transform: translateX(-5px); }
            75% { transform: translateX(5px); }
        }
    </style>
</head>
<body class="bg-gray-900 text-white min-h-screen">
    <div id="login-view" class="flex items-center justify-center min-h-screen">
        <div class="bg-gray-800 p-8 rounded-lg shadow-lg w-80">
            <h1 class="text-2xl font-bold mb-6 text-center">Court Annotator</h1>
            <form id="login-form" class="space-y-4">
                <div>
                    <label class="block text-sm mb-1">Username</label>
                    <input type="text" id="username" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600 focus:border-blue-500" required>
                </div>
                <div>
                    <label class="block text-sm mb-1">Password</label>
                    <input type="password" id="password" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600 focus:border-blue-500" required>
                </div>
                <p id="login-error" class="text-red-400 text-sm hidden"></p>
                <button type="submit" class="w-full bg-blue-600 hover:bg-blue-700 py-2 rounded font-medium">Login</button>
            </form>
        </div>
    </div>

    <div id="portal-view" class="hidden p-6">
        <div class="max-w-4xl mx-auto">
            <div class="flex justify-between items-center mb-6">
                <h1 class="text-2xl font-bold">Court Annotator Settings</h1>
                <div class="flex items-center gap-4">
                    <span id="user-display" class="text-gray-400"></span>
                    <button id="logout-btn" class="bg-red-600 hover:bg-red-700 px-4 py-2 rounded">Logout</button>
                </div>
            </div>

            <div class="bg-gray-800 rounded-lg p-6 space-y-6">
                <h2 class="text-xl font-semibold border-b border-gray-700 pb-2">Tool Settings</h2>
                
                <div class="grid grid-cols-2 gap-6">
                    <div>
                        <label class="block text-sm mb-2">Tool</label>
                        <select id="tool-select" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600">
                            <option value="pen">Pen</option>
                            <option value="line">Line</option>
                            <option value="rect">Rectangle</option>
                            <option value="circle">Circle</option>
                            <option value="arrow">Arrow</option>
                            <option value="text">Text</option>
                            <option value="select">Select</option>
                            <option value="eraser">Eraser</option>
                            <option value="highlight">Highlight</option>
                            <option value="laser">Laser</option>
                        </select>
                    </div>
                    <div>
                        <label class="block text-sm mb-2">Color</label>
                        <div class="flex gap-2 flex-wrap">
                            <button class="color-btn w-8 h-8 rounded bg-white border-2 border-gray-600" data-color="ffffff"></button>
                            <button class="color-btn w-8 h-8 rounded bg-black border-2 border-gray-600" data-color="000000"></button>
                            <button class="color-btn w-8 h-8 rounded bg-red-500 border-2 border-gray-600" data-color="ff0000"></button>
                            <button class="color-btn w-8 h-8 rounded bg-green-500 border-2 border-gray-600" data-color="00ff00"></button>
                            <button class="color-btn w-8 h-8 rounded bg-blue-500 border-2 border-gray-600" data-color="0000ff"></button>
                            <button class="color-btn w-8 h-8 rounded bg-yellow-500 border-2 border-gray-600" data-color="ffff00"></button>
                            <button class="color-btn w-8 h-8 rounded bg-cyan-500 border-2 border-gray-600" data-color="00ffff"></button>
                            <button class="color-btn w-8 h-8 rounded bg-magenta-500 border-2 border-gray-600" data-color="ff00ff"></button>
                        </div>
                    </div>
                </div>

                <div>
                    <label class="block text-sm mb-2">Brush Size: <span id="brush-size-value">4</span>px</label>
                    <input type="range" id="brush-size" min="1" max="200" value="4" class="w-full">
                </div>

                <h2 class="text-xl font-semibold border-b border-gray-700 pb-2">Video Input</h2>
                <div>
                    <label class="block text-sm mb-2">Input Source</label>
                    <select id="input-select" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600">
                        <option value="">Loading...</option>
                    </select>
                </div>

                <h2 class="text-xl font-semibold border-b border-gray-700 pb-2">Canvas Actions</h2>
                <div class="flex gap-3 flex-wrap">
                    <button id="btn-undo" class="bg-gray-600 hover:bg-gray-500 px-4 py-2 rounded">Undo</button>
                    <button id="btn-redo" class="bg-gray-600 hover:bg-gray-500 px-4 py-2 rounded">Redo</button>
                    <button id="btn-clear" class="bg-red-600 hover:bg-red-700 px-4 py-2 rounded">Clear Canvas</button>
                    <button id="btn-freeze" class="bg-blue-600 hover:bg-blue-700 px-4 py-2 rounded">Freeze Frame</button>
                </div>

                <h2 class="text-xl font-semibold border-b border-gray-700 pb-2">Status</h2>
                <div class="grid grid-cols-2 gap-4 text-sm">
                    <div class="bg-gray-700 p-3 rounded">
                        <span class="text-gray-400">FPS:</span> <span id="fps-display">--</span>
                    </div>
                    <div class="bg-gray-700 p-3 rounded">
                        <span class="text-gray-400">Resolution:</span> <span id="res-display">--</span>
                    </div>
                </div>
            </div>
        </div>
    </div>

    <script>
    const API_BASE = '/api/v1';
    let token = localStorage.getItem('annotator_token') || '';
    let username = localStorage.getItem('annotator_user') || '';

    async function api(endpoint, opts = {}) {
        if (token) endpoint += (endpoint.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(token);
        const res = await fetch(API_BASE + endpoint, opts);
        const json = await res.json();
        if (!json.ok && res.status === 401) logout();
        return json;
    }

    async function checkAuth() {
        if (!token) { showLogin(); return; }
        try {
            const r = await api('/state');
            if (r.ok) {
                document.getElementById('user-display').textContent = r.authenticated_as || username;
                showPortal();
                updateState(r);
            } else { showLogin(); }
        } catch { showLogin(); }
    }

    function showLogin() {
        document.getElementById('login-view').classList.remove('hidden');
        document.getElementById('portal-view').classList.add('hidden');
    }

    function showPortal() {
        document.getElementById('login-view').classList.add('hidden');
        document.getElementById('portal-view').classList.remove('hidden');
    }

    function logout() {
        token = '';
        username = '';
        localStorage.removeItem('annotator_token');
        localStorage.removeItem('annotator_user');
        showLogin();
    }

    function updateState(state) {
        // Update tool
        if (state.tool) document.getElementById('tool-select').value = state.tool;
        // Update brush size
        if (state.brush_size) {
            document.getElementById('brush-size').value = state.brush_size;
            document.getElementById('brush-size-value').textContent = state.brush_size;
        }
    }

    // Login form
    document.getElementById('login-form').onsubmit = async (e) => {
        e.preventDefault();
        const u = document.getElementById('username').value;
        const p = document.getElementById('password').value;
        const err = document.getElementById('login-error');
        try {
            const r = await fetch('/portal/login', {
                method: 'POST',
                headers: {'Content-Type': 'application/x-www-form-urlencoded'},
                body: 'username=' + encodeURIComponent(u) + '&password=' + encodeURIComponent(p)
            });
            const json = await r.json();
            if (json.ok) {
                token = json.token;
                username = json.username;
                localStorage.setItem('annotator_token', token);
                localStorage.setItem('annotator_user', username);
                err.classList.add('hidden');
                checkAuth();
            } else {
                err.textContent = json.error || 'Login failed';
                err.classList.remove('hidden');
                document.getElementById('login-form').classList.add('shake');
                setTimeout(() => document.getElementById('login-form').classList.remove('shake'), 300);
            }
        } catch(e) { err.textContent = 'Connection error'; err.classList.remove('hidden'); }
    };

    // Logout
    document.getElementById('logout-btn').onclick = async () => {
        await fetch('/portal/logout', {method:'POST', body:'token='+encodeURIComponent(token)});
        logout();
    };

    // Tool select
    document.getElementById('tool-select').onchange = async (e) => {
        await api('/tool?name=' + encodeURIComponent(e.target.value), {method:'POST'});
    };

    // Color buttons
    document.querySelectorAll('.color-btn').forEach(btn => {
        btn.onclick = async () => {
            document.querySelectorAll('.color-btn').forEach(b => b.classList.remove('border-blue-500'));
            btn.classList.add('border-blue-500');
            await api('/color?hex=' + btn.dataset.color, {method:'POST'});
        };
    });

    // Brush size
    document.getElementById('brush-size').oninput = (e) => {
        document.getElementById('brush-size-value').textContent = e.target.value;
    };
    document.getElementById('brush-size').onchange = async (e) => {
        await api('/brush_size?px=' + e.target.value, {method:'POST'});
    };

    // Canvas actions
    document.getElementById('btn-undo').onclick = () => api('/tool?name=undo', {method:'POST'});
    document.getElementById('btn-redo').onclick = () => api('/tool?name=redo', {method:'POST'});
    document.getElementById('btn-clear').onclick = () => api('/tool?name=clear', {method:'POST'});
    document.getElementById('btn-freeze').onclick = () => api('/tool?name=freeze', {method:'POST'});

    // Load inputs
    async function loadInputs() {
        try {
            const r = await api('/inputs');
            const sel = document.getElementById('input-select');
            sel.innerHTML = r.inputs.map(i => '<option value="'+i.index+'">'+i.name+'</option>').join('');
            if (r.active !== undefined) sel.value = r.active;
            sel.onchange = () => api('/input?index=' + sel.value, {method:'POST'});
        } catch {}
    }

    // Poll state
    async function poll() {
        try {
            const r = await api('/state');
            if (r.ok) updateState(r);
        } catch {}
    }

    // Init
    checkAuth();
    loadInputs();
    setInterval(poll, 2000);
    </script>
</body>
</html>"#;
