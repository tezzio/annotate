mod api;
mod config;
mod capture;
mod canvas;
mod render_pipeline;
mod tools;
mod scene;
mod state;
mod ui;
mod input;

use std::{
    sync::{
        atomic::Ordering,
        mpsc::Receiver,
        Arc,
    },
    time::Instant,
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
use ui::{DevicePicker, OverlaySettings, OverlayAction, TextCache, Toggle, toolbar_height, MENU_HEIGHT};

// ── Application state ─────────────────────────────────────────────────────────
// `AppState` lives in `state.rs` — one definition shared by the UI, the
// render loop, and the REST API (docs/crestron-rest-api-plan.md, Appendix B).

// ── Embedded fonts ────────────────────────────────────────────────────────────

const FONT_BYTES:      &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/DejaVuSans.ttf"));
const ICON_FONT_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fa-solid.otf"));

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cfg = config::load();

    // ── SDL2 init ──────────────────────────────────────────────────────────
    let sdl_context = sdl2::init().expect("SDL2 init");
    let video       = sdl_context.video().expect("SDL2 video");
    let ttf_context = sdl2::ttf::init().expect("SDL2 TTF init");

    // Load bundled font via RWops (no temp file needed)
    let rwops      = sdl2::rwops::RWops::from_bytes(FONT_BYTES).expect("font RWops");
    let font       = ttf_context.load_font_from_rwops(rwops, 16).expect("load font");
    let font_large = {
        let rw2 = sdl2::rwops::RWops::from_bytes(FONT_BYTES).expect("font RWops");
        ttf_context.load_font_from_rwops(rw2, 20).expect("load large font")
    };
    let icon_font = {
        let rw3 = sdl2::rwops::RWops::from_bytes(ICON_FONT_BYTES).expect("icon font RWops");
        ttf_context.load_font_from_rwops(rw3, 22).expect("load icon font")
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
    let win2 = video
        .window("Court Annotator", win_w, win_h)
        .position_centered()
        .build()
        .expect("fallback window");

    let window = win_builder.build().expect("create window");

    // Try hardware-accelerated rendering first; fall back to software if unavailable.
    // SDL2 picks the best available backend (OpenGL/GLES/Metal/Vulkan) automatically.
    // On VMs without GPU passthrough this returns Err — we use the second window
    // with a software renderer instead.
    let (mut sdl_canvas, using_hardware) = match window
        .into_canvas()
        .accelerated()
        .present_vsync()
        .build()
    {
        Ok(c) => (c, true),
        Err(reason) => {
            eprintln!("[annotator] hardware canvas failed ({reason}), falling back to software");
            let c = win2.into_canvas().software().build().expect("software renderer must succeed");
            (c, false)
        }
    };
    eprintln!(
        "[annotator] renderer: {}",
        if using_hardware { "hardware (accelerated)" } else { "software (fallback)" }
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
    // background rasterizer in `RenderPipeline` (TODO 9/10/11).
    //
    // The active stroke (TODO 5) no longer uses a separate CPU canvas —
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
    // into the back buffer.  The main thread reads `front_pixels()` and
    // uploads to `committed_tex`.
    let mut pipeline = RenderPipeline::new(canvas_w, canvas_h);
    let mut pipeline_seq: u64 = 0;

    // ── Video texture ──────────────────────────────────────────────────────
    let mut video_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::RGB24, canvas_w, canvas_h)
        .expect("video texture");
    let mut video_tex_w = canvas_w;
    let mut video_tex_h = canvas_h;
    let mut video_tex_fmt = PixelFormatEnum::RGB24;

    // ── App state ──────────────────────────────────────────────────────────
    let undo_limit = cfg.undo_stack_limit;
    let mut state = AppState::new(cfg);
    state.window_size = (canvas_w, canvas_h);

    // ── Settings overlay ────────────────────────────────────────────────────
    let renderer_info = if using_hardware { "hardware (accelerated)".into() } else { "software (fallback)".into() };
    state.settings = Some(OverlaySettings {
        cursor: 0,
        laser_fade: 1.2,
        laser_on: Toggle::On,
        show_fps: Toggle::On,
        fullscreen: Toggle::from_bool(state.is_fullscreen),
        undo_limit,
        renderer_info,
        scroll_offset: 0,
    });
    state.show_settings_overlay = false;
    state.settings_action = None;

    // ── Device enumeration ─────────────────────────────────────────────────
    let devices = capture::enumerate_devices();
    state.device_picker = Some(DevicePicker::new(devices.clone()));
    state.show_device_picker = false;  // Start with black screen, no picker shown

    // ── Menu bar ───────────────────────────────────────────────────────────
    let mut menu_bar = ui::MenuBar::new(devices);

    // ── REST API (Crestron) ────────────────────────────────────────────────
    // The main loop publishes a fresh snapshot at the bottom of every frame;
    // API threads only ever read it, so the render loop never blocks on HTTP.
    let api_snapshot: api::SharedSnapshot =
        std::sync::Arc::new(std::sync::RwLock::new(state.to_snapshot(
            menu_bar.active_idx,
            &menu_bar.devices,
            0.0,
            0.0,
        )));
    // The main loop drains this once per frame; HTTP threads block on the
    // per-command oneshot for at most 500 ms.
    let api_cmd_rx: api::ApiCommandRx = if state.cfg.api.enabled {
        api::serve(&state.cfg.api.bind, state.cfg.api.port, api_snapshot.clone(), state.cfg.clone())
    } else {
        eprintln!("[api] disabled by config ([api] enabled = false)");
        let (_tx, rx) = std::sync::mpsc::sync_channel(0);
        rx
    };

    // ── Capture receiver (null / black screen until picker confirms) ───────
    let mut capture_rx: Receiver<Option<Frame>> = capture::null_receiver();
    let mut capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut capture_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // ── UI: V2 side-rail layout ──────────────────────────────────────────────
    let mut v2_layout = ui::v2::V2Layout::build(canvas_w as i32, canvas_h as i32);

    // ── Event loop ─────────────────────────────────────────────────────────
    let mut event_pump = sdl_context.event_pump().expect("event pump");
    // The render loop is paced by `present_vsync()` below — no manual sleep
    // here.  Capping the loop to a fixed 60 Hz would throttle capture
    // consumption on 120/144 Hz displays and force the `sync_channel(1)`
    // sender to drop frames we already paid the decode cost for.
    let mut last_video_frame: Option<Frame> = None;
    let debug = std::env::var("ANNOTATOR_DEBUG").is_ok();
    let mut dbg_render_count  = 0u64;
    let mut dbg_upload_count  = 0u64;
    let mut dbg_stat_timer    = Instant::now();
    // Live FPS counters shown in the menu bar
    let mut display_fps: f32   = 0.0;
    let mut fps_frame_count    = 0u32;
    let mut fps_timer          = Instant::now();
    let mut text_cache         = TextCache::new();

    'main: loop {
        let clipboard = video.clipboard();
        for event in event_pump.poll_iter() {
            if !input::process_event(&event, &mut state, &mut committed_canvas, &mut v2_layout, Some(&clipboard)) {
                break 'main;
            }

            // ── Settings overlay keyboard / mouse events ──────────────────────
            if state.show_settings_overlay {
                match &event {
                    sdl2::event::Event::KeyDown { keycode: Some(k), .. } => {
                        match *k {
                            sdl2::keyboard::Keycode::Escape => {
                                state.show_settings_overlay = false;
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
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    sdl2::event::Event::MouseButtonDown { x, y, .. } => {
                        if let Some(s) = &mut state.settings {
                            let canvas_w_i = canvas_w as i32;
                            let canvas_h_i = canvas_h as i32;
                            let action = s.click(*x as i32, *y as i32, canvas_w_i, canvas_h_i);
                            match action {
                                OverlayAction::Close => { state.show_settings_overlay = false; }
                                OverlayAction::ToggleFullscreen => {
                                    state.toggle_fullscreen = true;
                                    state.show_settings_overlay = false;
                                }
                                OverlayAction::None => {}
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                // F3 opens the settings overlay
                if let sdl2::event::Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F3), .. } = &event {
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
            video_tex = texture_creator
                .create_texture_streaming(video_tex_fmt, canvas_w, canvas_h)
                .expect("video texture");
            video_tex_w = canvas_w;
            video_tex_h = canvas_h;
            committed_canvas.dirty = true;
            pipeline.render_into(&committed_canvas, &state.scene);
            pipeline_seq = 0;  // reset so the next upload is detected
            v2_layout = ui::v2::V2Layout::build(canvas_w as i32, canvas_h as i32);
        }

        // ── Menu bar clicks ────────────────────────────────────────────────
        if let Some((mx, my)) = state.pending_menu_click.take() {
            let action = menu_bar.click(mx, my, canvas_w as i32);
            match action {
                ui::MenuAction::Undo  => { state.scene.undo(); }
                ui::MenuAction::Redo  => { state.scene.redo(); }
                ui::MenuAction::Clear => { state.scene.clear(); }
                ui::MenuAction::OpenSettings => { state.show_settings_overlay = true; }
                _ => {}
            }
            if let ui::MenuAction::SelectDevice(idx) = action {
                menu_bar.active_idx = idx;
                state.input_mode = 0;  // menu quick-switch always uses best mode
                if idx == 0 {
                    capture_stop.store(true, Ordering::Relaxed);
                    capture_rx = capture::null_receiver();
                    capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                    last_video_frame = None;
                } else if let Some(picker) = &state.device_picker {
                    capture_stop.store(true, Ordering::Relaxed);
                    let dev = &picker.devices[idx - 1];
                    // Use first (best) mode for menu bar quick-switch
                    let mode = dev.modes.first().cloned();
                    let (rx, fps, stop) = capture::start_capture(dev.path.clone(), mode);
                    capture_rx = rx;
                    capture_fps_counter = fps;
                    capture_stop = stop;
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
            } else if dev_idx == usize::MAX {
                // RTSP stream selected - get config from picker
                if let Some(ref picker) = state.device_picker {
                    if let Some(rtsp) = picker.rtsp_config() {
                        // Save RTSP URL for persistence.  Credentials live in
                        // the URL itself (e.g. rtsp://user:pass@host/path),
                        // which is what ffmpeg accepts directly.
                        let url = rtsp.url.trim().to_string();
                        if !url.is_empty() {
                            state.cfg.rtsp.url = Some(url.clone());
                            state.cfg.rtsp.enabled = true;
                            eprintln!("[annotator] starting RTSP stream: {}", url);
                            let (rx, fps, stop) =
                                capture::start_rtsp_capture(&url, state.cfg.rtsp.width, state.cfg.rtsp.height);
                            capture_rx = rx;
                            capture_fps_counter = fps;
                            capture_stop = stop;
                        }
                    }
                }
            } else if let Some(picker) = &state.device_picker {
                capture_stop.store(true, Ordering::Relaxed);
                let dev = &picker.devices[dev_idx - 1];
                let mode = dev.modes.get(mode_idx).cloned();
                let (rx, fps, stop) = capture::start_capture(dev.path.clone(), mode);
                capture_rx = rx;
                capture_fps_counter = fps;
                capture_stop = stop;
            }
            state.input_mode = if dev_idx == 0 { 0 } else { mode_idx };
            menu_bar.active_idx = dev_idx;
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
                    state::ApiResult::Ok(serde_json::json!({
                        "color": format!("{r:02x}{g:02x}{b:02x}"),
                    }))
                }
                state::ApiCommandKind::SetBrushSize(px) => {
                    state.brush_size = state::clamp_brush_size(px);
                    state.tool_sizes[state.active_tool.index()] = state.brush_size;
                    state::ApiResult::Ok(serde_json::json!({
                        "brush_size": state.brush_size,
                    }))
                }
                state::ApiCommandKind::SetBrushSizeDelta(d) => {
                    let cur = state.brush_size as i64;
                    state.brush_size =
                        state::clamp_brush_size((cur + d).max(0) as u32);
                    state.tool_sizes[state.active_tool.index()] = state.brush_size;
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
                            let (rx, fps, stop) = capture::start_capture(
                                dev.path.clone(),
                                dev.modes.get(mode).cloned(),
                            );
                            capture_rx = rx;
                            capture_fps_counter = fps;
                            capture_stop = stop;
                        }
                        menu_bar.active_idx = index;
                        state.input_mode = mode;
                        state::ApiResult::Ok(serde_json::json!({
                            "input": index,
                            "input_mode": mode,
                        }))
                    }
                }
            };
            let _ = cmd.reply.send(result);
        }

        // ── Latest video frame (non-blocking drain) ────────────────────────
        let mut new_video_frame = false;
        if !state.freeze_frame {
            while let Ok(f) = capture_rx.try_recv() {
                if let Some(frame) = f { last_video_frame = Some(frame); new_video_frame = true; }
            }
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

        // ── Text commit onto canvas ────────────────────────────────────────
        if state.pending_text_commit {
            state.pending_text_commit = false;
            blit_text_to_canvas(
                &mut committed_canvas,
                &state.text_buffer,
                state.text_pos,
                state.color,
                &font_large,
            );
            state.text_buffer.clear();
        }

        // ── Render ────────────────────────────────────────────────────────
        sdl_canvas.set_draw_color(Color::RGB(0, 0, 0));
        sdl_canvas.clear();

        // Video layer — upload to GPU only when a new frame arrived; always blit existing
        if let Some(frame) = &last_video_frame {
            if frame.width > 0 && frame.height > 0 {
                if new_video_frame {
                    dbg_upload_count += 1;
                    let desired_fmt = match frame.format {
                        FrameFormat::Yuyv   => PixelFormatEnum::YUY2,
                        FrameFormat::Yuv420 => PixelFormatEnum::IYUV,
                        FrameFormat::Rgb24  => PixelFormatEnum::RGB24,
                    };
                    // Recreate texture if frame dimensions or format changed
                    if frame.width != video_tex_w || frame.height != video_tex_h || desired_fmt != video_tex_fmt {
                        if let Ok(t) = texture_creator
                            .create_texture_streaming(desired_fmt, frame.width, frame.height)
                        {
                            video_tex     = t;
                            video_tex_w   = frame.width;
                            video_tex_h   = frame.height;
                            video_tex_fmt = desired_fmt;
                        }
                    }
                    // YUV420 is planar: full Y plane, then U, then V (each half-res).
                    // SDL2's IYUV texture expects width*height*3/2 bytes in a single
                    // contiguous buffer with this exact layout — which is what ffmpeg
                    // produces when `-pix_fmt yuv420p` is set.
                    let pitch = match frame.format {
                        FrameFormat::Yuyv   => (frame.width * 2) as usize,
                        FrameFormat::Yuv420 => (frame.width as usize) * (frame.height as usize) * 3 / 2,
                        FrameFormat::Rgb24  => (frame.width * 3) as usize,
                    };
                    video_tex.update(None, &frame.data, pitch).ok();
                }
                dbg_render_count += 1;
                let dst = scale_rect(canvas_w, canvas_h, frame.width, frame.height);
                sdl_canvas.copy(&video_tex, None, dst).ok();
            }
        }

        if debug && dbg_stat_timer.elapsed().as_secs() >= 2 {
            eprintln!(
                "render: blit_count={} tex_uploads={} (~{:.1} fps)",
                dbg_render_count,
                dbg_upload_count,
                dbg_render_count as f64 / dbg_stat_timer.elapsed().as_secs_f64().max(0.001)
            );
            dbg_render_count = 0;
            dbg_upload_count = 0;
            dbg_stat_timer   = Instant::now();
        }

        // Update display fps counter every second
        fps_frame_count += 1;
        let fps_elapsed = fps_timer.elapsed().as_secs_f32();
        if fps_elapsed >= 1.0 {
            display_fps  = fps_frame_count as f32 / fps_elapsed;
            fps_frame_count = 0;
            fps_timer    = Instant::now();
        }

        // ── Annotation overlay ─────────────────────────────────────────
        // Two layers:
        //   1. Committed scene — rasterized off-thread by `pipeline` (TODO
        //      9 + 11).  When the scene becomes dirty OR the input has
        //      written to `committed_canvas` (eraser/text-blit), we submit
        //      a render job; the worker (TODO 11) rasterizes in the
        //      background; the main thread (TODO 10) re-uploads the front
        //      buffer to the GPU texture when the worker signals completion.
        //   2. Active stroke — drawn directly via SDL2 GPU primitives on
        //      top of `committed_tex` (TODO 5).  No CPU canvas, no 8.3 MB
        //      upload per frame.
        let overlay_src = Rect::new(0, MENU_HEIGHT, canvas_w, canvas_h.saturating_sub(toolbar_height(canvas_w as i32) as u32 + MENU_HEIGHT as u32));

        // Submit a render job whenever scene or input is dirty.
        if state.scene.dirty || committed_canvas.dirty {
            // Copy the input canvas into the pipeline's back buffer so the
            // worker can use it as the base layer, then submit the scene.
            // The worker reads front_pixels (== previous render result)
            // and writes the new render into back_pixels.
            pipeline.submit_scene(&committed_canvas, &state.scene);
            committed_canvas.dirty = false;
            state.scene.dirty = false;
        }

        // Drain any completed worker frames into the GPU texture.
        if pipeline.poll(&mut pipeline_seq) {
            pipeline.upload_to(&mut committed_tex);
        }

        sdl_canvas.copy(&committed_tex, overlay_src, overlay_src).ok();

        // Layer 2: active stroke — direct GPU primitives (TODO 5).
        if let Some(ref active) = state.active_stroke {
            draw_active_stroke(&mut sdl_canvas, active, overlay_src);
        }

        // Shape tool preview — draw directly via SDL2 GPU calls (no CPU canvas work)
        if let Some(prev_obj) = input::preview_object(&state) {
            draw_preview_sdl(&mut sdl_canvas, &prev_obj);
        }
        // Selection bounding box — drawn on top of everything when the Select
        // tool has a hit.  Figma/Canva style: a thin accent-colored rectangle
        // with a 2-pixel margin around the object's bounds.
        if state.active_tool == Tool::Select {
            if let Some(idx) = state.scene.selected {
                if let Some(obj) = state.scene.objects.get(idx) {
                    let (bx, by, bw, bh) = obj.bounds();
                    let pad = 4i32;
                    let x = bx - pad;
                    let y = by - pad;
                    let w = (bw + pad * 2).max(1) as u32;
                    let h = (bh + pad * 2).max(1) as u32;
                    let r = Rect::new(x, y, w, h);
                    // 2px border accent — draw two concentric rects for a
                    // subtle inset effect
                    sdl_canvas.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                    sdl_canvas.draw_rect(r).ok();
                    let r2 = Rect::new(x + 2, y + 2, w.saturating_sub(4), h.saturating_sub(4));
                    sdl_canvas.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                    sdl_canvas.draw_rect(r2).ok();
                }
            }
        }
        // Laser pointer trail — Tron-style solid fading line, never committed to scene
        {
            let laser_fade = state.settings.as_ref().map(|s| s.laser_fade).unwrap_or(1.2);
            let now = Instant::now();
            state.laser_trail.retain(|(_, _, t, _)| now.duration_since(*t).as_secs_f32() < laser_fade);
            let tlen = state.laser_trail.len();
            let size = (state.tool_sizes[Tool::Laser.index()] as i32).max(1).min(20);
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
                    sdl_canvas.draw_line(
                        Point::new(x0 + ox, y0 + oy),
                        Point::new(x1 + ox, y1 + oy),
                    ).ok();
                }
            }
            // Bright head dot at the current cursor position
            if let Some((x, y, _, (r, g, b))) = state.laser_trail.last() {
                let core = half + 3;
                sdl_canvas.set_draw_color(Color::RGBA(*r, *g, *b, 255));
                for dy in -core..=core {
                    let xw = (((core * core - dy * dy) as f32).sqrt()) as i32;
                    sdl_canvas.draw_line(
                        Point::new(x - xw, y + dy),
                        Point::new(x + xw, y + dy),
                    ).ok();
                }
            }
        }

        // Eraser preview — outline of the eraser disc at the most recent few
        // drag points so the user can see what they're about to wipe.  Drawn
        // via SDL2 directly (never composited into the canvas) so it's purely
        // a UI hint and disappears on mouse-up.
        if let Some(ref drag) = state.eraser_drag {
            let r = (drag.radius as i32).max(1).min(80);
            sdl_canvas.set_draw_color(Color::RGBA(220, 220, 220, 220));
            for &(cx, cy) in drag.points.iter().rev().take(6) {
                let steps = 36;
                let pts: Vec<Point> = (0..=steps).map(|i| {
                    let a = 2.0 * std::f32::consts::PI * i as f32 / steps as f32;
                    Point::new(
                        cx + (r as f32 * a.cos()).round() as i32,
                        cy + (r as f32 * a.sin()).round() as i32,
                    )
                }).collect();
                sdl_canvas.draw_lines(pts.as_slice()).ok();
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

        // Tool rail (V2)
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

        // Device picker on top
        if state.show_device_picker {
            if let Some(picker) = &state.device_picker {
                picker.draw(&mut sdl_canvas, &font, canvas_w as i32, canvas_h as i32, &mut text_cache);
            }
        }

        // Settings overlay — modal panel drawn above everything else
        if state.show_settings_overlay {
            if let Some(ref mut settings) = state.settings {
                settings.draw(&mut sdl_canvas, &font, canvas_w as i32, canvas_h as i32, &mut text_cache);
            }
        }

        // Menu bar (always on top)
        let capture_fps = capture_fps_counter.load(Ordering::Relaxed) as f32 / 10.0;
        let show_fps = state.settings.as_ref().map(|s| s.show_fps == Toggle::On).unwrap_or(true);
        menu_bar.draw(&mut sdl_canvas, &font, canvas_w as i32, !state.scene.undo_stack.is_empty(), !state.scene.redo_stack.is_empty(), show_fps, capture_fps, display_fps, &mut text_cache);

        sdl_canvas.present();

        // ── Publish API snapshot (HTTP threads only ever read it) ─────────
        if let Ok(mut snap) = api_snapshot.write() {
            *snap = state.to_snapshot(menu_bar.active_idx, &menu_bar.devices, display_fps, capture_fps);
        }

        // No manual frame-cap here.  `present_vsync()` above already paces the
        // loop to the display refresh rate.  Adding a sleep would throttle
        // capture consumption on high-refresh-rate displays and force the
        // capture pipeline to drop frames we already decoded.
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// TODO 5 — render the active stroke (pen / highlight) directly via
/// SDL2 GPU draw calls on top of `committed_tex`, instead of clearing +
/// rasterizing an 8.3 MB CPU canvas and re-uploading it each frame.
///
/// `overlay_src` is the screen rect that the annotation overlay covers
/// (i.e. below the menu bar, above the toolbar); the active stroke is
/// clipped to this rect because that's where the user is drawing.
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
    let r2 = (r * r) as i32;
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
        sdl.draw_line(
            Point::new(x0 + ox, y0 + oy),
            Point::new(x1 + ox, y1 + oy),
        ).ok();
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
            let t = (s.size as i32).max(1).min(40);
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
                ).ok();
            }
        }
        DrawObject::Rect(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).max(1).min(20);
            let (x0, x1) = (s.x0.min(s.x1), s.x0.max(s.x1));
            let (y0, y1) = (s.y0.min(s.y1), s.y0.max(s.y1));
            for i in 0..t {
                let w = (x1 - x0 - 2 * i).max(0) as u32;
                let h = (y1 - y0 - 2 * i).max(0) as u32;
                if w == 0 || h == 0 { break; }
                sdl.draw_rect(Rect::new(x0 + i, y0 + i, w, h)).ok();
            }
        }
        DrawObject::Circle(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).max(1).min(20);
            let cx = (s.x0 + s.x1) / 2;
            let cy = (s.y0 + s.y1) / 2;
            let base_rx = ((s.x1 - s.x0).abs() / 2) as f32;
            let base_ry = ((s.y1 - s.y0).abs() / 2) as f32;
            let perimeter = 2.0 * std::f32::consts::PI * base_rx.max(base_ry);
            let steps = (perimeter as usize).max(16).min(512);
            for dr in 0..t {
                let pts: Vec<Point> = (0..=steps).map(|i| {
                    let a = 2.0 * std::f32::consts::PI * i as f32 / steps as f32;
                    Point::new(
                        cx + ((base_rx + dr as f32) * a.cos()).round() as i32,
                        cy + ((base_ry + dr as f32) * a.sin()).round() as i32,
                    )
                }).collect();
                sdl.draw_lines(pts.as_slice()).ok();
            }
        }
        DrawObject::Arrow(s) => {
            let (r, g, b) = s.color;
            sdl.set_draw_color(Color::RGBA(r, g, b, ALPHA));
            let t = (s.size as i32).max(1).min(40);
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
                ).ok();
            }
            // arrowhead outline
            let head_len = ((t * 4 + 20) as f32).min(60.0);
            let head_w   = ((t * 2 + 10) as f32).min(30.0);
            let bx = s.x1 - (ux * head_len).round() as i32;
            let by = s.y1 - (uy * head_len).round() as i32;
            let left  = Point::new(bx + (px * head_w).round() as i32, by + (py * head_w).round() as i32);
            let right = Point::new(bx - (px * head_w).round() as i32, by - (py * head_w).round() as i32);
            let tip   = Point::new(s.x1, s.y1);
            sdl.draw_line(tip, left).ok();
            sdl.draw_line(tip, right).ok();
            sdl.draw_line(left, right).ok();
        }
        _ => {}
    }
}

fn scale_rect(canvas_w: u32, canvas_h: u32, frame_w: u32, frame_h: u32) -> Rect {
    let avail_h = canvas_h.saturating_sub(toolbar_height(canvas_w as i32) as u32 + MENU_HEIGHT as u32);
    let avail_w = canvas_w;
    // Scale frame to fit within available area, preserving aspect ratio
    let scale = (avail_w as f32 / frame_w as f32).min(avail_h as f32 / frame_h as f32);
    let dst_w = (frame_w as f32 * scale).round() as u32;
    let dst_h = (frame_h as f32 * scale).round() as u32;
    let x = ((avail_w - dst_w) / 2) as i32;
    let y = MENU_HEIGHT + ((avail_h - dst_h) / 2) as i32;
    Rect::new(x, y, dst_w, dst_h)
}

/// Blit SDL_ttf-rendered text into the RGBA annotation canvas buffer
fn blit_text_to_canvas(
    canvas: &mut Canvas,
    text: &str,
    pos: (i32, i32),
    color: (u8, u8, u8),
    font: &sdl2::ttf::Font,
) {
    if text.is_empty() { return; }

    let surface = match font
        .render(text)
        .blended(sdl2::pixels::Color::RGB(color.0, color.1, color.2))
    {
        Ok(s) => s,
        Err(_) => return,
    };

    let sw    = surface.width()  as i32;
    let sh    = surface.height() as i32;
    let pitch = surface.pitch()  as i32;

    surface.with_lock(|pixels| {
        for sy in 0..sh {
            for sx in 0..sw {
                let dx = pos.0 + sx;
                let dy = pos.1 + sy;
                if dx < 0 || dy < 0 { continue; }
                // SDL blended surface: ARGB8888 layout (B G R A in little-endian)
                let base = (sy * pitch + sx * 4) as usize;
                if base + 3 >= pixels.len() { continue; }
                let b = pixels[base];
                let g = pixels[base + 1];
                let r = pixels[base + 2];
                let a = pixels[base + 3];
                if a > 0 {
                    canvas.put_pixel(dx, dy, r, g, b, a);
                }
            }
        }
    });
}
