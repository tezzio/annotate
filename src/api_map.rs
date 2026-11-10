//! A self-describing index of the REST API.
//!
//! `GET /api/v1/` lists every endpoint, what it does and what it takes.
//! A client — or a person at the console — can discover the API without
//! having to read the source or guess at parameter names.
//!
//! The map is public: it needs no token, so a client can find out how
//! to authenticate before it has a token.

use serde::Serialize;

/// One documented endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct Endpoint {
    /// The path, e.g. `/api/v1/tool`.
    pub path: &'static str,
    /// The HTTP methods it accepts.
    pub methods: &'static [&'static str],
    /// What the endpoint does.
    pub summary: &'static str,
    /// The query or form parameters it takes.  Each entry is
    /// `name (description)` — the description says whether the parameter
    /// is required.
    pub params: &'static [&'static str],
    /// The values a parameter accepts, where that is not obvious.
    pub values: &'static [&'static str],
    /// The response codes it can return.
    pub returns: &'static [&'static str],
}

/// The tool names the API accepts.
const TOOL_NAMES: &[&str] = &[
    "pen",
    "line",
    "rect",
    "circle",
    "arrow",
    "text",
    "select",
    "eraser",
    "highlight",
    "laser",
];

/// Every endpoint the API exposes.
pub const ENDPOINTS: &[Endpoint] = &[
    // ── Discovery ───────────────────────────────────────────────────────
    Endpoint {
        path: "/api/v1/",
        methods: &["GET"],
        summary: "This map — every endpoint, its parameters and what it returns.",
        params: &[],
        values: &[],
        returns: &["200: the endpoint map"],
    },
    // ── Reading ─────────────────────────────────────────────────────────
    Endpoint {
        path: "/api/v1/health",
        methods: &["GET"],
        summary: "Liveness check and the version of the running build.",
        params: &[],
        values: &[],
        returns: &["200: version"],
    },
    Endpoint {
        path: "/api/v1/state",
        methods: &["GET"],
        summary: "The full state of the annotator: tool, colour, brush size, \
                  active input, freeze state, undo availability and so on.",
        params: &["fmt (optional: 'text' for the plain-text format)"],
        values: &["fmt=text | fmt=json"],
        returns: &["200: the state snapshot"],
    },
    Endpoint {
        path: "/api/v1/inputs",
        methods: &["GET"],
        summary: "The video inputs available, and which one is active.",
        params: &["fmt (optional: 'text' for the plain-text format)"],
        values: &["fmt=text | fmt=json"],
        returns: &["200: the input list"],
    },
    Endpoint {
        path: "/api/v1/saves",
        methods: &["GET"],
        summary: "The scenes saved to disk, newest first.",
        params: &["fmt (optional: 'text' for the plain-text format)"],
        values: &["fmt=text | fmt=json"],
        returns: &["200: the saved scenes"],
    },
    Endpoint {
        path: "/api/v1/exports",
        methods: &["GET"],
        summary: "The images exported to disk, newest first.",
        params: &["fmt (optional: 'text' for the plain-text format)"],
        values: &["fmt=text | fmt=json"],
        returns: &["200: the exports"],
    },
    // ── Drawing ────────────────────────────────────────────────────────
    Endpoint {
        path: "/api/v1/tool",
        methods: &["GET", "POST"],
        summary: "Select the active tool.",
        params: &["name (required: the tool to switch to)"],
        values: &[
            "name=pen | name=line | name=rect | name=circle | name=arrow | \
                   name=text | name=select | name=eraser | name=highlight | name=laser",
        ],
        returns: &[
            "200: the new tool and brush size",
            "400: unknown or missing tool",
        ],
    },
    Endpoint {
        path: "/api/v1/color",
        methods: &["GET", "POST"],
        summary: "Set the drawing colour.",
        params: &[
            "hex (required unless 'palette' is given: the colour as rrggbb)",
            "palette (optional: a palette index, 1-8, instead of a hex value)",
        ],
        values: &["hex=ff3333 | palette=1..8"],
        returns: &["200: the new colour", "400: missing or malformed colour"],
    },
    Endpoint {
        path: "/api/v1/brush_size",
        methods: &["GET", "POST"],
        summary: "Set the brush size, or change it by a relative amount.",
        params: &[
            "px (optional: the new size in pixels, 1-200)",
            "delta (optional: a relative change, -50..50, used when 'px' is absent)",
        ],
        values: &["px=12 | delta=-4"],
        returns: &[
            "200: the new brush size",
            "400: missing or out-of-range size",
        ],
    },
    Endpoint {
        path: "/api/v1/input",
        methods: &["GET", "POST"],
        summary: "Select the video input and the capture mode.",
        params: &[
            "index (required: 0 for no input, 1..n for a device)",
            "mode (optional: the capture mode index; 0 is the device's best mode)",
        ],
        values: &["index=0 | index=1", "mode=0 | mode=1..n"],
        returns: &[
            "200: the new input and mode",
            "409: the input or mode index is out of range",
        ],
    },
    // ── Canvas actions ──────────────────────────────────────────────────
    Endpoint {
        path: "/api/v1/undo",
        methods: &["GET", "POST"],
        summary: "Undo the last annotation.",
        params: &[],
        values: &[],
        returns: &["200: the undo state", "409: there is nothing to undo"],
    },
    Endpoint {
        path: "/api/v1/redo",
        methods: &["GET", "POST"],
        summary: "Redo the last undone annotation.",
        params: &[],
        values: &[],
        returns: &["200: the redo state", "409: there is nothing to redo"],
    },
    Endpoint {
        path: "/api/v1/clear",
        methods: &["GET", "POST"],
        summary: "Clear the canvas.  The clear can itself be undone.",
        params: &[],
        values: &[],
        returns: &["200: the state after the clear"],
    },
    Endpoint {
        path: "/api/v1/freeze",
        methods: &["GET", "POST"],
        summary: "Freeze or unfreeze the video.  Without a parameter it toggles.",
        params: &["on (optional: '1' to freeze, '0' to unfreeze, 'toggle' to flip)"],
        values: &["on=1 | on=0 | on=toggle"],
        returns: &["200: the new freeze state", "400: an unrecognised value"],
    },
    // ── Persistence ─────────────────────────────────────────────────────
    Endpoint {
        path: "/api/v1/save",
        methods: &["GET", "POST"],
        summary: "Save the current annotations to disk.",
        params: &["name (optional: the file name; a timestamped one is generated if absent)"],
        values: &["name=case-42"],
        returns: &[
            "200: the file name and size",
            "400: the name is not allowed",
        ],
    },
    Endpoint {
        path: "/api/v1/load",
        methods: &["GET", "POST"],
        summary: "Load a saved scene, replacing the current one.",
        params: &["name (required: the file to load, without the .json suffix)"],
        values: &["name=case-42"],
        returns: &[
            "200: the file name and object count",
            "409: the file could not be read",
        ],
    },
    Endpoint {
        path: "/api/v1/export",
        methods: &["GET", "POST"],
        summary: "Export the annotations as a PNG, with the case metadata \
                  embedded in the file.",
        params: &["name (optional: the file name; a timestamped one is generated if absent)"],
        values: &["name=exhibit-a"],
        returns: &[
            "200: the file name and size",
            "400: the name is not allowed",
        ],
    },
];

/// The whole map, as a JSON value.
pub fn to_json() -> serde_json::Value {
    serde_json::json!({
        "name": "Court Annotator REST API",
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": "1",
        "base_url": "/api/v1",
        "authentication": {
            "required": true,
            "how": "Send the shared token in the X-Auth-Token header, \
                    or as a 'token' query parameter.",
            "note": "This map is public so a client can discover the API \
                     before it has a token. Every other endpoint requires it \
                     when one is configured.",
        },
        "formats": {
            "json": "The default. Every response is a JSON object with \
                     an 'ok' field.",
            "text": "Add fmt=text to any endpoint for plain 'OK key=value' \
                     and 'ERR message' lines, for a client with no JSON \
                     parser.",
        },
        "errors": {
            "400": "The request was malformed or a parameter was invalid.",
            "401": "The token was missing or wrong.",
            "404": "No such endpoint.",
            "405": "The method is not allowed on that endpoint.",
            "409": "The request was valid but conflicts with the current \
                    state — an input index that does not exist, or an \
                    empty undo stack.",
            "429": "Too many requests.  A Retry-After header says how long \
                    to wait.",
            "503": "The main loop is not responding.",
        },
        "endpoints": ENDPOINTS,
        "tools": TOOL_NAMES,
    })
}

/// The whole map, in the plain-text format.
///
/// The format is the same `OK key=value` shape as every other endpoint
/// in text mode, so a client needs only one parser.
pub fn to_text() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "OK name=Court Annotator REST API version={} api_version=1 base_url=/api/v1\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str("OK auth_required=1 auth_header=X-Auth-Token auth_param=token\n");
    out.push_str("OK format_json=default format_text=fmt=text\n");

    out.push_str(&format!("OK endpoint_count={}\n", ENDPOINTS.len()));
    for (i, e) in ENDPOINTS.iter().enumerate() {
        out.push_str(&format!(
            "OK endpoint[{}]={} methods={} summary={}\n",
            i,
            e.path,
            e.methods.join(","),
            e.summary.replace('\n', " ")
        ));
        for p in e.params {
            out.push_str(&format!("OK endpoint[{}].param={}\n", i, p));
        }
        for v in e.values {
            out.push_str(&format!(
                "OK endpoint[{}].value={}\n",
                i,
                v.replace('\n', " ")
            ));
        }
        for r in e.returns {
            out.push_str(&format!("OK endpoint[{}].returns={}\n", i, r));
        }
    }

    out.push_str(&format!("OK tools={}\n", TOOL_NAMES.join(",")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_covers_every_api_route() {
        // Every path the router handles under /api/v1/ must appear in the
        // map, or the map is lying to a client.
        let mut paths: Vec<&str> = ENDPOINTS.iter().map(|e| e.path).collect();
        paths.sort_unstable();

        // The routes the router actually implements.
        for route in [
            "/api/v1/",
            "/api/v1/health",
            "/api/v1/state",
            "/api/v1/inputs",
            "/api/v1/saves",
            "/api/v1/exports",
            "/api/v1/tool",
            "/api/v1/color",
            "/api/v1/brush_size",
            "/api/v1/input",
            "/api/v1/undo",
            "/api/v1/redo",
            "/api/v1/clear",
            "/api/v1/freeze",
            "/api/v1/save",
            "/api/v1/load",
            "/api/v1/export",
        ] {
            assert!(paths.contains(&route), "the map is missing '{route}'");
        }
    }

    #[test]
    fn the_map_has_no_duplicate_paths() {
        let mut paths: Vec<&str> = ENDPOINTS.iter().map(|e| e.path).collect();
        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "the map lists a path twice");
    }

    #[test]
    fn every_endpoint_has_a_summary_and_a_method() {
        for e in ENDPOINTS {
            assert!(!e.summary.is_empty(), "{} has no summary", e.path);
            assert!(!e.methods.is_empty(), "{} has no method", e.path);
            for m in e.methods {
                assert!(
                    ["GET", "POST"].contains(m),
                    "{} lists an odd method '{m}'",
                    e.path
                );
            }
        }
    }

    #[test]
    fn the_json_map_is_well_formed() {
        let v = to_json();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["api_version"], "1");
        let endpoints = v["endpoints"]
            .as_array()
            .expect("endpoints must be an array");
        assert_eq!(endpoints.len(), ENDPOINTS.len());
        // Every endpoint has a path and a summary in the JSON too.
        for e in endpoints {
            assert!(e["path"].is_string());
            assert!(e["summary"].is_string());
            assert!(e["methods"].is_array());
        }
    }

    #[test]
    fn the_text_map_lists_every_endpoint() {
        let text = to_text();
        assert!(text.starts_with("OK "));
        for e in ENDPOINTS {
            assert!(
                text.contains(e.path),
                "the text map is missing '{}'",
                e.path
            );
        }
        // It must not contain JSON.
        assert!(!text.contains('{'), "the text map must not be JSON");
    }

    #[test]
    fn the_text_map_names_the_tools() {
        let text = to_text();
        for t in TOOL_NAMES {
            assert!(text.contains(t), "the text map is missing the tool '{t}'");
        }
    }
}
