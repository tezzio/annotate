//! V2 — modern side rail UI: a single scrollable left rail containing all tools
//! and controls.  When the rail content is taller than the canvas, the rail
//! becomes scrollable via two arrow buttons at the bottom.

use sdl2::{pixels::Color, rect::Rect, render::Canvas as SdlCanvas, ttf::Font, video::Window};

use crate::ui::{MENU_HEIGHT, TOOL_ICONS, render_text_centered};
use crate::{
    input::switch_tool,
    state::AppState,
    tools::{PALETTE, Tool},
};

// Static FontAwesome codepoints (TODO 3 — eliminates per-frame
// `.to_string()` allocations in the V2 toolbar).
const FREEZE_ICON_PAUSE: &str = "\u{f04b}";
const FREEZE_ICON_PLAY: &str = "\u{f04c}";
const CAMERA_ICON: &str = "\u{f03d}";
const SCREENSHOT_ICON: &str = "\u{f030}";
const CHEVRON_UP: &str = "\u{f077}";
const CHEVRON_DOWN: &str = "\u{f078}";

// ── Layout constants ─────────────────────────────────────────────────────────

pub const RAIL_WIDTH: i32 = 60;
const BTN_SIZE: i32 = 44;
const BTN_GAP: i32 = 4;
const SWATCH_SIZE: i32 = 32;
const SWATCH_GAP: i32 = 4;
const SECTION_PAD: i32 = 8; // padding between sections
const SCROLL_BTN_SIZE: i32 = 32;

// ── Hit-test regions ────────────────────────────────────────────────────────

pub struct RailLayout {
    pub tool_btns: Vec<(Tool, Rect)>,
    pub color_tile: Rect,
    pub color_popout: Vec<Rect>, // 8 swatch rects when popout is open
    pub brush_minus: Rect,
    pub brush_label: Rect,
    pub brush_plus: Rect,
    pub freeze: Rect,
    pub screenshot: Rect,
    pub change_input: Rect,
    pub scroll_up: Rect,
    pub scroll_down: Rect,
    /// Total content height (in pixels) including all sections.
    pub content_height: i32,
    /// Available height (canvas height minus menu bar minus scroll buttons).
    pub viewport_h: i32,
    /// True if scrolling is needed (content > viewport).
    pub needs_scroll: bool,
}

impl Default for RailLayout {
    fn default() -> Self {
        let z = Rect::new(0, 0, 1, 1);
        Self {
            tool_btns: Vec::new(),
            color_tile: z,
            color_popout: Vec::new(),
            brush_minus: z,
            brush_label: z,
            brush_plus: z,
            freeze: z,
            screenshot: z,
            change_input: z,
            scroll_up: z,
            scroll_down: z,
            content_height: 0,
            viewport_h: 0,
            needs_scroll: false,
        }
    }
}

// ── V2 layout state ──────────────────────────────────────────────────────────

#[derive(Default)]
pub struct V2Layout {
    /// All hit-test regions, recomputed each frame from the current scroll offset.
    pub rail: RailLayout,
    /// Current scroll offset in pixels (how far the content has been scrolled up).
    pub scroll: i32,
    /// Whether the color popout is open.
    pub popout_open: bool,
}

impl V2Layout {
    pub fn build(canvas_w: i32, canvas_h: i32) -> Self {
        let mut layout = Self {
            rail: RailLayout::default(),
            scroll: 0,
            popout_open: false,
        };
        layout.rebuild(canvas_w, canvas_h);
        layout
    }

    /// Recompute the rail layout. Accounts for the current scroll offset.
    pub fn rebuild(&mut self, canvas_w: i32, canvas_h: i32) {
        let _ = canvas_w; // not used; rail is anchored at x=0

        // Compute total content height (in un-scrolled coordinates)
        let tools_h = 10 * BTN_SIZE + 9 * BTN_GAP; // 10 tools
        let color_h = BTN_SIZE; // 1 color tile
        let brush_h = BTN_SIZE + 2 + BTN_SIZE; // 3 buttons stacked: -, size, +
        let actions_h = BTN_SIZE + BTN_GAP + BTN_SIZE + BTN_GAP + BTN_SIZE; // freeze, screenshot, change input
        let content_h = tools_h
            + SECTION_PAD
            + color_h
            + SECTION_PAD
            + brush_h
            + SECTION_PAD
            + actions_h
            + SECTION_PAD * 4; // pad above each section

        let viewport_h = canvas_h - MENU_HEIGHT - 2 * SCROLL_BTN_SIZE - 8;
        let needs_scroll = content_h > viewport_h;

        self.rail.content_height = content_h;
        self.rail.viewport_h = viewport_h;
        self.rail.needs_scroll = needs_scroll;

        // If we deleted scroll, clamp it
        let max_scroll = (content_h - viewport_h).max(0);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        if self.scroll < 0 {
            self.scroll = 0;
        }

        // Anchor Y for content (starts below the menu bar, minus scroll offset)
        let mut y = MENU_HEIGHT - self.scroll + SECTION_PAD;

        // Tools section
        //
        // NOTE: Tool::Arrow and Tool::Text are intentionally excluded from the
        // toolbar.  Arrow has a segfault when switching away from it to another
        // tool (see the conversation log); Text ("add text") is being retired
        // from the UI per user request.  The underlying tool implementations
        // in `crate::tools` and the keyboard shortcuts in `crate::input`
        // remain intact so these features can be re-enabled later by simply
        // adding the entries back to this list.
        self.rail.tool_btns.clear();
        let tools = [
            Tool::Pen,
            Tool::Line,
            Tool::Rect,
            Tool::Circle,
            Tool::Select,
            Tool::Eraser,
            Tool::Highlight,
            Tool::Laser,
        ];
        for tool in tools {
            let x = (RAIL_WIDTH - BTN_SIZE) / 2;
            self.rail
                .tool_btns
                .push((tool, Rect::new(x, y, BTN_SIZE as u32, BTN_SIZE as u32)));
            y += BTN_SIZE + BTN_GAP;
        }
        y += SECTION_PAD;

        // Color tile
        let color_size = BTN_SIZE;
        self.rail.color_tile = Rect::new(
            (RAIL_WIDTH - color_size) / 2,
            y,
            color_size as u32,
            color_size as u32,
        );
        y += color_size + SECTION_PAD;

        // Brush controls (−, label, +)
        let brush_minus = Rect::new(
            (RAIL_WIDTH - BTN_SIZE) / 2,
            y,
            BTN_SIZE as u32,
            BTN_SIZE as u32,
        );
        y += BTN_SIZE + 2;
        let brush_label = Rect::new((RAIL_WIDTH - BTN_SIZE) / 2, y, BTN_SIZE as u32, 24);
        y += 24 + 2;
        let brush_plus = Rect::new(
            (RAIL_WIDTH - BTN_SIZE) / 2,
            y,
            BTN_SIZE as u32,
            BTN_SIZE as u32,
        );
        self.rail.brush_minus = brush_minus;
        self.rail.brush_label = brush_label;
        self.rail.brush_plus = brush_plus;
        y += BTN_SIZE + SECTION_PAD;

        // Action buttons (freeze, screenshot, change input)
        self.rail.freeze = Rect::new(
            (RAIL_WIDTH - BTN_SIZE) / 2,
            y,
            BTN_SIZE as u32,
            BTN_SIZE as u32,
        );
        y += BTN_SIZE + BTN_GAP;
        self.rail.screenshot = Rect::new(
            (RAIL_WIDTH - BTN_SIZE) / 2,
            y,
            BTN_SIZE as u32,
            BTN_SIZE as u32,
        );
        y += BTN_SIZE + BTN_GAP;
        self.rail.change_input = Rect::new(
            (RAIL_WIDTH - BTN_SIZE) / 2,
            y,
            BTN_SIZE as u32,
            BTN_SIZE as u32,
        );

        // Popout swatches (computed from color tile, fixed positions to the right)
        self.rail.color_popout.clear();
        for i in 0..8 {
            let sw_x = RAIL_WIDTH + 6;
            let sw_y = self.rail.color_tile.y + i as i32 * (SWATCH_SIZE + SWATCH_GAP);
            self.rail.color_popout.push(Rect::new(
                sw_x,
                sw_y,
                SWATCH_SIZE as u32,
                SWATCH_SIZE as u32,
            ));
        }

        // Scroll buttons at the very bottom of the rail
        let scroll_y_up = canvas_h - 2 * SCROLL_BTN_SIZE - 4;
        let scroll_y_down = canvas_h - SCROLL_BTN_SIZE - 4;
        self.rail.scroll_up = Rect::new(
            (RAIL_WIDTH - SCROLL_BTN_SIZE) / 2,
            scroll_y_up,
            SCROLL_BTN_SIZE as u32,
            SCROLL_BTN_SIZE as u32,
        );
        self.rail.scroll_down = Rect::new(
            (RAIL_WIDTH - SCROLL_BTN_SIZE) / 2,
            scroll_y_down,
            SCROLL_BTN_SIZE as u32,
            SCROLL_BTN_SIZE as u32,
        );
    }

    pub fn scroll_by(&mut self, canvas_w: i32, canvas_h: i32, dy: i32) {
        self.scroll += dy;
        self.rebuild(canvas_w, canvas_h);
    }
}

// ── Drawing ─────────────────────────────────────────────────────────────────

/// Draw the V2 scrollable left rail.
pub fn draw_v2_toolbar(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    icon_font: &Font,
    layout: &V2Layout,
    active_tool: Tool,
    active_color: (u8, u8, u8),
    brush_size: u32,
    freeze_frame: bool,
    cache: &mut crate::ui::TextCache,
) {
    let canvas_h = sdl.output_size().expect("output_size").1 as i32;
    let tc = sdl.texture_creator();

    // Rail background
    sdl.set_draw_color(Color::RGB(18, 18, 20));
    sdl.fill_rect(Rect::new(
        0,
        MENU_HEIGHT,
        RAIL_WIDTH as u32,
        (canvas_h - MENU_HEIGHT) as u32,
    ))
    .ok();
    sdl.set_draw_color(Color::RGB(35, 35, 40));
    sdl.fill_rect(Rect::new(
        RAIL_WIDTH - 2,
        MENU_HEIGHT,
        2,
        (canvas_h - MENU_HEIGHT) as u32,
    ))
    .ok();

    // Tool buttons — indexed by `tool.index()` into the TOOL_ICONS const
    // table to avoid per-frame `String` allocations from `tool_fa_icon`.
    for (tool, rect) in &layout.rail.tool_btns {
        let is_active = *tool == active_tool;
        let bg = if is_active {
            Color::RGB(30, 60, 110)
        } else {
            Color::RGB(28, 28, 32)
        };
        let border = if is_active {
            Color::RGB(74, 158, 255)
        } else {
            Color::RGB(55, 55, 60)
        };
        sdl.set_draw_color(bg);
        sdl.fill_rect(*rect).ok();
        sdl.set_draw_color(border);
        sdl.draw_rect(*rect).ok();
        let icon_color = if is_active {
            Color::RGB(255, 255, 255)
        } else {
            Color::RGB(160, 160, 170)
        };
        render_text_centered(
            sdl,
            icon_font,
            &tc,
            cache,
            TOOL_ICONS[tool.index()],
            icon_color,
            *rect,
        );
    }

    // Color tile
    let ct = layout.rail.color_tile;
    let (ar, ag, ab) = active_color;
    sdl.set_draw_color(Color::RGB(ar, ag, ab));
    sdl.fill_rect(ct).ok();
    let tile_border = if layout.popout_open {
        Color::RGB(74, 158, 255)
    } else {
        Color::RGB(55, 55, 60)
    };
    sdl.set_draw_color(tile_border);
    sdl.draw_rect(ct).ok();
    // Chevron indicator
    let chev = if layout.popout_open {
        CHEVRON_UP
    } else {
        CHEVRON_DOWN
    };
    render_text_centered(
        sdl,
        icon_font,
        &tc,
        cache,
        chev,
        Color::RGB(180, 180, 190),
        Rect::new(
            ct.x + ct.width() as i32 - 14,
            ct.y + ct.height() as i32 - 14,
            12,
            12,
        ),
    );

    // Color popout
    if layout.popout_open {
        // Background panel covering all swatches
        if let Some(first) = layout.rail.color_popout.first() {
            if let Some(last) = layout.rail.color_popout.last() {
                let px = first.x - 6;
                let py = first.y - 6;
                let pw = SWATCH_SIZE + 12;
                let ph = (last.y + SWATCH_SIZE) - first.y + 12;
                sdl.set_draw_color(Color::RGB(25, 25, 28));
                sdl.fill_rect(Rect::new(px, py, pw as u32, ph as u32)).ok();
                sdl.set_draw_color(Color::RGB(45, 45, 50));
                sdl.draw_rect(Rect::new(px, py, pw as u32, ph as u32)).ok();
            }
        }
        for (i, sr) in layout.rail.color_popout.iter().enumerate() {
            let (r, g, b) = PALETTE[i];
            sdl.set_draw_color(Color::RGB(r, g, b));
            sdl.fill_rect(*sr).ok();
            if (r, g, b) == active_color {
                sdl.set_draw_color(Color::RGB(74, 158, 255));
                sdl.draw_rect(Rect::new(
                    sr.x - 2,
                    sr.y - 2,
                    sr.width() + 4,
                    sr.height() + 4,
                ))
                .ok();
            } else {
                sdl.set_draw_color(Color::RGB(55, 55, 60));
                sdl.draw_rect(*sr).ok();
            }
        }
    }

    // Brush controls
    let white = Color::RGB(220, 220, 220);
    let grey = Color::RGB(140, 140, 150);
    sdl.set_draw_color(Color::RGB(28, 28, 32));
    sdl.fill_rect(layout.rail.brush_minus).ok();
    sdl.fill_rect(layout.rail.brush_plus).ok();
    sdl.set_draw_color(Color::RGB(55, 55, 60));
    sdl.draw_rect(layout.rail.brush_minus).ok();
    sdl.draw_rect(layout.rail.brush_plus).ok();
    render_text_centered(sdl, font, &tc, cache, "−", white, layout.rail.brush_minus);
    render_text_centered(sdl, font, &tc, cache, "+", white, layout.rail.brush_plus);
    render_text_centered(
        sdl,
        font,
        &tc,
        cache,
        &brush_size.to_string(),
        grey,
        layout.rail.brush_label,
    );

    // Action buttons
    let fz_bg = if freeze_frame {
        Color::RGB(60, 40, 0)
    } else {
        Color::RGB(28, 28, 32)
    };
    let fz_border = if freeze_frame {
        Color::RGB(255, 170, 0)
    } else {
        Color::RGB(55, 55, 60)
    };
    let fz_icon_col = if freeze_frame {
        Color::RGB(255, 204, 85)
    } else {
        Color::RGB(160, 160, 170)
    };
    let fz_icon = if freeze_frame {
        FREEZE_ICON_PAUSE
    } else {
        FREEZE_ICON_PLAY
    };
    sdl.set_draw_color(fz_bg);
    sdl.fill_rect(layout.rail.freeze).ok();
    sdl.set_draw_color(fz_border);
    sdl.draw_rect(layout.rail.freeze).ok();
    render_text_centered(
        sdl,
        icon_font,
        &tc,
        cache,
        fz_icon,
        fz_icon_col,
        layout.rail.freeze,
    );

    sdl.set_draw_color(Color::RGB(28, 28, 32));
    sdl.fill_rect(layout.rail.change_input).ok();
    sdl.set_draw_color(Color::RGB(50, 100, 50));
    sdl.draw_rect(layout.rail.change_input).ok();
    render_text_centered(
        sdl,
        icon_font,
        &tc,
        cache,
        CAMERA_ICON,
        Color::RGB(100, 200, 100),
        layout.rail.change_input,
    );

    // Screenshot button
    sdl.set_draw_color(Color::RGB(28, 28, 32));
    sdl.fill_rect(layout.rail.screenshot).ok();
    sdl.set_draw_color(Color::RGB(80, 120, 80));
    sdl.draw_rect(layout.rail.screenshot).ok();
    render_text_centered(
        sdl,
        icon_font,
        &tc,
        cache,
        SCREENSHOT_ICON,
        Color::RGB(140, 200, 140),
        layout.rail.screenshot,
    );

    // Scroll buttons (only visible if needed)
    if layout.rail.needs_scroll {
        // Up arrow
        let up_active = layout.scroll > 0;
        sdl.set_draw_color(if up_active {
            Color::RGB(38, 38, 42)
        } else {
            Color::RGB(25, 25, 28)
        });
        sdl.fill_rect(layout.rail.scroll_up).ok();
        sdl.set_draw_color(if up_active {
            Color::RGB(60, 60, 65)
        } else {
            Color::RGB(40, 40, 45)
        });
        sdl.draw_rect(layout.rail.scroll_up).ok();
        render_text_centered(
            sdl,
            icon_font,
            &tc,
            cache,
            CHEVRON_UP,
            if up_active {
                Color::RGB(200, 200, 210)
            } else {
                Color::RGB(90, 90, 95)
            },
            layout.rail.scroll_up,
        );

        // Down arrow
        let max_scroll = (layout.rail.content_height - layout.rail.viewport_h).max(0);
        let down_active = layout.scroll < max_scroll;
        sdl.set_draw_color(if down_active {
            Color::RGB(38, 38, 42)
        } else {
            Color::RGB(25, 25, 28)
        });
        sdl.fill_rect(layout.rail.scroll_down).ok();
        sdl.set_draw_color(if down_active {
            Color::RGB(60, 60, 65)
        } else {
            Color::RGB(40, 40, 45)
        });
        sdl.draw_rect(layout.rail.scroll_down).ok();
        render_text_centered(
            sdl,
            icon_font,
            &tc,
            cache,
            CHEVRON_DOWN,
            if down_active {
                Color::RGB(200, 200, 210)
            } else {
                Color::RGB(90, 90, 95)
            },
            layout.rail.scroll_down,
        );
    }
}

/// Handle a click in the V2 left rail.
pub fn click_v2_toolbar(mx: i32, my: i32, layout: &mut V2Layout, state: &mut AppState) {
    let canvas_w = state.window_size.0 as i32;
    let canvas_h = state.window_size.1 as i32;

    // Scroll buttons (only if scrollable)
    if layout.rail.needs_scroll {
        if layout.rail.scroll_up.contains_point((mx, my)) {
            layout.scroll_by(canvas_w, canvas_h, -BTN_SIZE - BTN_GAP);
            return;
        }
        if layout.rail.scroll_down.contains_point((mx, my)) {
            layout.scroll_by(canvas_w, canvas_h, BTN_SIZE + BTN_GAP);
            return;
        }
    }

    // Color popout (open): check popout swatches, click outside closes
    if layout.popout_open {
        for (i, sr) in layout.rail.color_popout.iter().enumerate() {
            if sr.contains_point((mx, my)) {
                state.color = PALETTE[i];
                state.mark_snapshot_dirty();
                layout.popout_open = false;
                return;
            }
        }
        if !layout.rail.color_tile.contains_point((mx, my)) {
            layout.popout_open = false;
            return;
        }
    }

    // Tool buttons
    for (tool, rect) in &layout.rail.tool_btns {
        if rect.contains_point((mx, my)) {
            switch_tool(state, *tool);
            return;
        }
    }

    // Color tile
    if layout.rail.color_tile.contains_point((mx, my)) {
        layout.popout_open = !layout.popout_open;
        return;
    }

    // Brush controls
    if layout.rail.brush_minus.contains_point((mx, my)) {
        state.brush_size = state.brush_size.saturating_sub(4).max(1);
        state.tool_sizes[state.active_tool.index()] = state.brush_size;
        state.mark_snapshot_dirty();
    } else if layout.rail.brush_plus.contains_point((mx, my)) {
        state.brush_size = (state.brush_size + 4).min(200);
        state.tool_sizes[state.active_tool.index()] = state.brush_size;
        state.mark_snapshot_dirty();
    } else if layout.rail.freeze.contains_point((mx, my)) {
        state.freeze_frame = !state.freeze_frame;
        state.mark_snapshot_dirty();
    } else if layout.rail.screenshot.contains_point((mx, my)) {
        // Screenshot is handled in the main loop via a flag
        state.screenshot_requested = true;
    } else if layout.rail.change_input.contains_point((mx, my)) {
        state.show_device_picker = true;
        state.mark_snapshot_dirty();
    }
}
