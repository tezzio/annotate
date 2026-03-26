use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;

use crate::{
    canvas::Canvas,
    scene::{DrawObject, PenStroke, ShapeObj, EraseObj, HighlightStroke},
    tools::{Tool, PALETTE},
    ui::{ToolbarLayout, TOOLBAR_HEIGHT, MENU_HEIGHT},
    AppState,
};

// ── Input event processor ─────────────────────────────────────────────────────

/// Process a single SDL event.  Returns `false` when the application should quit.
pub fn process_event(
    event:   &Event,
    state:   &mut AppState,
    canvas:  &mut Canvas,
    layout:  &ToolbarLayout,
) -> bool {
    match event {
        // ── Quit ──────────────────────────────────────────────────────────────
        Event::Quit { .. } => return false,

        Event::KeyDown { keycode: Some(k), keymod, .. } => {
            if !handle_keydown(*k, *keymod, state) {
                return false;
            }
        }

        // SDL text input — used when text tool is active
        Event::TextInput { text, .. } => {
            if state.active_tool == Tool::Text && state.text_input_active {
                state.text_buffer.push_str(text);
            }
        }

        // ── Mouse button down ─────────────────────────────────────────────────
        Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
            handle_press(*x, *y, state, canvas, layout);
        }

        // ── Mouse motion ──────────────────────────────────────────────────────
        Event::MouseMotion { x, y, .. } => {
            handle_move(*x, *y, state, canvas);
        }

        // ── Mouse button up ───────────────────────────────────────────────────
        Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
            handle_release(*x, *y, state);
        }

        // ── Touch / stylus (single-touch) ─────────────────────────────────────
        Event::FingerDown { x, y, .. } => {
            let (mx, my) = finger_to_px(*x, *y, canvas);
            handle_press(mx, my, state, canvas, layout);
        }
        Event::FingerMotion { x, y, .. } => {
            let (mx, my) = finger_to_px(*x, *y, canvas);
            handle_move(mx, my, state, canvas);
        }
        Event::FingerUp { x, y, .. } => {
            let (mx, my) = finger_to_px(*x, *y, canvas);
            handle_release(mx, my, state);
        }

        Event::Window { win_event: WindowEvent::Resized(w, h), .. }
        | Event::Window { win_event: WindowEvent::SizeChanged(w, h), .. } => {
            state.window_size = (*w as u32, *h as u32);
        }

        _ => {}
    }
    true
}

// ── Touch coordinate helper ──────────────────────────────────────────────────

/// Convert SDL2 normalized finger coords (0.0–1.0) to canvas pixel coords.
fn finger_to_px(x: f32, y: f32, canvas: &Canvas) -> (i32, i32) {
    ((x * canvas.width as f32).round() as i32, (y * canvas.height as f32).round() as i32)
}

// ── Shared press / move / release handlers ────────────────────────────────────

fn handle_press(mx: i32, my: i32, state: &mut AppState, canvas: &Canvas, layout: &ToolbarLayout) {
    if state.show_device_picker {
        if let Some(picker) = &mut state.device_picker {
            if let Some(idx) = picker.click_hit(mx, my, canvas.width as i32, canvas.height as i32) {
                picker.selected = idx;
                state.picker_confirmed = Some(idx);
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
    if my >= canvas.height as i32 - TOOLBAR_HEIGHT {
        handle_toolbar_click(mx, my, layout, state);
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
            state.scene.add(DrawObject::Pen(PenStroke {
                points: vec![(mx, my)],
                color:  state.color,
                size:   state.brush_size,
            }));
        }
        Tool::Eraser => {
            state.scene.push_undo();
            state.scene.add(DrawObject::Erase(EraseObj {
                points: vec![(mx, my)],
                size:   state.brush_size,
            }));
        }
        Tool::Highlight => {
            state.scene.push_undo();
            state.scene.add(DrawObject::Highlight(HighlightStroke {
                points: vec![(mx, my)],
                color:  state.color,
                size:   state.brush_size,
            }));
        }
        // Shape tools: push undo here; object committed on release
        _ => {
            state.scene.push_undo();
        }
    }
}

fn handle_move(mx: i32, my: i32, state: &mut AppState, canvas: &Canvas) {
    if !state.mouse_down || state.show_device_picker { return; }
    if my < MENU_HEIGHT || my >= canvas.height as i32 - TOOLBAR_HEIGHT { return; }
    let prev = state.drag_cur;
    state.drag_cur = (mx, my);
    let (dx, dy) = (mx - prev.0, my - prev.1);
    match state.active_tool {
        Tool::Select => {
            if let Some(idx) = state.scene.selected {
                if let Some(obj) = state.scene.objects.get_mut(idx) {
                    obj.translate(dx, dy);
                }
            }
        }
        Tool::Pen => {
            if let Some(DrawObject::Pen(s)) = state.scene.objects.last_mut() {
                s.points.push((mx, my));
            }
        }
        Tool::Eraser => {
            if let Some(DrawObject::Erase(e)) = state.scene.objects.last_mut() {
                e.points.push((mx, my));
            }
        }
        Tool::Highlight => {
            if let Some(DrawObject::Highlight(h)) = state.scene.objects.last_mut() {
                h.points.push((mx, my));
            }
        }
        // shape tools: preview is drawn by main.rs via preview_object()
        _ => {}
    }
}

fn handle_release(x1: i32, y1: i32, state: &mut AppState) {
    if !state.mouse_down { return; }
    state.mouse_down = false;
    let (x0, y0) = state.drag_start;
    let s = ShapeObj { x0, y0, x1, y1, color: state.color, size: state.brush_size };
    match state.active_tool {
        Tool::Line   => { state.scene.add(DrawObject::Line(s)); }
        Tool::Rect   => { state.scene.add(DrawObject::Rect(s)); }
        Tool::Circle => { state.scene.add(DrawObject::Circle(s)); }
        Tool::Arrow  => { state.scene.add(DrawObject::Arrow(s)); }
        // Pen/Eraser/Highlight: already appended point-by-point
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

// ── Keyboard handler ──────────────────────────────────────────────────────────

fn handle_keydown(
    k: Keycode,
    mods: Mod,
    state: &mut AppState,
) -> bool {
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
            _ => {}
        }
        return true;
    }

    if state.show_device_picker {
        if let Some(picker) = &mut state.device_picker {
            match k {
                Keycode::Up    => picker.move_up(),
                Keycode::Down  => picker.move_down(),
                Keycode::Return | Keycode::KpEnter => {
                    let idx = picker.selected;
                    state.picker_confirmed = Some(idx);
                    state.show_device_picker = false;
                }
                Keycode::Escape | Keycode::F2 => {
                    state.show_device_picker = false;
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

// ── Toolbar click handler ─────────────────────────────────────────────────────

fn handle_toolbar_click(mx: i32, my: i32, layout: &ToolbarLayout, state: &mut AppState) {
    // Tool buttons
    for (tool, rect) in &layout.tools {
        if rect.contains_point((mx, my)) {
            switch_tool(state, *tool);
            return;
        }
    }
    // Palette
    for (i, rect) in layout.palette.iter().enumerate() {
        if rect.contains_point((mx, my)) {
            state.color = PALETTE[i];
            return;
        }
    }
    // Brush size
    if layout.brush_minus.contains_point((mx, my)) {
        state.brush_size = state.brush_size.saturating_sub(4).max(1);
        state.tool_sizes[state.active_tool.index()] = state.brush_size;
    } else if layout.brush_plus.contains_point((mx, my)) {
        state.brush_size = (state.brush_size + 4).min(200);
        state.tool_sizes[state.active_tool.index()] = state.brush_size;
    } else if layout.change_input.contains_point((mx, my)) {
        state.show_device_picker = true;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Switch to a different tool, saving the current brush size and restoring
/// the new tool's remembered size.
fn switch_tool(state: &mut AppState, tool: Tool) {
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
