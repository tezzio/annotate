use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;

use crate::{
    canvas::Canvas,
    tools::{
        Tool, PALETTE,
        draw_line, draw_rect_outline, draw_ellipse_outline,
        draw_arrow, draw_highlight, draw_erase, draw_circle_fill,
    },
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
            if !handle_keydown(*k, *keymod, state, canvas) {
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
            let (mx, my) = (*x, *y);

            // Device picker intercepts all clicks when visible
            if state.show_device_picker {
                if let Some(picker) = &mut state.device_picker {
                    if let Some(idx) = picker.click_hit(mx, my, canvas.width as i32, canvas.height as i32) {
                        picker.selected = idx;
                        state.picker_confirmed = Some(idx);
                        state.show_device_picker = false;
                    }
                }
                return true;
            }

            // Menu bar
            if my < MENU_HEIGHT {
                // handled in main.rs via menu_bar.click()
                state.pending_menu_click = Some((mx, my));
                return true;
            }
            // Close menu if clicking outside it while it's open
            state.menu_close_requested = true;

            // Toolbar hit-test
            if my >= canvas.height as i32 - TOOLBAR_HEIGHT {
                handle_toolbar_click(mx, my, layout, state);
                return true;
            }

            // Drawing area — below menu bar, above toolbar
            push_undo(state, canvas);
            state.mouse_down = true;
            state.drag_start = (mx, my);
            state.drag_cur   = (mx, my);

            match state.active_tool {
                Tool::Pen => {
                    draw_circle_fill(
                        canvas,
                        mx, my,
                        state.brush_size as i32 / 2,
                        state.color.0, state.color.1, state.color.2, 255,
                    );
                }
                Tool::Eraser => {
                    draw_erase(canvas, mx, my, state.brush_size as i32);
                }
                Tool::Highlight => {
                    draw_highlight(canvas, mx, my, state.brush_size as i32 * 2,
                        state.color.0, state.color.1, state.color.2);
                }
                Tool::Text => {
                    // Commit any existing text first
                    commit_text(state, canvas);
                    state.text_pos = (mx, my);
                    state.text_buffer.clear();
                    state.text_input_active = true;
                    sdl2::hint::set("SDL_IME_SHOW_UI", "1");
                }
                _ => {
                    // Shape tools: start preview
                    state.preview_base = Some(canvas.snapshot());
                }
            }
        }

        // ── Mouse motion ──────────────────────────────────────────────────────
        Event::MouseMotion { x, y, .. } => {
            let (mx, my) = (*x, *y);
            if !state.mouse_down { return true; }
            if state.show_device_picker { return true; }
            if my < MENU_HEIGHT { return true; }
            if my >= canvas.height as i32 - TOOLBAR_HEIGHT { return true; }

            let prev = state.drag_cur;
            state.drag_cur = (mx, my);

            match state.active_tool {
                Tool::Pen => {
                    draw_line(
                        canvas,
                        prev.0, prev.1, mx, my,
                        state.color.0, state.color.1, state.color.2, 255,
                        state.brush_size as i32,
                    );
                }
                Tool::Eraser => {
                    draw_erase(canvas, mx, my, state.brush_size as i32);
                }
                Tool::Highlight => {
                    draw_highlight(canvas, mx, my, state.brush_size as i32 * 2,
                        state.color.0, state.color.1, state.color.2);
                }
                _ => {
                    // Rebuild preview from snapshot
                    rebuild_preview(state, canvas, mx, my);
                }
            }
        }

        // ── Mouse button up ───────────────────────────────────────────────────
        Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
            if !state.mouse_down { return true; }
            state.mouse_down = false;

            let (x1, y1) = (*x, *y);
            let (x0, y0) = state.drag_start;

            if state.active_tool != Tool::Pen
                && state.active_tool != Tool::Eraser
                && state.active_tool != Tool::Highlight
                && state.active_tool != Tool::Text
            {
                // Commit shape onto real canvas
                if let Some(base) = state.preview_base.take() {
                    canvas.restore(base);
                }
                let (r, g, b) = state.color;
                let t = state.brush_size as i32;
                match state.active_tool {
                    Tool::Line   => draw_line(canvas, x0, y0, x1, y1, r, g, b, 255, t),
                    Tool::Rect   => draw_rect_outline(canvas, x0, y0, x1, y1, r, g, b, 255, t),
                    Tool::Circle => draw_ellipse_outline(canvas, x0, y0, x1, y1, r, g, b, 255, t),
                    Tool::Arrow  => draw_arrow(canvas, x0, y0, x1, y1, r, g, b, 255, t),
                    _ => {}
                }
            }
        }

        Event::Window { win_event: WindowEvent::Resized(w, h), .. }
        | Event::Window { win_event: WindowEvent::SizeChanged(w, h), .. } => {
            state.window_size = (*w as u32, *h as u32);
        }

        _ => {}
    }
    true
}

// ── Keyboard handler ──────────────────────────────────────────────────────────

fn handle_keydown(
    k: Keycode,
    mods: Mod,
    state: &mut AppState,
    canvas: &mut Canvas,
) -> bool {
    let ctrl  = mods.contains(Mod::LCTRLMOD)  || mods.contains(Mod::RCTRLMOD);
    let shift = mods.contains(Mod::LSHIFTMOD) || mods.contains(Mod::RSHIFTMOD);

    // Text input mode intercepts most keys
    if state.text_input_active && state.active_tool == Tool::Text {
        match k {
            Keycode::Return | Keycode::KpEnter => {
                commit_text(state, canvas);
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
        Keycode::P => { commit_text(state, canvas); switch_tool(state, Tool::Pen);       }
        Keycode::L => { commit_text(state, canvas); switch_tool(state, Tool::Line);      }
        Keycode::R => { commit_text(state, canvas); switch_tool(state, Tool::Rect);      }
        Keycode::C => { commit_text(state, canvas); switch_tool(state, Tool::Circle);    }
        Keycode::A => { commit_text(state, canvas); switch_tool(state, Tool::Arrow);     }
        Keycode::T => { commit_text(state, canvas); switch_tool(state, Tool::Text);      }
        Keycode::E => { commit_text(state, canvas); switch_tool(state, Tool::Eraser);    }
        Keycode::H => { commit_text(state, canvas); switch_tool(state, Tool::Highlight); }

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
        Keycode::Z if ctrl && shift => redo(state, canvas),
        Keycode::Y if ctrl          => redo(state, canvas),
        Keycode::Z if ctrl          => undo(state, canvas),

        // ── Clear ─────────────────────────────────────────────────────────────
        Keycode::Delete | Keycode::Backspace => {
            push_undo(state, canvas);
            canvas.clear();
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

// ── Preview rebuild ───────────────────────────────────────────────────────────

/// Switch to a different tool, saving the current brush size and restoring
/// the new tool's remembered size.
fn switch_tool(state: &mut AppState, tool: Tool) {
    state.tool_sizes[state.active_tool.index()] = state.brush_size;
    state.active_tool = tool;
    state.brush_size  = state.tool_sizes[tool.index()];
}

fn rebuild_preview(state: &mut AppState, canvas: &mut Canvas, x1: i32, y1: i32) {
    if let Some(ref base) = state.preview_base {
        canvas.restore(base.clone());
    }
    let (x0, y0) = state.drag_start;
    let (r, g, b) = state.color;
    let t = state.brush_size as i32;
    match state.active_tool {
        Tool::Line   => draw_line(canvas, x0, y0, x1, y1, r, g, b, 255, t),
        Tool::Rect   => draw_rect_outline(canvas, x0, y0, x1, y1, r, g, b, 255, t),
        Tool::Circle => draw_ellipse_outline(canvas, x0, y0, x1, y1, r, g, b, 255, t),
        Tool::Arrow  => draw_arrow(canvas, x0, y0, x1, y1, r, g, b, 255, t),
        _ => {}
    }
}

// ── Undo / Redo ───────────────────────────────────────────────────────────────

pub fn push_undo(state: &mut AppState, canvas: &Canvas) {
    state.undo_stack.push_back(canvas.snapshot());
    if state.undo_stack.len() > state.undo_limit { state.undo_stack.pop_front(); }
    state.redo_stack.clear();
}

fn undo(state: &mut AppState, canvas: &mut Canvas) {
    if let Some(snap) = state.undo_stack.pop_back() {
        state.redo_stack.push_back(canvas.snapshot());
        canvas.restore(snap);
    }
}

fn redo(state: &mut AppState, canvas: &mut Canvas) {
    if let Some(snap) = state.redo_stack.pop_back() {
        state.undo_stack.push_back(canvas.snapshot());
        canvas.restore(snap);
    }
}

// ── Text commit ───────────────────────────────────────────────────────────────

fn commit_text(state: &mut AppState, _canvas: &mut Canvas) {
    if !state.text_buffer.is_empty() { state.pending_text_commit = true; }
    state.text_input_active = false;
}
