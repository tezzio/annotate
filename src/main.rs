mod api;
mod api_map;
mod canvas;
mod capture;
mod config;
mod export;
mod input;
mod net;
mod password_cli;
mod persist;
mod ratelimit;
mod render_pipeline;
mod scene;
mod state;
mod tools;
mod ui;

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc::Receiver},
    time::{Duration, Instant},
};

use sdl2::{
    pixels::{Color, PixelFormatEnum},
    rect::{Point, Rect},
    render::BlendMode,
};

use canvas::Canvas;
use capture::{Frame, FrameFormat};
use render_pipeline::RenderPipeline;
use scene::DrawObject;
use state::AppState;
use tools::Tool;
use ui::{
    DevicePicker, MENU_HEIGHT, OverlayAction, OverlaySettings, TextCache, Toggle, toolbar_height,
};

// ── Application state ─────────────────────────────────────────────────────────
// `AppState` lives in `state.rs` — one definition shared by the UI, the
// render loop, and the REST API (ROADMAP.md).

// ── Embedded fonts ────────────────────────────────────────────────────────────

const FONT_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/DejaVuSans.ttf"));
const ICON_FONT_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fa-solid.ttf"));

// ── Per-thread scratch buffers ─────────────────────────────────────────────────
// `Vec<Point>` allocations inside the render loop (eraser preview,
// Circle shape preview) used to allocate per frame.  We reuse a thread-local
// buffer instead.
std::thread_local! {
    static PREVIEW_PTS: std::cell::RefCell<Vec<Point>> = std::cell::RefCell::new(Vec::with_capacity(512));
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // The password mode runs before the window is created, so it works
    // even when the app itself cannot start.
    let args: Vec<String> = std::env::args().collect();
    match password_cli::parse(&args) {
        Ok(Some(action)) => {
            std::process::exit(password_cli::run(action));
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    }

    let cfg = config::load();
    // Captured before `cfg` is moved into the state.
    let cfg_show_time = cfg.show_time;
    let cfg_hide_toolbar = cfg.hide_toolbar;

    // ── SDL2 init ──────────────────────────────────────────────────────────
    let sdl_context = sdl2::init().expect("SDL2 init");
    let video = sdl_context.video().expect("SDL2 video");
    let ttf_context = sdl2::ttf::init().expect("SDL2 TTF init");

    // Load bundled font via RWops (no temp file needed)
    let rwops = sdl2::rwops::RWops::from_bytes(FONT_BYTES).expect("font RWops");
    let font = ttf_context
        .load_font_from_rwops(rwops, 16)
        .expect("load font");
    let font_large = {
        let rw2 = sdl2::rwops::RWops::from_bytes(FONT_BYTES).expect("font RWops");
        ttf_context
            .load_font_from_rwops(rw2, 20)
            .expect("load large font")
    };
    let icon_font = {
        let rw3 = sdl2::rwops::RWops::from_bytes(ICON_FONT_BYTES).expect("icon font RWops");
        ttf_context
            .load_font_from_rwops(rw3, 22)
            .expect("load icon font")
    };

    // ── Window creation ────────────────────────────────────────────────────
    let (win_w, win_h) = (cfg.width, cfg.height);
    let windowed = cfg.windowed;

    let mut win_builder = video.window("Court Annotator", win_w, win_h);
    win_builder.position_centered();
    if !windowed {
        win_builder.fullscreen_desktop();
    } else {
        win_builder.resizable();
    }

    // Create a window we can fall back on if hardware rendering fails.
    // (When CanvasBuilder::build() fails, the window it holds is already consumed,
    // so we need a second window to attempt the software renderer.)
    //
    // It is created HIDDEN.  SDL windows are shown by default, so without
    // this the fallback window appeared as a second, blank window next to
    // the real one — even though it was only ever used on the machines
    // where the hardware renderer fails.  It is shown explicitly below, if
    // it actually has to be used.
    let mut win2 = video
        .window("Court Annotator", win_w, win_h)
        .position_centered()
        .hidden()
        .build()
        .expect("fallback window");

    let window = win_builder.build().expect("create window");

    // Try hardware-accelerated rendering first; fall back to software if unavailable.
    // SDL2 picks the best available backend (OpenGL/GLES/Metal/Vulkan) automatically.
    // On VMs without GPU passthrough this returns Err — we use the second window
    // with a software renderer instead.
    let (mut sdl_canvas, using_hardware) =
        match window.into_canvas().accelerated().present_vsync().build() {
            Ok(c) => (c, true),
            Err(reason) => {
                eprintln!(
                    "[annotator] hardware canvas failed ({reason}), falling back to software"
                );
                // This window is the one the user will actually see, so
                // it has to be shown — it was created hidden.
                win2.show();
                let c = win2
                    .into_canvas()
                    .software()
                    .build()
                    .expect("software renderer must succeed");
                (c, false)
            }
        };
    eprintln!(
        "[annotator] renderer: {}",
        if using_hardware {
            "hardware (accelerated)"
        } else {
            "software (fallback)"
        }
    );

    sdl_canvas.set_blend_mode(BlendMode::Blend);

    // Query actual dimensions — fullscreen_desktop gives native screen res;
    // windowed gives WM-assigned size (tiling WMs resize the window on map).
    let (mut canvas_w, mut canvas_h) = sdl_canvas.output_size().expect("output size");

    let texture_creator = sdl_canvas.texture_creator();

    // ── Annotation overlay ──────────────────────────────────────────────
    // `committed_canvas` is the input sink: the eraser and text-blit
    // handlers write into it directly.  `committed_tex` is the GPU
    // texture the renderer blits; its pixels are populated by the
    // background rasterizer in `RenderPipeline`.
    //
    // The active stroke no longer uses a separate CPU canvas —
    // it's drawn directly via SDL2 GPU primitives (see `draw_active_stroke`)
    // on top of `committed_tex`, avoiding an 8.3 MB GPU upload per frame
    // while drawing.
    let mut committed_canvas = Canvas::new(canvas_w, canvas_h);
    let mut committed_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
        .expect("committed overlay texture");
    committed_tex.set_blend_mode(BlendMode::Blend);

    // The pipeline owns two CPU canvases (front/back) and a worker thread
    // that rasterizes `scene` (with `committed_canvas` as the input base)
    // into the back buffer.  The main thread drains completed frames via
    // `pipeline.upload_to(&mut committed_tex)` each vsync.
    let pipeline = RenderPipeline::new(canvas_w, canvas_h);
    let mut pipeline_seq: u64 = 0;

    // Top-bar clock.  Recomputed at most once a second.
    let mut clock_label = String::new();
    let mut clock_updated = Instant::now();

    // ── Video texture (double-buffered) ────────────────────────────────────
    // A single streaming texture produces a subtle horizontal tear line with
    // USB HDMI capture cards because their frame delivery is jittery relative
    // to the display vsync boundary: the GPU's fragment shader samples the
    // texture while the CPU `update()` is still mid-way through the
    // glTexSubImage2D copy, so scanout sees half-old / half-new pixel data at
    // the line where the upload crossed the screen.  Fix: keep TWO textures
    // and rotate.  Upload writes to the BACK buffer; the blit samples the
    // FRONT buffer; the GPU command queue guarantees the in-flight blit
    // completes before the next iteration's blit re-samples the rotated-in
    // buffer — so the GPU is never sampling a texture being uploaded to.
    let mk_video_tex = |fmt: PixelFormatEnum, w: u32, h: u32| -> sdl2::render::Texture {
        texture_creator
            .create_texture_streaming(fmt, w, h)
            .expect("video texture")
    };
    let mut video_tex: [sdl2::render::Texture; 2] = [
        mk_video_tex(PixelFormatEnum::RGB24, canvas_w, canvas_h),
        mk_video_tex(PixelFormatEnum::RGB24, canvas_w, canvas_h),
    ];
    let mut video_tex_idx: usize = 0; // index of the FRONT (currently-displayed) buffer
    let mut video_tex_w = canvas_w;
    let mut video_tex_h = canvas_h;
    let mut video_tex_fmt = PixelFormatEnum::RGB24;

    // ── App state ──────────────────────────────────────────────────────────
    let undo_limit = cfg.undo_stack_limit;
    let cfg_api = cfg.api.clone();
    let mut state = AppState::new(cfg);
    state.window_size = (canvas_w, canvas_h);

    // ── Settings overlay ────────────────────────────────────────────────────
    let renderer_info = if using_hardware {
        "hardware (accelerated)".into()
    } else {
        "software (fallback)".into()
    };
    state.settings = Some(OverlaySettings {
        cursor: 0,
        laser_fade: 1.2,
        laser_on: Toggle::On,
        show_fps: Toggle::On,
        show_time: Toggle::from_bool(cfg_show_time),
        hide_toolbar: Toggle::from_bool(cfg_hide_toolbar),
        fullscreen: Toggle::from_bool(state.is_fullscreen),
        undo_limit,
        api_enabled: Toggle::from_bool(cfg_api.enabled),
        api_addr: format!("{}:{}", cfg_api.bind, cfg_api.port),
        network: net::interface_addresses(),
        renderer_info,
        scroll_offset: 0,
    });
    state.show_settings_overlay = false;
    state.settings_action = None;

    // ── Device enumeration ─────────────────────────────────────────────────
    let devices = capture::enumerate_devices();
    state.device_picker = Some(DevicePicker::new(devices.clone()));
    state.show_device_picker = false; // Start with black screen, no picker shown

    // ── Menu bar ───────────────────────────────────────────────────────────
    let mut menu_bar = ui::MenuBar::new(devices);

    // ── Recover the last saved session ─────────────────────────────────────
    // A crash or power cut must not destroy the annotations.  If there is
    // a saved scene, the most recent one is restored at startup.
    match persist::list_saves().first() {
        Some(info) => {
            let path = PathBuf::from(&info.path);
            match persist::load_from_path(&path) {
                Ok(doc) => {
                    persist::apply_document(&mut state.scene, &doc);
                    eprintln!(
                        "[annotator] recovered {} ({} objects) from {}",
                        info.name,
                        doc.object_count(),
                        path.display()
                    );
                }
                Err(e) => eprintln!("[annotator] could not recover {}: {e}", path.display()),
            }
        }
        None => {
            eprintln!("[annotator] no saved session to recover");
        }
    }

    // ── REST API (Crestron) ────────────────────────────────────────────────
    // The main loop publishes a fresh snapshot at the bottom of every frame;
    // API threads only ever read it, so the render loop never blocks on HTTP.
    let api_snapshot: api::SharedSnapshot = std::sync::Arc::new(std::sync::RwLock::new(
        state.to_snapshot(menu_bar.active_idx, &menu_bar.devices, 0.0, 0.0),
    ));
    // The main loop drains this once per frame; HTTP threads block on the
    // per-command oneshot for at most 500 ms.
    // Set by the API when it changes the config (a password change), so
    // the main loop writes it to disk.
    let config_dirty: api::ConfigDirtyFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The API server is held so the settings panel can stop and start it.
    // When it is stopped the command channel is drained by a null
    // receiver, so the main loop's drain simply finds nothing.
    let mut api_server: Option<api::ApiServer> = None;
    let mut api_cmd_rx: api::ApiCommandRx = {
        let cfg_api = state.cfg.read().unwrap_or_else(|p| p.into_inner());
        if cfg_api.api.enabled {
            if cfg_api.api.token.as_deref().is_none_or(|t| t.is_empty()) {
                eprintln!(
                    "[api] WARNING — no shared token configured ([api] token = \"...\"). \
                     The REST API is open to anyone who can reach {}. \
                     Set a token to require X-Auth-Token on every /api/v1/* call.",
                    cfg_api.api.bind
                );
            }
            let (server, rx) = api::serve(
                &cfg_api.api.bind,
                cfg_api.api.port,
                api_snapshot.clone(),
                state.cfg.clone(),
                config_dirty.clone(),
            );
            api_server = Some(server);
            rx
        } else {
            eprintln!("[api] disabled by config ([api] enabled = false)");
            let (_tx, rx) = std::sync::mpsc::sync_channel(0);
            rx
        }
    };

    // ── Capture receiver (null / black screen until picker confirms) ───────
    let mut capture_rx: Receiver<Option<Frame>> = capture::null_receiver();
    let mut capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut capture_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Carries device health events from the capture thread — a device
    // being unplugged is noticed here.
    let mut capture_status_rx: Receiver<capture::CaptureStatus> = capture::null_status_receiver();

    // ── UI: V2 side-rail layout ──────────────────────────────────────────────
    let mut v2_layout = ui::v2::V2Layout::build(canvas_w as i32, canvas_h as i32);

    // ── Event loop ─────────────────────────────────────────────────────────
    let mut event_pump = sdl_context.event_pump().expect("event pump");
    // The render loop is paced by `present_vsync()` below — no manual sleep
    // here.  Capping the loop to a fixed 60 Hz would throttle capture
    // consumption on 120/144 Hz displays and force the `sync_channel(1)`
    // sender to drop frames we already paid the decode cost for.
    let mut last_video_frame: Option<Frame> = None;
    // Object count at the last autosave — used to detect that the scene
    // changed and an autosave is due.
    let mut last_autosaved_objects: usize = 0;
    let debug = std::env::var("ANNOTATOR_DEBUG").is_ok();
    render_pipeline::DEBUG.store(debug, Ordering::Relaxed);
    if debug {
        eprintln!("[dbg] ANNOTATOR_DEBUG enabled — verbose tracing on");
        eprintln!(
            "[dbg] init: canvas={}x{} device_picker={} tools=[{:#?}]",
            canvas_w,
            canvas_h,
            state.show_device_picker,
            [
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
            ],
        );
    }
    let mut dbg_render_count = 0u64;
    let mut dbg_upload_count = 0u64;
    let mut dbg_stat_timer = Instant::now();
    // Live FPS counters shown in the menu bar
    let mut display_fps: f32 = 0.0;
    let mut fps_frame_count = 0u32;
    let mut fps_timer = Instant::now();
    let mut text_cache = TextCache::new();

    'main: loop {
        let clipboard = video.clipboard();
        for event in event_pump.poll_iter() {
            if !input::process_event(
                &event,
                &mut state,
                &mut committed_canvas,
                &mut v2_layout,
                Some(&clipboard),
            ) {
                break 'main;
            }

            // ── Settings overlay keyboard / mouse events ──────────────────────
            if state.show_settings_overlay {
                match &event {
                    sdl2::event::Event::KeyDown {
                        keycode: Some(k), ..
                    } => match *k {
                        sdl2::keyboard::Keycode::Escape => {
                            state.show_settings_overlay = false;
                            save_settings(&mut state);
                            state.mark_snapshot_dirty();
                        }
                        sdl2::keyboard::Keycode::Up => {
                            if let Some(s) = &mut state.settings {
                                s.move_up();
                            }
                        }
                        sdl2::keyboard::Keycode::Down => {
                            if let Some(s) = &mut state.settings {
                                s.move_down(canvas_h as i32);
                            }
                        }
                        sdl2::keyboard::Keycode::Left => {
                            if let Some(s) = &mut state.settings {
                                s.adjust(false);
                            }
                        }
                        sdl2::keyboard::Keycode::Right => {
                            if let Some(s) = &mut state.settings {
                                s.adjust(true);
                            }
                        }
                        sdl2::keyboard::Keycode::Return | sdl2::keyboard::Keycode::KpEnter => {
                            if let Some(s) = &mut state.settings {
                                let action = s.enter();
                                if action == OverlayAction::ToggleFullscreen {
                                    state.toggle_fullscreen = true;
                                    state.show_settings_overlay = false;
                                    save_settings(&mut state);
                                    state.mark_snapshot_dirty();
                                }
                            }
                        }
                        _ => {}
                    },
                    sdl2::event::Event::MouseButtonDown { x, y, .. } => {
                        if let Some(s) = &mut state.settings {
                            let canvas_w_i = canvas_w as i32;
                            let canvas_h_i = canvas_h as i32;
                            let action = s.click(*x, *y, canvas_w_i, canvas_h_i);
                            match action {
                                OverlayAction::Close => {
                                    state.show_settings_overlay = false;
                                    save_settings(&mut state);
                                    state.mark_snapshot_dirty();
                                }
                                OverlayAction::ToggleFullscreen => {
                                    state.toggle_fullscreen = true;
                                    state.show_settings_overlay = false;
                                    save_settings(&mut state);
                                    state.mark_snapshot_dirty();
                                }
                                OverlayAction::None => {}
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                // F3 opens the settings overlay
                if let sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::F3),
                    ..
                } = &event
                {
                    state.show_settings_overlay = true;
                }
            }
        }

        // ── Window resize (WM tiling, user drag, fullscreen toggle) ───────
        let (new_w, new_h) = state.window_size;
        if new_w != canvas_w || new_h != canvas_h {
            canvas_w = new_w;
            canvas_h = new_h;
            // Recreate pixel canvas and textures at the new size
            committed_canvas = Canvas::new(canvas_w, canvas_h);
            committed_tex = texture_creator
                .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
                .expect("committed overlay texture");
            committed_tex.set_blend_mode(BlendMode::Blend);
            // Resize the pipeline's double buffer to match.
            pipeline.resize(canvas_w, canvas_h);
            // Recreate BOTH video textures at the new size and reset the
            // front-index; the render loop's dimension check will recreate
            // them again at the frame's native size if those differ.
            video_tex = [
                mk_video_tex(video_tex_fmt, canvas_w, canvas_h),
                mk_video_tex(video_tex_fmt, canvas_w, canvas_h),
            ];
            video_tex_idx = 0;
            video_tex_w = canvas_w;
            video_tex_h = canvas_h;
            committed_canvas.dirty = true;
            pipeline.render_into(&committed_canvas, &state.scene);
            pipeline_seq = 0; // reset so the next upload is detected
            v2_layout = ui::v2::V2Layout::build(canvas_w as i32, canvas_h as i32);
        }

        // ── Menu bar clicks ────────────────────────────────────────────────
        if let Some((mx, my)) = state.pending_menu_click.take() {
            let action = menu_bar.click(mx, my, canvas_w as i32);
            match action {
                ui::MenuAction::Undo => {
                    state.scene.undo();
                    state.mark_snapshot_dirty();
                }
                ui::MenuAction::Redo => {
                    state.scene.redo();
                    state.mark_snapshot_dirty();
                }
                ui::MenuAction::Clear => {
                    state.scene.clear();
                    state.mark_snapshot_dirty();
                }
                ui::MenuAction::OpenSettings => {
                    state.show_settings_overlay = true;
                    refresh_settings(&mut state);
                    state.mark_snapshot_dirty();
                }
                _ => {}
            }
            if let ui::MenuAction::SelectDevice(idx) = action {
                menu_bar.active_idx = idx;
                state.mark_snapshot_dirty();
                if idx == 0 {
                    capture_stop.store(true, Ordering::Relaxed);
                    capture_rx = capture::null_receiver();
                    capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                    last_video_frame = None;
                    state.input_mode = 0;
                } else if let Some(picker) = &state.device_picker {
                    capture_stop.store(true, Ordering::Relaxed);
                    let dev = &picker.devices[idx - 1];
                    // Use first (best) mode for menu bar quick-switch
                    let mode = dev.modes.first().cloned();
                    let (rx, fps, stop, status) = capture::start_capture(dev.path.clone(), mode);
                    capture_rx = rx;
                    capture_fps_counter = fps;
                    capture_stop = stop;
                    capture_status_rx = status;
                    // `input_mode` is authoritative — it always reports the
                    // mode index the capture thread was actually started
                    // with (0 = the "best" mode).
                    state.input_mode = 0;
                }
            }
        }
        if state.menu_close_requested {
            state.menu_close_requested = false;
            // Only close if not clicking inside the dropdown area
            // (menu_bar.click handles dropdown clicks; this fires for canvas/toolbar clicks)
            if menu_bar.open {
                menu_bar.close();
            }
        }

        // ── Device picker selection ────────────────────────────────────────
        if let Some((dev_idx, mode_idx)) = state.picker_confirmed.take() {
            if dev_idx == 0 {
                // No input - black screen
                capture_stop.store(true, Ordering::Relaxed);
                capture_rx = capture::null_receiver();
                capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                last_video_frame = None;
            } else if let Some(picker) = &state.device_picker {
                capture_stop.store(true, Ordering::Relaxed);
                let dev = &picker.devices[dev_idx - 1];
                let mode = dev.modes.get(mode_idx).cloned();
                let (rx, fps, stop, status) = capture::start_capture(dev.path.clone(), mode);
                capture_rx = rx;
                capture_fps_counter = fps;
                capture_stop = stop;
                capture_status_rx = status;
            }
            state.input_mode = if dev_idx == 0 { 0 } else { mode_idx };
            menu_bar.active_idx = dev_idx;
            state.mark_snapshot_dirty();
        }

        // ── API commands (Crestron mutations; drained once per frame) ─────
        for cmd in api_cmd_rx.try_iter() {
            let result = match cmd.kind {
                state::ApiCommandKind::SetTool(tool) => {
                    input::switch_tool(&mut state, tool); // preserves per-tool sizes
                    state::ApiResult::Ok(serde_json::json!({
                        "tool": tool.name(),
                        "brush_size": state.brush_size,
                    }))
                }
                state::ApiCommandKind::SetColor(r, g, b) => {
                    state.color = (r, g, b);
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "color": format!("{r:02x}{g:02x}{b:02x}"),
                    }))
                }
                state::ApiCommandKind::SetBrushSize(px) => {
                    state.brush_size = state::clamp_brush_size(px);
                    state.tool_sizes[state.active_tool.index()] = state.brush_size;
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "brush_size": state.brush_size,
                    }))
                }
                state::ApiCommandKind::SetBrushSizeDelta(d) => {
                    let cur = state.brush_size as i64;
                    state.brush_size = state::clamp_brush_size((cur + d).max(0) as u32);
                    state.tool_sizes[state.active_tool.index()] = state.brush_size;
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "brush_size": state.brush_size,
                    }))
                }
                state::ApiCommandKind::SetInput { index, mode } => {
                    if index > menu_bar.devices.len() {
                        state::ApiResult::Conflict(format!(
                            "input index {index} out of range (0..={})",
                            menu_bar.devices.len()
                        ))
                    } else if index > 0 && mode >= menu_bar.devices[index - 1].modes.len() {
                        state::ApiResult::Rejected(format!(
                            "mode index {mode} out of range for input {index} (0..={})",
                            menu_bar.devices[index - 1].modes.len().saturating_sub(1)
                        ))
                    } else {
                        // Same stop/start sequence as the menu-bar quick-switch.
                        capture_stop.store(true, Ordering::Relaxed);
                        if index == 0 {
                            capture_rx = capture::null_receiver();
                            capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                            last_video_frame = None;
                        } else {
                            let dev = &menu_bar.devices[index - 1];
                            let (rx, fps, stop, status) = capture::start_capture(
                                dev.path.clone(),
                                dev.modes.get(mode).cloned(),
                            );
                            capture_rx = rx;
                            capture_fps_counter = fps;
                            capture_stop = stop;
                            capture_status_rx = status;
                        }
                        menu_bar.active_idx = index;
                        state.input_mode = mode;
                        state.mark_snapshot_dirty();
                        state::ApiResult::Ok(serde_json::json!({
                            "input": index,
                            "input_mode": mode,
                            "mode_label": mode_label(&menu_bar.devices, index, mode),
                        }))
                    }
                }
                state::ApiCommandKind::Undo => {
                    if state.scene.undo_stack.is_empty() {
                        state::ApiResult::Conflict("nothing to undo".into())
                    } else {
                        state.scene.undo();
                        state.mark_snapshot_dirty();
                        state::ApiResult::Ok(serde_json::json!({
                            "can_undo": !state.scene.undo_stack.is_empty(),
                            "can_redo": !state.scene.redo_stack.is_empty(),
                            "object_count": state.scene.objects().len(),
                        }))
                    }
                }
                state::ApiCommandKind::Redo => {
                    if state.scene.redo_stack.is_empty() {
                        state::ApiResult::Conflict("nothing to redo".into())
                    } else {
                        state.scene.redo();
                        state.mark_snapshot_dirty();
                        state::ApiResult::Ok(serde_json::json!({
                            "can_undo": !state.scene.undo_stack.is_empty(),
                            "can_redo": !state.scene.redo_stack.is_empty(),
                            "object_count": state.scene.objects().len(),
                        }))
                    }
                }
                state::ApiCommandKind::Clear => {
                    // `clear` pushes an undo point of its own, so a clear
                    // can be undone like any other edit.
                    state.scene.clear();
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "can_undo": !state.scene.undo_stack.is_empty(),
                        "can_redo": !state.scene.redo_stack.is_empty(),
                        "object_count": state.scene.objects().len(),
                    }))
                }
                state::ApiCommandKind::ToggleFreeze => {
                    state.freeze_frame = !state.freeze_frame;
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "freeze": state.freeze_frame,
                    }))
                }
                state::ApiCommandKind::SetFreeze(on) => {
                    state.freeze_frame = on;
                    state.mark_snapshot_dirty();
                    state::ApiResult::Ok(serde_json::json!({
                        "freeze": state.freeze_frame,
                    }))
                }
                state::ApiCommandKind::SaveScene { name } => {
                    let file = if name.is_empty() {
                        persist::timestamped_name()
                    } else {
                        format!("{name}.json")
                    };
                    match persist::ensure_save_dir() {
                        Ok(dir) => {
                            match persist::save_to_path(
                                &state.scene,
                                canvas_w,
                                canvas_h,
                                &dir.join(&file),
                            ) {
                                Ok(bytes) => state::ApiResult::Ok(serde_json::json!({
                                    "saved": file,
                                    "bytes": bytes,
                                    "objects": state.scene.objects().len(),
                                })),
                                Err(msg) => state::ApiResult::Rejected(msg),
                            }
                        }
                        Err(e) => state::ApiResult::Rejected(format!(
                            "cannot create the save directory: {e}"
                        )),
                    }
                }
                state::ApiCommandKind::LoadScene { name } => {
                    let file = if name.ends_with(".json") {
                        name.clone()
                    } else {
                        format!("{name}.json")
                    };
                    let dir = persist::save_dir();
                    let path = dir.join(&file);
                    match persist::load_from_path(&path) {
                        Ok(doc) => {
                            persist::apply_document(&mut state.scene, &doc);
                            state.mark_snapshot_dirty();
                            state::ApiResult::Ok(serde_json::json!({
                                "loaded": file,
                                "objects": state.scene.objects().len(),
                                "canvas_width": doc.canvas_width,
                                "canvas_height": doc.canvas_height,
                            }))
                        }
                        Err(msg) => state::ApiResult::Conflict(msg),
                    }
                }
                state::ApiCommandKind::Export { name } => {
                    let file = if name.is_empty() {
                        export::timestamped_name()
                    } else if name.ends_with(".png") {
                        name.clone()
                    } else {
                        format!("{name}.png")
                    };
                    match export::ensure_export_dir() {
                        Ok(dir) => {
                            let path = dir.join(&file);
                            let cfg_api = state.cfg.read().unwrap_or_else(|p| p.into_inner());
                            let meta = export::ExportMeta {
                                case_reference: cfg_api.api.case_reference.clone(),
                                annotator: cfg_api.api.annotator.clone(),
                                note: cfg_api.api.case_note.clone(),
                                input: menu_bar
                                    .devices
                                    .get(menu_bar.active_idx.saturating_sub(1))
                                    .map(|d| d.name.clone())
                                    .unwrap_or_default(),
                                tool: state.active_tool.name().to_string(),
                                color: format!(
                                    "{:02x}{:02x}{:02x}",
                                    state.color.0, state.color.1, state.color.2
                                ),
                                brush_size: state.brush_size,
                            };
                            // The export is the annotation layer only —
                            // no menu bar, no tool rail — composited over
                            // the current video frame so the result is a
                            // usable case artifact rather than an
                            // annotation layer floating on black.
                            let canvas =
                                export::render_scene_to_canvas(&state.scene, canvas_w, canvas_h);
                            let result = match export_video_frame(
                                &canvas,
                                last_video_frame.as_ref(),
                                canvas_w,
                                canvas_h,
                            ) {
                                Some(rgb) => export::write_rgb_png(&rgb, canvas_w, canvas_h, &path),
                                None => export::write_png(&canvas, &meta, &path),
                            };
                            match result {
                                Ok(bytes) => state::ApiResult::Ok(serde_json::json!({
                                    "exported": file,
                                    "path": path.display().to_string(),
                                    "bytes": bytes,
                                    "objects": state.scene.objects().len(),
                                })),
                                Err(msg) => state::ApiResult::Rejected(msg),
                            }
                        }
                        Err(e) => state::ApiResult::Rejected(format!(
                            "cannot create the export directory: {e}"
                        )),
                    }
                }
            };
            let _ = cmd.reply.send(result);
        }

        // ── Latest video frame (non-blocking drain) ────────────────────────
        let mut new_video_frame = false;
        if !state.freeze_frame {
            while let Ok(f) = capture_rx.try_recv() {
                if let Some(frame) = f {
                    last_video_frame = Some(frame);
                    new_video_frame = true;
                }
            }
        }

        // ── Capture device health ──────────────────────────────────────────
        // The capture thread reports when a device is unplugged.  The
        // device list is re-enumerated so the UI reflects what is
        // actually plugged in, and the state is published so the API and
        // the portal can say so too.
        let mut device_lost = false;
        while let Ok(status) = capture_status_rx.try_recv() {
            match status {
                capture::CaptureStatus::DeviceLost => {
                    device_lost = true;
                    eprintln!("[annotator] the capture device was unplugged");
                }
                capture::CaptureStatus::Streaming => {
                    eprintln!("[annotator] the capture device is streaming again");
                    state.capture_device_present = true;
                    state.mark_snapshot_dirty();
                }
            }
        }
        if device_lost {
            state.capture_device_present = false;
            // Re-enumerate: the device is gone, and something else may
            // have been plugged in at the same time.
            let devices = capture::enumerate_devices();
            menu_bar.devices = devices.clone();
            if let Some(picker) = &mut state.device_picker {
                picker.devices = devices;
                picker.device_cursor = picker
                    .device_cursor
                    .min(picker.devices.len().saturating_sub(1));
            }
            // The active index may now point past the end of the list.
            if menu_bar.active_idx > menu_bar.devices.len() {
                menu_bar.active_idx = 0;
            }
            state.mark_snapshot_dirty();
        }

        // ── Fullscreen toggle ──────────────────────────────────────────────
        if state.toggle_fullscreen {
            state.toggle_fullscreen = false;
            use sdl2::video::FullscreenType;
            let win = sdl_canvas.window_mut();
            if state.is_fullscreen {
                win.set_fullscreen(FullscreenType::Off).ok();
                state.is_fullscreen = false;
            } else {
                win.set_fullscreen(FullscreenType::Desktop).ok();
                state.is_fullscreen = true;
            }
            state.mark_snapshot_dirty();
        }

        // ── Screenshot capture ─────────────────────────────────────────────
        if state.screenshot_requested {
            state.screenshot_requested = false;
            if let Some(path) = capture::save_screenshot(&sdl_canvas, canvas_w, canvas_h) {
                eprintln!("[annotator] screenshot saved: {}", path);
            } else {
                eprintln!("[annotator] screenshot failed");
            }
        }

        // ── Save / load requests ───────────────────────────────────────────
        if let Some(name) = state.save_requested.take() {
            let file = if name.is_empty() {
                persist::timestamped_name()
            } else {
                format!("{name}.json")
            };
            match persist::ensure_save_dir() {
                Ok(dir) => {
                    let path = dir.join(&file);
                    match persist::save_to_path(&state.scene, canvas_w, canvas_h, &path) {
                        Ok(bytes) => eprintln!(
                            "[annotator] scene saved: {} ({} bytes, {} objects)",
                            path.display(),
                            bytes,
                            state.scene.objects().len()
                        ),
                        Err(e) => eprintln!("[annotator] save failed: {e}"),
                    }
                }
                Err(e) => eprintln!("[annotator] cannot create the save directory: {e}"),
            }
        }
        if let Some(name) = state.load_requested.take() {
            // An empty name means "the most recent save".
            let file = if name.is_empty() {
                match persist::list_saves().first() {
                    Some(info) => info.name.clone(),
                    None => {
                        eprintln!("[annotator] no saved scenes to load");
                        continue;
                    }
                }
            } else if name.ends_with(".json") {
                name
            } else {
                format!("{name}.json")
            };
            let path = persist::save_dir().join(&file);
            match persist::load_from_path(&path) {
                Ok(doc) => {
                    persist::apply_document(&mut state.scene, &doc);
                    state.mark_snapshot_dirty();
                    eprintln!(
                        "[annotator] scene loaded: {} ({} objects)",
                        path.display(),
                        state.scene.objects().len()
                    );
                }
                Err(e) => eprintln!("[annotator] load failed: {e}"),
            }
        }

        // ── Text commit onto the scene ─────────────────────────────────────
        // Text becomes a real scene object rather than pixels painted
        // straight onto the canvas.  That way it can be selected, moved,
        // undone and saved like any other annotation.
        if state.pending_text_commit {
            state.pending_text_commit = false;
            let text = state.text_buffer.clone();
            let pos = state.text_pos;
            let color = state.color;
            if let Some(obj) = make_text_obj(&text, pos, color, 20, &font_large) {
                state.scene.push_undo();
                state.scene.add(scene::DrawObject::Text(obj));
            }
            state.text_buffer.clear();
            state.mark_snapshot_dirty();
        }

        // ── Render ────────────────────────────────────────────────────────
        sdl_canvas.set_draw_color(Color::RGB(0, 0, 0));
        sdl_canvas.clear();

        // Video layer — double-buffered upload to eliminate capture-card tearing.
        // See the declaration above for the rationale.  In brief: BACK gets the
        // upload, FRONT gets the blit, rotate after the blit so the freshly-
        // written buffer becomes next frame's front.  We only rotate on frames
        // where `new_video_frame` is true, so when no capture frame arrives
        // the GPU keeps blitting from the same front buffer (no spurious
        // texture switches).
        if let Some(frame) = &last_video_frame
            && frame.width > 0
            && frame.height > 0
        {
            let desired_fmt = match frame.format {
                FrameFormat::Yuyv => PixelFormatEnum::YUY2,
                FrameFormat::Rgb24 => PixelFormatEnum::RGB24,
            };
            // Recreate BOTH textures if dimensions or format changed.
            // Recreating only one is a tearing window (the in-flight blit
            // could be sampling the one we just replaced), so we replace
            // both atomically and reset the front-index.
            if frame.width != video_tex_w
                || frame.height != video_tex_h
                || desired_fmt != video_tex_fmt
            {
                let t0 = texture_creator.create_texture_streaming(
                    desired_fmt,
                    frame.width,
                    frame.height,
                );
                let t1 = texture_creator.create_texture_streaming(
                    desired_fmt,
                    frame.width,
                    frame.height,
                );
                if let (Ok(t0), Ok(t1)) = (t0, t1) {
                    video_tex = [t0, t1];
                    video_tex_idx = 0;
                    video_tex_w = frame.width;
                    video_tex_h = frame.height;
                    video_tex_fmt = desired_fmt;
                }
            }
            if new_video_frame {
                dbg_upload_count += 1;
                let pitch = match frame.format {
                    FrameFormat::Yuyv => (frame.width * 2) as usize,
                    FrameFormat::Rgb24 => (frame.width * 3) as usize,
                };
                // Upload into the BACK buffer (the one not being sampled by
                // this frame's blit).
                let back_idx = video_tex_idx ^ 1;
                video_tex[back_idx].update(None, &frame.data, pitch).ok();
            }
            dbg_render_count += 1;
            let dst = scale_rect(canvas_w, canvas_h, frame.width, frame.height);
            // Blit the FRONT buffer — the GPU is currently sampling this
            // texture and our upload targeted the OTHER one, so there is
            // no write-during-read hazard.
            sdl_canvas.copy(&video_tex[video_tex_idx], None, dst).ok();
            // Rotate after the blit: the buffer we just wrote becomes the
            // front for next frame.
            if new_video_frame {
                video_tex_idx ^= 1;
            }
        }

        if debug && dbg_stat_timer.elapsed().as_secs() >= 2 {
            eprintln!(
                "render: blit_count={} tex_uploads={} (~{:.1} fps)",
                dbg_render_count,
                dbg_upload_count,
                dbg_render_count as f64 / dbg_stat_timer.elapsed().as_secs_f64().max(0.001)
            );
            // ── DBG: frame-state summary — what the render loop is seeing.
            // Useful for diagnosing "strokes don't commit" on resource-
            // constrained platforms: if scene.dirty never goes true, input
            // events aren't reaching handle_release.  If pipeline_seq never
            // advances, the worker is stuck or failed to spawn.
            eprintln!(
                "[dbg] frame: scene.dirty={} committed.dirty={} active_stroke={} scene.objects={} pipeline_seq={}",
                state.scene.dirty,
                committed_canvas.dirty,
                state.active_stroke.is_some(),
                state.scene.objects().len(),
                pipeline_seq,
            );
            dbg_render_count = 0;
            dbg_upload_count = 0;
            dbg_stat_timer = Instant::now();
        }

        // Update display fps counter every second
        fps_frame_count += 1;
        let fps_elapsed = fps_timer.elapsed().as_secs_f32();
        if fps_elapsed >= 1.0 {
            display_fps = fps_frame_count as f32 / fps_elapsed;
            fps_frame_count = 0;
            fps_timer = Instant::now();
        }

        // ── Annotation overlay ─────────────────────────────────────────
        // Two layers:
        //   1. Committed scene — rasterized off-thread by `pipeline`.  When
        //      the scene becomes dirty OR the input has written to
        //      `committed_canvas` (eraser/text-blit), we submit a render
        //      job; the worker rasterizes in the background; the main
        //      thread re-uploads the front buffer to the GPU texture when
        //      the worker signals completion.
        //   2. Active stroke — drawn directly via SDL2 GPU primitives on
        //      top of `committed_tex`.  No CPU canvas, no 8.3 MB
        //      upload per frame.
        let overlay_src = Rect::new(
            0,
            MENU_HEIGHT,
            canvas_w,
            canvas_h.saturating_sub(toolbar_height(canvas_w as i32) as u32 + MENU_HEIGHT as u32),
        );

        // Live eraser preview: while the eraser is being dragged, skip the
        // background pipeline (the worker would re-rasterize the pre-wipe
        // scene on top of our wipe and undo the visible erase) and instead
        // upload `committed_canvas` to the GPU texture each frame.
        //
        // IMPORTANT: we upload ONLY the bounding box of pixels touched
        // since the last upload, not the whole 8.3 MB.  Full-canvas
        // uploads were flagged as the dominant
        // per-frame CPU↔GPU traffic cost (~500 MB/s on a 1080p stream);
        // on RPi3/R6A that's enough to drop a few fps by itself.  The
        // partial-rect upload costs O(rect_area * 4) bytes — a few KB
        // per drag point — which is why we accumulate a bbox in
        // `state.eraser_dirty_*` rather than uploading per point.
        //
        // On release, `committed_canvas` retains the wipe, `erase_path`
        // sets `scene.dirty`, and the pipeline takes over on the next
        // frame — `submit_scene` copies the wiped canvas into the back
        // buffer and the worker bakes the trimmed scene on top.
        let erasing_now = state.eraser_drag.is_some();
        if erasing_now {
            let (min_x, min_y) = state.eraser_dirty_min;
            let (max_x, max_y) = state.eraser_dirty_max;
            if min_x < max_x && min_y < max_y {
                let x = min_x;
                let y = min_y;
                let w = (max_x - min_x) as u32;
                let h = (max_y - min_y) as u32;
                let stride = canvas_w as usize * 4;
                let offset = (y as usize) * stride + (x as usize) * 4;
                let rect = Rect::new(x, y, w, h);
                committed_tex
                    .update(Some(rect), &committed_canvas.pixels[offset..], stride)
                    .ok();
                // Reset the accumulator so the next frame starts fresh.
                state.eraser_dirty_min = (i32::MAX, i32::MAX);
                state.eraser_dirty_max = (i32::MIN, i32::MIN);
                committed_canvas.dirty = false;
            }
        } else {
            // Submit a render job whenever scene or input is dirty.
            if state.scene.dirty || committed_canvas.dirty {
                if debug {
                    eprintln!(
                        "[dbg] render: scene.dirty={} committed.dirty={} -> submitting scene (objects={})",
                        state.scene.dirty,
                        committed_canvas.dirty,
                        state.scene.objects().len(),
                    );
                }
                // Copy the input canvas into the pipeline's back buffer so the
                // worker can use it as the base layer, then submit the scene.
                // The worker rasterizes `scene` into the back buffer, then
                // atomically swaps front/back; the next `poll()` here will
                // see the new front and call `upload_to()` to push it to GPU.
                pipeline.submit_scene(&committed_canvas, &state.scene);
                committed_canvas.dirty = false;
                state.scene.dirty = false;
            }

            // Drain any completed worker frames into the GPU texture.
            if pipeline.poll(&mut pipeline_seq) {
                if debug {
                    eprintln!(
                        "[dbg] render: poll returned true (new frame ready), uploading to texture"
                    );
                }
                pipeline.upload_to(&mut committed_tex);
                if debug {
                    eprintln!("[dbg] render: texture upload complete");
                }
            }
        }

        sdl_canvas
            .copy(&committed_tex, overlay_src, overlay_src)
            .ok();

        // Layer 2: active stroke — direct GPU primitives.
        if let Some(ref active) = state.active_stroke {
            draw_active_stroke(&mut sdl_canvas, active, overlay_src);
        }

        // Shape tool preview — draw directly via SDL2 GPU calls (no CPU canvas work)
        if let Some(prev_obj) = input::preview_object(&state) {
            draw_preview_sdl(&mut sdl_canvas, &prev_obj);
        }
        // Selection bounding boxes — drawn on top of everything when the Select
        // tool has one or more hits.  Figma/Canva style: a thin accent-
        // colored rectangle with a 4-pixel margin around each object's
        // bounds.  Multi-select draws one box per selected object so the
        // user sees every member, not just the union.  All drawn directly
        // via SDL2 GPU primitives — no CPU canvas work, no extra upload.
        if state.active_tool == Tool::Select {
            let accent = Color::RGB(0x4a, 0x9e, 0xff);
            // Cache the indices to avoid borrowing `state.scene` while we
            // also want to borrow `state.scene.objects()` in the loop.
            let selected: Vec<usize> = state.scene.selected.clone();
            for idx in selected {
                if let Some(obj) = state.scene.objects().get(idx) {
                    let (bx, by, bw, bh) = obj.bounds();
                    let pad = 4i32;
                    let x = bx - pad;
                    let y = by - pad;
                    let w = (bw + pad * 2).max(1) as u32;
                    let h = (bh + pad * 2).max(1) as u32;
                    let r = Rect::new(x, y, w, h);
                    // 2px border accent — draw two concentric rects for a
                    // subtle inset effect.
                    sdl_canvas.set_draw_color(accent);
                    sdl_canvas.draw_rect(r).ok();
                    let r2 = Rect::new(x + 2, y + 2, w.saturating_sub(4), h.saturating_sub(4));
                    sdl_canvas.set_draw_color(accent);
                    sdl_canvas.draw_rect(r2).ok();
                }
            }
            // Live marquee outline.  Drawn with a slightly different
            // color (cyan-ish) so it reads as "selection in progress"
            // versus the per-object accent.
            if let Some((x0, y0, x1, y1)) = state.marquee {
                let lx = x0.min(x1);
                let rx = x0.max(x1);
                let ty = y0.min(y1);
                let by = y0.max(y1);
                let r = Rect::new(lx, ty, (rx - lx).max(1) as u32, (by - ty).max(1) as u32);
                sdl_canvas.set_draw_color(Color::RGBA(0x4a, 0x9e, 0xff, 90));
                // Fill is a translucent rectangle so the user can see which
                // objects will be picked up.  SDL2's BlendMode::Blend is on.
                sdl_canvas.fill_rect(r).ok();
                sdl_canvas.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                sdl_canvas.draw_rect(r).ok();
            }
        }
        // Laser pointer trail — Tron-style solid fading line, never committed to scene
        {
            let laser_fade = state.settings.as_ref().map(|s| s.laser_fade).unwrap_or(1.2);
            let now = Instant::now();
            state
                .laser_trail
                .retain(|(_, _, t, _)| now.duration_since(*t).as_secs_f32() < laser_fade);
            let tlen = state.laser_trail.len();
            let size = (state.tool_sizes[Tool::Laser.index()] as i32).clamp(1, 20);
            let half = size / 2;
            // Draw connected segments — old end is transparent, new end is opaque
            for i in 1..tlen {
                let (x0, y0, t0, (r, g, b)) = state.laser_trail[i - 1];
                let (x1, y1, _, _) = state.laser_trail[i];
                let age = now.duration_since(t0).as_secs_f32();
                let frac = (1.0 - age / laser_fade).max(0.0_f32);
                let alpha = (frac * frac * 230.0) as u8;
                sdl_canvas.set_draw_color(Color::RGBA(r, g, b, alpha));
                let dx = (x1 - x0) as f32;
                let dy = (y1 - y0) as f32;
                let len = (dx * dx + dy * dy).sqrt().max(0.001);
                let (px, py) = (-dy / len, dx / len);
                for k in -half..=half {
                    let ox = (px * k as f32).round() as i32;
                    let oy = (py * k as f32).round() as i32;
                    sdl_canvas
                        .draw_line(Point::new(x0 + ox, y0 + oy), Point::new(x1 + ox, y1 + oy))
                        .ok();
                }
            }
            // Bright head dot at the current cursor position
            if let Some((x, y, _, (r, g, b))) = state.laser_trail.last() {
                let core = half + 3;
                sdl_canvas.set_draw_color(Color::RGBA(*r, *g, *b, 255));
                for dy in -core..=core {
                    let xw = (((core * core - dy * dy) as f32).sqrt()) as i32;
                    sdl_canvas
                        .draw_line(Point::new(x - xw, y + dy), Point::new(x + xw, y + dy))
                        .ok();
                }
            }
        }

        // Eraser preview — outline of the eraser disc at the most recent few
        // drag points so the user can see what they're about to wipe.  Drawn
        // via SDL2 directly (never composited into the canvas) so it's purely
        // a UI hint and disappears on mouse-up.
        if let Some(ref drag) = state.eraser_drag {
            let r = (drag.radius as i32).clamp(1, 80);
            sdl_canvas.set_draw_color(Color::RGBA(220, 220, 220, 220));
            for &(cx, cy) in drag.points.iter().rev().take(6) {
                let steps = 36usize;
                PREVIEW_PTS.with(|cell| {
                    let mut pts = cell.borrow_mut();
                    pts.clear();
                    pts.reserve(steps + 1);
                    for i in 0..=steps {
                        let a = 2.0 * std::f32::consts::PI * i as f32 / steps as f32;
                        pts.push(Point::new(
                            cx + (r as f32 * a.cos()).round() as i32,
                            cy + (r as f32 * a.sin()).round() as i32,
                        ));
                    }
                    sdl_canvas.draw_lines(pts.as_slice()).ok();
                });
            }
        }

        // Live text cursor preview
        if state.text_input_active && !state.text_buffer.is_empty() {
            let (tx, ty) = state.text_pos;
            let (r, g, b) = state.color;
            let tc = sdl_canvas.texture_creator();
            let preview = format!("{}|", state.text_buffer);
            ui::render_text(
                &mut sdl_canvas,
                &font_large,
                &tc,
                &mut text_cache,
                &preview,
                Color::RGBA(r, g, b, 230),
                Rect::new(tx, ty, (canvas_w as i32 - tx).max(0) as u32, 32),
            );
        }

        // Tool rail (V2) — hidden when the setting is on.
        let hide_toolbar = state
            .settings
            .as_ref()
            .map(|s| s.hide_toolbar == Toggle::On)
            .unwrap_or(false);
        if !hide_toolbar {
            ui::v2::draw_v2_toolbar(
                &mut sdl_canvas,
                &font,
                &icon_font,
                &v2_layout,
                state.active_tool,
                state.color,
                state.brush_size,
                state.freeze_frame,
                &mut text_cache,
            );
        }

        // Device picker on top
        if state.show_device_picker
            && let Some(picker) = &state.device_picker
        {
            picker.draw(
                &mut sdl_canvas,
                &font,
                canvas_w as i32,
                canvas_h as i32,
                &mut text_cache,
            );
        }

        // Settings overlay — modal panel drawn above everything else
        if state.show_settings_overlay
            && let Some(ref mut settings) = state.settings
        {
            settings.draw(
                &mut sdl_canvas,
                &font,
                canvas_w as i32,
                canvas_h as i32,
                &mut text_cache,
            );
        }

        // Menu bar (always on top)
        let capture_fps = capture_fps_counter.load(Ordering::Relaxed) as f32 / 10.0;
        let show_fps = state
            .settings
            .as_ref()
            .map(|s| s.show_fps == Toggle::On)
            .unwrap_or(true);
        let show_time = state
            .settings
            .as_ref()
            .map(|s| s.show_time == Toggle::On)
            .unwrap_or(false);
        // The clock is only recomputed once a second; between ticks the
        // previous label is reused.
        if show_time {
            let now = Instant::now();
            if now.duration_since(clock_updated) >= Duration::from_secs(1) {
                clock_label = format_clock(now);
                clock_updated = now;
            }
        }
        menu_bar.draw(
            &mut sdl_canvas,
            &font,
            canvas_w as i32,
            !state.scene.undo_stack.is_empty(),
            !state.scene.redo_stack.is_empty(),
            show_fps,
            show_time,
            &clock_label,
            capture_fps,
            display_fps,
            &mut text_cache,
        );

        sdl_canvas.present();

        // ── API enable / disable ───────────────────────────────────────────
        // The settings panel can switch the REST API off.  The change is
        // applied here rather than in the event handler, because stopping
        // the server joins its thread — which must not happen while the
        // event handler holds the borrow on the settings.
        if let Some(s) = &state.settings {
            let wanted = s.api_enabled == Toggle::On;
            let running = api_server.as_ref().is_some_and(|a| a.is_running());
            if wanted != running {
                if wanted {
                    // Start it.  The bind address and port come from the
                    // config, so a token set there still applies.
                    let cfg_api = state.cfg.read().unwrap_or_else(|p| p.into_inner());
                    let (server, rx) = api::serve(
                        &cfg_api.api.bind,
                        cfg_api.api.port,
                        api_snapshot.clone(),
                        state.cfg.clone(),
                        config_dirty.clone(),
                    );
                    api_server = Some(server);
                    api_cmd_rx = rx;
                    eprintln!("[api] started");
                } else {
                    // Stop it.  The listening socket is released, so the
                    // API is genuinely unreachable.
                    if let Some(mut server) = api_server.take() {
                        server.stop();
                    }
                    // Drain the channel with a receiver nobody writes to.
                    let (_tx, rx) = std::sync::mpsc::sync_channel(0);
                    api_cmd_rx = rx;
                    eprintln!("[api] disabled");
                }
                // Remember the choice so it survives a restart.
                {
                    let mut cfg = state.cfg.write().unwrap_or_else(|p| p.into_inner());
                    cfg.api.enabled = wanted;
                    if let Err(e) = config::save(&cfg) {
                        eprintln!("[annotator] could not save the config: {e}");
                    }
                }
                state.mark_snapshot_dirty();
            }
        }

        // ── Autosave ──────────────────────────────────────────────────────
        // A crash or power cut must not destroy the annotations.  The
        // scene is autosaved periodically, but only when it has actually
        // changed — the object count is the cheap signal for that, since
        // the scene's own `dirty` flag is cleared by the renderer and so
        // cannot be used here.
        const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
        let object_count = state.scene.objects().len();
        if object_count != last_autosaved_objects {
            state.scene_changed = true;
        }
        if state.scene_changed
            && state.last_autosave.elapsed() >= AUTOSAVE_INTERVAL
            && object_count > 0
        {
            state.scene_changed = false;
            state.last_autosave = Instant::now();
            last_autosaved_objects = object_count;
            let file = persist::timestamped_name();
            match persist::ensure_save_dir() {
                Ok(dir) => {
                    let path = dir.join(&file);
                    match persist::save_to_path(&state.scene, canvas_w, canvas_h, &path) {
                        Ok(_) => eprintln!(
                            "[annotator] autosaved {} ({} objects)",
                            path.display(),
                            object_count
                        ),
                        Err(e) => eprintln!("[annotator] autosave failed: {e}"),
                    }
                }
                Err(e) => eprintln!("[annotator] cannot create the save directory: {e}"),
            }
        }

        // ── Publish API snapshot (HTTP threads only ever read it) ─────────
        // Only rebuild the snapshot when something changed OR the
        // FPS counters drift past a 500 ms ceiling.  The HTTP API polls
        // < 1 Hz so a half-second staleness on `fps` / `capture_fps` is
        // imperceptible; everything else is rebuilt instantly on change.
        let snap_due = state.snapshot_dirty
            || state.last_snapshot_publish.elapsed() >= Duration::from_millis(500);
        if snap_due {
            if let Ok(mut snap) = api_snapshot.write() {
                *snap = state.to_snapshot(
                    menu_bar.active_idx,
                    &menu_bar.devices,
                    display_fps,
                    capture_fps,
                );
            }
            state.snapshot_dirty = false;
            state.last_snapshot_publish = Instant::now();
        }

        // ── Persist config changes made outside the main loop ─────────────
        // The API's password-change endpoint mutates the shared config.
        // Write it to disk so the change survives a restart — otherwise
        // the generated password would come back on the next boot.
        if config_dirty.swap(false, Ordering::SeqCst) {
            let cfg = state.cfg.read().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = config::save(&cfg) {
                eprintln!("[annotator] could not save the config: {e}");
            }
            state.last_config_save = Instant::now();
        }

        // No manual frame-cap here.  `present_vsync()` above already paces the
        // loop to the display refresh rate.  Adding a sleep would throttle
        // capture consumption on high-refresh-rate displays and force the
        // capture pipeline to drop frames we already decoded.
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Human-readable label for the capture mode a device is running.
///
/// Returns an empty string for the "no input" entry or for a mode index
/// that does not exist, so the caller can fall back to the raw index.
fn mode_label(devices: &[capture::DeviceInfo], index: usize, mode: usize) -> String {
    if index == 0 {
        return String::new();
    }
    devices
        .get(index - 1)
        .and_then(|d| d.modes.get(mode))
        .map(|m| m.label())
        .unwrap_or_default()
}

/// Composite the annotation layer over the current video frame.
///
/// Returns `None` when there is no video — the caller then exports the
/// annotation layer on its own.
///
/// The video is placed where it appears on screen (letterboxed, centred)
/// so the exported image matches what the user was looking at.
fn export_video_frame(
    canvas: &Canvas,
    frame: Option<&Frame>,
    canvas_w: u32,
    canvas_h: u32,
) -> Option<Vec<u8>> {
    let frame = frame?;
    if frame.width == 0 || frame.height == 0 {
        return None;
    }

    // Decode the frame to RGB.  YUYV is expanded in place; MJPEG has
    // already been decoded by the capture thread.
    let rgb: Vec<u8> = match frame.format {
        capture::FrameFormat::Rgb24 => frame.data.as_ref().clone(),
        capture::FrameFormat::Yuyv => yuyv_to_rgb(&frame.data, frame.width, frame.height)?,
    };
    if rgb.len() < (frame.width * frame.height * 3) as usize {
        return None;
    }

    // Place the video where it sits on screen.
    let dst = scale_rect(canvas_w, canvas_h, frame.width, frame.height);
    let mut out = vec![0u8; (canvas_w * canvas_h * 3) as usize];

    // Scale the video into the destination rectangle (nearest neighbour
    // is fine for a case artifact and avoids a resampling dependency).
    for dy in 0..dst.height() {
        let sy = (dy as u64 * frame.height as u64 / dst.height().max(1) as u64) as usize;
        for dx in 0..dst.width() {
            let sx = (dx as u64 * frame.width as u64 / dst.width().max(1) as u64) as usize;
            let src = (sy * frame.width as usize + sx) * 3;
            if src + 2 >= rgb.len() {
                continue;
            }
            let px = (dst.x() + dx as i32).clamp(0, canvas_w as i32 - 1) as u32;
            let py = (dst.y() + dy as i32).clamp(0, canvas_h as i32 - 1) as u32;
            let di = (py * canvas_w + px) as usize * 3;
            if di + 2 < out.len() {
                out[di] = rgb[src];
                out[di + 1] = rgb[src + 1];
                out[di + 2] = rgb[src + 2];
            }
        }
    }

    // Overlay the annotations.
    Some(export::composite_over_video(&canvas.pixels, &out))
}

/// Expand a YUYV frame into RGB24.
fn yuyv_to_rgb(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let needed = (width as usize) * (height as usize) * 2;
    if data.len() < needed {
        return None;
    }
    let mut out = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for row in 0..height as usize {
        let base = row * width as usize * 2;
        for col in 0..width as usize {
            let i = base + col * 2;
            let y0 = data[i] as u32;
            let u = data[i + 1] as u32;
            let y1 = data[i + 2] as u32;
            let v = data[i + 3] as u32;

            // BT.601 studio-swing to full range.
            let c = y0.saturating_sub(16);
            let d = u.saturating_sub(128);
            let e = v.saturating_sub(128);
            let clamp = |x: i32| x.clamp(0, 255) as u8;
            out.push(clamp((298 * c as i32 + 409 * e as i32 + 128) >> 8));
            out.push(clamp(
                (298 * c as i32 - 100 * d as i32 - 208 * e as i32 + 128) >> 8,
            ));
            out.push(clamp((298 * c as i32 + 516 * d as i32 + 128) >> 8));

            let c1 = y1.saturating_sub(16);
            out.push(clamp((298 * c1 as i32 + 409 * e as i32 + 128) >> 8));
            out.push(clamp(
                (298 * c1 as i32 - 100 * d as i32 - 208 * e as i32 + 128) >> 8,
            ));
            out.push(clamp((298 * c1 as i32 + 516 * d as i32 + 128) >> 8));
        }
    }
    Some(out)
}

/// Render the active stroke (pen / highlight) directly via
/// SDL2 GPU draw calls on top of `committed_tex`, instead of clearing +
/// rasterizing an 8.3 MB CPU canvas and re-uploading it each frame.
///
/// `overlay_src` is the screen rect that the annotation overlay covers
/// (i.e. below the menu bar, above the toolbar); the active stroke is
/// clipped to this rect because that's where the user is drawing.
/// Refresh the live values shown in the settings panel.
///
/// The network addresses and the API state can change while the app is
/// running, so they are re-read each time the panel is opened rather
/// than being frozen at startup.
fn refresh_settings(state: &mut state::AppState) {
    let Some(s) = &mut state.settings else {
        return;
    };
    s.network = net::interface_addresses();
    let cfg_api = state.cfg.read().unwrap_or_else(|p| p.into_inner());
    s.api_addr = format!("{}:{}", cfg_api.api.bind, cfg_api.api.port);
    s.api_enabled = Toggle::from_bool(cfg_api.api.enabled);
}

/// Write the settings panel's values back to the config file so that
/// they survive a restart.
fn save_settings(state: &mut state::AppState) {
    let Some(s) = &state.settings else {
        return;
    };
    {
        let mut cfg = state.cfg.write().unwrap_or_else(|p| p.into_inner());
        cfg.show_time = s.show_time == Toggle::On;
        cfg.hide_toolbar = s.hide_toolbar == Toggle::On;
        cfg.laser_on = s.laser_on == Toggle::On;
        cfg.show_fps = s.show_fps == Toggle::On;
        cfg.undo_stack_limit = s.undo_limit;
        // The API toggle is applied by the main loop, which writes the
        // config itself; it is not saved here so the two paths cannot
        // disagree.
        if let Err(e) = config::save(&cfg) {
            eprintln!("[annotator] could not save settings: {e}");
        }
    }
}

/// Format the current local time as HH:MM for the top-bar clock.
///
/// The system clock is read through libc rather than a date-time crate:
/// the application needs one label once a second, and adding a
/// dependency for that would be more than the feature is worth.
fn format_clock(_since_start: Instant) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if now == -1 || unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn draw_active_stroke(
    sdl: &mut sdl2::render::Canvas<sdl2::video::Window>,
    active: &DrawObject,
    _overlay_src: Rect,
) {
    match active {
        DrawObject::Pen(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, 255));
            let half = (s.size as i32 / 2).max(1);
            if s.points.len() == 1 {
                draw_gpu_disc(sdl, s.points[0], half);
            } else {
                for w in s.points.windows(2) {
                    draw_gpu_thick_line(sdl, w[0], w[1], half);
                }
            }
        }
        DrawObject::Highlight(h) => {
            let (r, g, b) = h.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, 100));
            for &(x, y) in &h.points {
                draw_gpu_disc(sdl, (x, y), h.size as i32 * 2);
            }
        }
        _ => {
            // Shapes use the existing shape-preview path.
        }
    }
}

/// Draw a filled disc by walking every pixel in the bounding square
/// and discarding those outside the radius.  Uses SDL2's `draw_point`
/// which goes straight to the GPU.
fn draw_gpu_disc(
    sdl: &mut sdl2::render::Canvas<sdl2::video::Window>,
    (cx, cy): (i32, i32),
    r: i32,
) {
    let r2 = r * r;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r2 {
                sdl.draw_point(Point::new(cx + dx, cy + dy)).ok();
            }
        }
    }
}

/// Draw a thick line by stamping `2*half+1` parallel 1-pixel lines
/// perpendicular to the segment.  Cheap on RPi3 because SDL2's GLES2
/// backend handles each `draw_line` as a hardware-rendered primitive.
fn draw_gpu_thick_line(
    sdl: &mut sdl2::render::Canvas<sdl2::video::Window>,
    (x0, y0): (i32, i32),
    (x1, y1): (i32, i32),
    half: i32,
) {
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let len = (dx * dx + dy * dy).sqrt().max(0.001);
    let (px, py) = (-dy / len, dx / len);
    for k in -half..=half {
        let ox = (px * k as f32).round() as i32;
        let oy = (py * k as f32).round() as i32;
        sdl.draw_line(Point::new(x0 + ox, y0 + oy), Point::new(x1 + ox, y1 + oy))
            .ok();
    }
}

/// Render a shape-tool ghost preview directly via SDL2 draw calls.
/// This avoids clearing and re-uploading the 8MB CPU annotation canvas
/// every frame during a drag operation.
fn draw_preview_sdl(sdl: &mut sdl2::render::Canvas<sdl2::video::Window>, obj: &DrawObject) {
    const ALPHA: u8 = 210;
    match obj {
        DrawObject::Line(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).clamp(1, 40);
            let dx = (s.x1 - s.x0) as f32;
            let dy = (s.y1 - s.y0) as f32;
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            let (px, py) = (-dy / len, dx / len);
            for i in -(t / 2)..=(t / 2) {
                let ox = (px * i as f32).round() as i32;
                let oy = (py * i as f32).round() as i32;
                sdl.draw_line(
                    Point::new(s.x0 + ox, s.y0 + oy),
                    Point::new(s.x1 + ox, s.y1 + oy),
                )
                .ok();
            }
        }
        DrawObject::Rect(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).clamp(1, 20);
            let (x0, x1) = (s.x0.min(s.x1), s.x0.max(s.x1));
            let (y0, y1) = (s.y0.min(s.y1), s.y0.max(s.y1));
            for i in 0..t {
                let w = (x1 - x0 - 2 * i).max(0) as u32;
                let h = (y1 - y0 - 2 * i).max(0) as u32;
                if w == 0 || h == 0 {
                    break;
                }
                sdl.draw_rect(Rect::new(x0 + i, y0 + i, w, h)).ok();
            }
        }
        DrawObject::Circle(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).clamp(1, 20);
            let cx = (s.x0 + s.x1) / 2;
            let cy = (s.y0 + s.y1) / 2;
            let base_rx = ((s.x1 - s.x0).abs() / 2) as f32;
            let base_ry = ((s.y1 - s.y0).abs() / 2) as f32;
            let perimeter = 2.0 * std::f32::consts::PI * base_rx.max(base_ry);
            let steps = (perimeter as usize).clamp(16, 512);
            for dr in 0..t {
                PREVIEW_PTS.with(|cell| {
                    let mut pts = cell.borrow_mut();
                    pts.clear();
                    pts.reserve(steps + 1);
                    for i in 0..=steps {
                        let a = 2.0 * std::f32::consts::PI * i as f32 / steps as f32;
                        pts.push(Point::new(
                            cx + ((base_rx + dr as f32) * a.cos()).round() as i32,
                            cy + ((base_ry + dr as f32) * a.sin()).round() as i32,
                        ));
                    }
                    sdl.draw_lines(pts.as_slice()).ok();
                });
            }
        }
        DrawObject::Arrow(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).clamp(1, 40);
            let dx = (s.x1 - s.x0) as f32;
            let dy = (s.y1 - s.y0) as f32;
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            let (ux, uy) = (dx / len, dy / len);
            let (px, py) = (-uy, ux);
            // shaft
            for i in -(t / 2)..=(t / 2) {
                let ox = (px * i as f32).round() as i32;
                let oy = (py * i as f32).round() as i32;
                sdl.draw_line(
                    Point::new(s.x0 + ox, s.y0 + oy),
                    Point::new(s.x1 + ox, s.y1 + oy),
                )
                .ok();
            }
            // arrowhead outline
            let head_len = ((t * 4 + 20) as f32).min(60.0);
            let head_w = ((t * 2 + 10) as f32).min(30.0);
            let bx = s.x1 - (ux * head_len).round() as i32;
            let by = s.y1 - (uy * head_len).round() as i32;
            let left = Point::new(
                bx + (px * head_w).round() as i32,
                by + (py * head_w).round() as i32,
            );
            let right = Point::new(
                bx - (px * head_w).round() as i32,
                by - (py * head_w).round() as i32,
            );
            let tip = Point::new(s.x1, s.y1);
            sdl.draw_line(tip, left).ok();
            sdl.draw_line(tip, right).ok();
            sdl.draw_line(left, right).ok();
        }
        _ => {}
    }
}

fn scale_rect(canvas_w: u32, canvas_h: u32, frame_w: u32, frame_h: u32) -> Rect {
    let avail_h =
        canvas_h.saturating_sub(toolbar_height(canvas_w as i32) as u32 + MENU_HEIGHT as u32);
    let avail_w = canvas_w;
    // Scale frame to fit within available area, preserving aspect ratio
    let scale = (avail_w as f32 / frame_w as f32).min(avail_h as f32 / frame_h as f32);
    let dst_w = (frame_w as f32 * scale).round() as u32;
    let dst_h = (frame_h as f32 * scale).round() as u32;
    let x = ((avail_w - dst_w) / 2) as i32;
    let y = MENU_HEIGHT + ((avail_h - dst_h) / 2) as i32;
    Rect::new(x, y, dst_w, dst_h)
}

/// Rasterize text into a [`TextObj`] — a first-class scene object.
///
/// The glyphs are rendered once, here on the main thread where the SDL TTF
/// font is available, and stored as an RGBA bitmap.  The scene then
/// renders text like any other object, which keeps the render path free
/// of font dependencies and therefore still safe to run on the background
/// rasterizer thread.
///
/// Returns `None` for empty text or when the font cannot render it.
fn make_text_obj(
    text: &str,
    pos: (i32, i32),
    color: (u8, u8, u8),
    size: u32,
    font: &sdl2::ttf::Font,
) -> Option<scene::TextObj> {
    if text.is_empty() {
        return None;
    }
    let surface = font
        .render(text)
        .blended(sdl2::pixels::Color::RGB(color.0, color.1, color.2))
        .ok()?;

    let sw = surface.width();
    let sh = surface.height();
    if sw == 0 || sh == 0 {
        return None;
    }
    let pitch = surface.pitch() as usize;

    // Extract the alpha channel into a single-channel bitmap.  The scene
    // stores the glyph shape plus the text colour, so the RGB of the
    // rendered surface is redundant.
    let mut bitmap = vec![0u8; (sw as usize) * (sh as usize)];
    surface.with_lock(|pixels| {
        for sy in 0..sh as usize {
            for sx in 0..sw as usize {
                let base = sy * pitch + sx * 4;
                if base + 3 >= pixels.len() {
                    continue;
                }
                // SDL blended surface: ARGB8888 (B G R A in little-endian).
                bitmap[sy * sw as usize + sx] = pixels[base + 3];
            }
        }
    });

    Some(scene::TextObj {
        text: text.to_string(),
        x: pos.0,
        y: pos.1,
        color,
        size,
        bitmap,
        bitmap_width: sw,
        bitmap_height: sh,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Initialise SDL TTF once for the whole test binary.
    ///
    /// `TTF_Init` is not safe to call from several threads at once, and
    /// the test harness runs tests in parallel — calling it per test
    /// caused an intermittent SIGSEGV.  A `OnceLock` makes the
    /// initialisation happen exactly once, and the mutex serialises the
    /// font loading that follows.
    fn ttf_context() -> Option<&'static sdl2::ttf::Sdl2TtfContext> {
        static CONTEXT: std::sync::OnceLock<Option<sdl2::ttf::Sdl2TtfContext>> =
            std::sync::OnceLock::new();
        CONTEXT.get_or_init(|| sdl2::ttf::init().ok()).as_ref()
    }

    /// Serialises the font work — SDL TTF's font objects are not safe to
    /// create concurrently even once the library is initialised.
    static FONT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Rasterize a string with the real bundled font.
    ///
    /// SDL TTF needs no window for surface rendering, so this works
    /// headless.  If the font cannot be loaded the test is skipped rather
    /// than failed — a missing bundled font is an environment problem, not
    /// a logic error.
    fn rasterize(text: &str) -> Option<scene::TextObj> {
        let ttf = ttf_context()?;
        let _guard = FONT_LOCK.lock().ok()?;
        let rwops = sdl2::rwops::RWops::from_bytes(FONT_BYTES).ok()?;
        let font = ttf.load_font_from_rwops(rwops, 20).ok()?;
        make_text_obj(text, (10, 20), (255, 0, 0), 20, &font)
    }

    #[test]
    fn text_rasterizes_to_a_non_empty_bitmap() {
        let Some(obj) = rasterize("Hello") else {
            eprintln!("skipping: SDL TTF font unavailable in this environment");
            return;
        };
        assert_eq!(obj.text, "Hello");
        assert_eq!(obj.x, 10);
        assert_eq!(obj.y, 20);
        assert_eq!(obj.color, (255, 0, 0));
        assert_eq!(obj.size, 20);
        assert!(obj.bitmap_width > 0);
        assert!(obj.bitmap_height > 0);
        assert_eq!(
            obj.bitmap.len(),
            (obj.bitmap_width * obj.bitmap_height) as usize
        );
        // Some pixels must carry ink.
        assert!(obj.bitmap.iter().any(|&a| a > 0));
    }

    #[test]
    fn empty_text_produces_no_object() {
        assert!(rasterize("").is_none());
    }

    #[test]
    fn a_rasterized_text_object_renders_onto_a_canvas() {
        let Some(obj) = rasterize("Ag") else {
            eprintln!("skipping: SDL TTF font unavailable in this environment");
            return;
        };
        let mut canvas = Canvas::new(200, 100);
        scene::DrawObject::Text(obj.clone()).render(&mut canvas);
        // At least one pixel of ink reached the canvas.
        assert!(canvas.pixels.iter().any(|&p| p > 0));
        // The bounds match the bitmap size.
        let (bx, by, bw, bh) = scene::DrawObject::Text(obj.clone()).bounds();
        assert_eq!(bw as u32, obj.bitmap_width);
        assert_eq!(bh as u32, obj.bitmap_height);
        assert_eq!(bx, obj.x);
        assert_eq!(by, obj.y);
    }

    #[test]
    fn the_icon_font_renders_the_toolbar_glyphs() {
        // The toolbar draws Font Awesome icons from the Private Use Area.
        // If the bundled font lost those codepoints the toolbar would
        // render blank boxes — so check that each one resolves to a real
        // glyph with an outline.
        let Some(ttf) = ttf_context() else {
            eprintln!("skipping: SDL TTF unavailable in this environment");
            return;
        };
        let _guard = FONT_LOCK.lock().ok();
        let Ok(rwops) = sdl2::rwops::RWops::from_bytes(ICON_FONT_BYTES) else {
            eprintln!("skipping: the icon font could not be loaded");
            return;
        };
        let Ok(font) = ttf.load_font_from_rwops(rwops, 22) else {
            eprintln!("skipping: the icon font could not be loaded");
            return;
        };

        // Every codepoint the toolbar and the V2 rail use.
        let codepoints = [
            0xF04B, // freeze: pause
            0xF04C, // freeze: play
            0xF03D, // camera
            0xF030, // screenshot
            0xF040, // pen / highlight
            0xF068, // line
            0xF03C, // rectangle
            0xF111, // circle
            0xF061, // arrow
            0xF031, // text
            0xF0C9, // select
            0xF12A, // eraser
            0xF0FB, // laser
        ];
        for cp in codepoints {
            let ch = char::from_u32(cp).expect("valid codepoint");
            let surface = font
                .render(&ch.to_string())
                .blended(sdl2::pixels::Color::RGB(255, 255, 255))
                .expect("the codepoint must resolve to a glyph");
            assert!(
                surface.width() > 0 && surface.height() > 0,
                "U+{cp:04X} rendered as an empty box — the icon font is missing it"
            );
        }
    }

    #[test]
    fn a_rasterized_text_object_can_be_selected_and_moved() {
        let Some(obj) = rasterize("Test") else {
            eprintln!("skipping: SDL TTF font unavailable in this environment");
            return;
        };
        let mut sc = scene::Scene::new(8);
        sc.add(scene::DrawObject::Text(obj));
        let (bx, by, _, _) = sc.objects()[0].bounds();
        assert_eq!(sc.hit_test(bx, by), Some(0));
        sc.objects_mut()[0].translate(4, 6);
        assert_eq!(sc.objects()[0].bounds().0, bx + 4);
        assert_eq!(sc.objects()[0].bounds().1, by + 6);
    }
}
