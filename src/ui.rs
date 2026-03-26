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

pub const TOOLBAR_HEIGHT: i32 = 64;
pub const MENU_HEIGHT:    i32 = 30;
const TOOL_BTN_SIZE:  i32 = 48;
const TOOL_BTN_PAD:   i32 = 8;
const PALETTE_SWATCH: i32 = 28;
const PALETTE_PAD:    i32 = 5;

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
        Tool::Eraser    => '\u{f12d}', // eraser
        Tool::Highlight => '\u{f5c1}', // highlighter
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
            Tool::Arrow, Tool::Text, Tool::Eraser, Tool::Highlight,
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
        let by  = y_base + (TOOLBAR_HEIGHT - TOOL_BTN_SIZE) / 2;
        // change_input sits at canvas_width - 52; leave 10px gap before it
        let bx = canvas_width - 52 - 10
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

        // Change Input button — far right
        layout.change_input = Rect::new(
            canvas_width - 52,
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
        render_text_centered(sdl, icon_font, &tc, &icon, icon_color, *rect);
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
    render_text_centered(sdl, font, &tc, "−", white, layout.brush_minus);
    render_text_centered(sdl, font, &tc, "+", white, layout.brush_plus);
    render_text_centered(sdl, font, &tc, &brush_size.to_string(), grey, layout.brush_label);

    // Change Input button (video camera icon)
    sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
    sdl.fill_rect(layout.change_input).ok();
    sdl.set_draw_color(Color::RGB(0x44, 0x66, 0x44));
    sdl.draw_rect(layout.change_input).ok();
    let cam_icon = '\u{f03d}'.to_string(); // fa-video
    render_text_centered(sdl, icon_font, &tc, &cam_icon, Color::RGB(120, 200, 120), layout.change_input);
}

// ── Menu bar ──────────────────────────────────────────────────────────────────

/// Actions the menu bar can return from a click
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
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
}

impl MenuBar {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        Self { open: false, devices, active_idx: 0 }
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

    fn menu_item_rect(&self, canvas_w: i32) -> Rect {
        // The "Input" menu item in the bar
        Rect::new(canvas_w - 120, 0, 120, MENU_HEIGHT as u32)
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
    pub fn draw(&self, sdl: &mut SdlCanvas<Window>, font: &Font, canvas_w: i32) {
        // Background bar
        sdl.set_draw_color(Color::RGB(0x14, 0x14, 0x14));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, MENU_HEIGHT as u32)).ok();
        // Bottom border
        sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
        sdl.draw_line((0, MENU_HEIGHT - 1), (canvas_w, MENU_HEIGHT - 1)).ok();

        let tc = sdl.texture_creator();

        // App title — left
        render_text(
            sdl, font, &tc,
            "Court Annotator",
            Color::RGB(120, 120, 120),
            Rect::new(12, 0, 220, MENU_HEIGHT as u32),
        );

        // "Input" menu button — right
        let item_rect = self.menu_item_rect(canvas_w);
        if self.open {
            sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
            sdl.fill_rect(item_rect).ok();
        }
        let item_color = if self.open { Color::RGB(255,255,255) } else { Color::RGB(200, 200, 200) };
        render_text(sdl, font, &tc, "Input  ▾", item_color, item_rect);

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
                render_text(sdl, font, &tc, &label,
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
        } else if self.open {
            self.open = false;
        }
        MenuAction::None
    }

    /// Close the dropdown (e.g. when a key is pressed)
    pub fn close(&mut self) { self.open = false; }
}

// ── Device picker modal ───────────────────────────────────────────────────────

pub struct DevicePicker {
    pub devices: Vec<DeviceInfo>,
    pub selected: usize,
}

impl DevicePicker {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        Self { devices, selected: 0 }
    }

    pub fn entry_count(&self) -> usize { self.devices.len() + 1 }

    pub fn label(&self, idx: usize) -> String {
        if idx == 0 {
            "No input  (black screen)".to_string()
        } else {
            let dev = &self.devices[idx - 1];
            format!("{}  [{}]", dev.name, dev.path.display())
        }
    }

    pub fn move_up(&mut self)   { if self.selected > 0 { self.selected -= 1; } }
    pub fn move_down(&mut self) { if self.selected + 1 < self.entry_count() { self.selected += 1; } }

    pub fn draw(&self, sdl: &mut SdlCanvas<Window>, font: &Font, canvas_w: i32, canvas_h: i32) {
        sdl.set_draw_color(Color::RGBA(0, 0, 0, 180));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, canvas_h as u32)).ok();

        let panel_w = 640i32;
        let row_h   = 44i32;
        let count   = self.entry_count() as i32;
        let panel_h = 64 + row_h * count + 32;
        let px = (canvas_w - panel_w) / 2;
        let py = (canvas_h - panel_h) / 2;

        sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
        sdl.fill_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();
        sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
        sdl.draw_rect(Rect::new(px, py, panel_w as u32, panel_h as u32)).ok();

        let tc = sdl.texture_creator();
        render_text(sdl, font, &tc, "Select capture device",
            Color::RGB(220, 220, 220),
            Rect::new(px + 16, py + 16, (panel_w - 32) as u32, 32));

        for i in 0..self.entry_count() {
            let ry = py + 64 + (i as i32) * row_h;
            let row_rect = Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32);
            if i == self.selected {
                sdl.set_draw_color(Color::RGB(0x2a, 0x4a, 0x7a));
                sdl.fill_rect(row_rect).ok();
                sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                sdl.draw_rect(row_rect).ok();
            }
            let color = if i == self.selected { Color::RGB(255,255,255) } else { Color::RGB(180,180,180) };
            render_text(sdl, font, &tc, &self.label(i), color,
                Rect::new(px + 20, ry + 4, (panel_w - 40) as u32, (row_h - 8) as u32));
        }

        render_text(sdl, font, &tc,
            "↑↓ navigate   Enter / click to confirm   F2 to cancel",
            Color::RGB(100, 100, 100),
            Rect::new(px + 16, py + panel_h - 28, (panel_w - 32) as u32, 20));
    }

    pub fn click_hit(&self, mx: i32, my: i32, canvas_w: i32, canvas_h: i32) -> Option<usize> {
        let panel_w = 640i32;
        let row_h   = 44i32;
        let count   = self.entry_count() as i32;
        let panel_h = 64 + row_h * count + 32;
        let px = (canvas_w - panel_w) / 2;
        let py = (canvas_h - panel_h) / 2;
        for i in 0..self.entry_count() {
            let ry = py + 64 + (i as i32) * row_h;
            if Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32)
                .contains_point((mx, my)) {
                return Some(i);
            }
        }
        None
    }
}

// ── Text rendering helpers ────────────────────────────────────────────────────

/// Render text, vertically centred within `dst`, left-aligned.
pub fn render_text(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    tc: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() { return; }
    let surface = match font.render(text).blended(color) { Ok(s) => s, Err(_) => return };
    let texture = match tc.create_texture_from_surface(&surface) { Ok(t) => t, Err(_) => return };
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
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() { return; }
    let surface = match font.render(text).blended(color) { Ok(s) => s, Err(_) => return };
    let texture = match tc.create_texture_from_surface(&surface) { Ok(t) => t, Err(_) => return };
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
