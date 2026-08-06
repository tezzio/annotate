mod config;
mod capture;
mod canvas;
mod tools;
mod scene;
mod ui;
mod input;

use std::{
    sync::{
        atomic::Ordering,
        mpsc::Receiver,
        Arc,
    },
    time::{Duration, Instant},
};

use sdl2::{
    pixels::{Color, PixelFormatEnum},
    rect::{Point, Rect},
    render::BlendMode,
};

use canvas::Canvas;
use capture::{Frame, FrameFormat};
use scene::{DrawObject, Scene};
use tools::Tool;
use ui::{DevicePicker, OverlaySettings, OverlayAction, TextCache, Toggle, toolbar_height, MENU_HEIGHT};

// ── Application state ─────────────────────────────────────────────────────────

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

    /// Active stroke being drawn (pen/eraser/highlight).  Lives outside the
    /// scene while in progress so the scene's committed texture isn't
    /// re-rasterized every frame as points are added.  On mouse-up, the stroke
    /// is appended to `scene.objects` and this is cleared.
    pub active_stroke:       Option<DrawObject>,

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
    fn new(cfg: config::Config) -> Self {
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
            laser_trail:         Vec::new(),
            pending_menu_click:  None,
            menu_close_requested: false,
            cfg,
            show_settings_overlay: false,
            settings:             None,
            settings_action:     None,
        }
    }
}

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

    // ── Annotation overlay (two layers) ──────────────────────────────────
    // `committed_*` holds the finalized scene (rarely re-rasterized).
    // `active_*` holds the stroke currently being drawn (re-rasterized each
    //  frame while mouse is down).  Each frame, committed is blitted first,
    //  then active is blitted on top.
    let mut committed_canvas = Canvas::new(canvas_w, canvas_h);
    let mut committed_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
        .expect("committed overlay texture");
    committed_tex.set_blend_mode(BlendMode::Blend);

    let mut active_canvas = Canvas::new(canvas_w, canvas_h);
    let mut active_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
        .expect("active overlay texture");
    active_tex.set_blend_mode(BlendMode::Blend);

    committed_canvas.dirty = true;
    committed_canvas.upload_texture(&mut committed_tex);

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
    state.show_device_picker = true;

    // ── Menu bar ───────────────────────────────────────────────────────────
    let mut menu_bar = ui::MenuBar::new(devices);

    // ── Capture receiver (null / black screen until picker confirms) ───────
    let mut capture_rx: Receiver<Option<Frame>> = capture::null_receiver();
    let mut capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut capture_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // ── UI: V2 side-rail layout ──────────────────────────────────────────────
    let mut v2_layout = ui::v2::V2Layout::build(canvas_w as i32, canvas_h as i32);

    // ── Event loop ─────────────────────────────────────────────────────────
    let mut event_pump = sdl_context.event_pump().expect("event pump");
    let frame_duration = Duration::from_millis(1000 / 60);
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
        let frame_start = Instant::now();

        for event in event_pump.poll_iter() {
            if !input::process_event(&event, &mut state, &mut committed_canvas, &mut v2_layout) {
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
            active_canvas = Canvas::new(canvas_w, canvas_h);
            active_tex = texture_creator
                .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
                .expect("active overlay texture");
            active_tex.set_blend_mode(BlendMode::Blend);
            video_tex = texture_creator
                .create_texture_streaming(video_tex_fmt, canvas_w, canvas_h)
                .expect("video texture");
            video_tex_w = canvas_w;
            video_tex_h = canvas_h;
            committed_canvas.dirty = true;
            committed_canvas.upload_texture(&mut committed_tex);
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
                capture_stop.store(true, Ordering::Relaxed);
                capture_rx = capture::null_receiver();
                capture_fps_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                last_video_frame = None;
            } else if let Some(picker) = &state.device_picker {
                capture_stop.store(true, Ordering::Relaxed);
                let dev = &picker.devices[dev_idx - 1];
                let mode = dev.modes.get(mode_idx).cloned();
                let (rx, fps, stop) = capture::start_capture(dev.path.clone(), mode);
                capture_rx = rx;
                capture_fps_counter = fps;
                capture_stop = stop;
            }
            menu_bar.active_idx = dev_idx;
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
                    let desired_fmt = if frame.format == FrameFormat::Yuyv {
                        PixelFormatEnum::YUY2
                    } else {
                        PixelFormatEnum::RGB24
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
                    let pitch = if frame.format == FrameFormat::Yuyv {
                        (frame.width * 2) as usize  // 2 bytes per pixel for YUYV
                    } else {
                        (frame.width * 3) as usize  // 3 bytes per pixel for RGB24
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

        // ── Annotation overlay (two layers) ──────────────────────────────
        // Layer 1: committed scene (re-rasterized only when scene.dirty)
        let was_scene_dirty = state.scene.dirty;
        let was_canvas_dirty = committed_canvas.dirty;
        state.scene.render_to(&mut committed_canvas);
        committed_canvas.upload_texture(&mut committed_tex);
        if debug && (was_scene_dirty || was_canvas_dirty) {
            eprintln!("[debug] render_to: scene_dirty={} canvas_dirty={} objects={}", was_scene_dirty, was_canvas_dirty, state.scene.objects.len());
        }
        let overlay_src = Rect::new(0, MENU_HEIGHT, canvas_w, canvas_h.saturating_sub(toolbar_height(canvas_w as i32) as u32 + MENU_HEIGHT as u32));
        sdl_canvas.copy(&committed_tex, overlay_src, overlay_src).ok();

        // Layer 2: active stroke (re-rasterized every frame it's being drawn).
        // Only the active stroke is rasterized — no full canvas clear.
        if let Some(ref active) = state.active_stroke {
            active_canvas.clear();
            active.render(&mut active_canvas);
            active_canvas.dirty = true;
            active_canvas.upload_texture(&mut active_tex);
            sdl_canvas.copy(&active_tex, overlay_src, overlay_src).ok();
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

        // Fallback frame cap — present_vsync already handles timing; this only
        // activates when vsync is unavailable (e.g., compositor off or headless).
        let used = frame_start.elapsed();
        if used < frame_duration {
            std::thread::sleep(frame_duration - used);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
