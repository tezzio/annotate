use sdl2::clipboard::ClipboardUtil;
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;
use std::sync::atomic::Ordering;

use crate::{
    canvas::Canvas,
    render_pipeline::DEBUG,
    scene::{DrawObject, HighlightStroke, PenStroke, ShapeObj},
    state::{AppState, EraserDrag},
    tools::{PALETTE, Tool},
    ui::{MENU_HEIGHT, PickerStage},
};

// ── Input event processor ─────────────────────────────────────────────────────

/// Process a single SDL event.  Returns `false` when the application should quit.
///
/// `clipboard` is used to support Ctrl+V paste in the RTSP URL field and the
/// text tool.  Pass `None` if the video subsystem isn't available (paste will
/// silently no-op).
pub fn process_event(
    event: &Event,
    state: &mut AppState,
    canvas: &mut Canvas,
    v2_layout: &mut crate::ui::v2::V2Layout,
    clipboard: Option<&ClipboardUtil>,
) -> bool {
    match event {
        // ── Quit ──────────────────────────────────────────────────────────────
        Event::Quit { .. } => return false,

        Event::KeyDown {
            keycode: Some(k),
            keymod,
            ..
        } => {
            // Track Shift / Ctrl modifier state for mouse events (which
            // don't carry modifier flags).  Only update on the physical
            // modifier keys themselves so the flag stays accurate across
            // focus changes.
            match *k {
                Keycode::LShift | Keycode::RShift => state.shift_down = true,
                _ => {}
            }
            if !handle_keydown(*k, *keymod, state, clipboard) {
                return false;
            }
            state.mark_snapshot_dirty();
        }

        Event::KeyUp {
            keycode: Some(k), ..
        } => {
            if matches!(*k, Keycode::LShift | Keycode::RShift) {
                state.shift_down = false;
            }
        }

        // SDL text input — used when the text tool is active
        Event::TextInput { text, .. } => {
            if state.active_tool == Tool::Text && state.text_input_active {
                state.text_buffer.push_str(text);
                state.mark_snapshot_dirty();
            }
        }

        // ── Mouse button down ─────────────────────────────────────────────────
        // Accept both real mouse and SDL-synthesized touch events (u32::MAX).
        // Finger events are NOT handled separately — SDL's auto touch→mouse
        // synthesis gives correct window-local pixels; raw FingerDown coords
        // are normalized to the full desktop and break on multi-monitor setups.
        Event::MouseButtonDown {
            mouse_btn: MouseButton::Left,
            x,
            y,
            ..
        } => {
            handle_press(*x, *y, state, canvas, v2_layout);
            state.mark_snapshot_dirty();
        }

        // ── Mouse motion ──────────────────────────────────────────────────────
        Event::MouseMotion { x, y, .. } => {
            handle_move(*x, *y, state, canvas);
        }

        // ── Mouse wheel — scroll the device/mode picker when it's open ───────
        // Without this, users who only see MJPEG modes in a 16-row window
        // have no way to reach the YUYV modes below them (key-only scrolling
        // is awkward once the list is taller than the panel).
        Event::MouseWheel { y, .. } if state.show_device_picker => {
            if let Some(picker) = &mut state.device_picker {
                // SDL2 reports y > 0 for wheel-up (away from user); invert
                // so positive `delta` scrolls DOWN — matching on-screen
                // direction.  Each tick = 3 rows for a snappier feel.
                let delta = -*y * 3;
                picker.scroll_by(state.window_size.1 as i32, delta);
                state.mark_snapshot_dirty();
            }
        }

        // ── Mouse button up ───────────────────────────────────────────────────
        Event::MouseButtonUp {
            mouse_btn: MouseButton::Left,
            x,
            y,
            ..
        } => {
            handle_release(*x, *y, state, canvas);
            state.mark_snapshot_dirty();
        }

        Event::Window {
            win_event: WindowEvent::Resized(w, h),
            ..
        }
        | Event::Window {
            win_event: WindowEvent::SizeChanged(w, h),
            ..
        } => {
            state.window_size = (*w as u32, *h as u32);
            state.mark_snapshot_dirty();
        }

        _ => {}
    }
    true
}

// ── Shared press / move / release handlers ────────────────────────────────────

fn handle_press(
    mx: i32,
    my: i32,
    state: &mut AppState,
    canvas: &Canvas,
    v2_layout: &mut crate::ui::v2::V2Layout,
) {
    // Settings overlay is open — all clicks go to the overlay, not toolbar/picker
    if state.show_settings_overlay {
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] press({mx},{my}) consumed by settings overlay");
        }
        return;
    }

    if state.show_device_picker {
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] press({mx},{my}) consumed by device picker");
        }
        if let Some(picker) = &mut state.device_picker {
            use crate::ui::PickerDismiss;
            match picker.click_dismiss(mx, my, canvas.width as i32, canvas.height as i32) {
                PickerDismiss::Close => {
                    state.show_device_picker = false;
                    return;
                }
                PickerDismiss::None => {}
            }
            if let Some(result) =
                picker.click_enter(mx, my, canvas.width as i32, canvas.height as i32)
            {
                state.picker_confirmed = Some(result);
                state.show_device_picker = false;
            }
        }
        return;
    }
    if my < MENU_HEIGHT {
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] press({mx},{my}) consumed by menu bar (my<{MENU_HEIGHT})");
        }
        state.pending_menu_click = Some((mx, my));
        return;
    }
    state.menu_close_requested = true;

    // V2: clicks in the left rail are toolbar clicks. If the popout is open,
    // the clickable area extends to the right of the rail (for swatches).
    let popout_extends_right = v2_layout.popout_open
        && (crate::ui::v2::RAIL_WIDTH..=crate::ui::v2::RAIL_WIDTH + 6 + 32 + 8).contains(&mx); // rail edge + gap + swatch + margin
    if mx < crate::ui::v2::RAIL_WIDTH || popout_extends_right {
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] press({mx},{my}) consumed by V2 toolbar");
        }
        crate::ui::v2::click_v2_toolbar(mx, my, v2_layout, state);
        return;
    }
    if state
        .draw_locked_until
        .is_some_and(|t| std::time::Instant::now() < t)
    {
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] press({mx},{my}) suppressed by draw_locked_until");
        }
        return;
    }
    state.mouse_down = true;
    state.drag_start = (mx, my);
    state.drag_cur = (mx, my);
    match state.active_tool {
        Tool::Select => {
            let hit = state.scene.hit_test(mx, my);
            let shift = state.shift_down;
            if let Some(idx) = hit {
                // Click on an object.
                if shift {
                    // Shift-click: toggle this object in/out of the
                    // current selection.  No drag-move, no undo snapshot.
                    state.scene.select_toggle(idx);
                    state.pending_select_undo = false;
                    state.marquee = None;
                } else if state.scene.is_selected(idx) {
                    // Plain-click on an already-selected object: keep the
                    // selection as-is and let the drag-move path below
                    // translate every selected object together.
                    state.scene.push_undo();
                    state.pending_select_undo = true;
                    state.marquee = None;
                } else {
                    // Plain-click on an unselected object: replace the
                    // selection with just this one, then start a drag.
                    state.scene.select_one(idx);
                    state.scene.push_undo();
                    state.pending_select_undo = true;
                    state.marquee = None;
                }
            } else {
                // Click on empty canvas: start a marquee selection from
                // this point.  Shift extends an existing selection (add
                // without clearing); plain replaces it.
                if !shift {
                    state.scene.select_clear();
                }
                state.marquee = Some((mx, my, mx, my));
                state.pending_select_undo = false;
            }
        }
        Tool::Text => {
            commit_text(state);
            state.text_pos = (mx, my);
            state.text_buffer.clear();
            state.text_input_active = true;
            sdl2::hint::set("SDL_IME_SHOW_UI", "1");
        }
        Tool::Pen => {
            state.scene.push_undo();
            // Start the stroke in state.active_stroke instead of the scene,
            // so the committed overlay isn't re-rasterized on every mouse-move.
            state.active_stroke = Some(DrawObject::Pen(PenStroke {
                points: vec![(mx, my)],
                color: state.color,
                size: state.brush_size,
            }));
            if DEBUG.load(Ordering::Relaxed) {
                eprintln!(
                    "[dbg] pen_press: ({},{}) color={:?} brush={} scene.objects={}",
                    mx,
                    my,
                    state.color,
                    state.brush_size,
                    state.scene.objects().len(),
                );
            }
        }
        Tool::Eraser => {
            // Eraser is NOT a drawable scene object.  On mouse-down we just
            // snapshot the current scene for undo and start tracking the
            // erase circles in `state.eraser_drag`.  On mouse-up the canvas
            // gets the alpha-zeroed circles and then we bake the result.
            state.scene.push_undo();
            state.eraser_drag = Some(EraserDrag {
                points: vec![(mx, my)],
                radius: state.brush_size,
            });
        }
        Tool::Highlight => {
            state.scene.push_undo();
            state.active_stroke = Some(DrawObject::Highlight(HighlightStroke {
                points: vec![(mx, my)],
                color: state.color,
                size: state.brush_size,
            }));
            if DEBUG.load(Ordering::Relaxed) {
                eprintln!(
                    "[dbg] highlight_press: ({},{}) color={:?} brush={} scene.objects={}",
                    mx,
                    my,
                    state.color,
                    state.brush_size,
                    state.scene.objects().len(),
                );
            }
        }
        Tool::Laser => {
            state
                .laser_trail
                .push((mx, my, std::time::Instant::now(), state.color));
        }
        // Shape tools: push undo here; object committed on release
        _ => {
            state.scene.push_undo();
        }
    }
}

fn handle_move(mx: i32, my: i32, state: &mut AppState, canvas: &mut Canvas) {
    if state.show_device_picker {
        return;
    }
    if my < MENU_HEIGHT || mx < crate::ui::v2::RAIL_WIDTH {
        return;
    }
    // Laser pointer tracks hover — no mouse button required
    if state.active_tool == Tool::Laser {
        state
            .laser_trail
            .push((mx, my, std::time::Instant::now(), state.color));
        return;
    }
    if !state.mouse_down {
        return;
    }
    let prev = state.drag_cur;
    state.drag_cur = (mx, my);
    let (dx, dy) = (mx - prev.0, my - prev.1);
    match state.active_tool {
        Tool::Select => {
            // If a marquee is in progress, just update the rect and let
            // the render loop draw the live outline.  Otherwise translate
            // every selected object together.
            if let Some((x0, y0, _, _)) = state.marquee {
                state.marquee = Some((x0, y0, mx, my));
            } else if !state.scene.selected.is_empty() {
                let n = state.scene.objects().len();
                // Snapshot indices up front so a translate that grows the
                // objects vec (e.g. via make_mut) doesn't invalidate our
                // iterators.  We also clamp to current length defensively.
                let indices: Vec<usize> = state
                    .scene
                    .selected
                    .iter()
                    .copied()
                    .filter(|&i| i < n)
                    .collect();
                if !indices.is_empty() {
                    for i in indices {
                        if let Some(obj) = state.scene.objects_mut().get_mut(i) {
                            obj.translate(dx, dy);
                        }
                    }
                    state.scene.dirty = true;
                }
            }
        }
        Tool::Pen => {
            if let Some(DrawObject::Pen(s)) = state.active_stroke.as_mut() {
                s.points.push((mx, my));
                if DEBUG.load(Ordering::Relaxed) && s.points.len() % 16 == 0 {
                    eprintln!("[dbg] pen_move: stroke now has {} points", s.points.len());
                }
            } else if DEBUG.load(Ordering::Relaxed) && state.mouse_down {
                eprintln!("[dbg] pen_move: mouse_down but NO active_stroke!");
            }
        }
        Tool::Eraser => {
            if let Some(drag) = state.eraser_drag.as_mut() {
                drag.points.push((mx, my));
                // Live wipe: zero the alpha disc on `committed_canvas` right
                // now so the user sees the erasure happen as the brush moves.
                // The render loop uploads `committed_canvas` to the GPU
                // texture each frame, gated by `committed_canvas.dirty`,
                // but uses a partial-rect upload driven by the bbox in
                // `state.eraser_dirty_*` — so each drag point only uploads
                // a few KB of GPU texture data, not the full 8.3 MB
                // (TODO 5 in perf-todos.md flagged full uploads as the
                // main per-frame cost).  The scene-mutation + baked_bg
                // update happens in `Scene::erase_path` on release.
                let r = drag.radius as i32;
                if r > 0 {
                    let r2 = r * r;
                    let w = canvas.width as i32;
                    let h = canvas.height as i32;
                    let xa = (mx - r).max(0);
                    let xb = (mx + r).min(w - 1);
                    let ya = (my - r).max(0);
                    let yb = (my + r).min(h - 1);
                    for y in ya..=yb {
                        let dy = y - my;
                        let row_base = (y as u32 * canvas.width) as usize * 4;
                        for x in xa..=xb {
                            let dx = x - mx;
                            if dx * dx + dy * dy > r2 {
                                continue;
                            }
                            let idx = row_base + x as usize * 4;
                            canvas.pixels[idx + 3] = 0;
                        }
                    }
                    canvas.dirty = true;
                    // Expand the partial-upload bbox so the render loop
                    // uploads exactly the wiped region (with a 1-pixel
                    // pad so bilinear sampling doesn't blend the edge
                    // with the pre-wipe texel above/below the rect).
                    let pad = 1i32;
                    let xa_p = (xa - pad).max(0);
                    let ya_p = (ya - pad).max(0);
                    let xb_p = (xb + pad + 1).min(w);
                    let yb_p = (yb + pad + 1).min(h);
                    let (min_x, min_y) = state.eraser_dirty_min;
                    let (max_x, max_y) = state.eraser_dirty_max;
                    state.eraser_dirty_min = (min_x.min(xa_p), min_y.min(ya_p));
                    state.eraser_dirty_max = (max_x.max(xb_p), max_y.max(yb_p));
                    state.mark_snapshot_dirty();
                }
            }
        }
        Tool::Highlight => {
            if let Some(DrawObject::Highlight(h)) = state.active_stroke.as_mut() {
                h.points.push((mx, my));
                if DEBUG.load(Ordering::Relaxed) && h.points.len() % 16 == 0 {
                    eprintln!(
                        "[dbg] highlight_move: stroke now has {} points",
                        h.points.len()
                    );
                }
            }
        }
        // shape tools: preview is drawn by main.rs via preview_object()
        _ => {}
    }
}

fn handle_release(x1: i32, y1: i32, state: &mut AppState, canvas: &mut Canvas) {
    if !state.mouse_down {
        return;
    }
    state.mouse_down = false;
    let (x0, y0) = state.drag_start;
    let s = ShapeObj {
        x0,
        y0,
        x1,
        y1,
        color: state.color,
        size: state.brush_size,
    };
    match state.active_tool {
        Tool::Line => {
            state.scene.add(DrawObject::Line(s));
        }
        Tool::Rect => {
            state.scene.add(DrawObject::Rect(s));
        }
        Tool::Circle => {
            state.scene.add(DrawObject::Circle(s));
        }
        Tool::Arrow => {
            state.scene.add(DrawObject::Arrow(s));
        }
        // Pen/Highlight: commit the active stroke into the scene
        Tool::Pen | Tool::Highlight => {
            if let Some(stroke) = state.active_stroke.take() {
                let pts = match &stroke {
                    DrawObject::Pen(p) => p.points.len(),
                    DrawObject::Highlight(h) => h.points.len(),
                    _ => 0,
                };
                let obj_count_before = state.scene.objects().len();
                state.scene.add(stroke);
                if DEBUG.load(Ordering::Relaxed) {
                    eprintln!(
                        "[dbg] pen_release: pts={} scene.objects: {} -> {} scene.dirty={}",
                        pts,
                        obj_count_before,
                        state.scene.objects().len(),
                        state.scene.dirty,
                    );
                }
            } else if DEBUG.load(Ordering::Relaxed) {
                eprintln!(
                    "[dbg] pen_release: NO active_stroke (tool={:?})",
                    state.active_tool,
                );
            }
        }
        // Eraser (Photoshop-style layer-aware): the brush already wiped the
        // visible pixels in `canvas` during handle_move (live preview), so
        // `Scene::erase_path` just needs to commit that wipe into the
        // scene's persistent state — trim live strokes that the brush
        // crossed, re-bake the scene into a fresh baked_bg, and zero the
        // remaining brush pixels on the bake so non-live pixels wipe
        // through to transparency too.
        Tool::Eraser => {
            if let Some(drag) = state.eraser_drag.take()
                && !drag.points.is_empty()
            {
                state.scene.erase_path(&drag.points, drag.radius, canvas);
            }
        }
        Tool::Select => {
            // If a marquee was in progress, commit it: add every object
            // whose bounds overlap the rect to the selection.
            if let Some((x0, y0, x1, y1)) = state.marquee.take() {
                let added = state.scene.select_add_in_rect(x0, y0, x1, y1);
                if !added.is_empty() {
                    state.mark_snapshot_dirty();
                }
                state.pending_select_undo = false;
            } else if state.pending_select_undo {
                state.pending_select_undo = false;
                // If the object(s) didn't actually move, discard the undo
                // snapshot.  Multi-select: pop one entry per object that
                // got its own COW Arc — in practice it's one snapshot per
                // press, so one pop.
                if state.drag_start == state.drag_cur {
                    state.scene.undo_stack.pop_back();
                }
            }
        }
        _ => {}
    }
}

// `apply_erase_to_canvas` was deleted — `Scene::erase_path` now does the
// pixel-zero on baked_bg internally as part of the layer-aware flow.

// ── Keyboard handler ──────────────────────────────────────────────────────────

fn handle_keydown(
    k: Keycode,
    mods: Mod,
    state: &mut AppState,
    clipboard: Option<&ClipboardUtil>,
) -> bool {
    // Settings overlay is open — defer all keys to the overlay handler in main.rs
    if state.show_settings_overlay {
        return true;
    }

    let ctrl = mods.contains(Mod::LCTRLMOD) || mods.contains(Mod::RCTRLMOD);
    let shift = mods.contains(Mod::LSHIFTMOD) || mods.contains(Mod::RSHIFTMOD);

    // Text input mode intercepts most keys
    if state.text_input_active && state.active_tool == Tool::Text {
        match k {
            Keycode::Return | Keycode::KpEnter => {
                commit_text(state);
            }
            Keycode::Escape => {
                state.text_buffer.clear();
                state.text_input_active = false;
            }
            Keycode::Backspace => {
                state.text_buffer.pop();
            }
            Keycode::V if ctrl => {
                if let Some(cb) = clipboard
                    && let Ok(text) = cb.clipboard_text()
                {
                    // Strip newlines — single-line text tool.
                    let cleaned: String =
                        text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                    state.text_buffer.push_str(&cleaned);
                }
            }
            // ── Select-all shortcut for multi-select ────────────────────────
            Keycode::A if ctrl && state.active_tool == Tool::Select => {
                // Select every live object.  No undo snapshot — this is
                // a pure selection change, no scene mutation.
                let n = state.scene.objects().len();
                let mut sel: Vec<usize> = (0..n).collect();
                state.scene.selected.clear();
                state.scene.selected.append(&mut sel);
                state.mark_snapshot_dirty();
            }
            // Outside Select, Ctrl+A is intentionally unbound (the
            // existing `Keycode::A` (without Ctrl) below still switches
            // to the Arrow tool).
            _ => {}
        }
        return true;
    }

    if state.show_device_picker {
        if let Some(picker) = &mut state.device_picker {
            match k {
                Keycode::Up => {
                    picker.move_up();
                    picker.scroll_to_cursor(state.window_size.1 as i32);
                }
                Keycode::Down => {
                    picker.move_down();
                    picker.scroll_to_cursor(state.window_size.1 as i32);
                }
                Keycode::PageUp => {
                    // Page Up = wheel-up * a page: a comfortable jump without
                    // pulling the cursor off the list.  8 rows = roughly half
                    // a typical 1080p panel.
                    picker.scroll_by(state.window_size.1 as i32, -8);
                }
                Keycode::PageDown => {
                    picker.scroll_by(state.window_size.1 as i32, 8);
                }
                Keycode::Home => {
                    // Jump to first row (matches browser/OS list convention).
                    match picker.stage {
                        PickerStage::Devices => picker.device_cursor = 0,
                        PickerStage::Modes => picker.mode_cursor = 0,
                    }
                    picker.scroll_to_cursor(state.window_size.1 as i32);
                }
                Keycode::End => {
                    // Jump to last row.
                    match picker.stage {
                        PickerStage::Devices => {
                            picker.device_cursor = picker.entry_count().saturating_sub(1);
                        }
                        PickerStage::Modes => {
                            picker.mode_cursor = picker.current_mode_count().saturating_sub(1);
                        }
                    }
                    picker.scroll_to_cursor(state.window_size.1 as i32);
                }
                Keycode::Return | Keycode::KpEnter => {
                    if let Some(result) = picker.enter() {
                        state.picker_confirmed = Some(result);
                        state.show_device_picker = false;
                    }
                }
                Keycode::Escape | Keycode::F2 => {
                    if picker.stage == PickerStage::Devices {
                        state.show_device_picker = false;
                    } else {
                        // Modes: go back to Devices without closing the
                        // modal.  `dismiss()` resets the cursors so the
                        // next open is clean.
                        picker.dismiss();
                    }
                }
                _ => {}
            }
        }
        return true;
    }

    match k {
        // ── Quit ─────────────────────────────────────────────────────────────
        Keycode::Escape => return false,

        // ── Tool selection ────────────────────────────────────────────────────
        Keycode::P => {
            commit_text(state);
            switch_tool(state, Tool::Pen);
        }
        Keycode::L => {
            commit_text(state);
            switch_tool(state, Tool::Line);
        }
        Keycode::R => {
            commit_text(state);
            switch_tool(state, Tool::Rect);
        }
        Keycode::C => {
            commit_text(state);
            switch_tool(state, Tool::Circle);
        }
        Keycode::A => {
            commit_text(state);
            switch_tool(state, Tool::Arrow);
        }
        Keycode::T => {
            commit_text(state);
            switch_tool(state, Tool::Text);
        }
        Keycode::S => {
            commit_text(state);
            switch_tool(state, Tool::Select);
        }
        Keycode::E => {
            commit_text(state);
            switch_tool(state, Tool::Eraser);
        }
        Keycode::H => {
            commit_text(state);
            switch_tool(state, Tool::Highlight);
        }

        // ── Colour palette ────────────────────────────────────────────────────
        Keycode::Num1 => state.color = PALETTE[0],
        Keycode::Num2 => state.color = PALETTE[1],
        Keycode::Num3 => state.color = PALETTE[2],
        Keycode::Num4 => state.color = PALETTE[3],
        Keycode::Num5 => state.color = PALETTE[4],
        Keycode::Num6 => state.color = PALETTE[5],
        Keycode::Num7 => state.color = PALETTE[6],
        Keycode::Num8 => state.color = PALETTE[7],

        // ── Brush size ────────────────────────────────────────────────────────
        Keycode::LeftBracket => {
            state.brush_size = state.brush_size.saturating_sub(4).max(1);
            state.tool_sizes[state.active_tool.index()] = state.brush_size;
        }
        Keycode::RightBracket => {
            state.brush_size = (state.brush_size + 4).min(200);
            state.tool_sizes[state.active_tool.index()] = state.brush_size;
        }

        // ── Undo / Redo ───────────────────────────────────────────────────────
        Keycode::Z if ctrl && shift => {
            state.scene.redo();
            state.draw_locked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }
        Keycode::Y if ctrl => {
            state.scene.redo();
            state.draw_locked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }
        Keycode::Z if ctrl => {
            state.scene.undo();
            state.draw_locked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }

        // ── Clear / Delete selected ───────────────────────────────────────────
        Keycode::Delete | Keycode::Backspace => {
            if state.active_tool == Tool::Select {
                if !state.scene.selected.is_empty() {
                    state.scene.push_undo();
                    // Sort indices high-to-low so removal doesn't shift
                    // the indices we still want to delete.  `swap_remove`
                    // also avoids the O(n) shift of `remove`; the post-
                    // deletion ordering doesn't matter because we clear
                    // the selection anyway.
                    let mut indices = state.scene.selected.clone();
                    indices.sort_unstable_by(|a, b| b.cmp(a));
                    state.scene.selected.clear();
                    for idx in indices {
                        if idx < state.scene.objects().len() {
                            state.scene.objects_mut().swap_remove(idx);
                        }
                    }
                    state.scene.dirty = true;
                }
            } else {
                state.scene.clear();
            }
        }

        // ── Fullscreen toggle ─────────────────────────────────────────────────
        Keycode::F => state.toggle_fullscreen = true,

        // ── Device picker ─────────────────────────────────────────────────────
        Keycode::F2 => state.show_device_picker = true,

        _ => {}
    }
    true
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Switch to a different tool, saving the current brush size and restoring
/// the new tool's remembered size.
pub fn switch_tool(state: &mut AppState, tool: Tool) {
    state.tool_sizes[state.active_tool.index()] = state.brush_size;
    state.active_tool = tool;
    state.brush_size = state.tool_sizes[tool.index()];
    state.mark_snapshot_dirty();
}

fn commit_text(state: &mut AppState) {
    if !state.text_buffer.is_empty() {
        state.pending_text_commit = true;
    }
    state.text_input_active = false;
    state.mark_snapshot_dirty();
}

/// Returns a live preview `DrawObject` for the current shape drag, if any.
/// main.rs calls this to render the ghost shape on top of the committed scene.
pub fn preview_object(state: &AppState) -> Option<DrawObject> {
    if !state.mouse_down {
        return None;
    }
    let (x0, y0) = state.drag_start;
    let (x1, y1) = state.drag_cur;
    let s = ShapeObj {
        x0,
        y0,
        x1,
        y1,
        color: state.color,
        size: state.brush_size,
    };
    match state.active_tool {
        Tool::Line => Some(DrawObject::Line(s)),
        Tool::Rect => Some(DrawObject::Rect(s)),
        Tool::Circle => Some(DrawObject::Circle(s)),
        Tool::Arrow => Some(DrawObject::Arrow(s)),
        _ => None,
    }
}
