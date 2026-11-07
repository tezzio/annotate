//! UI module — V2 side-rail toolbar.

use sdl2::{
    pixels::Color,
    rect::Rect,
    render::{Canvas as SdlCanvas, Texture, TextureCreator},
    ttf::Font,
    video::{Window, WindowContext},
};

use crate::capture::DeviceInfo;

/// Toolbar height — slightly shorter on narrow viewports to give the canvas more room.
/// 1280 px → 80 px
/// 1920+ px → 100 px
pub fn toolbar_height(w: i32) -> i32 {
    let min = 80f32;
    let base = 100f32;
    let t = ((w as f32 - 1280.0) / (1920.0 - 1280.0)).clamp(0.0, 1.0);
    (base - (base - min) * t) as i32
}

// ── V2: Modern side-rail UI ─────────────────────────────────────────────────

pub mod v2;

// ── Text cache (surface + GPU texture) ───────────────────────────────────────

/// Per-(font, text, color) cache of the rendered glyph surface and its GPU
/// `Texture`.
///
/// **TODO 1** in `docs/perf-todos.md`: previously only the surface was
/// cached; `tc.create_texture_from_surface()` was called every frame for
/// every label, recreating and destroying an `SDL_Texture` ~24× per frame.
/// That costs ~0.5–1.5 ms per call on RPi3's software-shader GLES2
/// fallback.  We now cache the GPU texture too — first call pays the
/// create cost; subsequent calls are a HashMap lookup.
///
/// # Lifetime story
///
/// SDL2's `Texture<'r>` borrows from the `TextureCreator` it was made from,
/// which is owned by the `Canvas<Window>` (and hence by the render loop's
/// stack).  We extend the lifetime to `'static` via `unsafe { mem::transmute }`.
/// This is the standard sdl2-rs pattern and is sound because:
///   1. SDL owns the underlying `SDL_Texture`; we don't free it.
///   2. `SDL_DestroyTexture` is called only when the `Texture` is dropped —
///      and we drop it only when the `TextCache` is dropped (program exit),
///      at which point the renderer is being torn down anyway.
///   3. The `TextureCreator` (which holds the renderer) outlives `TextCache`.
pub struct TextCache {
    surfaces: std::collections::HashMap<u64, sdl2::surface::Surface<'static>>,
    textures: std::collections::HashMap<u64, Texture<'static>>,
}

impl TextCache {
    pub fn new() -> Self {
        Self {
            surfaces: std::collections::HashMap::new(),
            textures: std::collections::HashMap::new(),
        }
    }

    fn key(font: &Font, text: &str, color: Color) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        (font as *const Font as usize).hash(&mut h);
        text.hash(&mut h);
        [color.r, color.g, color.b, color.a].hash(&mut h);
        h.finish()
    }

    /// Render the surface if not cached yet, returning a reference to it.
    /// Used as the source for [`Self::texture`] on a cache miss.
    fn surface<'a>(
        &'a mut self,
        font: &Font,
        text: &str,
        color: Color,
    ) -> &'a sdl2::surface::Surface<'static> {
        let k = Self::key(font, text, color);
        self.surfaces.entry(k).or_insert_with(|| {
            font.render(text).blended(color).unwrap_or_else(|_| {
                sdl2::surface::Surface::new(1, 1, sdl2::pixels::PixelFormatEnum::RGBA8888).unwrap()
            })
        })
    }

    /// Return a reference to the cached GPU `Texture` for
    /// `(font, text, color)`, creating it on first access.
    ///
    /// Pass the canvas's `TextureCreator`.  The returned `Texture` borrows
    /// for `'static` (see struct-level docs); the renderer must outlive
    /// the `TextCache`.
    pub fn texture<'a>(
        &'a mut self,
        tc: &'a TextureCreator<WindowContext>,
        font: &Font,
        text: &str,
        color: Color,
    ) -> Option<&'a Texture<'static>> {
        let k = Self::key(font, text, color);
        if !self.textures.contains_key(&k) {
            // Re-render the surface if missing too.  The surface cache is
            // separate from the texture cache because a future caller
            // might want raw CPU pixels (e.g. screenshot overlays); we
            // keep it warm.
            let surface = self.surface(font, text, color);
            match tc.create_texture_from_surface(surface) {
                Ok(t) => {
                    // SAFETY: see struct-level docs.  SDL owns the
                    // `SDL_Texture`; the renderer (held by `tc`) outlives
                    // this cache; we drop `t` exactly once at program exit.
                    let t: Texture<'static> = unsafe { std::mem::transmute(t) };
                    self.textures.insert(k, t);
                }
                Err(_) => return None,
            }
        }
        self.textures.get(&k)
    }
}

// ── FontAwesome 6 Solid codepoints ───────────────────────────────────────────

/// Stable &str codepoints for tool icons, indexed by [`Tool::index`].
/// Used to avoid per-frame `String` allocations in the V2 toolbar loop
/// (TODO 3 in perf-todos.md).  Replaces the older `tool_fa_icon` helper
/// which returned a `char` and required `.to_string()` per call.
pub const TOOL_ICONS: [&str; 10] = [
    "\u{f304}", // Pen
    "\u{f547}", // Line
    "\u{f0c8}", // Rect
    "\u{f111}", // Circle
    "\u{f061}", // Arrow
    "\u{f031}", // Text
    "\u{f245}", // Select
    "\u{f12d}", // Eraser
    "\u{f5c1}", // Highlight
    "\u{f140}", // Laser
];

// ── Menu bar ─────────────────────────────────────────────────────────────────

/// Actions the menu bar can return from a click
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    Undo,
    Redo,
    Clear,
    OpenSettings,
    SelectDevice(usize),
}

/// The thin menu bar at the top of the window.
pub struct MenuBar {
    pub open: bool,
    pub devices: Vec<DeviceInfo>,
    pub active_idx: usize,
    pub hostname: String,
}

impl MenuBar {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        let hostname = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_default()
            .trim()
            .to_string();
        let hostname = if hostname.is_empty() {
            "annotator".to_string()
        } else {
            hostname
        };
        Self {
            open: false,
            devices,
            active_idx: 0,
            hostname,
        }
    }

    pub fn entry_count(&self) -> usize {
        self.devices.len() + 1
    }

    pub fn entry_label(&self, idx: usize) -> String {
        if idx == 0 {
            "No input (black screen)".to_string()
        } else {
            let d = &self.devices[idx - 1];
            format!("{}  [{}]", d.name, d.path.display())
        }
    }

    pub fn menu_item_rect(&self, canvas_w: i32) -> Rect {
        Rect::new(canvas_w - 120, 0, 120, MENU_HEIGHT as u32)
    }

    pub fn undo_rect(&self) -> Rect {
        Rect::new(220, 0, 72, MENU_HEIGHT as u32)
    }
    pub fn redo_rect(&self) -> Rect {
        Rect::new(296, 0, 72, MENU_HEIGHT as u32)
    }
    pub fn clear_rect(&self) -> Rect {
        Rect::new(372, 0, 80, MENU_HEIGHT as u32)
    }
    pub fn settings_rect(&self) -> Rect {
        Rect::new(456, 0, 84, MENU_HEIGHT as u32)
    }

    pub fn fps_rect(&self, canvas_w: i32) -> Rect {
        Rect::new(canvas_w - 360, 0, 230, MENU_HEIGHT as u32)
    }

    fn dropdown_rect(&self, canvas_w: i32) -> Rect {
        let row_h = 36i32;
        let panel_w = 480i32;
        let panel_h = row_h * self.entry_count() as i32 + 8;
        Rect::new(
            canvas_w - panel_w,
            MENU_HEIGHT,
            panel_w as u32,
            panel_h as u32,
        )
    }

    /// Close button in the top-right of the open dropdown panel.
    fn dropdown_close_rect(&self, canvas_w: i32) -> Rect {
        let dr = self.dropdown_rect(canvas_w);
        Rect::new(dr.x + dr.width() as i32 - 32, dr.y + 4, 28, 28)
    }

    fn row_rect(&self, canvas_w: i32, idx: usize) -> Rect {
        let row_h = 36i32;
        let panel_w = 480i32;
        let dx = canvas_w - panel_w;
        Rect::new(
            dx + 4,
            MENU_HEIGHT + 4 + idx as i32 * row_h,
            (panel_w - 8) as u32,
            (row_h - 2) as u32,
        )
    }

    /// Draw the menu bar (and dropdown if open)
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        sdl: &mut SdlCanvas<Window>,
        font: &Font,
        canvas_w: i32,
        has_undo: bool,
        has_redo: bool,
        show_fps: bool,
        capture_fps: f32,
        display_fps: f32,
        cache: &mut TextCache,
    ) {
        sdl.set_draw_color(Color::RGB(0x14, 0x14, 0x14));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, MENU_HEIGHT as u32))
            .ok();
        sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
        sdl.draw_line((0, MENU_HEIGHT - 1), (canvas_w, MENU_HEIGHT - 1))
            .ok();

        let tc = sdl.texture_creator();

        // App title — left
        render_text(
            sdl,
            font,
            &tc,
            cache,
            &self.hostname,
            Color::RGB(120, 120, 120),
            Rect::new(12, 0, 220, MENU_HEIGHT as u32),
        );

        // FPS counter
        if show_fps {
            let fps_label = if capture_fps > 0.0 {
                format!("in {:.0}  disp {:.0}", capture_fps, display_fps)
            } else {
                format!("disp {:.0}", display_fps)
            };
            render_text(
                sdl,
                font,
                &tc,
                cache,
                &fps_label,
                Color::RGB(90, 180, 90),
                self.fps_rect(canvas_w),
            );
        }

        // "Input" menu button
        let item_rect = self.menu_item_rect(canvas_w);
        if self.open {
            sdl.set_draw_color(Color::RGB(0x2a, 0x2a, 0x2a));
            sdl.fill_rect(item_rect).ok();
        }
        let item_color = if self.open {
            Color::RGB(255, 255, 255)
        } else {
            Color::RGB(200, 200, 200)
        };
        render_text(sdl, font, &tc, cache, "Input  ▾", item_color, item_rect);

        // Undo / Redo / Clear / Settings
        let undo_r = self.undo_rect();
        let undo_col = if has_undo {
            Color::RGB(200, 200, 200)
        } else {
            Color::RGB(70, 70, 70)
        };
        render_text(sdl, font, &tc, cache, "↩ Undo", undo_col, undo_r);

        let redo_r = self.redo_rect();
        let redo_col = if has_redo {
            Color::RGB(200, 200, 200)
        } else {
            Color::RGB(70, 70, 70)
        };
        render_text(sdl, font, &tc, cache, "↪ Redo", redo_col, redo_r);

        let clear_r = self.clear_rect();
        render_text(
            sdl,
            font,
            &tc,
            cache,
            "✕ Clear",
            Color::RGB(200, 80, 80),
            clear_r,
        );

        let settings_r = self.settings_rect();
        render_text(
            sdl,
            font,
            &tc,
            cache,
            "⚙ Settings",
            Color::RGB(160, 160, 160),
            settings_r,
        );

        // Dropdown
        if self.open {
            let dr = self.dropdown_rect(canvas_w);
            sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
            sdl.fill_rect(dr).ok();
            sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
            sdl.draw_rect(dr).ok();

            // Close button (top-right of dropdown)
            let cr = self.dropdown_close_rect(canvas_w);
            sdl.set_draw_color(Color::RGB(60, 60, 60));
            sdl.fill_rect(cr).ok();
            sdl.set_draw_color(Color::RGB(0x55, 0x55, 0x55));
            sdl.draw_rect(cr).ok();
            render_text_centered(
                sdl,
                font,
                &tc,
                cache,
                "\u{2715}",
                Color::RGB(200, 200, 200),
                cr,
            );

            for i in 0..self.entry_count() {
                let rr = self.row_rect(canvas_w, i);
                let is_active = i == self.active_idx;
                if is_active {
                    sdl.set_draw_color(Color::RGB(0x1a, 0x3a, 0x1a));
                    sdl.fill_rect(rr).ok();
                }
                let bullet = if is_active { "● " } else { "  " };
                let label = format!("{}{}", bullet, self.entry_label(i));
                let color = if is_active {
                    Color::RGB(120, 220, 120)
                } else {
                    Color::RGB(200, 200, 200)
                };
                render_text(
                    sdl,
                    font,
                    &tc,
                    cache,
                    &label,
                    color,
                    Rect::new(rr.x + 8, rr.y, rr.width() - 8, rr.height()),
                );
            }
        }
    }

    /// Handle a mouse click in the top MENU_HEIGHT bar.
    pub fn click(&mut self, mx: i32, my: i32, canvas_w: i32) -> MenuAction {
        if my >= MENU_HEIGHT {
            if self.open {
                // Close button takes priority over row hits
                if self.dropdown_close_rect(canvas_w).contains_point((mx, my)) {
                    self.open = false;
                    return MenuAction::None;
                }
                for i in 0..self.entry_count() {
                    if self.row_rect(canvas_w, i).contains_point((mx, my)) {
                        self.open = false;
                        return MenuAction::SelectDevice(i);
                    }
                }
                self.open = false;
            }
            return MenuAction::None;
        }

        if self.menu_item_rect(canvas_w).contains_point((mx, my)) {
            self.open = !self.open;
        } else if self.undo_rect().contains_point((mx, my)) {
            if self.open {
                self.open = false;
            }
            return MenuAction::Undo;
        } else if self.redo_rect().contains_point((mx, my)) {
            if self.open {
                self.open = false;
            }
            return MenuAction::Redo;
        } else if self.clear_rect().contains_point((mx, my)) {
            if self.open {
                self.open = false;
            }
            return MenuAction::Clear;
        } else if self.settings_rect().contains_point((mx, my)) {
            if self.open {
                self.open = false;
            }
            return MenuAction::OpenSettings;
        } else if self.open {
            self.open = false;
        }
        MenuAction::None
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}

// ── Device picker modal ───────────────────────────────────────────────────────

#[derive(PartialEq)]
pub enum PickerStage {
    Devices,
    Modes,
}

/// What the header control (close X / back arrow) should do for a given
/// stage.  See [`DevicePicker::click_dismiss`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerDismiss {
    /// No header hit.
    None,
    /// Header was clicked on the top-level stage — close the modal.
    Close,
}

pub struct DevicePicker {
    pub devices: Vec<DeviceInfo>,
    pub device_cursor: usize,
    pub mode_cursor: usize,
    pub stage: PickerStage,
    pub scroll_offset: usize,
}

impl DevicePicker {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        Self {
            devices,
            device_cursor: 0,
            mode_cursor: 0,
            stage: PickerStage::Devices,
            scroll_offset: 0,
        }
    }

    /// Reset the picker to its initial Devices stage.  Used by both the
    /// close button and the keyboard dismiss path so reopening the picker
    /// doesn't land the user on a stale Modes screen.
    pub fn dismiss(&mut self) {
        self.stage = PickerStage::Devices;
        self.device_cursor = 0;
        self.mode_cursor = 0;
        self.scroll_offset = 0;
    }

    pub fn entry_count(&self) -> usize {
        self.devices.len() + 1
    } // +1 for "No input"

    pub fn label(&self, idx: usize) -> String {
        if idx == 0 {
            "No input  (black screen)".to_string()
        } else {
            let dev = &self.devices[idx - 1];
            format!("{}  [{}]", dev.name, dev.path.display())
        }
    }

    fn caps_label(&self, idx: usize) -> String {
        if idx == 0 {
            String::new()
        } else {
            self.devices[idx - 1].caps_summary.clone()
        }
    }

    pub fn scroll_to_cursor(&mut self, canvas_h: i32) {
        let row_h = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let avail_h = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let cursor = match self.stage {
            PickerStage::Devices => self.device_cursor,
            PickerStage::Modes => self.mode_cursor,
        };
        if cursor >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor + 1 - max_visible;
        }
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
                if self.mode_cursor + 1 < n {
                    self.mode_cursor += 1;
                }
            }
        }
    }

    /// Scroll the list by `delta` rows (positive = down).  Only moves the
    /// scroll offset; the cursor follows if it falls off-screen so the
    /// highlight stays visible.  Used for mouse-wheel and Page Up/Down.
    pub fn scroll_by(&mut self, canvas_h: i32, delta: i32) {
        let row_h = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let avail_h = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1);

        let (cursor_ref, total) = match self.stage {
            PickerStage::Devices => {
                let t = self.entry_count() as i32;
                (&mut self.device_cursor, t)
            }
            PickerStage::Modes => {
                let t = self.current_mode_count() as i32;
                (&mut self.mode_cursor, t)
            }
        };

        if total <= 0 {
            return;
        }

        // Move the cursor first by the wheel delta; that naturally also
        // advances the scroll window when the cursor hits the edge, which
        // is what users expect from a scroll wheel on a highlighted list.
        let new_cursor = (*cursor_ref as i32 + delta).clamp(0, total - 1);
        *cursor_ref = new_cursor as usize;

        // Snap scroll_offset so the cursor is always on screen.
        let max_v = max_visible.min(total) as usize;
        if *cursor_ref >= self.scroll_offset + max_v {
            self.scroll_offset = *cursor_ref + 1 - max_v;
        }
        if *cursor_ref < self.scroll_offset {
            self.scroll_offset = *cursor_ref;
        }
    }

    pub fn current_mode_count(&self) -> usize {
        if self.device_cursor == 0 {
            0
        } else {
            self.devices[self.device_cursor - 1].modes.len()
        }
    }

    pub fn enter(&mut self) -> Option<(usize, usize)> {
        match self.stage {
            PickerStage::Devices => {
                if self.device_cursor == 0 {
                    return Some((0, 0));
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

    /// Top-right close button in the panel header.
    pub fn close_rect(&self, canvas_w: i32, canvas_h: i32) -> Rect {
        let panel_w = 720i32;
        let row_h = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let avail_h = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let px = (canvas_w - panel_w) / 2;
        let visible = match self.stage {
            PickerStage::Devices => self.entry_count().min(max_visible),
            PickerStage::Modes => self.current_mode_count().min(max_visible),
        };
        let panel_h = header_h + row_h * visible as i32 + footer_h;
        let py = (canvas_h - panel_h) / 2;
        Rect::new(px + panel_w - 40, py + 8, 32, 32)
    }

    /// Result of a click on the header control.  The Devices/Modes stages
    /// have a true close button (dismisses the modal entirely); the Rtsp
    /// stage has a back button (returns to Devices, leaving the modal
    /// open).  In both cases the picker stage is reset to Devices via
    /// [`dismiss`] so reopening the modal lands on a clean screen.
    pub fn click_dismiss(
        &mut self,
        mx: i32,
        my: i32,
        canvas_w: i32,
        canvas_h: i32,
    ) -> PickerDismiss {
        if !self.close_rect(canvas_w, canvas_h).contains_point((mx, my)) {
            return PickerDismiss::None;
        }
        self.dismiss();
        PickerDismiss::Close
    }

    pub fn click_enter(
        &mut self,
        mx: i32,
        my: i32,
        canvas_w: i32,
        canvas_h: i32,
    ) -> Option<(usize, usize)> {
        let panel_w = 720i32;
        let row_h = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let avail_h = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let px = (canvas_w - panel_w) / 2;

        match self.stage {
            PickerStage::Devices => {
                let total = self.entry_count();
                let visible = total.min(max_visible);
                let scroll = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py = (canvas_h - panel_h) / 2;
                for vi in 0..visible {
                    let i = scroll + vi;
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
                let total = self.current_mode_count();
                let visible = total.min(max_visible);
                let scroll = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py = (canvas_h - panel_h) / 2;
                for vi in 0..visible {
                    let i = scroll + vi;
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

    pub fn draw(
        &self,
        sdl: &mut SdlCanvas<Window>,
        font: &Font,
        canvas_w: i32,
        canvas_h: i32,
        cache: &mut TextCache,
    ) {
        sdl.set_draw_color(Color::RGBA(0, 0, 0, 180));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, canvas_h as u32))
            .ok();

        let panel_w = 720i32;
        let row_h = 56i32;
        let header_h = 64i32;
        let footer_h = 36i32;
        let tc = sdl.texture_creator();

        let avail_h = canvas_h - 40;
        let max_visible = ((avail_h - header_h - footer_h) / row_h).max(1) as usize;
        let px = (canvas_w - panel_w) / 2;

        match self.stage {
            PickerStage::Devices => {
                let total = self.entry_count();
                let visible = total.min(max_visible);
                let scroll = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py = (canvas_h - panel_h) / 2;

                sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
                sdl.fill_rect(Rect::new(px, py, panel_w as u32, panel_h as u32))
                    .ok();
                sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
                sdl.draw_rect(Rect::new(px, py, panel_w as u32, panel_h as u32))
                    .ok();

                // Close button (top-right of header)
                let cr = self.close_rect(canvas_w, canvas_h);
                sdl.set_draw_color(Color::RGB(60, 60, 60));
                sdl.fill_rect(cr).ok();
                sdl.set_draw_color(Color::RGB(0x55, 0x55, 0x55));
                sdl.draw_rect(cr).ok();
                render_text_centered(
                    sdl,
                    font,
                    &tc,
                    cache,
                    "\u{2715}",
                    Color::RGB(200, 200, 200),
                    cr,
                );

                let title = if total > max_visible {
                    format!(
                        "Select capture device  ({}/{})",
                        self.device_cursor + 1,
                        total
                    )
                } else {
                    "Select capture device".to_string()
                };
                render_text(
                    sdl,
                    font,
                    &tc,
                    cache,
                    &title,
                    Color::RGB(220, 220, 220),
                    Rect::new(px + 16, py + 16, (panel_w - 32) as u32, 32),
                );

                if scroll > 0 {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "\u{25b2} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + 18, 70, 20),
                    );
                }
                if scroll + visible < total {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "\u{25bc} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + panel_h - footer_h + 8, 70, 20),
                    );
                }

                for vi in 0..visible {
                    let i = scroll + vi;
                    let ry = py + header_h + (vi as i32) * row_h;
                    let row_rect = Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32);
                    let is_sel = i == self.device_cursor;
                    if is_sel {
                        sdl.set_draw_color(Color::RGB(0x2a, 0x4a, 0x7a));
                        sdl.fill_rect(row_rect).ok();
                        sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                        sdl.draw_rect(row_rect).ok();
                    }
                    let name_col = if is_sel {
                        Color::RGB(255, 255, 255)
                    } else {
                        Color::RGB(180, 180, 180)
                    };
                    let caps_col = if is_sel {
                        Color::RGB(140, 210, 255)
                    } else {
                        Color::RGB(100, 140, 100)
                    };
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        &self.label(i),
                        name_col,
                        Rect::new(px + 20, ry + 4, (panel_w - 40) as u32, 22),
                    );
                    let caps = self.caps_label(i);
                    if !caps.is_empty() {
                        render_text(
                            sdl,
                            font,
                            &tc,
                            cache,
                            &caps,
                            caps_col,
                            Rect::new(px + 28, ry + 28, (panel_w - 48) as u32, 20),
                        );
                    }
                }
                render_text(
                    sdl,
                    font,
                    &tc,
                    cache,
                    "\u{2191}\u{2193} navigate   Enter to select   F2 to cancel",
                    Color::RGB(100, 100, 100),
                    Rect::new(
                        px + 16,
                        py + panel_h - footer_h + 8,
                        (panel_w - 32) as u32,
                        20,
                    ),
                );
            }

            PickerStage::Modes => {
                let dev = &self.devices[self.device_cursor - 1];
                let total = dev.modes.len();
                let visible = total.min(max_visible);
                let scroll = self.scroll_offset.min(total.saturating_sub(visible));
                let panel_h = header_h + row_h * visible as i32 + footer_h;
                let py = (canvas_h - panel_h) / 2;

                sdl.set_draw_color(Color::RGB(0x22, 0x22, 0x22));
                sdl.fill_rect(Rect::new(px, py, panel_w as u32, panel_h as u32))
                    .ok();
                sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
                sdl.draw_rect(Rect::new(px, py, panel_w as u32, panel_h as u32))
                    .ok();

                // Close button (top-right of header)
                let cr = self.close_rect(canvas_w, canvas_h);
                sdl.set_draw_color(Color::RGB(60, 60, 60));
                sdl.fill_rect(cr).ok();
                sdl.set_draw_color(Color::RGB(0x55, 0x55, 0x55));
                sdl.draw_rect(cr).ok();
                render_text_centered(
                    sdl,
                    font,
                    &tc,
                    cache,
                    "\u{2715}",
                    Color::RGB(200, 200, 200),
                    cr,
                );

                let title = if total > max_visible {
                    format!(
                        "Select format \u{2014} {}  ({}/{})",
                        dev.name,
                        self.mode_cursor + 1,
                        total
                    )
                } else {
                    format!("Select format \u{2014} {}", dev.name)
                };
                render_text(
                    sdl,
                    font,
                    &tc,
                    cache,
                    &title,
                    Color::RGB(220, 220, 220),
                    Rect::new(px + 16, py + 16, (panel_w - 32) as u32, 32),
                );

                if scroll > 0 {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "\u{25b2} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + 18, 70, 20),
                    );
                }
                if scroll + visible < total {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "\u{25bc} more",
                        Color::RGB(150, 150, 150),
                        Rect::new(px + panel_w - 80, py + panel_h - footer_h + 8, 70, 20),
                    );
                }

                for vi in 0..visible {
                    let i = scroll + vi;
                    let mode = &dev.modes[i];
                    let ry = py + header_h + (vi as i32) * row_h;
                    let row_rect = Rect::new(px + 8, ry, (panel_w - 16) as u32, (row_h - 4) as u32);
                    let is_sel = i == self.mode_cursor;
                    if is_sel {
                        sdl.set_draw_color(Color::RGB(0x2a, 0x4a, 0x7a));
                        sdl.fill_rect(row_rect).ok();
                        sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                        sdl.draw_rect(row_rect).ok();
                    }
                    let col = if is_sel {
                        Color::RGB(255, 255, 255)
                    } else {
                        Color::RGB(180, 180, 180)
                    };
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        &mode.label(),
                        col,
                        Rect::new(px + 20, ry + 14, (panel_w - 40) as u32, 24),
                    );
                }
                render_text(
                    sdl,
                    font,
                    &tc,
                    cache,
                    "\u{2191}\u{2193} navigate   Enter to confirm   Esc to go back",
                    Color::RGB(100, 100, 100),
                    Rect::new(
                        px + 16,
                        py + panel_h - footer_h + 8,
                        (panel_w - 32) as u32,
                        20,
                    ),
                );
            }
        }
    }
}

// ── Overlay settings panel ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Toggle {
    On,
    Off,
}

impl Toggle {
    pub fn next(&mut self) {
        *self = match self {
            Toggle::Off => Toggle::On,
            Toggle::On => Toggle::Off,
        };
    }
    pub fn as_bool(&self) -> bool {
        *self == Toggle::On
    }
    pub fn from_bool(b: bool) -> Self {
        if b { Toggle::On } else { Toggle::Off }
    }
}

pub struct OverlaySettings {
    pub cursor: usize,
    pub laser_fade: f32,
    pub laser_on: Toggle,
    pub show_fps: Toggle,
    pub fullscreen: Toggle,
    pub undo_limit: usize,
    /// Renderer name (e.g. "hardware (accelerated)" / "software (fallback)").
    /// Populated at startup for diagnostics; not currently rendered in
    /// the settings panel but kept so operators can `grep` it from logs.
    #[allow(dead_code)]
    pub renderer_info: String,
    pub scroll_offset: usize,
}

impl OverlaySettings {
    const ROW_H: i32 = 56;
    const PANEL_W: i32 = 560;
    const HEADER_H: i32 = 64;
    const FOOTER_H: i32 = 36;
    const SLIDER_MIN: f32 = 0.2;
    const SLIDER_MAX: f32 = 3.0;
    const UNDO_MIN: usize = 5;
    const UNDO_MAX: usize = 100;
    const COUNT: usize = 5;

    fn visible_rows(&self, canvas_h: i32) -> usize {
        let avail_h = canvas_h - 40;
        let max_vis = ((avail_h - Self::HEADER_H - Self::FOOTER_H) / Self::ROW_H).max(1) as usize;
        Self::COUNT.min(max_vis)
    }

    fn panel_h(&self, canvas_h: i32) -> i32 {
        Self::HEADER_H + Self::ROW_H * self.visible_rows(canvas_h) as i32 + Self::FOOTER_H
    }

    fn panel_rect(&self, canvas_w: i32, canvas_h: i32) -> Rect {
        let pw = Self::PANEL_W;
        let ph = self.panel_h(canvas_h);
        Rect::new(
            (canvas_w - pw) / 2,
            (canvas_h - ph) / 2,
            pw as u32,
            ph as u32,
        )
    }

    fn row_y(&self, canvas_h: i32, vi: usize) -> i32 {
        let ph = self.panel_h(canvas_h);
        let py = (canvas_h - ph) / 2;
        py + Self::HEADER_H + (vi as i32) * Self::ROW_H
    }

    pub fn move_down(&mut self, canvas_h: i32) {
        if self.cursor + 1 < Self::COUNT {
            self.cursor += 1;
            self.scroll_to_cursor(canvas_h);
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            if self.cursor < self.scroll_offset {
                self.scroll_offset = self.cursor;
            }
        }
    }

    fn scroll_to_cursor(&mut self, canvas_h: i32) {
        let vis = self.visible_rows(canvas_h);
        if self.cursor >= self.scroll_offset + vis {
            self.scroll_offset = self.cursor + 1 - vis;
        }
    }

    pub fn adjust(&mut self, right: bool) -> bool {
        match self.cursor {
            0 => {
                let step = 0.1;
                if right {
                    self.laser_fade = (self.laser_fade + step).min(Self::SLIDER_MAX);
                } else {
                    self.laser_fade = (self.laser_fade - step).max(Self::SLIDER_MIN);
                }
                self.laser_fade = (self.laser_fade * 10.0).round() / 10.0;
                true
            }
            1 => {
                self.laser_on.next();
                true
            }
            2 => {
                self.show_fps.next();
                true
            }
            3 => {
                let step = 5;
                if right {
                    self.undo_limit = self.undo_limit.saturating_add(step).min(Self::UNDO_MAX);
                } else {
                    self.undo_limit = self.undo_limit.saturating_sub(step).max(Self::UNDO_MIN);
                }
                true
            }
            4 => {
                self.fullscreen.next();
                true
            }
            _ => false,
        }
    }

    pub fn enter(&mut self) -> OverlayAction {
        match self.cursor {
            4 => OverlayAction::ToggleFullscreen,
            _ => OverlayAction::None,
        }
    }

    /// Top-right close button in the panel header.
    pub fn close_rect(&self, canvas_w: i32, canvas_h: i32) -> Rect {
        let pr = self.panel_rect(canvas_w, canvas_h);
        Rect::new(pr.x + pr.width() as i32 - 40, pr.y + 8, 32, 32)
    }

    pub fn click(&mut self, mx: i32, my: i32, canvas_w: i32, canvas_h: i32) -> OverlayAction {
        let pr = self.panel_rect(canvas_w, canvas_h);
        if !pr.contains_point((mx, my)) {
            return OverlayAction::Close;
        }
        if self.close_rect(canvas_w, canvas_h).contains_point((mx, my)) {
            return OverlayAction::Close;
        }

        if my < pr.y + Self::HEADER_H || my >= pr.y + pr.height() as i32 - Self::FOOTER_H {
            return OverlayAction::None;
        }

        let rel_y = my - (pr.y + Self::HEADER_H);
        let clicked_row = (rel_y / Self::ROW_H) as usize + self.scroll_offset;
        if clicked_row >= Self::COUNT {
            return OverlayAction::None;
        }

        self.cursor = clicked_row;
        self.scroll_to_cursor(canvas_h);

        let label_w = 200i32;
        let slider_x = pr.x + label_w;
        let slider_w = Self::PANEL_W - label_w - 16;
        if mx > slider_x + 40 {
            let in_right = mx > slider_x + slider_w / 2;
            self.adjust(in_right);
        }
        OverlayAction::None
    }

    pub fn draw(
        &self,
        sdl: &mut SdlCanvas<Window>,
        font: &Font,
        canvas_w: i32,
        canvas_h: i32,
        cache: &mut TextCache,
    ) {
        sdl.set_draw_color(Color::RGBA(0, 0, 0, 160));
        sdl.fill_rect(Rect::new(0, 0, canvas_w as u32, canvas_h as u32))
            .ok();

        let pr = self.panel_rect(canvas_w, canvas_h);
        let px = pr.x;
        let py = pr.y;
        let pw = pr.width() as i32;
        let vis = self.visible_rows(canvas_h);

        sdl.set_draw_color(Color::RGB(0x1c, 0x1c, 0x1c));
        sdl.fill_rect(pr).ok();
        sdl.set_draw_color(Color::RGB(0x44, 0x44, 0x44));
        sdl.draw_rect(pr).ok();

        let tc = sdl.texture_creator();

        // Header
        render_text(
            sdl,
            font,
            &tc,
            cache,
            "Settings",
            Color::RGB(220, 220, 220),
            Rect::new(px + 16, py + 16, (pw - 32) as u32, 32),
        );
        sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
        sdl.draw_line((px, py + Self::HEADER_H), (px + pw, py + Self::HEADER_H))
            .ok();

        // Close button (top-right of header)
        let cr = self.close_rect(canvas_w, canvas_h);
        sdl.set_draw_color(Color::RGB(60, 60, 60));
        sdl.fill_rect(cr).ok();
        sdl.set_draw_color(Color::RGB(0x55, 0x55, 0x55));
        sdl.draw_rect(cr).ok();
        render_text_centered(
            sdl,
            font,
            &tc,
            cache,
            "\u{2715}",
            Color::RGB(200, 200, 200),
            cr,
        );

        // Rows
        for vi in 0..vis {
            let i = self.scroll_offset + vi;
            let ry = self.row_y(canvas_h, vi);
            let is_sel = i == self.cursor;
            let row_rect = Rect::new(px + 8, ry, (pw - 16) as u32, (Self::ROW_H - 4) as u32);

            if is_sel {
                sdl.set_draw_color(Color::RGB(0x2a, 0x3a, 0x5a));
                sdl.fill_rect(row_rect).ok();
                sdl.set_draw_color(Color::RGB(0x4a, 0x9e, 0xff));
                sdl.draw_rect(row_rect).ok();
            }

            let label_col = if is_sel {
                Color::RGB(255, 255, 255)
            } else {
                Color::RGB(170, 170, 170)
            };
            let val_col = if is_sel {
                Color::RGB(140, 210, 255)
            } else {
                Color::RGB(120, 120, 120)
            };
            let label_x = px + 16;
            let val_x = px + 220;
            let val_w = pw - 220 - 16;

            match i {
                0 => {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "Laser fade",
                        label_col,
                        Rect::new(label_x, ry + 4, 200, 24),
                    );
                    let bar_x = val_x;
                    let bar_y = ry + 24;
                    let bar_w = val_w - 80;
                    let bar_h = 16;
                    let t = (self.laser_fade - Self::SLIDER_MIN)
                        / (Self::SLIDER_MAX - Self::SLIDER_MIN);
                    let fill_w = (t * bar_w as f32) as i32;
                    sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
                    sdl.fill_rect(Rect::new(bar_x, bar_y, bar_w as u32, bar_h as u32))
                        .ok();
                    sdl.set_draw_color(if is_sel {
                        Color::RGB(0x4a, 0x9e, 0xff)
                    } else {
                        Color::RGB(0x55, 0x55, 0x55)
                    });
                    sdl.fill_rect(Rect::new(bar_x, bar_y, fill_w as u32, bar_h as u32))
                        .ok();
                    let val_str = format!("{:.1}s", self.laser_fade);
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        &val_str,
                        val_col,
                        Rect::new(bar_x + bar_w + 4, bar_y - 4, 72, bar_h as u32 + 8),
                    );
                }
                1 => {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "Laser pointer",
                        label_col,
                        Rect::new(label_x, ry + 4, 200, 24),
                    );
                    let on_str = if self.laser_on == Toggle::On {
                        "ON"
                    } else {
                        "OFF"
                    };
                    let on_col = if self.laser_on == Toggle::On {
                        Color::RGB(80, 220, 80)
                    } else {
                        Color::RGB(180, 80, 80)
                    };
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        on_str,
                        on_col,
                        Rect::new(val_x, ry + 4, 80, 24),
                    );
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "use L key",
                        Color::RGB(90, 90, 90),
                        Rect::new(val_x, ry + 28, 200, 18),
                    );
                }
                2 => {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "Show FPS",
                        label_col,
                        Rect::new(label_x, ry + 4, 200, 24),
                    );
                    let on_str = if self.show_fps == Toggle::On {
                        "ON"
                    } else {
                        "OFF"
                    };
                    let on_col = if self.show_fps == Toggle::On {
                        Color::RGB(80, 220, 80)
                    } else {
                        Color::RGB(180, 80, 80)
                    };
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        on_str,
                        on_col,
                        Rect::new(val_x, ry + 4, 80, 24),
                    );
                }
                3 => {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "Undo depth",
                        label_col,
                        Rect::new(label_x, ry + 4, 200, 24),
                    );
                    let bar_x = val_x;
                    let bar_y = ry + 24;
                    let bar_w = val_w - 80;
                    let bar_h = 16;
                    let t = (self.undo_limit as f32 - Self::UNDO_MIN as f32)
                        / (Self::UNDO_MAX as f32 - Self::UNDO_MIN as f32);
                    let fill_w = (t * bar_w as f32) as i32;
                    sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
                    sdl.fill_rect(Rect::new(bar_x, bar_y, bar_w as u32, bar_h as u32))
                        .ok();
                    sdl.set_draw_color(if is_sel {
                        Color::RGB(0x4a, 0x9e, 0xff)
                    } else {
                        Color::RGB(0x55, 0x55, 0x55)
                    });
                    sdl.fill_rect(Rect::new(bar_x, bar_y, fill_w as u32, bar_h as u32))
                        .ok();
                    let val_str = self.undo_limit.to_string();
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        &val_str,
                        val_col,
                        Rect::new(bar_x + bar_w + 4, bar_y - 4, 72, bar_h as u32 + 8),
                    );
                }
                4 => {
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "Fullscreen",
                        label_col,
                        Rect::new(label_x, ry + 4, 200, 24),
                    );
                    let on_str = if self.fullscreen == Toggle::On {
                        "ON"
                    } else {
                        "OFF"
                    };
                    let on_col = if self.fullscreen == Toggle::On {
                        Color::RGB(80, 220, 80)
                    } else {
                        Color::RGB(180, 80, 80)
                    };
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        on_str,
                        on_col,
                        Rect::new(val_x, ry + 4, 80, 24),
                    );
                    render_text(
                        sdl,
                        font,
                        &tc,
                        cache,
                        "or press F",
                        Color::RGB(90, 90, 90),
                        Rect::new(val_x, ry + 28, 200, 18),
                    );
                }
                _ => {}
            }
        }

        // Footer
        sdl.set_draw_color(Color::RGB(0x33, 0x33, 0x33));
        let footer_y = py + pr.height() as i32 - Self::FOOTER_H;
        sdl.draw_line((px, footer_y), (px + pw, footer_y)).ok();
        render_text(
            sdl,
            font,
            &tc,
            cache,
            "\u{2191}\u{2193} navigate   \u{2190}\u{2192} adjust   Enter confirm   Esc close",
            Color::RGB(90, 90, 90),
            Rect::new(px + 16, footer_y + 8, (pw - 32) as u32, 20),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    None,
    Close,
    ToggleFullscreen,
}

// ── Text rendering helpers ────────────────────────────────────────────────────

/// Render text, vertically centred within `dst`, left-aligned.
pub fn render_text(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    tc: &TextureCreator<WindowContext>,
    cache: &mut TextCache,
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() {
        return;
    }
    let texture = match cache.texture(tc, font, text, color) {
        Some(t) => t,
        None => return,
    };
    let q = texture.query();
    let tw = q.width.min(dst.width()) as i32;
    let th = q.height.min(dst.height()) as i32;
    let src = Rect::new(0, 0, tw as u32, th as u32);
    let dest = Rect::new(
        dst.x + 4,
        dst.y + (dst.height() as i32 - th) / 2,
        tw as u32,
        th as u32,
    );
    sdl.copy(texture, src, dest).ok();
}

/// Render text horizontally and vertically centred within `dst` (for icon buttons).
pub fn render_text_centered(
    sdl: &mut SdlCanvas<Window>,
    font: &Font,
    tc: &TextureCreator<WindowContext>,
    cache: &mut TextCache,
    text: &str,
    color: Color,
    dst: Rect,
) {
    if text.is_empty() {
        return;
    }
    let texture = match cache.texture(tc, font, text, color) {
        Some(t) => t,
        None => return,
    };
    let q = texture.query();
    let tw = q.width.min(dst.width()) as i32;
    let th = q.height.min(dst.height()) as i32;
    let src = Rect::new(0, 0, tw as u32, th as u32);
    let dest = Rect::new(
        dst.x + (dst.width() as i32 - tw) / 2,
        dst.y + (dst.height() as i32 - th) / 2,
        tw as u32,
        th as u32,
    );
    sdl.copy(texture, src, dest).ok();
}

// ── UI trait ─────────────────────────────────────────────────────────────────

/// Menu bar height — shared by all UI variants.
pub const MENU_HEIGHT: i32 = 52;
