//! Application state — single definition shared by the UI, the render loop,
//! and the REST API (see docs/crestron-rest-api-plan.md, Appendix B).
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
    tools::{Tool, PALETTE},
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
    pub fn status_and_body(self) -> (u16, String) {
        match self {
            ApiResult::Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ok".into(), true.into());
                }
                (200, v.to_string())
            }
            ApiResult::Rejected(msg) => (400, err_json(&msg)),
            ApiResult::Conflict(msg) => (409, err_json(&msg)),
            ApiResult::NoReply => (503, err_json("main loop did not respond")),
        }
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

/// A mutation request handed from an API connection thread to the main loop.
/// The `reply` oneshot MUST be answered — the HTTP thread is blocked on it.
pub struct ApiCommand {
    pub kind:  ApiCommandKind,
    pub reply: std::sync::mpsc::SyncSender<ApiResult>,
}

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
        return Err(format!("bad hex color '{s}' — expected 6 hex digits (rrggbb)"));
    }
    let n = u32::from_str_radix(s, 16).unwrap(); // 6 hexdigits always parse
    Ok((((n >> 16) & 0xff) as u8, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8))
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
    pub active_tool:         Tool,
    pub color:               (u8, u8, u8),
    pub brush_size:          u32,
    /// Per-tool remembered sizes; indexed by Tool::index()
    pub tool_sizes:          [u32; 10],

    // Laser pointer ephemeral trail: (x, y, timestamp, color)
    pub laser_trail:         Vec<(i32, i32, Instant, (u8, u8, u8))>,

    // Mouse drag
    pub mouse_down:          bool,
    pub drag_start:          (i32, i32),
    pub drag_cur:            (i32, i32),

    /// Active stroke being drawn (pen or highlight).  Lives outside the
    /// scene while in progress so the scene's committed texture isn't
    /// re-rasterized as points are added.  On mouse-up, the stroke
    /// is appended to `scene.objects` and this is cleared.
    ///
    /// The eraser is *not* tracked here — see [`eraser_drag`].  An eraser
    /// commit mutates the committed canvas pixels directly and then bakes
    /// them into `scene.baked_bg`.
    pub active_stroke:       Option<DrawObject>,

    /// In-progress eraser drag (mouse-down through mouse-up).  When this
    /// is `Some`, `input::handle_release` will apply the erase circles to
    /// the committed canvas and bake the result into the scene.
    pub eraser_drag:         Option<EraserDrag>,

    // Scene (object model)
    pub scene:               Scene,

    // Text input
    pub text_input_active:   bool,
    pub text_buffer:         String,
    pub text_pos:            (i32, i32),
    pub pending_text_commit: bool,

    // Device picker
    pub show_device_picker:  bool,
    pub device_picker:       Option<DevicePicker>,
    pub picker_confirmed:    Option<(usize, usize)>,  // (device_idx, mode_idx)

    // Set when a select-drag undo snapshot was pushed; cancelled if no movement
    pub pending_select_undo: bool,

    // Draw lock (set after undo/redo to absorb accidental mouse-down)
    pub draw_locked_until:   Option<Instant>,

    // Video
    pub freeze_frame:        bool,
    /// Screenshot requested via UI button - consumed by main loop
    pub screenshot_requested: bool,
    /// Mode index (into the active device's `modes[]`) the capture thread was
    /// started with.  0 = "best" (used by menu-bar quick-switch).
    pub input_mode:          usize,

    // Menu bar
    pub pending_menu_click:  Option<(i32, i32)>,
    pub menu_close_requested: bool,

    // Window
    pub toggle_fullscreen:   bool,
    pub is_fullscreen:       bool,
    pub window_size:         (u32, u32),

    // Config ref
    pub cfg: config::Config,

    // Overlay settings panel
    pub show_settings_overlay: bool,
    pub settings:             Option<OverlaySettings>,
    pub settings_action:     Option<OverlayAction>,
}

impl AppState {
    pub fn new(cfg: config::Config) -> Self {
        let color      = config::parse_color(&cfg.tool_color);
        let undo_limit = cfg.undo_stack_limit;
        let tool_size  = cfg.tool_size.max(1);
        let is_full    = cfg.fullscreen && !cfg.windowed;
        let tool_sizes: [u32; 10] = [8, tool_size, tool_size, tool_size, tool_size, tool_size, tool_size, 40, 12, 6];
        let brush_size = tool_sizes[Tool::Pen.index()];
        Self {
            active_tool:         Tool::Pen,
            color,
            brush_size,
            tool_sizes,
            mouse_down:          false,
            drag_start:          (0, 0),
            drag_cur:            (0, 0),
            active_stroke:       None,
            eraser_drag:         None,
            scene:               Scene::new(undo_limit),
            text_input_active:   false,
            text_buffer:         String::new(),
            text_pos:            (0, 0),
            pending_text_commit: false,
            show_device_picker:  true,
            device_picker:       None,
            picker_confirmed:    None,
            toggle_fullscreen:   false,
            is_fullscreen:       is_full,
            window_size:         (cfg.width, cfg.height),
            pending_select_undo: false,
            draw_locked_until:   None,
            freeze_frame:        false,
            screenshot_requested: false,
            input_mode:          0,
            laser_trail:         Vec::new(),
            pending_menu_click:  None,
            menu_close_requested: false,
            cfg,
            show_settings_overlay: false,
            settings:             None,
            settings_action:     None,
        }
    }

    // ── REST API snapshot ─────────────────────────────────────────────────

    /// Build an immutable snapshot of every piece of state the API exposes
    /// (plan Appendix A.1–A.3).  Values that live outside `AppState` (menu
    /// bar input selection, FPS counters) are passed in by the main loop.
    pub fn to_snapshot(
        &self,
        active_input: usize,
        devices:      &[DeviceInfo],
        fps:          f32,
        capture_fps:  f32,
    ) -> StateSnapshot {
        let mut inputs = Vec::with_capacity(devices.len() + 1);
        inputs.push(InputInfo {
            index: 0,
            name:  "No input (black screen)".to_string(),
            path:  None,
            modes: Vec::new(),
        });
        for (i, d) in devices.iter().enumerate() {
            inputs.push(InputInfo {
                index: i + 1,
                name:  d.name.clone(),
                path:  Some(d.path.display().to_string()),
                modes: d.modes.iter().map(|m| m.label()).collect(),
            });
        }

        let (laser_on, laser_fade, show_fps, undo_limit) = match &self.settings {
            Some(s) => (
                s.laser_on.as_bool(),
                // Round to the slider's 0.1 step and store as f64 — f32 1.2
                // would serialize as 1.2000000476837158 on the wire.
                ((s.laser_fade as f64) * 10.0).round() / 10.0,
                s.show_fps.as_bool(),
                s.undo_limit,
            ),
            None => (true, 1.2, false, self.scene.undo_limit),
        };

        StateSnapshot {
            api_version:         "1",
            version:             env!("CARGO_PKG_VERSION"),
            tool:                self.active_tool.name(),
            color:               format!(
                "{:02x}{:02x}{:02x}", self.color.0, self.color.1, self.color.2
            ),
            brush_size:          self.brush_size,
            tool_sizes:          self.tool_sizes,
            input:               active_input,
            input_mode:          self.input_mode,
            inputs,
            freeze:              self.freeze_frame,
            fullscreen:          self.is_fullscreen,
            laser_on,
            laser_fade,
            show_fps,
            undo_limit,
            can_undo:            !self.scene.undo_stack.is_empty(),
            can_redo:            !self.scene.redo_stack.is_empty(),
            object_count:        self.scene.objects.len(),
            selected:            self.scene.selected.is_some(),
            drawing:             self.active_stroke.is_some(),
            text_input_active:   self.text_input_active,
            device_picker_open:  self.show_device_picker,
            window_size:         [self.window_size.0, self.window_size.1],
            fps:                 fps.round() as u32,
            capture_fps:         capture_fps.round() as u32,
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
    pub name:  String,
    /// Device path (`/dev/video0`); absent for the index-0 "no input" entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path:  Option<String>,
    pub modes: Vec<String>,
}

/// Immutable, serializable snapshot published by the main loop for the API
/// thread.  Contains ONLY the Appendix A.1–A.3 fields — no internal flags.
/// Field order matches the plan's Appendix A.5 payload.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StateSnapshot {
    pub api_version:        &'static str,
    pub version:            &'static str,
    pub tool:               &'static str,
    pub color:              String,
    pub brush_size:         u32,
    pub tool_sizes:         [u32; 10],
    pub input:              usize,
    pub input_mode:         usize,
    pub inputs:             Vec<InputInfo>,
    pub freeze:             bool,
    pub fullscreen:         bool,
    pub laser_on:           bool,
    /// Seconds; f64 (not f32) so serde_json emits clean decimals like 1.2.
    pub laser_fade:         f64,
    pub show_fps:           bool,
    pub undo_limit:         usize,
    pub can_undo:           bool,
    pub can_redo:           bool,
    pub object_count:       usize,
    pub selected:           bool,
    pub drawing:            bool,
    pub text_input_active:  bool,
    pub device_picker_open: bool,
    pub window_size:        [u32; 2],
    pub fps:                u32,
    pub capture_fps:        u32,
}
