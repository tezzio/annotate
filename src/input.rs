use sdl2::clipboard::ClipboardUtil;
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;

use crate::{
    canvas::Canvas,
    scene::{DrawObject, PenStroke, ShapeObj, HighlightStroke},
    state::{AppState, EraserDrag},
    tools::{Tool, PALETTE},
    ui::{PickerStage, MENU_HEIGHT},
};

// ── Input event processor ─────────────────────────────────────────────────────

/// Process a single SDL event.  Returns `false` when the application should quit.
///
/// `clipboard` is used to support Ctrl+V paste in the RTSP URL field and the
/// text tool.  Pass `None` if the video subsystem isn't available (paste will
/// silently no-op).
pub fn process_event(
    event:     &Event,
    state:     &mut AppState,
    canvas:    &mut Canvas,
    v2_layout: &mut crate::ui::v2::V2Layout,
    clipboard: Option<&ClipboardUtil>,
) -> bool {
    match event {
        // ── Quit ──────────────────────────────────────────────────────────────
        Event::Quit { .. } => return false,

        Event::KeyDown { keycode: Some(k), keymod, .. } => {
            if !handle_keydown(*k, *keymod, state, clipboard) {
                return false;
            }
        }

        // SDL text input — used when text tool is active or RTSP picker is open
        Event::TextInput { text, .. } => {
            if state.active_tool == Tool::Text && state.text_input_active {
                state.text_buffer.push_str(text);
            } else if state.show_device_picker {
                if let Some(picker) = &mut state.device_picker {
                    if picker.stage == PickerStage::Rtsp {
                        picker.rtsp.url.push_str(text);
                    }
                }
            }
        }

        // ── Mouse button down ─────────────────────────────────────────────────
        // Accept both real mouse and SDL-synthesized touch events (u32::MAX).
        // Finger events are NOT handled separately — SDL's auto touch→mouse
        // synthesis gives correct window-local pixels; raw FingerDown coords
        // are normalized to the full desktop and break on multi-monitor setups.
        Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
            handle_press(*x, *y, state, canvas, v2_layout);
        }

        // ── Mouse motion ──────────────────────────────────────────────────────
        Event::MouseMotion { x, y, .. } => {
            handle_move(*x, *y, state, canvas);
        }

        // ── Mouse button up ───────────────────────────────────────────────────
        Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
            handle_release(*x, *y, state, canvas);
        }

        Event::Window { win_event: WindowEvent::Resized(w, h), .. }
        | Event::Window { win_event: WindowEvent::SizeChanged(w, h), .. } => {
            state.window_size = (*w as u32, *h as u32);
        }

        _ => {}
    }
    true
}

// ── Shared press / move / release handlers ────────────────────────────────────

fn handle_press(mx: i32, my: i32, state: &mut AppState, canvas: &Canvas, v2_layout: &mut crate::ui::v2::V2Layout) {
    // Settings overlay is open — all clicks go to the overlay, not toolbar/picker
    if state.show_settings_overlay {
        return;
    }

    if state.show_device_picker {
        if let Some(picker) = &mut state.device_picker {
            use crate::ui::PickerDismiss;
            match picker.click_dismiss(mx, my, canvas.width as i32, canvas.height as i32) {
                PickerDismiss::Close => {
                    state.show_device_picker = false;
                    return;
                }
                PickerDismiss::Back => {
                    // Stage already reset to Devices by click_dismiss; leave modal open.
                    return;
                }
                PickerDismiss::None => {}
            }
            if let Some(result) = picker.click_enter(mx, my, canvas.width as i32, canvas.height as i32) {
                state.picker_confirmed = Some(result);
                state.show_device_picker = false;
            }
        }
        return;
    }
    if my < MENU_HEIGHT {
        state.pending_menu_click = Some((mx, my));
        return;
    }
    state.menu_close_requested = true;

    // V2: clicks in the left rail are toolbar clicks. If the popout is open,
    // the clickable area extends to the right of the rail (for swatches).
    let popout_extends_right = v2_layout.popout_open
        && mx >= crate::ui::v2::RAIL_WIDTH
        && mx <= crate::ui::v2::RAIL_WIDTH + 6 + 32 + 8;  // rail edge + gap + swatch + margin
    if mx < crate::ui::v2::RAIL_WIDTH || popout_extends_right {
        crate::ui::v2::click_v2_toolbar(mx, my, v2_layout, state);
        return;
    }
    if state.draw_locked_until.map_or(false, |t| std::time::Instant::now() < t) { return; }
    state.mouse_down = true;
    state.drag_start = (mx, my);
    state.drag_cur   = (mx, my);
    match state.active_tool {
        Tool::Select => {
            let hit = state.scene.hit_test(mx, my);
            state.scene.selected = hit;
            if hit.is_some() {
                // Snapshot before potential move so it can be undone
                state.scene.push_undo();
                state.pending_select_undo = true;
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
                color:  state.color,
                size:   state.brush_size,
            }));
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
                color:  state.color,
                size:   state.brush_size,
            }));
        }
        Tool::Laser => {
            state.laser_trail.push((mx, my, std::time::Instant::now(), state.color));
        }
        // Shape tools: push undo here; object committed on release
        _ => {
            state.scene.push_undo();
        }
    }
}

fn handle_move(mx: i32, my: i32, state: &mut AppState, _canvas: &Canvas) {
    if state.show_device_picker { return; }
    if my < MENU_HEIGHT || mx < crate::ui::v2::RAIL_WIDTH { return; }
    // Laser pointer tracks hover — no mouse button required
    if state.active_tool == Tool::Laser {
        state.laser_trail.push((mx, my, std::time::Instant::now(), state.color));
        return;
    }
    if !state.mouse_down { return; }
    let prev = state.drag_cur;
    state.drag_cur = (mx, my);
    let (dx, dy) = (mx - prev.0, my - prev.1);
    match state.active_tool {
        Tool::Select => {
            if let Some(idx) = state.scene.selected {
                if let Some(obj) = state.scene.objects.get_mut(idx) {
                    obj.translate(dx, dy);
                    state.scene.dirty = true;
                }
            }
        }
        Tool::Pen => {
            if let Some(DrawObject::Pen(s)) = state.active_stroke.as_mut() {
                s.points.push((mx, my));
            }
        }
        Tool::Eraser => {
            if let Some(drag) = state.eraser_drag.as_mut() {
                drag.points.push((mx, my));
            }
        }
        Tool::Highlight => {
            if let Some(DrawObject::Highlight(h)) = state.active_stroke.as_mut() {
                h.points.push((mx, my));
            }
        }
        // shape tools: preview is drawn by main.rs via preview_object()
        _ => {}
    }
}

fn handle_release(x1: i32, y1: i32, state: &mut AppState, canvas: &mut Canvas) {
    if !state.mouse_down { return; }
    state.mouse_down = false;
    let (x0, y0) = state.drag_start;
    let s = ShapeObj { x0, y0, x1, y1, color: state.color, size: state.brush_size };
    match state.active_tool {
        Tool::Line   => { state.scene.add(DrawObject::Line(s)); }
        Tool::Rect   => { state.scene.add(DrawObject::Rect(s)); }
        Tool::Circle => { state.scene.add(DrawObject::Circle(s)); }
        Tool::Arrow  => { state.scene.add(DrawObject::Arrow(s)); }
        // Pen/Highlight: commit the active stroke into the scene
        Tool::Pen | Tool::Highlight => {
            if let Some(stroke) = state.active_stroke.take() {
                state.scene.add(stroke);
            }
        }
        // Eraser: zero alpha on the committed canvas at each drag point,
        // then bake that canvas into the scene's permanent background and
        // clear the live objects.  From this point on the pre-erase scene
        // is no longer selectable, but undo still restores it because the
        // snapshot was pushed on mouse-down.
        Tool::Eraser => {
            if let Some(drag) = state.eraser_drag.take() {
                if !drag.points.is_empty() {
                    apply_erase_to_canvas(canvas, &drag.points, drag.radius);
                    canvas.dirty = true;
                    state.scene.bake_from_canvas(canvas);
                }
            }
        }
        Tool::Select => {
            if state.pending_select_undo {
                state.pending_select_undo = false;
                // If the object didn't actually move, discard the undo snapshot
                if state.drag_start == state.drag_cur {
                    state.scene.undo_stack.pop_back();
                }
            }
        }
        _ => {}
    }
}

/// Zero alpha in `canvas` inside a disc of `radius` around every point in
/// `points`.  Bounds-checked against the canvas size.
fn apply_erase_to_canvas(canvas: &mut Canvas, points: &[(i32, i32)], radius: u32) {
    let r = radius as i32;
    if r <= 0 { return; }
    let r2 = r * r;
    let w = canvas.width as i32;
    let h = canvas.height as i32;
    for &(cx, cy) in points {
        for dy in -r..=r {
            let py = cy + dy;
            if py < 0 || py >= h { continue; }
            for dx in -r..=r {
                let px = cx + dx;
                if px < 0 || px >= w { continue; }
                if dx * dx + dy * dy > r2 { continue; }
                let idx = ((py as u32 * canvas.width + px as u32) * 4) as usize;
                canvas.pixels[idx + 3] = 0;
            }
        }
        canvas.dirty = true;
    }
}

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

    let ctrl  = mods.contains(Mod::LCTRLMOD)  || mods.contains(Mod::RCTRLMOD);
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
                if let Some(cb) = clipboard {
                    if let Ok(text) = cb.clipboard_text() {
                        // Strip newlines — single-line text tool.
                        let cleaned: String = text.chars()
                            .filter(|c| *c != '\n' && *c != '\r')
                            .collect();
                        state.text_buffer.push_str(&cleaned);
                    }
                }
            }
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
                        // Modes / Rtsp: go back to Devices without closing
                        // the modal.  `dismiss()` resets cursors and the
                        // RTSP URL buffer so the next open is clean.
                        picker.dismiss();
                    }
                }
                Keycode::Backspace => {
                    if picker.stage == PickerStage::Rtsp {
                        picker.rtsp.url.pop();
                    }
                }
                Keycode::V if ctrl => {
                    if picker.stage == PickerStage::Rtsp {
                        if let Some(cb) = clipboard {
                            match cb.clipboard_text() {
                                Ok(text) => {
                                    // Strip whitespace — RTSP URLs are
                                    // single-line and surrounding newlines
                                    // would break ffmpeg invocation.
                                    let cleaned: String = text
                                        .chars()
                                        .filter(|c| !c.is_whitespace())
                                        .collect();
                                    picker.rtsp.url.push_str(&cleaned);
                                }
                                Err(e) => eprintln!("[input] clipboard read failed: {e}"),
                            }
                        }
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
        Keycode::P => { commit_text(state); switch_tool(state, Tool::Pen);       }
        Keycode::L => { commit_text(state); switch_tool(state, Tool::Line);      }
        Keycode::R => { commit_text(state); switch_tool(state, Tool::Rect);      }
        Keycode::C => { commit_text(state); switch_tool(state, Tool::Circle);    }
        Keycode::A => { commit_text(state); switch_tool(state, Tool::Arrow);     }
        Keycode::T => { commit_text(state); switch_tool(state, Tool::Text);      }
        Keycode::S => { commit_text(state); switch_tool(state, Tool::Select);    }
        Keycode::E => { commit_text(state); switch_tool(state, Tool::Eraser);    }
        Keycode::H => { commit_text(state); switch_tool(state, Tool::Highlight); }

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
        Keycode::LeftBracket  => { state.brush_size = state.brush_size.saturating_sub(4).max(1); state.tool_sizes[state.active_tool.index()] = state.brush_size; }
        Keycode::RightBracket => { state.brush_size = (state.brush_size + 4).min(200); state.tool_sizes[state.active_tool.index()] = state.brush_size; }

        // ── Undo / Redo ───────────────────────────────────────────────────────
        Keycode::Z if ctrl && shift => {
            state.scene.redo();
            state.draw_locked_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }
        Keycode::Y if ctrl => {
            state.scene.redo();
            state.draw_locked_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }
        Keycode::Z if ctrl => {
            state.scene.undo();
            state.draw_locked_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        }

        // ── Clear / Delete selected ───────────────────────────────────────────
        Keycode::Delete | Keycode::Backspace => {
            if state.active_tool == Tool::Select {
                if let Some(idx) = state.scene.selected.take() {
                    state.scene.push_undo();
                    state.scene.objects.remove(idx);
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
    state.brush_size  = state.tool_sizes[tool.index()];
}

fn commit_text(state: &mut AppState) {
    if !state.text_buffer.is_empty() { state.pending_text_commit = true; }
    state.text_input_active = false;
}

/// Returns a live preview `DrawObject` for the current shape drag, if any.
/// main.rs calls this to render the ghost shape on top of the committed scene.
pub fn preview_object(state: &AppState) -> Option<DrawObject> {
    if !state.mouse_down { return None; }
    let (x0, y0) = state.drag_start;
    let (x1, y1) = state.drag_cur;
    let s = ShapeObj { x0, y0, x1, y1, color: state.color, size: state.brush_size };
    match state.active_tool {
        Tool::Line   => Some(DrawObject::Line(s)),
        Tool::Rect   => Some(DrawObject::Rect(s)),
        Tool::Circle => Some(DrawObject::Circle(s)),
        Tool::Arrow  => Some(DrawObject::Arrow(s)),
        _ => None,
    }
}
