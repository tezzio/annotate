//! Minimal HTTP/1.1 REST API server — Crestron Pro3 integration.
//!
//! See ROADMAP.md.  Design constraints:
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
        Arc, RwLock, RwLockReadGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::Duration,
};

use crate::{
    config::Config,
    export, persist,
    ratelimit::{self, Decision, RateLimiter},
    state::{
        ApiCommand, ApiCommandKind, ApiPayload, ApiResult, StateSnapshot, parse_hex_color,
        parse_palette_index, parse_usize,
    },
    tools::Tool,
};

/// Simple session token -> username mapping
type SessionStore = Arc<RwLock<HashMap<String, String>>>;

/// Shared, mutable handle to the configuration.
///
/// The portal needs to change a user's password, and that change has to
/// outlive the request — so the config is shared rather than cloned per
/// connection.  The main loop persists it to disk.
type SharedConfig = Arc<RwLock<Config>>;

/// Set by the API when it changes the config, so the main loop knows to
/// write it to disk.
pub type ConfigDirtyFlag = Arc<AtomicBool>;

/// The shared state every connection handler needs.
///
/// Bundled into one struct so the handler signatures stay readable.
struct ServerState {
    snapshot: SharedSnapshot,
    cmd_tx: SyncSender<ApiCommand>,
    cfg: SharedConfig,
    sessions: SessionStore,
    limiter: Arc<RateLimiter>,
    config_dirty: ConfigDirtyFlag,
}

/// Snapshot handle shared between the main loop (writer) and the API threads.
pub type SharedSnapshot = Arc<RwLock<StateSnapshot>>;

/// Receiving end of the mutation channel, owned by the main loop.
pub type ApiCommandRx = Receiver<ApiCommand>;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_IN_FLIGHT: usize = 4;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a connection thread waits for the main loop to apply a mutation.
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);
/// Channel depth — a Crestron sends one command at a time; 16 is generous.
const COMMAND_QUEUE: usize = 16;

// ── Server ────────────────────────────────────────────────────────────────────

/// Bind the listener and serve on a detached thread.  Returns the command
/// receiver the main loop drains each frame.  Bind failure logs and disables
/// the API — a kiosk box must keep running even without it.
pub fn serve(
    bind: &str,
    port: u16,
    snapshot: SharedSnapshot,
    cfg: std::sync::Arc<std::sync::RwLock<Config>>,
    config_dirty: ConfigDirtyFlag,
) -> ApiCommandRx {
    let cfg: SharedConfig = cfg;
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
    let limiter = Arc::new(RateLimiter::with_defaults());
    if let Err(e) = std::thread::Builder::new()
        .name("api-accept".into())
        .spawn(move || {
            accept_loop(
                listener,
                snapshot,
                cmd_tx,
                cfg,
                sessions,
                limiter,
                config_dirty,
            )
        })
    {
        eprintln!("[api] cannot spawn accept thread: {e} — REST API disabled");
    } else {
        eprintln!("[api] listening on http://{addr}/api/v1");
    }
    cmd_rx
}

fn accept_loop(
    listener: TcpListener,
    snapshot: SharedSnapshot,
    cmd_tx: SyncSender<ApiCommand>,
    cfg: SharedConfig,
    sessions: SessionStore,
    limiter: Arc<RateLimiter>,
    config_dirty: ConfigDirtyFlag,
) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let server_state = ServerState {
        snapshot,
        cmd_tx,
        cfg,
        sessions,
        limiter,
        config_dirty,
    };
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
            let _ = write_response(
                &mut stream,
                503,
                &err_body("busy — too many connections"),
                "application/json",
            );
            continue;
        }

        in_flight.fetch_add(1, Ordering::SeqCst);
        let count = in_flight.clone();
        let state = ServerState {
            snapshot: server_state.snapshot.clone(),
            cmd_tx: server_state.cmd_tx.clone(),
            cfg: server_state.cfg.clone(),
            sessions: server_state.sessions.clone(),
            limiter: server_state.limiter.clone(),
            config_dirty: server_state.config_dirty.clone(),
        };
        // The client identity is the source IP.  `peer_addr` fails only
        // for an unconnected socket, which cannot happen here.
        let peer = stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let spawned = std::thread::Builder::new().spawn(move || {
            isolate_panic(|| {
                handle_connection(stream, &state, &peer);
            });
            count.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Run `f`, containing any panic it raises.
///
/// A panic in one request must not take the whole API thread with it.
/// This keeps the failure local to the connection: the client gets a
/// 500 and the server carries on serving the next one.
fn isolate_panic<F: FnOnce()>(f: F) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(()) => false,
        Err(_) => {
            eprintln!("[api] panic while handling a connection");
            true
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &ServerState, peer: &str) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let (status, body, ctype) = match read_request(&mut stream) {
        Ok(Some(req)) => {
            // Rate-limit after the path is known — reads and mutations
            // draw on separate budgets.  The portal is not limited.
            if let Some(bucket) = ratelimit::bucket_for(&req.path)
                && let Decision::Reject(wait) = state.limiter.check(peer, bucket)
            {
                let _ = write_rate_limited(&mut stream, wait);
                return;
            }
            route(
                &req,
                &state.snapshot,
                &state.cmd_tx,
                &state.cfg,
                &state.sessions,
                &state.config_dirty,
            )
        }
        Ok(None) => return, // peer connected, sent nothing
        Err(ReadError::TooLarge) => (400, err_body("request too large"), "application/json"),
        Err(ReadError::Malformed) => (400, err_body("malformed request"), "application/json"),
        Err(ReadError::Io(_)) => return, // timeout / reset — peer is gone
    };
    let _ = write_response(&mut stream, status, &body, ctype);
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    content_type: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Status",
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

/// Write a 429 with a `Retry-After` header.
fn write_rate_limited(stream: &mut TcpStream, retry_after_secs: u64) -> std::io::Result<()> {
    let body = err_body("too many requests");
    let head = format!(
        "HTTP/1.1 429 Too Many Requests\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Retry-After: {}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
        retry_after_secs
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
    path: String,
    /// Decoded query-string params (merged with the POSTed form on control calls).
    query: HashMap<String, String>,
    /// Request headers, keyed by lower-cased name (`X-Auth-Token` →
    /// `x-auth-token`).  The first occurrence of a repeated header wins.
    headers: HashMap<String, String>,
    /// Raw request body, form-decoded by the control endpoints.
    body: Vec<u8>,
}

/// Read one request.  `Ok(None)` = clean EOF before any bytes arrived.
fn read_request<R: Read>(r: &mut R) -> Result<Option<Request>, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];

    // ── Header block (terminated by a blank line) ────────────────────────
    let header_len = loop {
        match r.read(&mut chunk) {
            Ok(0) if buf.is_empty() => return Ok(None),
            Ok(0) => return Err(ReadError::Malformed), // EOF mid-headers
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
    let method = parts
        .next()
        .ok_or(ReadError::Malformed)?
        .to_ascii_uppercase();
    let target = parts.next().ok_or(ReadError::Malformed)?.to_string();
    if parts.next().is_none() {
        return Err(ReadError::Malformed);
    }

    // Collect the headers we act on.  `content-length` drives the body
    // read; the rest are kept so the shared API token can arrive in the
    // `X-Auth-Token` header.
    let mut content_length = 0usize;
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().map_err(|_| ReadError::Malformed)?;
            }
            headers.entry(name).or_insert(value);
        }
    }
    if header_len + content_length > MAX_REQUEST_BYTES {
        return Err(ReadError::TooLarge);
    }

    // ── Body ─────────────────────────────────────────────────────────────
    while buf.len() < header_len + content_length {
        match r.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Malformed), // EOF mid-body
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
        headers,
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

fn route(
    req: &Request,
    snapshot: &SharedSnapshot,
    cmd_tx: &SyncSender<ApiCommand>,
    shared_cfg: &SharedConfig,
    sessions: &SessionStore,
    config_dirty: &ConfigDirtyFlag,
) -> (u16, String, &'static str) {
    // Plain-text mode (`?fmt=text`) — the Crestron Pro3 has no JSON parser,
    // so every response in that mode is rendered as `OK …` / `ERR …` lines.
    let text_mode = wants_text(req);

    // A read lock on the shared config.  The password-change route needs
    // a write lock, so it takes one separately — the read guard is
    // dropped before that happens.
    let cfg_guard = shared_cfg.read().unwrap_or_else(|p| p.into_inner());
    let cfg = &*cfg_guard;

    // ── Portal routes ────────────────────────────────────────────────────────────
    match req.path.as_str() {
        "/portal/login" if req.method == "POST" => {
            let p = params(req);
            let username = p.get("username").cloned().unwrap_or_default();
            let password = p.get("password").cloned().unwrap_or_default();
            // Only the users within the cap are honoured, so an
            // over-long list in the config file cannot widen the
            // account set.
            let user = cfg
                .active_users()
                .iter()
                .find(|u| u.username == username && u.verify_password(&password))
                .cloned();
            if let Some(user) = user {
                let token = generate_token();
                sessions
                    .write()
                    .unwrap()
                    .insert(token.clone(), user.username.clone());
                return (
                    200,
                    serde_json::json!({
                        "ok": true,
                        "token": token,
                        "username": user.username,
                        // The client shows a "change your password" prompt
                        // when this is set — it is how a first-run account
                        // is forced off its generated password.
                        "must_change_password": user.must_change_password,
                    })
                    .to_string(),
                    "application/json",
                );
            } else {
                return (401, err_body("invalid credentials"), "application/json");
            }
        }
        // Change the logged-in user's password.
        "/portal/change_password" if req.method == "POST" => {
            let p = params(req);
            let token = match p.get("token") {
                Some(t) => t.clone(),
                None => return (401, err_body("not authenticated"), "application/json"),
            };
            let username = match sessions.read().unwrap().get(&token).cloned() {
                Some(u) => u,
                None => return (401, err_body("not authenticated"), "application/json"),
            };
            let new_password = p.get("password").cloned().unwrap_or_default();
            if new_password.len() < 8 {
                return (
                    400,
                    err_body("the new password must be at least 8 characters"),
                    "application/json",
                );
            }
            // The change is applied to the live config, which the main
            // loop persists.  The read guard above must be dropped first.
            drop(cfg_guard);
            let mut cfg_write = shared_cfg.write().unwrap_or_else(|p| p.into_inner());
            return match crate::config::set_password(&mut cfg_write, &username, &new_password) {
                Ok(()) => {
                    // The main loop persists the config; tell it to.
                    config_dirty.store(true, Ordering::SeqCst);
                    (
                        200,
                        serde_json::json!({ "ok": true }).to_string(),
                        "application/json",
                    )
                }
                Err(msg) => (400, err_body(&msg), "application/json"),
            };
        }
        "/portal/logout" if req.method == "POST" => {
            let p = params(req);
            if let Some(token) = p.get("token") {
                sessions.write().unwrap().remove(token);
            }
            return (
                200,
                serde_json::json!({ "ok": true }).to_string(),
                "application/json",
            );
        }
        "/portal/check" if req.method == "GET" => {
            let p = &req.query;
            if let Some(token) = p.get("token")
                && let Some(username) = sessions.read().unwrap().get(token)
            {
                return (
                    200,
                    serde_json::json!({ "ok": true, "username": username }).to_string(),
                    "application/json",
                );
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

    // ── Shared-token authentication for /api/v1/* ─────────────────────────
    // Two kinds of credential are accepted on the REST API:
    //
    // * the shared token from `[api] token = "..."` — for machine clients
    //   (the Crestron Pro3), sent as `X-Auth-Token` or `token=`
    // * a portal session token — for the browser portal
    //
    // When no shared token is configured the API is open (and the startup
    // log says so), which keeps single-user kiosk installs working.
    let authenticated = session_user(req, sessions);

    if req.path.starts_with("/api/v1/")
        && let Some(expected) = cfg.api.token.as_deref().filter(|t| !t.is_empty())
        && !authenticated.is_some()
    {
        let provided = req
            .headers
            .get("x-auth-token")
            .map(|s| s.as_str())
            .or_else(|| req.query.get("token").map(|s| s.as_str()));
        let ok = provided.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
        if !ok {
            return reply_err(401, "unauthorized", text_mode);
        }
    }

    // ── API routes ─────────────────────────────────────────────────────────
    match req.path.as_str() {
        // ── Read endpoints (GET only) ─────────────────────────────────────
        "/api/v1/health" | "/api/v1/state" | "/api/v1/inputs" if req.method != "GET" => {
            reply_err(405, "method not allowed — use GET", text_mode)
        }
        "/api/v1/health" => reply(
            200,
            ApiPayload::Ok(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
            })),
            text_mode,
        ),
        "/api/v1/state" => {
            let snap = read_snapshot(snapshot);
            if text_mode {
                return (200, text_state(&snap), "text/plain");
            }
            let mut v = serde_json::to_value(&*snap).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                // `input_mode` is the mode index the capture thread was
                // actually started with (0 = the device's "best" mode).
                // It is authoritative on every path — the menu bar, the
                // device picker and the API all report it the same way.
                if let Some(ref user) = authenticated {
                    obj.insert("authenticated_as".into(), user.clone().into());
                }
            }
            reply(200, ApiPayload::Ok(v), text_mode)
        }
        "/api/v1/inputs" => {
            let snap = read_snapshot(snapshot);
            if text_mode {
                return (200, text_inputs(&snap), "text/plain");
            }
            reply(
                200,
                ApiPayload::Ok(serde_json::json!({
                    "active": snap.input,
                    "input_mode": snap.input_mode,
                    "device_present": snap.capture_device_present,
                    "mode_label": snap
                        .inputs
                        .get(snap.input)
                        .and_then(|i| i.modes.get(snap.input_mode))
                        .cloned()
                        .unwrap_or_default(),
                    "inputs": snap.inputs,
                })),
                text_mode,
            )
        }

        // ── Canvas action endpoints (undo / redo / clear / freeze) ─────────
        // These are deliberately separate routes rather than extra
        // `/api/v1/tool?name=…` values, so a tool name stays unambiguous.
        "/api/v1/undo" | "/api/v1/redo" | "/api/v1/clear" | "/api/v1/freeze" => {
            if req.method != "GET" && req.method != "POST" {
                return reply_err(405, "method not allowed — use GET or POST", text_mode);
            }
            let kind = match action_command(req) {
                Ok(k) => k,
                Err(msg) => return reply_err(400, &msg, text_mode),
            };
            let (s, payload) = dispatch(kind, cmd_tx);
            reply(s, payload, text_mode)
        }

        // ── Save / load endpoints ─────────────────────────────────────────
        "/api/v1/save" | "/api/v1/load" => {
            if req.method != "GET" && req.method != "POST" {
                return reply_err(405, "method not allowed — use GET or POST", text_mode);
            }
            let kind = match action_command(req) {
                Ok(k) => k,
                Err(msg) => return reply_err(400, &msg, text_mode),
            };
            let (s, payload) = dispatch(kind, cmd_tx);
            reply(s, payload, text_mode)
        }

        // ── Export endpoints ───────────────────────────────────────────────
        "/api/v1/export" => {
            if req.method != "GET" && req.method != "POST" {
                return reply_err(405, "method not allowed — use GET or POST", text_mode);
            }
            let kind = match action_command(req) {
                Ok(k) => k,
                Err(msg) => return reply_err(400, &msg, text_mode),
            };
            let (s, payload) = dispatch(kind, cmd_tx);
            reply(s, payload, text_mode)
        }

        // ── List saved scenes ──────────────────────────────────────────────
        "/api/v1/saves" => {
            let saves = persist::list_saves();
            reply(
                200,
                ApiPayload::Ok(serde_json::json!({
                    "count": saves.len(),
                    "saves": saves,
                })),
                text_mode,
            )
        }

        // ── List exports ───────────────────────────────────────────────────
        "/api/v1/exports" => {
            let exports = export::list_exports();
            reply(
                200,
                ApiPayload::Ok(serde_json::json!({
                    "count": exports.len(),
                    "exports": exports,
                })),
                text_mode,
            )
        }

        // ── Control endpoints (GET with query string or POST with form body) ──
        "/api/v1/tool" | "/api/v1/color" | "/api/v1/brush_size" | "/api/v1/input" => {
            if req.method != "GET" && req.method != "POST" {
                return reply_err(405, "method not allowed — use GET or POST", text_mode);
            }
            let kind = match control_command(req) {
                Ok(k) => k,
                Err(msg) => return reply_err(400, &msg, text_mode),
            };
            let (s, payload) = dispatch(kind, cmd_tx);
            reply(s, payload, text_mode)
        }

        _ => reply_err(404, "not found", text_mode),
    }
}

/// Render a payload in the wire format the caller asked for.
fn reply(status: u16, payload: ApiPayload, text_mode: bool) -> (u16, String, &'static str) {
    if text_mode {
        (status, payload.to_text(), "text/plain")
    } else {
        (status, payload.to_json(), "application/json")
    }
}

/// Render an error in the wire format the caller asked for.
fn reply_err(status: u16, msg: &str, text_mode: bool) -> (u16, String, &'static str) {
    reply(status, ApiPayload::Err(msg.to_string()), text_mode)
}

/// Plain-text rendering of `/api/v1/state`.
///
/// The headline fields come first on one line, so a Crestron can read the
/// common case with a single line match; the remaining fields follow.
fn text_state(snap: &StateSnapshot) -> String {
    let mut out = format!(
        "OK tool={} color={} size={} input={} input_mode={} freeze={} device_present={}\n",
        snap.tool,
        snap.color,
        snap.brush_size,
        snap.input,
        snap.input_mode,
        u8::from(snap.freeze),
        u8::from(snap.capture_device_present),
    );
    out.push_str(&format!(
        "OK objects={} can_undo={} can_redo={} selected={} drawing={}\n",
        snap.object_count,
        u8::from(snap.can_undo),
        u8::from(snap.can_redo),
        snap.selected,
        u8::from(snap.drawing),
    ));
    out.push_str(&format!(
        "OK fullscreen={} laser_on={} laser_fade={} show_fps={} show_time={} hide_toolbar={}\n",
        u8::from(snap.fullscreen),
        u8::from(snap.laser_on),
        snap.laser_fade,
        u8::from(snap.show_fps),
        u8::from(snap.show_time),
        u8::from(snap.hide_toolbar),
    ));
    out.push_str(&format!(
        "OK fps={} capture_fps={} window={}x{} undo_limit={} version={}\n",
        snap.fps,
        snap.capture_fps,
        snap.window_size[0],
        snap.window_size[1],
        snap.undo_limit,
        snap.version,
    ));
    out.push_str(&format!(
        "OK text_input_active={} device_picker_open={}",
        u8::from(snap.text_input_active),
        u8::from(snap.device_picker_open),
    ));
    out.trim_end().to_string()
}

/// Plain-text rendering of `/api/v1/inputs`.
///
/// The active device is on the first line; one `input=<index>:<name>` line
/// follows per device.
fn text_inputs(snap: &StateSnapshot) -> String {
    let mut out = format!(
        "OK active={} input_mode={} count={} device_present={}\n",
        snap.input,
        snap.input_mode,
        snap.inputs.len(),
        u8::from(snap.capture_device_present),
    );
    for i in &snap.inputs {
        out.push_str(&format!("input={}:{}\n", i.index, i.name));
    }
    out.trim_end().to_string()
}

/// Whether the caller asked for the plain-text wire format.
///
/// The parameter is accepted in the query string or in a POSTed form body,
/// so a Pro3 can use either `GET /state?fmt=text` or
/// `POST /state` with `fmt=text` in the body.
fn wants_text(req: &Request) -> bool {
    params(req)
        .get("fmt")
        .is_some_and(|v| v.eq_ignore_ascii_case("text"))
}

/// The portal session token carried by a request, if it is valid.
fn session_user(req: &Request, sessions: &SessionStore) -> Option<String> {
    let token = req
        .headers
        .get("x-auth-token")
        .map(|s| s.as_str())
        .or_else(|| req.query.get("token").map(|s| s.as_str()))?;
    sessions.read().unwrap().get(token).cloned()
}

/// Compare two byte strings without an early return on the first
/// difference — a timing side channel would otherwise leak the token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
    if req.method == "POST"
        && !req.body.is_empty()
        && let Ok(form) = std::str::from_utf8(&req.body)
    {
        p.extend(parse_query(form));
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
            let tool = Tool::from_name(name).ok_or_else(|| format!("unknown tool '{name}'"))?;
            Ok(ApiCommandKind::SetTool(tool))
        }
        "/api/v1/color" => match (p.get("hex"), p.get("palette")) {
            (Some(h), _) => {
                let (r, g, b) = parse_hex_color(h)?;
                Ok(ApiCommandKind::SetColor(r, g, b))
            }
            (None, Some(pal)) => {
                let (r, g, b) = parse_palette_index(pal)?;
                Ok(ApiCommandKind::SetColor(r, g, b))
            }
            (None, None) => Err("missing param 'hex' or 'palette'".into()),
        },
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

/// Turn a canvas-action request into a validated command.
///
/// These live on their own routes (`/api/v1/undo`, `/api/v1/redo`,
/// `/api/v1/clear`, `/api/v1/freeze`) rather than as extra values of
/// `/api/v1/tool?name=…`, so that a tool name stays unambiguous.
fn action_command(req: &Request) -> Result<ApiCommandKind, String> {
    let p = params(req);
    match req.path.as_str() {
        "/api/v1/undo" => Ok(ApiCommandKind::Undo),
        "/api/v1/redo" => Ok(ApiCommandKind::Redo),
        "/api/v1/clear" => Ok(ApiCommandKind::Clear),
        "/api/v1/freeze" => {
            // `on=1|0` sets the flag explicitly; `toggle` flips it; the
            // bare endpoint toggles as well.
            match p.get("on") {
                Some(v) if v.eq_ignore_ascii_case("toggle") => Ok(ApiCommandKind::ToggleFreeze),
                Some(v) if v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true") => {
                    Ok(ApiCommandKind::SetFreeze(true))
                }
                Some(v) if v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false") => {
                    Ok(ApiCommandKind::SetFreeze(false))
                }
                Some(v) => Err(format!("bad 'on' value '{v}' — expected 1, 0, or toggle")),
                None => Ok(ApiCommandKind::ToggleFreeze),
            }
        }
        "/api/v1/save" => {
            let name = p.get("name").cloned().unwrap_or_default();
            validate_scene_name(&name)?;
            Ok(ApiCommandKind::SaveScene { name })
        }
        "/api/v1/load" => {
            let name = p.get("name").ok_or("missing param 'name'")?.clone();
            validate_scene_name(&name)?;
            Ok(ApiCommandKind::LoadScene { name })
        }
        "/api/v1/export" => {
            let name = p.get("name").cloned().unwrap_or_default();
            validate_scene_name(&name)?;
            Ok(ApiCommandKind::Export { name })
        }
        _ => Err("unknown action endpoint".into()),
    }
}

/// Reject a scene name that would escape the save directory.
///
/// The name becomes a file name, so it must not contain a path
/// separator, a parent-directory reference, or anything else that could
/// escape the save directory.
fn validate_scene_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Ok(()); // empty means "generate one"
    }
    if name.len() > 128 {
        return Err("scene name too long (max 128 characters)".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("scene name must not contain a path separator".into());
    }
    if name.contains("..") {
        return Err("scene name must not reference a parent directory".into());
    }
    if name.starts_with('.') {
        return Err("scene name must not start with a dot".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err("scene name may only contain letters, digits, '_', '-' and '.'".into());
    }
    Ok(())
}

/// Hand a validated command to the main loop and wait for its verdict.
fn dispatch(kind: ApiCommandKind, cmd_tx: &SyncSender<ApiCommand>) -> (u16, ApiPayload) {
    let (reply_tx, reply_rx) = sync_channel::<ApiResult>(1);
    if cmd_tx
        .send(ApiCommand {
            kind,
            reply: reply_tx,
        })
        .is_err()
    {
        return (503, ApiPayload::Err("main loop unavailable".into()));
    }
    let result = match reply_rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => ApiResult::NoReply,
        Err(RecvTimeoutError::Disconnected) => ApiResult::NoReply,
    };
    result.into_payload()
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

    <div id="change-view" class="hidden flex items-center justify-center min-h-screen">
        <div class="bg-gray-800 p-8 rounded-lg shadow-lg w-80">
            <h1 class="text-2xl font-bold mb-2 text-center">Change Password</h1>
            <p class="text-sm text-gray-400 mb-6 text-center">
                This password was generated for you. Choose a new one to continue.
            </p>
            <form id="change-form" class="space-y-4">
                <div>
                    <label class="block text-sm mb-1">New Password</label>
                    <input type="password" id="new-password" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600 focus:border-blue-500" minlength="8" required>
                </div>
                <div>
                    <label class="block text-sm mb-1">Confirm Password</label>
                    <input type="password" id="new-password2" class="w-full px-3 py-2 bg-gray-700 rounded border border-gray-600 focus:border-blue-500" minlength="8" required>
                </div>
                <p id="change-error" class="text-red-400 text-sm hidden"></p>
                <button type="submit" class="w-full bg-blue-600 hover:bg-blue-700 py-2 rounded font-medium">Change Password</button>
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

                <p class="text-sm text-gray-400">
                    Undo, redo, clear and freeze are also available over the
                    REST API as <code>/api/v1/undo</code>,
                    <code>/api/v1/redo</code>, <code>/api/v1/clear</code>
                    and <code>/api/v1/freeze</code>.
                </p>

                <h2 class="text-xl font-semibold border-b border-gray-700 pb-2">Status</h2>
                <p id="device-warning" class="hidden bg-yellow-600 text-gray-900 px-3 py-2 rounded text-sm font-medium">
                    The capture device is unplugged. Plug it back in and it will reconnect automatically.
                </p>
                <div class="grid grid-cols-2 gap-4 text-sm">
                    <div class="bg-gray-700 p-3 rounded">
                        <span class="text-gray-400">FPS:</span> <span id="fps-display">--</span>
                    </div>
                    <div class="bg-gray-700 p-3 rounded">
                        <span class="text-gray-400">Resolution:</span> <span id="res-display">--</span>
                    </div>
                </div>
                <p id="status-line" class="text-sm hidden"></p>
            </div>
        </div>
    </div>

    <script>
    const API_BASE = '/api/v1';
    let token = localStorage.getItem('annotator_token') || '';
    let username = localStorage.getItem('annotator_user') || '';
    let statusTimer = null;

    async function api(endpoint, opts = {}) {
        if (token) endpoint += (endpoint.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(token);
        const res = await fetch(API_BASE + endpoint, opts);
        const json = await res.json();
        if (!json.ok && res.status === 401) logout();
        return json;
    }

    // ── Status line ──────────────────────────────────────────────────────
    // Every command result is surfaced here: a rejected command (e.g. an
    // out-of-range input) must be visible, not silent.
    function setStatus(msg, ok) {
        const el = document.getElementById('status-line');
        if (!el) return;
        el.textContent = msg;
        el.classList.remove('hidden', 'text-red-400', 'text-green-400', 'text-gray-400');
        el.classList.add(ok ? 'text-green-400' : 'text-red-400');
        clearTimeout(statusTimer);
        statusTimer = setTimeout(() => {
            el.classList.add('hidden');
            el.textContent = '';
        }, 4000);
    }

    // Run a command and report the outcome on the status line.
    async function run(endpoint, label) {
        try {
            const r = await api(endpoint, {method:'POST'});
            if (r.ok) {
                setStatus(label + ' OK', true);
            } else {
                setStatus(label + ' failed: ' + (r.error || 'unknown error'), false);
            }
            // Re-read the state either way: on success it shows the change
            // immediately, on failure it snaps the control back to the
            // value the server actually holds.
            await poll();
            return r;
        } catch (e) {
            setStatus(label + ' failed: ' + e.message, false);
            return {ok:false, error:String(e)};
        }
    }

    async function checkAuth() {
        if (!token) { showLogin(); return; }
        try {
            const r = await api('/state');
            if (r.ok) {
                document.getElementById('user-display').textContent = r.authenticated_as || username;
                showPortal();
                updateState(r);
                // The input list needs a valid session token, so it can
                // only be fetched once we are logged in.
                loadInputs();
            } else { showLogin(); }
        } catch { showLogin(); }
    }

    function showLogin() {
        document.getElementById('login-view').classList.remove('hidden');
        document.getElementById('change-view').classList.add('hidden');
        document.getElementById('portal-view').classList.add('hidden');
    }

    function showPortal() {
        document.getElementById('login-view').classList.add('hidden');
        document.getElementById('change-view').classList.add('hidden');
        document.getElementById('portal-view').classList.remove('hidden');
    }

    // Shown when the account still has its generated password.
    function showChangePassword() {
        document.getElementById('login-view').classList.add('hidden');
        document.getElementById('change-view').classList.remove('hidden');
        document.getElementById('portal-view').classList.add('hidden');
    }

    function logout() {
        token = '';
        username = '';
        localStorage.removeItem('annotator_token');
        localStorage.removeItem('annotator_user');
        showLogin();
    }

    // Refresh the controls from the server's state, so a change made from
    // the Crestron shows up here within a second.
    function updateState(state) {
        // Update tool
        if (state.tool) {
            const sel = document.getElementById('tool-select');
            if (sel.value !== state.tool) sel.value = state.tool;
        }
        // Update brush size
        if (state.brush_size) {
            const slider = document.getElementById('brush-size');
            // Don't fight the user while they are dragging the slider.
            if (document.activeElement !== slider && slider.value !== String(state.brush_size)) {
                slider.value = state.brush_size;
            }
            document.getElementById('brush-size-value').textContent = state.brush_size;
        }
        // Update colour
        if (state.color) {
            document.querySelectorAll('.color-btn').forEach(b => {
                const on = b.dataset.color.toLowerCase() === state.color.toLowerCase();
                b.classList.toggle('border-blue-500', on);
            });
        }
        // Update the active input
        if (state.input !== undefined) {
            const sel = document.getElementById('input-select');
            // Only select once the real device list is in place — the
            // placeholder option has no matching value.
            if (sel.options.length > 1 && sel.value !== String(state.input)) {
                sel.value = state.input;
            }
        }
        // Update the freeze button label
        const freezeBtn = document.getElementById('btn-freeze');
        if (freezeBtn) {
            freezeBtn.textContent = state.freeze ? 'Unfreeze Frame' : 'Freeze Frame';
            freezeBtn.classList.toggle('bg-blue-600', !state.freeze);
            freezeBtn.classList.toggle('bg-green-600', !!state.freeze);
            freezeBtn.classList.toggle('hover:bg-blue-700', !state.freeze);
            freezeBtn.classList.toggle('hover:bg-green-700', !!state.freeze);
        }
        // Warn when the capture device has been unplugged.
        const warn = document.getElementById('device-warning');
        if (warn) {
            warn.classList.toggle('hidden', state.capture_device_present !== false);
        }
        // Update the undo/redo button availability
        const undoBtn = document.getElementById('btn-undo');
        const redoBtn = document.getElementById('btn-redo');
        if (undoBtn) {
            undoBtn.disabled = !state.can_undo;
            undoBtn.classList.toggle('opacity-40', !state.can_undo);
        }
        if (redoBtn) {
            redoBtn.disabled = !state.can_redo;
            redoBtn.classList.toggle('opacity-40', !state.can_redo);
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
                // A generated password must be changed before the portal
                // can be used.
                if (json.must_change_password) {
                    showChangePassword();
                } else {
                    checkAuth();
                }
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

    // Change-password form
    document.getElementById('change-form').onsubmit = async (e) => {
        e.preventDefault();
        const p1 = document.getElementById('new-password').value;
        const p2 = document.getElementById('new-password2').value;
        const err = document.getElementById('change-error');
        if (p1 !== p2) {
            err.textContent = 'The passwords do not match';
            err.classList.remove('hidden');
            return;
        }
        if (p1.length < 8) {
            err.textContent = 'The password must be at least 8 characters';
            err.classList.remove('hidden');
            return;
        }
        try {
            const r = await fetch('/portal/change_password', {
                method: 'POST',
                headers: {'Content-Type': 'application/x-www-form-urlencoded'},
                body: 'token=' + encodeURIComponent(token) + '&password=' + encodeURIComponent(p1)
            });
            const json = await r.json();
            if (json.ok) {
                err.classList.add('hidden');
                document.getElementById('new-password').value = '';
                document.getElementById('new-password2').value = '';
                checkAuth();
            } else {
                err.textContent = json.error || 'Change failed';
                err.classList.remove('hidden');
            }
        } catch(e) {
            err.textContent = 'Connection error';
            err.classList.remove('hidden');
        }
    };

    // Tool select
    document.getElementById('tool-select').onchange = async (e) => {
        await run('/tool?name=' + encodeURIComponent(e.target.value), 'Tool change');
    };

    // Color buttons
    document.querySelectorAll('.color-btn').forEach(btn => {
        btn.onclick = async () => {
            await run('/color?hex=' + btn.dataset.color, 'Color change');
        };
    });

    // Brush size
    document.getElementById('brush-size').oninput = (e) => {
        document.getElementById('brush-size-value').textContent = e.target.value;
    };
    document.getElementById('brush-size').onchange = async (e) => {
        await run('/brush_size?px=' + e.target.value, 'Brush size');
    };

    // Canvas actions — each has its own endpoint now; `/tool?name=undo`
    // was never a valid tool and returned 400.
    document.getElementById('btn-undo').onclick = () => run('/undo', 'Undo');
    document.getElementById('btn-redo').onclick = () => run('/redo', 'Redo');
    document.getElementById('btn-clear').onclick = () => run('/clear', 'Clear canvas');
    document.getElementById('btn-freeze').onclick = () => run('/freeze', 'Freeze frame');

    // Load inputs
    let inputsLoaded = false;
    async function loadInputs() {
        try {
            const r = await api('/inputs');
            if (!r.ok) return;
            const sel = document.getElementById('input-select');
            sel.innerHTML = r.inputs.map(i => '<option value="'+i.index+'">'+i.name+'</option>').join('');
            if (r.active !== undefined) sel.value = r.active;
            sel.onchange = () => run('/input?index=' + sel.value, 'Input change');
            inputsLoaded = true;
        } catch {}
    }

    // Poll state — ~1 s so a Crestron command shows up almost immediately.
    async function poll() {
        try {
            const r = await api('/state');
            if (r.ok) {
                updateState(r);
                // A device can be plugged in while the portal is open, so
                // the input list is refreshed until it has been fetched at
                // least once.
                if (!inputsLoaded) loadInputs();
            }
        } catch {}
    }

    // Init
    checkAuth();
    setInterval(poll, 1000);
    </script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InputInfo;
    use crate::tools::Tool;

    fn get(target: &str) -> Request {
        let (path, query) = split_target(target);
        Request {
            method: "GET".into(),
            path,
            query,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    /// A request with an extra header (e.g. `X-Auth-Token`).
    fn get_with_header(target: &str, name: &str, value: &str) -> Request {
        let mut r = get(target);
        r.headers
            .insert(name.to_ascii_lowercase(), value.to_string());
        r
    }

    /// A config whose API requires the given shared token.
    fn cfg_with_token(token: &str) -> SharedConfig {
        let mut cfg = Config::default();
        cfg.api.token = Some(token.to_string());
        Arc::new(RwLock::new(cfg))
    }

    /// The default config, as a shared handle.
    fn default_cfg() -> SharedConfig {
        Arc::new(RwLock::new(Config::default()))
    }

    fn test_sessions() -> SessionStore {
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// A config-dirty flag for the tests.
    fn dirty_flag() -> ConfigDirtyFlag {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn percent_decode_handles_plus_hex_and_literals() {
        assert_eq!(percent_decode("a+b%20c%2fD"), "a b c/D");
        assert_eq!(percent_decode("%C3%BF"), "\u{FF}"); // valid UTF-8 for ÿ
        assert_eq!(percent_decode("%FF"), "\u{FFFD}"); // lone byte → replacement char
        assert_eq!(percent_decode("%zz"), "%zz"); // bad hex kept literal
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
        assert!(matches!(
            read_request(&mut garbage),
            Err(ReadError::Malformed)
        ));

        let big = vec![b'X'; MAX_REQUEST_BYTES + 16];
        let mut cur = std::io::Cursor::new(big);
        assert!(matches!(read_request(&mut cur), Err(ReadError::TooLarge)));
    }

    #[test]
    fn tool_name_mapping_round_trips() {
        for t in [
            Tool::Pen,
            Tool::Line,
            Tool::Rect,
            Tool::Circle,
            Tool::Arrow,
            Tool::Text,
            Tool::Select,
            Tool::Eraser,
            Tool::Highlight,
            Tool::Laser,
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
        let cfg = default_cfg();
        let sessions: SessionStore = Arc::new(RwLock::new(HashMap::new()));

        let (status, body, _) = route(
            &get("/api/v1/health"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

        let (status, body, _) = route(&get("/nope"), &snap, &tx, &cfg, &sessions, &dirty_flag());
        assert_eq!(status, 404);
        assert!(body.contains("\"ok\":false"));

        let mut post = get("/api/v1/state");
        post.method = "POST".into();
        let (status, _, _) = route(&post, &snap, &tx, &cfg, &sessions, &dirty_flag());
        assert_eq!(status, 405);
    }

    #[test]
    fn route_state_and_inputs_come_from_snapshot() {
        let mut s = StateSnapshot {
            tool: "pen",
            input: 1,
            ..Default::default()
        };
        s.inputs.push(InputInfo {
            index: 0,
            name: "No input (black screen)".into(),
            path: None,
            modes: vec![],
        });
        let snap: SharedSnapshot = Arc::new(RwLock::new(s));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions: SessionStore = Arc::new(RwLock::new(HashMap::new()));

        let (status, body, _) = route(
            &get("/api/v1/state?fmt=json"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["tool"], "pen");
        assert_eq!(v["input"], 1);

        let (status, body, _) = route(
            &get("/api/v1/inputs"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
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
        assert!(control_command(&get("/api/v1/tool")).is_err()); // missing
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
        let (status, payload) = dispatch(ApiCommandKind::SetTool(Tool::Pen), &tx);
        assert_eq!(status, 503);
        assert!(payload.to_json().contains("\"ok\":false"));
    }

    #[test]
    fn a_panic_in_one_request_does_not_escape() {
        // The hook below keeps the test output readable; the default
        // panic handler would print a backtrace.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let panicked = isolate_panic(|| panic!("boom"));
        assert!(panicked, "the panic should have been caught");

        // The guard must leave the thread usable afterwards.
        let ok = isolate_panic(|| {});
        assert!(!ok);

        std::panic::set_hook(previous);
    }

    // ── P1: canvas action routes ─────────────────────────────────────────

    #[test]
    fn action_endpoints_map_to_their_commands() {
        assert!(matches!(
            action_command(&get("/api/v1/undo")).unwrap(),
            ApiCommandKind::Undo
        ));
        assert!(matches!(
            action_command(&get("/api/v1/redo")).unwrap(),
            ApiCommandKind::Redo
        ));
        assert!(matches!(
            action_command(&get("/api/v1/clear")).unwrap(),
            ApiCommandKind::Clear
        ));
        // Bare `/freeze` toggles.
        assert!(matches!(
            action_command(&get("/api/v1/freeze")).unwrap(),
            ApiCommandKind::ToggleFreeze
        ));
    }

    #[test]
    fn freeze_endpoint_accepts_on_values() {
        assert!(matches!(
            action_command(&get("/api/v1/freeze?on=1")).unwrap(),
            ApiCommandKind::SetFreeze(true)
        ));
        assert!(matches!(
            action_command(&get("/api/v1/freeze?on=0")).unwrap(),
            ApiCommandKind::SetFreeze(false)
        ));
        assert!(matches!(
            action_command(&get("/api/v1/freeze?on=true")).unwrap(),
            ApiCommandKind::SetFreeze(true)
        ));
        assert!(matches!(
            action_command(&get("/api/v1/freeze?on=toggle")).unwrap(),
            ApiCommandKind::ToggleFreeze
        ));
        assert!(action_command(&get("/api/v1/freeze?on=maybe")).is_err());
    }

    #[test]
    fn tool_endpoint_still_rejects_action_names() {
        // The action names must not leak into the tool namespace — a tool
        // name stays unambiguous.
        for name in ["undo", "redo", "clear", "freeze"] {
            assert!(
                control_command(&get(&format!("/api/v1/tool?name={name}"))).is_err(),
                "'{name}' must not be accepted as a tool"
            );
        }
    }

    #[test]
    fn action_routes_reach_the_main_loop() {
        // The route must dispatch the command rather than 404.
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        // No main loop is draining, so the command is queued and the
        // reply times out — what matters is that the route was matched and
        // the command reached the channel.
        let (status, _, _) = route(
            &get("/api/v1/undo"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_ne!(status, 404);
        assert_ne!(status, 405);
        assert!(rx.try_iter().count() >= 1, "command should be queued");
    }

    // ── P2: plain-text format ────────────────────────────────────────────

    #[test]
    fn wants_text_reads_query_and_body() {
        assert!(wants_text(&get("/api/v1/state?fmt=text")));
        assert!(wants_text(&get("/api/v1/state?fmt=TEXT")));
        assert!(!wants_text(&get("/api/v1/state")));
        assert!(!wants_text(&get("/api/v1/state?fmt=json")));

        // A POSTed form body carries the flag too.
        let mut r = get("/api/v1/state");
        r.method = "POST".into();
        r.body = b"fmt=text".to_vec();
        assert!(wants_text(&r));
    }

    #[test]
    fn health_endpoint_renders_plain_text() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        let (status, body, ctype) = route(
            &get("/api/v1/health?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/plain");
        assert!(body.starts_with("OK "), "got: {body}");
        assert!(body.contains("version="));
        assert!(!body.contains('{'), "text mode must not emit JSON: {body}");
    }

    #[test]
    fn state_endpoint_renders_plain_text() {
        let mut s = StateSnapshot {
            tool: "pen",
            color: "ff3333".into(),
            brush_size: 8,
            input: 1,
            ..Default::default()
        };
        s.inputs.push(InputInfo {
            index: 0,
            name: "No input (black screen)".into(),
            path: None,
            modes: vec![],
        });
        let snap: SharedSnapshot = Arc::new(RwLock::new(s));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        let (status, body, ctype) = route(
            &get("/api/v1/state?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/plain");
        assert!(body.contains("tool=pen"), "got: {body}");
        assert!(body.contains("color=ff3333"), "got: {body}");
        assert!(body.contains("size=8"), "got: {body}");
        assert!(body.contains("input=1"), "got: {body}");
        assert!(body.contains("freeze=0"), "got: {body}");
        assert!(!body.contains('{'), "text mode must not emit JSON: {body}");
    }

    #[test]
    fn errors_render_as_err_lines_in_text_mode() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        // A missing required param is a 400 with an `ERR …` body.
        let (status, body, ctype) = route(
            &get("/api/v1/tool?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 400);
        assert_eq!(ctype, "text/plain");
        assert!(body.starts_with("ERR "), "got: {body}");

        // Unknown paths render the same way.
        let (status, body, _) = route(
            &get("/nope?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 404);
        assert!(body.starts_with("ERR "), "got: {body}");
    }

    #[test]
    fn payload_text_rendering() {
        let ok = ApiPayload::Ok(serde_json::json!({"tool": "pen", "brush_size": 4}));
        let text = ok.to_text();
        assert!(text.contains("tool=pen"));
        assert!(text.contains("brush_size=4"));
        // The "ok" marker is not a field — it is the line prefix.
        assert!(!text.contains("ok="));

        let err = ApiPayload::Err("out of range".into());
        assert_eq!(err.to_text(), "ERR out of range");
    }

    #[test]
    fn payload_text_flattens_nested_values() {
        let ok = ApiPayload::Ok(serde_json::json!({
            "active": 1,
            "inputs": [{"index": 0, "name": "No input"}],
        }));
        let text = ok.to_text();
        assert!(text.contains("active=1"), "got: {text}");
        assert!(text.contains("inputs[0].index=0"), "got: {text}");
        assert!(text.contains("inputs[0].name=No input"), "got: {text}");
        assert!(!text.contains('{'), "text mode must not emit JSON: {text}");
    }

    #[test]
    fn inputs_endpoint_renders_plain_text() {
        let mut s = StateSnapshot {
            input: 1,
            input_mode: 2,
            ..Default::default()
        };
        s.inputs.push(InputInfo {
            index: 0,
            name: "No input (black screen)".into(),
            path: None,
            modes: vec![],
        });
        s.inputs.push(InputInfo {
            index: 1,
            name: "USB Camera".into(),
            path: Some("/dev/video0".into()),
            modes: vec!["MJPEG 1920×1080 @30fps".into()],
        });
        let snap: SharedSnapshot = Arc::new(RwLock::new(s));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        let (status, body, ctype) = route(
            &get("/api/v1/inputs?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/plain");
        assert!(body.contains("active=1"), "got: {body}");
        assert!(body.contains("input_mode=2"), "got: {body}");
        assert!(
            body.contains("input=0:No input (black screen)"),
            "got: {body}"
        );
        assert!(body.contains("input=1:USB Camera"), "got: {body}");
        assert!(!body.contains('{'), "text mode must not emit JSON: {body}");
    }

    // ── P3: shared token authentication ──────────────────────────────────

    #[test]
    fn api_requires_the_configured_token() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = cfg_with_token("secret");
        let sessions = test_sessions();

        // No token at all → 401.
        let (status, body, _) = route(
            &get("/api/v1/state"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 401);
        assert!(body.contains("unauthorized"), "got: {body}");

        // Wrong token → 401.
        let (status, _, _) = route(
            &get("/api/v1/state?token=wrong"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 401);

        // Correct token in the query → 200.
        let (status, _, _) = route(
            &get("/api/v1/state?token=secret"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);

        // Correct token in the header → 200.
        let (status, _, _) = route(
            &get_with_header("/api/v1/state", "X-Auth-Token", "secret"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn api_is_open_when_no_token_is_configured() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg(); // token: None
        let sessions = test_sessions();

        let (status, _, _) = route(
            &get("/api/v1/state"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn a_portal_session_token_also_authorises_api_calls() {
        // The portal's own session tokens keep working when a shared
        // token is configured — they are a separate credential.
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = cfg_with_token("secret");
        let sessions = test_sessions();
        sessions
            .write()
            .unwrap()
            .insert("sess".into(), "admin".into());

        let (status, body, _) = route(
            &get("/api/v1/state?token=sess"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        assert!(body.contains("authenticated_as"), "got: {body}");
    }

    #[test]
    fn unauthorized_uses_the_plain_text_form_when_asked() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = cfg_with_token("secret");
        let sessions = test_sessions();

        let (status, body, ctype) = route(
            &get("/api/v1/state?fmt=text"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 401);
        assert_eq!(ctype, "text/plain");
        assert!(body.starts_with("ERR "), "got: {body}");
    }

    #[test]
    fn headers_are_parsed_from_the_request() {
        let raw: &[u8] = b"GET /api/v1/state HTTP/1.1\r\n\
                           Host: anno\r\n\
                           X-Auth-Token: secret\r\n\r\n";
        let mut cur = std::io::Cursor::new(raw);
        let r = read_request(&mut cur).unwrap().unwrap();
        assert_eq!(
            r.headers.get("x-auth-token").map(|s| s.as_str()),
            Some("secret")
        );
        assert_eq!(r.headers.get("host").map(|s| s.as_str()), Some("anno"));
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(constant_time_eq(b"", b""));
    }

    // ── Save / load endpoints ──────────────────────────────────────────

    #[test]
    fn save_endpoint_builds_a_save_command() {
        let r = action_command(&get("/api/v1/save")).unwrap();
        assert!(matches!(r, ApiCommandKind::SaveScene { ref name } if name.is_empty()));

        let r = action_command(&get("/api/v1/save?name=case-1")).unwrap();
        assert!(matches!(r, ApiCommandKind::SaveScene { ref name } if name == "case-1"));
    }

    #[test]
    fn load_endpoint_requires_a_name() {
        let r = action_command(&get("/api/v1/load?name=case-1")).unwrap();
        assert!(matches!(r, ApiCommandKind::LoadScene { ref name } if name == "case-1"));

        // Without a name the command cannot be built.
        assert!(action_command(&get("/api/v1/load")).is_err());
    }

    #[test]
    fn scene_names_cannot_escape_the_save_directory() {
        // A path separator would let a name write outside the save dir.
        assert!(validate_scene_name("ok-name_1.json").is_ok());
        assert!(validate_scene_name("").is_ok());
        assert!(validate_scene_name("../escape").is_err());
        assert!(validate_scene_name("a/b").is_err());
        assert!(validate_scene_name("a\\b").is_err());
        assert!(validate_scene_name("..").is_err());
        assert!(validate_scene_name(".hidden").is_err());
        assert!(validate_scene_name("has space").is_err());
        assert!(validate_scene_name(&"x".repeat(200)).is_err());
    }

    #[test]
    fn a_traversing_scene_name_is_rejected_by_the_endpoint() {
        assert!(action_command(&get("/api/v1/save?name=../escape")).is_err());
        assert!(action_command(&get("/api/v1/load?name=../escape")).is_err());
    }

    #[test]
    fn the_saves_route_lists_saved_scenes() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        let (status, body, ctype) = route(
            &get("/api/v1/saves"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        assert_eq!(ctype, "application/json");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["count"].is_number());
    }

    #[test]
    fn the_export_endpoint_builds_an_export_command() {
        let r = action_command(&get("/api/v1/export")).unwrap();
        assert!(matches!(r, ApiCommandKind::Export { ref name } if name.is_empty()));

        let r = action_command(&get("/api/v1/export?name=case-7")).unwrap();
        assert!(matches!(r, ApiCommandKind::Export { ref name } if name == "case-7"));

        // A traversing name is rejected.
        assert!(action_command(&get("/api/v1/export?name=../escape")).is_err());
    }

    #[test]
    fn the_exports_route_lists_exports() {
        let snap: SharedSnapshot = Arc::new(RwLock::new(StateSnapshot::default()));
        let (tx, _rx) = sync_channel::<ApiCommand>(4);
        let cfg = default_cfg();
        let sessions = test_sessions();

        let (status, body, _) = route(
            &get("/api/v1/exports"),
            &snap,
            &tx,
            &cfg,
            &sessions,
            &dirty_flag(),
        );
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["count"].is_number());
    }
}
