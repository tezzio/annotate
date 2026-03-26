mod config;
mod capture;
mod canvas;
mod tools;
mod ui;
mod input;

use std::{
    collections::VecDeque,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use sdl2::{
    pixels::{Color, PixelFormatEnum},
    rect::Rect,
    render::BlendMode,
};

use canvas::Canvas;
use capture::Frame;
use tools::Tool;
use ui::{DevicePicker, ToolbarLayout, TOOLBAR_HEIGHT, MENU_HEIGHT};

// ── Application state ─────────────────────────────────────────────────────────

pub struct AppState {
    // Tool / drawing
    pub active_tool:         Tool,
    pub color:               (u8, u8, u8),
    pub brush_size:          u32,
    /// Per-tool remembered sizes; indexed by Tool::index()
    pub tool_sizes:          [u32; 8],

    // Mouse drag
    pub mouse_down:          bool,
    pub drag_start:          (i32, i32),
    pub drag_cur:            (i32, i32),
    pub preview_base:        Option<Vec<u8>>,

    // Undo / redo
    pub undo_stack:          VecDeque<Vec<u8>>,
    pub redo_stack:          VecDeque<Vec<u8>>,
    pub undo_limit:          usize,

    // Text input
    pub text_input_active:   bool,
    pub text_buffer:         String,
    pub text_pos:            (i32, i32),
    pub pending_text_commit: bool,

    // Device picker
    pub show_device_picker:  bool,
    pub device_picker:       Option<DevicePicker>,
    pub picker_confirmed:    Option<usize>,

    // Draw lock (set after undo/redo to absorb accidental mouse-down)
    pub draw_locked_until:   Option<Instant>,

    // Menu bar
    pub pending_menu_click:  Option<(i32, i32)>,
    pub menu_close_requested: bool,

    // Window
    pub toggle_fullscreen:   bool,
    pub is_fullscreen:       bool,
    pub window_size:         (u32, u32),

    // Config ref
    pub cfg: config::Config,
}

impl AppState {
    fn new(cfg: config::Config) -> Self {
        let color      = config::parse_color(&cfg.tool_color);
        let undo_limit = cfg.undo_stack_limit;
        let tool_size  = cfg.tool_size.max(1);
        let is_full    = cfg.fullscreen && !cfg.windowed;
        let tool_sizes: [u32; 8] = [8, tool_size, tool_size, tool_size, tool_size, tool_size, 40, 12];
        let brush_size = tool_sizes[Tool::Pen.index()];
        Self {
            active_tool:         Tool::Pen,
            color,
            brush_size,
            tool_sizes,
            mouse_down:          false,
            drag_start:          (0, 0),
            drag_cur:            (0, 0),
            preview_base:        None,
            undo_stack:          VecDeque::new(),
            redo_stack:          VecDeque::new(),
            undo_limit,
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
            draw_locked_until:   None,
            pending_menu_click:  None,
            menu_close_requested: false,
            cfg,
        }
    }
}

// ── Embedded fonts ────────────────────────────────────────────────────────────

const FONT_BYTES:      &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
const ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/fa-solid.otf");

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

    let window = win_builder.build().expect("create window");

    let mut sdl_canvas = window
        .into_canvas()
        .accelerated()
        .present_vsync()
        .build()
        .expect("create renderer");

    sdl_canvas.set_blend_mode(BlendMode::Blend);

    // Query actual dimensions — fullscreen_desktop gives native screen res;
    // windowed gives WM-assigned size (tiling WMs resize the window on map).
    let (mut canvas_w, mut canvas_h) = sdl_canvas.output_size().expect("output size");

    let texture_creator = sdl_canvas.texture_creator();

    // ── Annotation canvas ──────────────────────────────────────────────────
    let mut ann_canvas = Canvas::new(canvas_w, canvas_h);

    let mut overlay_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
        .expect("overlay texture");
    overlay_tex.set_blend_mode(BlendMode::Blend);

    ann_canvas.dirty = true;
    ann_canvas.upload_texture(&mut overlay_tex);

    // ── Video texture ──────────────────────────────────────────────────────
    let mut video_tex = texture_creator
        .create_texture_streaming(PixelFormatEnum::RGB24, canvas_w, canvas_h)
        .expect("video texture");

    // ── App state ──────────────────────────────────────────────────────────
    let mut state = AppState::new(cfg);
    state.window_size = (canvas_w, canvas_h);

    // ── Device enumeration ─────────────────────────────────────────────────
    let devices = capture::enumerate_devices();
    state.device_picker = Some(DevicePicker::new(devices.clone()));
    state.show_device_picker = true;

    // ── Menu bar ───────────────────────────────────────────────────────────
    let mut menu_bar = ui::MenuBar::new(devices);

    // ── Capture receiver (null / black screen until picker confirms) ───────
    let mut capture_rx: Receiver<Option<Frame>> = capture::null_receiver();

    // ── Toolbar layout ─────────────────────────────────────────────────────
    let mut toolbar_layout = ToolbarLayout::build(canvas_w as i32, canvas_h as i32);

    // ── Event loop ─────────────────────────────────────────────────────────
    let mut event_pump = sdl_context.event_pump().expect("event pump");
    let frame_duration = Duration::from_millis(1000 / 60);
    let mut last_frame = Instant::now();

    'main: loop {
        for event in event_pump.poll_iter() {
            if !input::process_event(&event, &mut state, &mut ann_canvas, &toolbar_layout) {
                break 'main;
            }
        }

        // ── Window resize (WM tiling, user drag, fullscreen toggle) ───────
        let (new_w, new_h) = state.window_size;
        if new_w != canvas_w || new_h != canvas_h {
            canvas_w = new_w;
            canvas_h = new_h;
            // Recreate pixel canvas and textures at the new size
            ann_canvas = Canvas::new(canvas_w, canvas_h);
            overlay_tex = texture_creator
                .create_texture_streaming(PixelFormatEnum::ABGR8888, canvas_w, canvas_h)
                .expect("overlay texture");
            overlay_tex.set_blend_mode(BlendMode::Blend);
            video_tex = texture_creator
                .create_texture_streaming(PixelFormatEnum::RGB24, canvas_w, canvas_h)
                .expect("video texture");
            ann_canvas.dirty = true;
            ann_canvas.upload_texture(&mut overlay_tex);
            toolbar_layout = ToolbarLayout::build(canvas_w as i32, canvas_h as i32);
        }

        // ── Menu bar clicks ────────────────────────────────────────────────
        if let Some((mx, my)) = state.pending_menu_click.take() {
            let action = menu_bar.click(mx, my, canvas_w as i32);
            match action {
                ui::MenuAction::Undo => input::undo(&mut state, &mut ann_canvas),
                ui::MenuAction::Redo => input::redo(&mut state, &mut ann_canvas),
                _ => {}
            }
            if let ui::MenuAction::SelectDevice(idx) = action {
                menu_bar.active_idx = idx;
                if idx == 0 {
                    capture_rx = capture::null_receiver();
                } else if let Some(picker) = &state.device_picker {
                    let dev = &picker.devices[idx - 1];
                    capture_rx = capture::start_capture(dev.path.clone(), canvas_w, canvas_h);
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
        if let Some(picked_idx) = state.picker_confirmed.take() {
            if picked_idx == 0 {
                capture_rx = capture::null_receiver();
            } else if let Some(picker) = &state.device_picker {
                let dev = &picker.devices[picked_idx - 1];
                capture_rx = capture::start_capture(dev.path.clone(), canvas_w, canvas_h);
            }
            menu_bar.active_idx = picked_idx;
        }

        // ── Latest video frame (non-blocking drain) ────────────────────────
        let mut last_frame_data: Option<Frame> = None;
        while let Ok(f) = capture_rx.try_recv() {
            last_frame_data = f;
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
                &mut ann_canvas,
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

        // Video layer
        if let Some(frame) = last_frame_data {
            if frame.width > 0 && frame.height > 0 {
                let pitch = (frame.width * 3) as usize;
                video_tex.update(None, &frame.data, pitch).ok();
                let dst = scale_rect(canvas_w, canvas_h);
                sdl_canvas.copy(&video_tex, None, dst).ok();
            }
        }

        // Annotation overlay (excludes toolbar strip and menu bar)
        ann_canvas.upload_texture(&mut overlay_tex);
        let overlay_src = Rect::new(0, MENU_HEIGHT, canvas_w, canvas_h.saturating_sub(TOOLBAR_HEIGHT as u32 + MENU_HEIGHT as u32));
        let overlay_dst = overlay_src;
        sdl_canvas.copy(&overlay_tex, overlay_src, overlay_dst).ok();

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
                &preview,
                Color::RGBA(r, g, b, 230),
                Rect::new(tx, ty, (canvas_w as i32 - tx).max(0) as u32, 32),
            );
        }

        // Toolbar
        ui::draw_toolbar(
            &mut sdl_canvas,
            &font,
            &icon_font,
            &toolbar_layout,
            state.active_tool,
            state.color,
            state.brush_size,
            canvas_h as i32,
            canvas_w as i32,
        );

        // Device picker on top
        if state.show_device_picker {
            if let Some(picker) = &state.device_picker {
                picker.draw(&mut sdl_canvas, &font, canvas_w as i32, canvas_h as i32);
            }
        }

        // Menu bar (always on top)
        menu_bar.draw(&mut sdl_canvas, &font, canvas_w as i32, !state.undo_stack.is_empty(), !state.redo_stack.is_empty());

        sdl_canvas.present();

        let elapsed = last_frame.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration - elapsed);
        }
        last_frame = Instant::now();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Stretch-fill video into the canvas area between menu bar and toolbar.
fn scale_rect(canvas_w: u32, canvas_h: u32) -> Rect {
    let avail_h = canvas_h.saturating_sub(TOOLBAR_HEIGHT as u32 + MENU_HEIGHT as u32);
    Rect::new(0, MENU_HEIGHT, canvas_w, avail_h)
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
