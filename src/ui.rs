use sdl2::{
    pixels::Color,
    rect::Rect,
    render::Canvas as SdlCanvas,
    ttf::Font,
    video::Window,
};

use crate::{
    capture::DeviceInfo,
    tools::{Tool, PALETTE},
};

// ── Layout constants ──────────────────────────────────────────────────────────

pub const TOOLBAR_HEIGHT: i32 = 100;
pub const MENU_HEIGHT:    i32 = 52;
const TOOL_BTN_SIZE:  i32 = 76;
const TOOL_BTN_PAD:   i32 = 8;
const PALETTE_SWATCH: i32 = 44;
const PALETTE_PAD:    i32 = 6;

// ── Text surface cache ──────────────────────────────────────────────────────

/// Pre-rasterized surface cache. Avoids calling SDL2_TTF on every render frame
/// for text that hasn't changed (tool icons, labels, static UI strings).
pub struct TextCache {
    map: std::collections::HashMap<u64, sdl2::surface::Surface<'static>>,
}

impl TextCache {
    pub fn new() -> Self { Self { map: std::collections::HashMap::new() } }

    fn key(font: &Font, text: &str, color: Color) -> u64 {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        (font as *const Font as usize).hash(&mut h);
        text.hash(&mut h);
        [color.r, color.g, color.b, color.a].hash(&mut h);
        h.finish()
    }

    /// Return a reference to a pre-rasterized surface, rendering it on first
    /// access.  The returned reference is valid until the cache is mutated.
    pub fn surface<'a>(&'a mut self, font: &Font, text: &str, color: Color)
        -> &'a sdl2::surface::Surface<'static>
    {
        let k = Self::key(font, text, color);
        self.map.entry(k).or_insert_with(|| {
            font.render(text).blended(color).unwrap_or_else(|_| {
                sdl2::surface::Surface::new(1, 1, sdl2::pixels::PixelFormatEnum::RGBA8888).unwrap()
            })
        })
    }
}

// ── FontAwesome 6 Solid codepoints ───────────────────────────────────────────

/// Return the FontAwesome 6 Solid unicode character for each tool.
fn tool_fa_icon(tool: Tool) -> char {
    match tool {
        Tool::Pen       => '\u{f304}', // pen
        Tool::Line      => '\u{f547}', // ruler-horizontal
        Tool::Rect      => '\u{f0c8}', // square
        Tool::Circle    => '\u{f111}', // circle
        Tool::Arrow     => '\u{f061}', // arrow-right
        Tool::Text      => '\u{f031}', // font
        Tool::Select    => '\u{f245}', // mouse-pointer
        Tool::Eraser    => '\u{f12d}', // eraser
        Tool::Highlight => '\u{f5c1}', // highlighter
        Tool::Laser     => '\u{f140}', // bullseye
    }
}

// ── Toolbar ───────────────────────────────────────────────────────────────────

/// Pre-computed button rects for hit-testing
pub struct ToolbarLayout {
    pub tools:          Vec<(Tool, Rect)>,
    pub palette:        Vec<Rect>,
    pub brush_minus:    Rect,
    pub brush_plus:     Rect,
    pub brush_label:    Rect,
    pub freeze_frame:   Rect,
    pub change_input:   Rect,
}

impl Default for ToolbarLayout {
    fn default() -> Self {
        let z = Rect::new(0, 0, 1, 1);
        Self {
            tools:        Vec::new(),
            palette:      Vec::new(),
            brush_minus:  z,
            brush_plus:   z,
            brush_label:  z,
            freeze_frame: z,
            change_input: z,
        }
    }
}

impl ToolbarLayout {
    pub fn build(canvas_width: i32, canvas_height: i32) -> Self {
        let y_base = canvas_height - TOOLBAR_HEIGHT;
        let mut layout = ToolbarLayout::default();

        // Tool buttons — left side
        let tools_order = [
            Tool::Pen, Tool::Line, Tool::Rect, Tool::Circle,
            Tool::Arrow, Tool::Text, Tool::Select, Tool::Eraser, Tool::Highlight, Tool::Laser,
        ];
        let mut x = TOOL_BTN_PAD;
        for tool in tools_order {
            let rect = Rect::new(x, y_base + (TOOLBAR_HEIGHT - TOOL_BTN_SIZE) / 2, TOOL_BTN_SIZE as u32, TOOL_BTN_SIZE as u32);
            layout.tools.push((tool, rect));
            x += TOOL_BTN_SIZE + TOOL_BTN_PAD;
        }

        // Colour palette — after tools
        x += TOOL_BTN_PAD * 2;
        for _ in 0..8 {
            let rect = Rect::new(
                x,
                y_base + (TOOLBAR_HEIGHT - PALETTE_SWATCH) / 2,
                PALETTE_SWATCH as u32,
                PALETTE_SWATCH as u32,
            );
            layout.palette.push(rect);
            x += PALETTE_SWATCH + PALETTE_PAD;
        }

        // Brush size controls — right-anchored group of [−] [size] [+]
        // All three elements share the same height as tool buttons.
        // Right edge butts against the change_input button with a small gap.
        const BRUSH_BTN_W:   i32 = TOOL_BTN_SIZE;  // square
        const BRUSH_LABEL_W: i32 = 60;
        const BRUSH_GAP:     i32 = 6;
        const RIGHT_PAD:     i32 = 8;
        let by  = y_base + (TOOLBAR_HEIGHT - TOOL_BTN_SIZE) / 2;
        // change_input sits flush to the right
        let ci_x = canvas_width - TOOL_BTN_SIZE - RIGHT_PAD;
        // freeze sits immediately left of change_input
        let fz_x = ci_x - BRUSH_GAP - TOOL_BTN_SIZE;
        let bx = fz_x - BRUSH_GAP
            - BRUSH_BTN_W - BRUSH_GAP - BRUSH_LABEL_W - BRUSH_GAP - BRUSH_BTN_W;
        layout.brush_minus = Rect::new(bx, by, BRUSH_BTN_W as u32, TOOL_BTN_SIZE as u32);
        layout.brush_label = Rect::new(
            bx + BRUSH_BTN_W + BRUSH_GAP, by,
            BRUSH_LABEL_W as u32, TOOL_BTN_SIZE as u32,
        );
        layout.brush_plus  = Rect::new(
            bx + BRUSH_BTN_W + BRUSH_GAP + BRUSH_LABEL_W + BRUSH_GAP, by,
            BRUSH_BTN_W as u32, TOOL_BTN_SIZE as u32,
        );
        layout.freeze_frame = Rect::new(fz_x, by, TOOL_BTN_SIZE as u32, TOOL_BTN_SIZE as u32);

        // Change Input button — far right
        layout.change_input = Rect::new(
            ci_x,
            y_base + (TOOLBAR_HEIGHT - TOOL_BTN_SIZE) / 2,
            TOOL_BTN_SIZE as u32,
            TOOL_BTN_SIZE as u32,
        );

        layout
    }
}

/// Render the toolbar.
/// `icon_font` — FontAwesome 6 Solid loaded at ~22pt
pub fn draw_toolbar(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    icon_font: &Font,
    layout: &ToolbarLayout,
    active_tool: Tool,
    active_color: (u8, u8, u8),
    brush_size: u32,
    canvas_height: i32,
    canvas_width: i32,
    freeze_frame: bool,
    cache: &mut TextCache,
) {
    let y_base = canvas_height - TOOLBAR_HEIGHT;

    // Background bar
    sdl.set_draw_color(Color::RGB(0x1a, 0x1a, 0x1a));
    sdl.fill_rect(Rect::new(0, y_base, canvas_width as u32, TOOLBAR_HEIGHT as u32)).ok();

    // Top border line
    sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
    sdl.draw_line((0, y_base), (canvas_width, y_base)).ok();

    let tc = sdl.texture_creator();

    // Tool buttons
    for (tool, rect) in &layout.tools {
        let is_active = *tool == active_tool;
        let bg = if is_active { Color::RGB(0x2a, 0x4a, 0x7a) } else { Color::RGB(0x2a, 0x2a, 0x2a) };
        sdl.set_draw_color(bg);
        sdl.fill_rect(*rect).ok();
        if is_active {
            sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
        } else {
            sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
        }
        sdl.draw_rect(*rect).ok();

        // FA icon glyph
        let icon = tool_fa_icon(*tool).to_string();
        let icon_color = if is_active { Color::RGB(255, 255, 255) } else { Color::RGB(190, 190, 190) };
        render_text_centered(sdl, icon_font, &tc, cache, &icon, icon_color, *rect);
    }

    // Palette swatches
    for (i, rect) in layout.palette.iter().enumerate() {
        let (r, g, b) = PALETTE[i];
        sdl.set_draw_color(Color::RGB(r, g, b));
        sdl.fill_rect(*rect).ok();
        if (r, g, b) == active_color {
            sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
            sdl.draw_rect(Rect::new(rect.x - 2, rect.y - 2, rect.width() + 4, rect.height() + 4)).ok();
        } else {
            sdl.set_draw_color(Color::RGB(0x55, 0x55, 0x55));
            sdl.draw_rect(*rect).ok();
        }
    }

    // Brush size controls
    sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
    sdl.fill_rect(layout.brush_minus).ok();
    sdl.fill_rect(layout.brush_plus).ok();
    sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
    sdl.draw_rect(layout.brush_minus).ok();
    sdl.draw_rect(layout.brush_plus).ok();
    let white = Color::RGB(220, 220, 220);
    let grey  = Color::RGB(170, 170, 170);
    render_text_centered(sdl, font, &tc, cache, "−", white, layout.brush_minus);
    render_text_centered(sdl, font, &tc, cache, "+", white, layout.brush_plus);
    render_text_centered(sdl, font, &tc, cache, &brush_size.to_string(), grey, layout.brush_label);

    // Freeze frame button — fa-pause when live, fa-play when frozen
    let fz_bg = if freeze_frame { Color::RGB(0x5a, 0x3a, 0x00) } else { Color::RGB(0x2a, 0x2a, 0x2a) };
    let fz_border = if freeze_frame { Color::RGB(0xff, 0xaa, 0x00) } else { Color::RGB(0x44, 0x44, 0x44) };
    let fz_icon_col = if freeze_frame { Color::RGB(0xff, 0xcc, 0x55) } else { Color::RGB(190, 190, 190) };
    let fz_icon = if freeze_frame { '\u{f04b}' } else { '\u{f04c}' }; // fa-play / fa-pause
    sdl.set_draw_color(fz_bg);
    sdl.fill_rect(layout.freeze_frame).ok();
    sdl.set_draw_color(fz_border);
    sdl.draw_rect(layout.freeze_frame).ok();
    render_text_centered(sdl, icon_font, &tc, cache, &fz_icon.to_string(), fz_icon_col, layout.freeze_frame);

    // Change Input button (video camera icon)
    sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
    sdl.fill_rect(layout.change_input).ok();
    sdl.set_draw_color(Color::RGB(0x44, 0x66, 0x44));
    sdl.draw_rect(layout.change_input).ok();
    let cam_icon = '\u{f03d}'.to_string(); // fa-video
    render_text_centered(sdl, icon_font, &tc, cache, &cam_icon, Color::RGB(120, 200, 120), layout.change_input);
}

// ── Menu bar ──────────────────────────────────────────────────────────────────

/// Actions the menu bar can return from a click
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    Undo,
    Redo,
    Clear,
    /// User clicked a device entry; index 0 = "No input", 1+ = devices list
    SelectDevice(usize),
}

/// The thin menu bar at the top of the window.
pub struct MenuBar {
    /// Whether the Input dropdown is currently open
    pub open: bool,
    /// Cached copies of detected device entries
    pub devices: Vec<DeviceInfo>,
    /// Which device is currently active (0 = no input, 1+ = devices[idx-1])
    pub active_idx: usize,
    /// Machine hostname shown as the bar title
    pub hostname: String,
}

impl MenuBar {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        let hostname = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_default()
            .trim()
            .to_string();
        let hostname = if hostname.is_empty() { "annotator".to_string() } else { hostname };
        Self { open: false, devices, active_idx: 0, hostname }
    }

    fn entry_count(&self) -> usize { self.devices.len() + 1 }

    fn entry_label(&self, idx: usize) -> String {
        if idx == 0 {
            "No input (black screen)".to_string()
        } else {
            let d = &self.devices[idx - 1];
            format!("{}  [{}]", d.name, d.path.display())
        }
    }

    fn undo_rect(&self) -> Rect { Rect::new(220, 0, 72, MENU_HEIGHT as u32) }
    fn redo_rect(&self) -> Rect { Rect::new(296, 0, 72, MENU_HEIGHT as u32) }
    fn clear_rect(&self) -> Rect { Rect::new(372, 0, 80, MENU_HEIGHT as u32) }

    fn menu_item_rect(&self, canvas_w: i32) -> Rect {
        // The "Input" menu item in the bar
        Rect::new(canvas_w - 120, 0, 120, MENU_HEIGHT as u32)
    }

    fn fps_rect(&self, canvas_w: i32) -> Rect {
        // FPS counter sits to the left of the Input button
        Rect::new(canvas_w - 360, 0, 230, MENU_HEIGHT as u32)
    }

    fn dropdown_rect(&self, canvas_w: i32) -> Rect {
        let row_h     = 36i32;
        let panel_w   = 480i32;
        let panel_h   = row_h * self.entry_count() as i32 + 8;
        Rect::new(canvas_w - panel_w, MENU_HEIGHT, panel_w as u32, panel_h as u32)
    }

    fn row_rect(&self, canvas_w: i32, idx: usize) -> Rect {
        let row_h   = 36i32;
        let panel_w = 480i32;
        let dx = canvas_w - panel_w;
        Rect::new(dx + 4, MENU_HEIGHT + 4 + idx as i32 * row_h, (panel_w - 8) as u32, (row_h - 2) as u32)
    }

    /// Draw the menu bar (and dropdown if open)
    pub fn draw(&self, sdl: &mut SdlCanvas<Window>, font: &Font, canvas_w: i32, has_undo: bool, has_redo: bool, capture_fps: f32, display_fps: f32, cache: &mut TextCache) {
        // Background bar
        sdl.set_draw_color(Color::RGB(0x14, 0x14, 0x14));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, MENU_HEIGHT as u32)).ok();
        // Bottom border
        sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
        sdl.draw_line((0, MENU_HEIGHT - 1), (canvas_w, MENU_HEIGHT - 1)).ok();

        let tc = sdl.texture_creator();

        // App title — left
        render_text(
            sdl, font, &tc, cache,
            &self.hostname,
            Color::RGB(120, 120, 120),
            Rect::new(12, 0, 220, MENU_HEIGHT as u32),
        );

        // FPS counter — to the left of Input button
        let fps_label = if capture_fps > 0.0 {
            format!("in {:.0}  disp {:.0}", capture_fps, display_fps)
        } else {
            format!("disp {:.0}", display_fps)
        };
        render_text(sdl, font, &tc, cache, &fps_label, Color::RGB(90, 180, 90), self.fps_rect(canvas_w));

        // "Input" menu button — right
        let item_rect = self.menu_item_rect(canvas_w);
        if self.open {
            sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
            sdl.fill_rect(item_rect).ok();
        }
        let item_color = if self.open { Color::RGB(255,255,255) } else { Color::RGB(200, 200, 200) };
        render_text(sdl, font, &tc, cache, "Input  ▾", item_color, item_rect);

        // Undo button
        let undo_r = self.undo_rect();
        let undo_col = if has_undo { Color::RGB(200, 200, 200) } else { Color::RGB(70, 70, 70) };
        render_text(sdl, font, &tc, cache, "↩ Undo", undo_col, undo_r);

        // Redo button
        let redo_r = self.redo_rect();
        let redo_col = if has_redo { Color::RGB(200, 200, 200) } else { Color::RGB(70, 70, 70) };
        render_text(sdl, font, &tc, cache, "↪ Redo", redo_col, redo_r);

        // Clear button
        let clear_r = self.clear_rect();
        render_text(sdl, font, &tc, cache, "✕ Clear", Color::RGB(200, 80, 80), clear_r);

        // Dropdown
        if self.open {
            let dr = self.dropdown_rect(canvas_w);
            sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
            sdl.fill_rect(dr).ok();
            sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
            sdl.draw_rect(dr).ok();

            for i in 0..self.entry_count() {
                let rr = self.row_rect(canvas_w, i);
                let is_active = i == self.active_idx;
                if is_active {
                    sdl.set_draw_color(Color::RGB(0x1a, 0x3a, 0x1a));
                    sdl.fill_rect(rr).ok();
                }
                let bullet = if is_active { "● " } else { "  " };
                let label = format!("{}{}", bullet, self.entry_label(i));
                let color = if is_active { Color::RGB(120, 220, 120) } else { Color::RGB(200, 200, 200) };
                render_text(sdl, font, &tc, cache, &label,
                    color,
                    Rect::new(rr.x + 8, rr.y, (rr.width() - 8) as u32, rr.height()));
            }
        }
    }

    /// Handle a mouse click.  Returns the action to take, and updates open state.
    pub fn click(&mut self, mx: i32, my: i32, canvas_w: i32) -> MenuAction {
        if my >= MENU_HEIGHT {
            // Click below menu bar
            if self.open {
                // Check dropdown rows
                for i in 0..self.entry_count() {
                    if self.row_rect(canvas_w, i).contains_point((mx, my)) {
                        self.open = false;
                        return MenuAction::SelectDevice(i);
                    }
                }
                // Clicked outside dropdown — close
                self.open = false;
            }
            return MenuAction::None;
        }

        // Click inside menu bar
        if self.menu_item_rect(canvas_w).contains_point((mx, my)) {
            self.open = !self.open;
        } else if self.undo_rect().contains_point((mx, my)) {
            if self.open { self.open = false; }
            return MenuAction::Undo;
        } else if self.redo_rect().contains_point((mx, my)) {
            if self.open { self.open = false; }
            return MenuAction::Redo;
        } else if self.clear_rect().contains_point((mx, my)) {
            if self.open { self.open = false; }
            return MenuAction::Clear;
        } else if self.open {
            self.open = false;
        }
        MenuAction::None
    }

    /// Close the dropdown (e.g. when a key is pressed)
    pub fn close(&mut self) { self.open = false; }
}

// ── Device picker modal ───────────────────────────────────────────────────────

#[derive(PartialEq)]
pub enum PickerStage { Devices, Modes }

pub struct DevicePicker {
    pub devices:       Vec<DeviceInfo>,
    pub device_cursor: usize,
    pub mode_cursor:   usize,
    pub stage:         PickerStage,
    pub scroll_offset: usize,  // first visible row index
}

impl DevicePicker {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        Self { devices, device_cursor: 0, mode_cursor: 0, stage: PickerStage::Devices, scroll_offset: 0 }
    }

    /// Backward-compat alias used in a few places.
    pub fn entry_count(&self) -> usize { self.devices.len() + 1 }

    pub fn label(&self, idx: usize) -> String {
        if idx == 0 {
            "No input  (black screen)".to_string()
        } else {
            let dev = &self.devices[idx - 1];
            format!("{}  [{}]", dev.name, dev.path.display())
        }
    }

    fn caps_label(&self, idx: usize) -> String {
        if idx == 0 { String::new() }
        else { self.devices[idx - 1].caps_summary.clone() }
    }

    /// Ensure the cursor is within the visible scroll window for the given canvas height.
    pub fn scroll_to_cursor(&mut self, canvas_h: i32) {
        let row_h       = 56i32;
        let header_h    = 64i32;
        let footer_h    = 36i32;
        let avail_h     = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let cursor = match self.stage {
            PickerStage::Devices => self.device_cursor,
            PickerStage::Modes   => self.mode_cursor,
        };
        // Scroll down: cursor went below visible window
        if cursor >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor + 1 - max_visible;
        }
        // Scroll up: cursor went above visible window
        if cursor < self.scroll_offset {
            self.scroll_offset = cursor;
        }
    }

    pub fn move_up(&mut self) {
        match self.stage {
            PickerStage::Devices => {
                if self.device_cursor > 0 {
                    self.device_cursor -= 1;
                    if self.device_cursor < self.scroll_offset {
                        self.scroll_offset = self.device_cursor;
                    }
                }
            }
            PickerStage::Modes => {
                if self.mode_cursor > 0 {
                    self.mode_cursor -= 1;
                    if self.mode_cursor < self.scroll_offset {
                        self.scroll_offset = self.mode_cursor;
                    }
                }
            }
        }
    }
    pub fn move_down(&mut self) {
        match self.stage {
            PickerStage::Devices => {
                if self.device_cursor + 1 < self.entry_count() {
                    self.device_cursor += 1;
                }
            }
            PickerStage::Modes => {
                let n = self.current_mode_count();
                if self.mode_cursor + 1 < n { self.mode_cursor += 1; }
            }
        }
    }

    fn current_mode_count(&self) -> usize {
        if self.device_cursor == 0 { 0 }
        else { self.devices[self.device_cursor - 1].modes.len() }
    }

    /// Call on Enter key or row click. Returns Some((device_idx, mode_idx)) when confirmed.
    pub fn enter(&mut self) -> Option<(usize, usize)> {
        match self.stage {
            PickerStage::Devices => {
                if self.device_cursor == 0 {
                    return Some((0, 0));  // no input
                }
                let modes = &self.devices[self.device_cursor - 1].modes;
                if modes.len() <= 1 {
                    Some((self.device_cursor, 0))
                } else {
                    self.stage = PickerStage::Modes;
                    self.mode_cursor = 0;
                    self.scroll_offset = 0;
                    None
                }
            }
            PickerStage::Modes => {
                let result = Some((self.device_cursor, self.mode_cursor));
                self.stage = PickerStage::Devices;
                result
            }
        }
    }

    /// Call on click. Sets the cursor to the clicked row then calls enter().
    /// Returns Some if a selection was confirmed.
    pub fn click_enter(&mut self, mx: i32, my: i32, canvas_w: i32, canvas_h: i32) -> Option<(usize, usize)> {
        let panel_w     = 720i32;
        let row_h       = 56i32;
        let header_h    = 64i32;
        let footer_h    = 36i32;
        let avail_h     = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let px          = (canvas_w - panel_w) / 2;

        match self.stage {
            PickerStage::Devices => {
                let total   = self.entry_count();
                let visible = total.min(max_visible);
                let scroll  = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py      = (canvas_h - panel_h) / 2;
                for vi in 0..visible {
                    let i  = scroll + vi;
                    let ry = py + header_h + (vi as i32) * row_h;
                    if Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32)
                        .contains_point((mx, my))
                    {
                        self.device_cursor = i;
                        return self.enter();
                    }
                }
                None
            }
            PickerStage::Modes => {
                let total   = self.current_mode_count();
                let visible = total.min(max_visible);
                let scroll  = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py      = (canvas_h - panel_h) / 2;
                for vi in 0..visible {
                    let i  = scroll + vi;
                    let ry = py + header_h + (vi as i32) * row_h;
                    if Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32)
                        .contains_point((mx, my))
                    {
                        self.mode_cursor = i;
                        return self.enter();
                    }
                }
                None
            }
        }
    }

    pub fn draw(&self, sdl: &mut SdlCanvas<Window>, font: &Font, canvas_w: i32, canvas_h: i32, cache: &mut TextCache) {
        sdl.set_draw_color(Color::RGBA(0, 0, 0, 180));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, canvas_h as u32)).ok();

        let panel_w  = 720i32;
        let row_h    = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let tc = sdl.texture_creator();

        // Maximum rows we can show without overflowing the screen
        let avail_h      = canvas_h - 40;  // 20px margin top+bottom
        let max_visible  = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let px           = (canvas_w - panel_w) / 2;

        match self.stage {
            PickerStage::Devices => {
                let total        = self.entry_count();
                let visible      = total.min(max_visible);
                let scroll       = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h      = header_h + row_h * visible as i32 + footer_h;
                let py           = (canvas_h - panel_h) / 2;

                sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
                sdl.fill_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();
                sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
                sdl.draw_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();

                let title = if total > max_visible {
                    format!("Select capture device  ({}/{})", self.device_cursor + 1, total)
                } else {
                    "Select capture device".to_string()
                };
                render_text(sdl, font, &tc, cache, &title,
                    Color::RGB(220, 220, 220),
                    Rect::new(px + 16, py + 16, (panel_w - 32) as u32, 32));

                // Scroll indicator arrows
                if scroll > 0 {
                    render_text(sdl, font, &tc, cache, "\u{25b2} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + 18, 70, 20));
                }
                if scroll + visible < total {
                    render_text(sdl, font, &tc, cache, "\u{25bc} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + panel_h - footer_h + 8, 70, 20));
                }

                for vi in 0..visible {
                    let i  = scroll + vi;
                    let ry = py + header_h + (vi as i32) * row_h;
                    let row_rect = Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32);
                    let is_sel = i == self.device_cursor;
                    if is_sel {
                        sdl.set_draw_color(Color::RGB(0x2a, 0x4a, 0x7a));
                        sdl.fill_rect(row_rect).ok();
                        sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                        sdl.draw_rect(row_rect).ok();
                    }
                    let name_col = if is_sel { Color::RGB(255,255,255) } else { Color::RGB(180,180,180) };
                    let caps_col = if is_sel { Color::RGB(140,210,255) } else { Color::RGB(100,140,100) };
                    render_text(sdl, font, &tc, cache, &self.label(i), name_col,
                        Rect::new(px + 20, ry + 4, (panel_w - 40) as u32, 22));
                    let caps = self.caps_label(i);
                    if !caps.is_empty() {
                        render_text(sdl, font, &tc, cache, &caps, caps_col,
                            Rect::new(px + 28, ry + 28, (panel_w - 48) as u32, 20));
                    }
                }
                render_text(sdl, font, &tc, cache,
                    "\u{2191}\u{2193} navigate   Enter to select   F2 to cancel",
                    Color::RGB(100, 100, 100),
                    Rect::new(px + 16, py + panel_h - footer_h + 8, (panel_w - 32) as u32, 20));
            }

            PickerStage::Modes => {
                let dev          = &self.devices[self.device_cursor - 1];
                let total        = dev.modes.len();
                let visible      = total.min(max_visible);
                let scroll       = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h      = header_h + row_h * visible as i32 + footer_h;
                let py           = (canvas_h - panel_h) / 2;

                sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
                sdl.fill_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();
                sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
                sdl.draw_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();

                let title = if total > max_visible {
                    format!("Select format \u{2014} {}  ({}/{})", dev.name, self.mode_cursor + 1, total)
                } else {
                    format!("Select format \u{2014} {}", dev.name)
                };
                render_text(sdl, font, &tc, cache, &title,
                    Color::RGB(220, 220, 220),
                    Rect::new(px + 16, py + 16, (panel_w - 32) as u32, 32));

                // Scroll indicator arrows
                if scroll > 0 {
                    render_text(sdl, font, &tc, cache, "\u{25b2} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + 18, 70, 20));
                }
                if scroll + visible < total {
                    render_text(sdl, font, &tc, cache, "\u{25bc} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + panel_h - footer_h + 8, 70, 20));
                }

                for vi in 0..visible {
                    let i    = scroll + vi;
                    let mode = &dev.modes[i];
                    let ry   = py + header_h + (vi as i32) * row_h;
                    let row_rect = Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32);
                    let is_sel = i == self.mode_cursor;
                    if is_sel {
                        sdl.set_draw_color(Color::RGB(0x2a, 0x4a, 0x7a));
                        sdl.fill_rect(row_rect).ok();
                        sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                        sdl.draw_rect(row_rect).ok();
                    }
                    let col = if is_sel { Color::RGB(255,255,255) } else { Color::RGB(180,180,180) };
                    render_text(sdl, font, &tc, cache, &mode.label(), col,
                        Rect::new(px + 20, ry + 14, (panel_w - 40) as u32, 24));
                }
                render_text(sdl, font, &tc, cache,
                    "\u{2191}\u{2193} navigate   Enter to confirm   Esc to go back",
                    Color::RGB(100, 100, 100),
                    Rect::new(px + 16, py + panel_h - footer_h + 8, (panel_w - 32) as u32, 20));
            }
        }
    }
}

// ── Text rendering helpers ────────────────────────────────────────────────────

/// Render text, vertically centred within `dst`, left-aligned.
pub fn render_text(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    tc: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    cache: &mut TextCache,
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() { return; }
    let surface = cache.surface(font, text, color);
    let texture = match tc.create_texture_from_surface(surface) { Ok(t) => t, Err(_) => return };
    let q = texture.query();
    let tw = q.width.min(dst.width()) as i32;
    let th = q.height.min(dst.height()) as i32;
    let src  = Rect::new(0, 0, tw as u32, th as u32);
    let dest = Rect::new(dst.x + 4, dst.y + (dst.height() as i32 - th) / 2, tw as u32, th as u32);
    sdl.copy(&texture, src, dest).ok();
}

/// Render text horizontally and vertically centred within `dst` (for icon buttons).
fn render_text_centered(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    tc: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    cache: &mut TextCache,
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() { return; }
    let surface = cache.surface(font, text, color);
    let texture = match tc.create_texture_from_surface(surface) { Ok(t) => t, Err(_) => return };
    let q = texture.query();
    let tw = q.width.min(dst.width())  as i32;
    let th = q.height.min(dst.height()) as i32;
    let src  = Rect::new(0, 0, tw as u32, th as u32);
    let dest = Rect::new(
        dst.x + (dst.width()  as i32 - tw) / 2,
        dst.y + (dst.height() as i32 - th) / 2,
        tw as u32,
        th as u32,
    );
    sdl.copy(&texture, src, dest).ok();
}
