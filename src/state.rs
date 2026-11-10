//! Application state — single definition shared by the UI, the render loop,
//! and the REST API.
//!
//! `AppState` is owned exclusively by the single-threaded SDL2 main loop.
//! The API thread never touches it; instead the main loop publishes an
//! immutable [`StateSnapshot`] (via [`AppState::to_snapshot`]) into an
//! `Arc<RwLock<_>>` once per frame, and all API reads are served from that.

use std::time::Instant;

use serde::Serialize;

use crate::{
    capture::DeviceInfo,
    config,
    scene::{DrawObject, Scene},
    tools::{PALETTE, Tool},
    ui::{DevicePicker, OverlayAction, OverlaySettings},
};

/// In-progress eraser drag.  Recorded between mouse-down and mouse-up so the
/// input handler can apply the erase circles to the committed canvas and bake
/// the result into the scene on release.
#[derive(Clone, Debug)]
pub struct EraserDrag {
    pub points: Vec<(i32, i32)>,
    pub radius: u32,
}

// ── API command types (Phase 2 mutations) ─────────────────────────────────────

/// Result of a mutation applied by the main loop.  The HTTP thread turns
/// `Rejected` into 400/409 and `Timeout`/`NoReply` into 504/503.
pub enum ApiResult {
    Ok(serde_json::Value),
    /// 400 — the request itself was invalid (bad param, unknown tool, ...).
    Rejected(String),
    /// 409 — the request was well-formed but conflicts with live state
    /// (e.g. input index beyond the enumerated device list).
    Conflict(String),
    /// The main loop did not answer in time (hung or shutting down).
    NoReply,
}

impl ApiResult {
    /// Split into an HTTP status and a still-unserialised payload.  The
    /// HTTP layer picks the wire format (JSON, or the plain-text
    /// `OK …` / `ERR …` form used by `?fmt=text`), so the decision lives in
    /// one place instead of being duplicated per endpoint.
    pub fn into_payload(self) -> (u16, ApiPayload) {
        match self {
            ApiResult::Ok(v) => (200, ApiPayload::Ok(v)),
            ApiResult::Rejected(msg) => (400, ApiPayload::Err(msg)),
            ApiResult::Conflict(msg) => (409, ApiPayload::Err(msg)),
            ApiResult::NoReply => (503, ApiPayload::Err("main loop did not respond".into())),
        }
    }
}

/// A response body that has not been serialised yet.
pub enum ApiPayload {
    /// Success — carries the result fields.
    Ok(serde_json::Value),
    /// Failure — carries the human-readable message.
    Err(String),
}

impl ApiPayload {
    /// JSON wire format.  The `ok` marker is added here so every JSON
    /// response carries it, whichever endpoint produced it.
    pub fn to_json(&self) -> String {
        match self {
            ApiPayload::Ok(v) => {
                let mut v = v.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ok".into(), true.into());
                }
                v.to_string()
            }
            ApiPayload::Err(msg) => err_json(msg),
        }
    }

    /// Plain-text wire format — one `OK key=value` line per success field,
    /// or a single `ERR <message>` line for a failure.
    ///
    /// Nested objects are flattened with a dotted key (`inputs[0].name=…`)
    /// so that nothing in the text format is JSON.
    pub fn to_text(&self) -> String {
        match self {
            ApiPayload::Ok(v) => {
                let mut lines = Vec::new();
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        if k == "ok" {
                            continue;
                        }
                        flatten(k, val, &mut lines);
                    }
                }
                if lines.is_empty() {
                    return "OK".to_string();
                }
                lines
                    .into_iter()
                    .map(|l| format!("OK {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            ApiPayload::Err(msg) => format!("ERR {msg}"),
        }
    }
}

/// Flatten a JSON value into `key=value` lines.
///
/// * a scalar becomes one line
/// * an array becomes one line per element, keyed `key[0]`, `key[1]`, …
/// * an object becomes one line per field, keyed `key.field`
fn flatten(key: &str, v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                flatten(&format!("{key}[{i}]"), item, out);
            }
        }
        serde_json::Value::Object(fields) => {
            for (k, val) in fields {
                flatten(&format!("{key}.{k}"), val, out);
            }
        }
        serde_json::Value::String(s) => out.push(format!("{key}={s}")),
        serde_json::Value::Null => out.push(format!("{key}=")),
        other => out.push(format!("{key}={other}")),
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

/// A mutation request handed from an API connection thread to the main loop.
/// The `reply` oneshot MUST be answered — the HTTP thread is blocked on it.
pub struct ApiCommand {
    pub kind: ApiCommandKind,
    pub reply: std::sync::mpsc::SyncSender<ApiResult>,
}

// The `Set*` prefix groups the mutation commands together, which is what
// the REST handler dispatches on.
#[allow(clippy::enum_variant_names)]
pub enum ApiCommandKind {
    /// Switch tool, preserving per-tool size memory (like `input::switch_tool`).
    SetTool(Tool),
    /// Set stroke color to an arbitrary RGB value.
    SetColor(u8, u8, u8),
    /// Set the active tool's brush size in px (main loop applies the 1–200 clamp).
    SetBrushSize(u32),
    /// Relative brush-size change for the active tool (main loop clamps).
    SetBrushSizeDelta(i64),
    /// Select video input: 0 = no input, 1..=n = device index.
    /// `mode` indexes the device's mode list (0 = best).
    SetInput { index: usize, mode: usize },
    /// Undo the last committed change.  Rejected with 409 when the undo
    /// stack is empty.
    Undo,
    /// Redo the last undone change.  Rejected with 409 when the redo
    /// stack is empty.
    Redo,
    /// Clear the canvas.  Always succeeds — it pushes an undo point first,
    /// so the clear itself can be undone.
    Clear,
    /// Flip the freeze-frame flag.
    ToggleFreeze,
    /// Freeze the frame explicitly (`true` = frozen, `false` = live).
    SetFreeze(bool),
    /// Save the current scene to disk.  `name` is the file name; an empty
    /// name means "generate a timestamped one".
    SaveScene { name: String },
    /// Load a scene from disk, replacing the current one.
    LoadScene { name: String },
    /// Export the current annotations to a PNG.  `name` is the file
    /// name; an empty name means "generate a timestamped one".
    Export { name: String },
}

// ── Validation / parsing helpers (unit-testable, no SDL) ──────────────────────

/// Brush size bounds — same 1–200 rule as the keyboard path.
pub const BRUSH_SIZE_MIN: u32 = 1;
pub const BRUSH_SIZE_MAX: u32 = 200;

/// Clamp a requested brush size exactly like the keyboard path does.
pub fn clamp_brush_size(px: u32) -> u32 {
    px.clamp(BRUSH_SIZE_MIN, BRUSH_SIZE_MAX)
}

/// Parse `hex=rrggbb` (optional leading `#`, any case).
pub fn parse_hex_color(s: &str) -> Result<(u8, u8, u8), String> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "bad hex color '{s}' — expected 6 hex digits (rrggbb)"
        ));
    }
    let n = u32::from_str_radix(s, 16).unwrap(); // 6 hexdigits always parse
    Ok((
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

/// Parse `palette=1..8` against the on-screen `PALETTE`.
pub fn parse_palette_index(s: &str) -> Result<(u8, u8, u8), String> {
    let idx: usize = s
        .trim()
        .parse()
        .map_err(|_| format!("bad palette index '{s}' — expected 1..=8"))?;
    PALETTE
        .get(idx.wrapping_sub(1))
        .filter(|_| (1..=8).contains(&idx))
        .copied()
        .ok_or_else(|| format!("palette index {idx} out of range (1..=8)"))
}

/// Parse a non-negative integer parameter.
pub fn parse_usize(s: &str, what: &str) -> Result<usize, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("bad {what} '{s}' — expected a non-negative integer"))
}

// ── Live application state ────────────────────────────────────────────────────

pub struct AppState {
    // Tool / drawing
    pub active_tool: Tool,
    pub color: (u8, u8, u8),
    pub brush_size: u32,
    /// Per-tool remembered sizes; indexed by Tool::index()
    pub tool_sizes: [u32; 10],

    // Laser pointer ephemeral trail: (x, y, timestamp, color)
    #[allow(clippy::type_complexity)]
    pub laser_trail: Vec<(i32, i32, Instant, (u8, u8, u8))>,

    // Mouse drag
    pub mouse_down: bool,
    pub drag_start: (i32, i32),
    pub drag_cur: (i32, i32),

    /// Active stroke being drawn (pen or highlight).  Lives outside the
    /// scene while in progress so the scene's committed texture isn't
    /// re-rasterized as points are added.  On mouse-up, the stroke
    /// is appended to `scene.objects` and this is cleared.
    ///
    /// The eraser is *not* tracked here — see [`eraser_drag`].  An eraser
    /// commit mutates the committed canvas pixels directly and then bakes
    /// them into `scene.baked_bg`.
    pub active_stroke: Option<DrawObject>,

    /// In-progress eraser drag (mouse-down through mouse-up).  When this
    /// is `Some`, `input::handle_release` will apply the erase circles to
    /// the committed canvas and bake the result into the scene.
    pub eraser_drag: Option<EraserDrag>,

    /// Strokes committed to `scene` whose rasterization has not yet
    /// reached the committed texture.  On mouse-up the stroke leaves
    /// `active_stroke` (so the GPU overlay stops drawing it) but the
    /// pipeline only shows it once the worker renders + publishes a
    /// frame containing it — a gap of one or more frames in which the
    /// just-finished stroke is invisible, which makes fast successive
    /// strokes look separated.  The render loop draws these as GPU
    /// overlays until `in_flight_cover` says the texture caught up.
    pub in_flight: Vec<DrawObject>,

    /// `(submit id, covered prefix length)` recorded by the render loop
    /// when a scene snapshot containing the current `in_flight` prefix
    /// was submitted to the pipeline.  Once
    /// `RenderPipeline::rendered_submit()` reaches the id, the uploaded
    /// texture contains those strokes and the prefix is drained.
    /// Strokes appended after this point wait for a later submit.
    pub in_flight_cover: Option<(u64, usize)>,

    /// Bounding box (inclusive min, exclusive max in canvas pixel space)
    /// of the region of `committed_canvas` that was alpha-zeroed by the
    /// live eraser preview since the last frame's texture upload.  The
    /// render loop reads + resets this to drive a partial
    /// `SDL_UpdateTexture(rect, …)` instead of a full 8.3 MB upload per
    /// drag frame — a major win on bandwidth-limited systems (RPi3 / R6A)
    /// where full-canvas uploads were flagged as the dominant CPU↔GPU
    /// traffic cost.
    ///
    /// Sentinel values (`i32::MAX / i32::MIN`) mean "no wipes since last
    /// reset" and the upload is skipped.
    pub eraser_dirty_min: (i32, i32),
    pub eraser_dirty_max: (i32, i32),

    // Scene (object model)
    pub scene: Scene,

    // Text input
    pub text_input_active: bool,
    pub text_buffer: String,
    pub text_pos: (i32, i32),
    pub pending_text_commit: bool,

    // Device picker
    pub show_device_picker: bool,
    pub device_picker: Option<DevicePicker>,
    pub picker_confirmed: Option<(usize, usize)>, // (device_idx, mode_idx)

    // Set when a select-drag undo snapshot was pushed; cancelled if no movement
    pub pending_select_undo: bool,

    /// Marquee selection in progress.  When the user clicks on empty
    /// canvas (no hit) the input handler starts a marquee from the
    /// press point; on mouse-up every object whose bounds overlap the
    /// rect (start..cur) is added to the selection.  `None` outside of
    /// a marquee drag.  Drawn live via direct GPU primitives in main.rs.
    pub marquee: Option<(i32, i32, i32, i32)>, // (x0, y0, x1, y1)

    // Draw lock (set after undo/redo to absorb accidental mouse-down)
    pub draw_locked_until: Option<Instant>,

    // Video
    pub freeze_frame: bool,
    /// Screenshot requested via UI button - consumed by main loop
    pub screenshot_requested: bool,
    /// Save requested via the keyboard or a UI button.  Holds the file
    /// name; empty means "generate a timestamped one".  Consumed by the
    /// main loop.
    pub save_requested: Option<String>,
    /// Load requested.  Holds the file name (without the `.json`
    /// suffix).  Consumed by the main loop.
    pub load_requested: Option<String>,
    /// Set whenever the scene changed in a way that should be persisted.
    /// The main loop uses it to decide when an autosave is due.
    pub scene_changed: bool,
    /// When the last autosave happened.  Autosave is debounced against
    /// this so a busy annotator does not cause a write per mouse-move.
    pub last_autosave: Instant,
    /// Mode index (into the active device's `modes[]`) the capture thread was
    /// started with.  0 = "best" (used by menu-bar quick-switch).
    pub input_mode: usize,
    /// Whether the capture device is currently plugged in.
    ///
    /// Cleared when the capture thread reports the device was lost and
    /// set again once it streams once more.  Published in the snapshot so
    /// the API and the portal can tell the user the camera is unplugged.
    pub capture_device_present: bool,

    // Menu bar
    pub pending_menu_click: Option<(i32, i32)>,
    pub menu_close_requested: bool,

    // Window
    pub toggle_fullscreen: bool,
    pub is_fullscreen: bool,
    pub window_size: (u32, u32),

    /// True while Shift is physically held.  Updated on KeyDown/KeyUp so
    /// mouse-button events (which don't carry modifier state) can check
    /// it for shift-click semantics (e.g. multi-select toggle).
    pub shift_down: bool,

    // Config ref
    ///
    /// Shared so the API's password-change endpoint can update it and
    /// have the change persist.  The main loop owns the authoritative
    /// copy and writes it to disk.
    pub cfg: std::sync::Arc<std::sync::RwLock<config::Config>>,

    // Overlay settings panel
    pub show_settings_overlay: bool,
    pub settings: Option<OverlaySettings>,
    pub settings_action: Option<OverlayAction>,

    /// Set whenever a field that contributes to [`StateSnapshot`] has been
    /// mutated.  The main loop clears it after publishing a fresh snapshot
    /// to the API threads; while it's false, [`AppState::to_snapshot`] is
    /// not called.
    pub snapshot_dirty: bool,

    /// Wall-clock of the last published snapshot — used to throttle
    /// periodic rebuilds of FPS-bearing fields.
    pub last_snapshot_publish: Instant,
    /// Wall-clock of the last config write.
    pub last_config_save: Instant,
}

impl AppState {
    pub fn new(cfg: config::Config) -> Self {
        let color = config::parse_color(&cfg.tool_color);
        let undo_limit = cfg.undo_stack_limit;
        let tool_size = cfg.tool_size.max(1);
        let is_full = cfg.fullscreen && !cfg.windowed;
        let tool_sizes: [u32; 10] = [
            10, tool_size, tool_size, tool_size, tool_size, tool_size, tool_size, 40, 12, 6,
        ];
        let brush_size = tool_sizes[Tool::Pen.index()];
        Self {
            active_tool: Tool::Pen,
            color,
            brush_size,
            tool_sizes,
            mouse_down: false,
            drag_start: (0, 0),
            drag_cur: (0, 0),
            active_stroke: None,
            eraser_drag: None,
            in_flight: Vec::new(),
            in_flight_cover: None,
            scene: Scene::new(undo_limit),
            text_input_active: false,
            text_buffer: String::new(),
            text_pos: (0, 0),
            pending_text_commit: false,
            show_device_picker: true,
            device_picker: None,
            picker_confirmed: None,
            toggle_fullscreen: false,
            is_fullscreen: is_full,
            window_size: (cfg.width, cfg.height),
            shift_down: false,
            pending_select_undo: false,
            marquee: None,
            draw_locked_until: None,
            freeze_frame: false,
            screenshot_requested: false,
            save_requested: None,
            load_requested: None,
            scene_changed: false,
            last_autosave: Instant::now(),
            input_mode: 0,
            capture_device_present: true,
            laser_trail: Vec::new(),
            pending_menu_click: None,
            menu_close_requested: false,
            cfg: std::sync::Arc::new(std::sync::RwLock::new(cfg)),
            show_settings_overlay: false,
            settings: None,
            settings_action: None,
            snapshot_dirty: true, // first frame always rebuilds
            last_snapshot_publish: Instant::now() - std::time::Duration::from_secs(1),
            last_config_save: Instant::now(),
            eraser_dirty_min: (i32::MAX, i32::MAX),
            eraser_dirty_max: (i32::MIN, i32::MIN),
        }
    }

    /// Mark [`Self::snapshot_dirty`] so the next render-loop iteration
    /// rebuilds the API snapshot.  Call this from any mutator that
    /// affects a field consumed by [`AppState::to_snapshot`].
    #[inline]
    pub fn mark_snapshot_dirty(&mut self) {
        self.snapshot_dirty = true;
    }

    // ── REST API snapshot ─────────────────────────────────────────────────

    /// Build an immutable snapshot of every piece of state the API exposes
    /// (plan Appendix A.1–A.3).  Values that live outside `AppState` (menu
    /// bar input selection, FPS counters) are passed in by the main loop.
    pub fn to_snapshot(
        &self,
        active_input: usize,
        devices: &[DeviceInfo],
        fps: f32,
        capture_fps: f32,
    ) -> StateSnapshot {
        let mut inputs = Vec::with_capacity(devices.len() + 1);
        inputs.push(InputInfo {
            index: 0,
            name: "No input (black screen)".to_string(),
            path: None,
            modes: Vec::new(),
        });
        for (i, d) in devices.iter().enumerate() {
            inputs.push(InputInfo {
                index: i + 1,
                name: d.name.clone(),
                path: Some(d.path.display().to_string()),
                modes: d.modes.iter().map(|m| m.label()).collect(),
            });
        }

        let (laser_on, laser_fade, show_fps, show_time, hide_toolbar, undo_limit) =
            match &self.settings {
                Some(s) => (
                    s.laser_on.as_bool(),
                    // Round to the slider's 0.1 step and store as f64 — f32 1.2
                    // would serialize as 1.2000000476837158 on the wire.
                    ((s.laser_fade as f64) * 10.0).round() / 10.0,
                    s.show_fps.as_bool(),
                    s.show_time.as_bool(),
                    s.hide_toolbar.as_bool(),
                    s.undo_limit,
                ),
                None => (true, 1.2, false, false, false, self.scene.undo_limit),
            };

        StateSnapshot {
            api_version: "1",
            version: env!("CARGO_PKG_VERSION"),
            tool: self.active_tool.name(),
            color: format!(
                "{:02x}{:02x}{:02x}",
                self.color.0, self.color.1, self.color.2
            ),
            brush_size: self.brush_size,
            tool_sizes: self.tool_sizes,
            input: active_input,
            input_mode: self.input_mode,
            capture_device_present: self.capture_device_present,
            inputs,
            freeze: self.freeze_frame,
            fullscreen: self.is_fullscreen,
            laser_on,
            laser_fade,
            show_fps,
            show_time,
            hide_toolbar,
            undo_limit,
            can_undo: !self.scene.undo_stack.is_empty(),
            can_redo: !self.scene.redo_stack.is_empty(),
            object_count: self.scene.objects().len(),
            // The text of every text annotation, in drawing order.  Lets a
            // client read back what was written without pulling the whole
            // scene.
            texts: self
                .scene
                .text_contents()
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            // Point size of each text annotation, parallel to `texts`.
            text_sizes: self.scene.text_sizes(),
            // Number of selected live objects (0 = nothing selected).
            selected: self.scene.selected.len(),
            drawing: self.active_stroke.is_some(),
            text_input_active: self.text_input_active,
            device_picker_open: self.show_device_picker,
            window_size: [self.window_size.0, self.window_size.1],
            fps: fps.round() as u32,
            capture_fps: capture_fps.round() as u32,
        }
    }
}

// ── Published snapshot (REST API read model) ──────────────────────────────────

/// One entry of the `/state` / `/inputs` device list.  Index 0 is always the
/// synthesized "No input (black screen)" entry; indices 1..=n map to
/// `devices[n - 1]`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct InputInfo {
    pub index: usize,
    pub name: String,
    /// Device path (`/dev/video0`); absent for the index-0 "no input" entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub modes: Vec<String>,
}

/// Immutable, serializable snapshot published by the main loop for the API
/// thread.  Contains ONLY the Appendix A.1–A.3 fields — no internal flags.
/// Field order matches the plan's Appendix A.5 payload.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StateSnapshot {
    pub api_version: &'static str,
    pub version: &'static str,
    pub tool: &'static str,
    pub color: String,
    pub brush_size: u32,
    pub tool_sizes: [u32; 10],
    pub input: usize,
    pub input_mode: usize,
    /// Whether the capture device is plugged in and streaming.
    pub capture_device_present: bool,
    pub inputs: Vec<InputInfo>,
    pub freeze: bool,
    pub fullscreen: bool,
    pub laser_on: bool,
    /// Seconds; f64 (not f32) so serde_json emits clean decimals like 1.2.
    pub laser_fade: f64,
    pub show_fps: bool,
    pub show_time: bool,
    pub hide_toolbar: bool,
    pub undo_limit: usize,
    pub can_undo: bool,
    pub can_redo: bool,
    pub object_count: usize,
    /// The text content of every text annotation, in drawing order.
    pub texts: Vec<String>,
    /// Point size of each text annotation, parallel to `texts`.
    pub text_sizes: Vec<u32>,
    /// Number of currently-selected live objects (0 = nothing selected).
    /// Multi-select: any non-zero count is "something is selected".
    pub selected: usize,
    pub drawing: bool,
    pub text_input_active: bool,
    pub device_picker_open: bool,
    pub window_size: [u32; 2],
    pub fps: u32,
    pub capture_fps: u32,
}
