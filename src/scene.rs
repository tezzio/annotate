use std::collections::VecDeque;

use crate::canvas::Canvas;
use crate::tools::{
    draw_circle_fill, draw_line, draw_rect_outline, draw_ellipse_outline,
    draw_arrow, draw_highlight, draw_erase,
};

// ── Object types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct PenStroke {
    pub points: Vec<(i32, i32)>,
    pub color:  (u8, u8, u8),
    pub size:   u32,
}

#[derive(Clone, Debug)]
pub struct ShapeObj {
    pub x0: i32, pub y0: i32,
    pub x1: i32, pub y1: i32,
    pub color: (u8, u8, u8),
    pub size:  u32,
}

#[derive(Clone, Debug)]
pub struct EraseObj {
    pub points: Vec<(i32, i32)>,
    pub size:   u32,
}

#[derive(Clone, Debug)]
pub struct TextObj {
    pub text:  String,
    pub x:     i32,
    pub y:     i32,
    pub color: (u8, u8, u8),
}

#[derive(Clone, Debug)]
pub struct HighlightStroke {
    pub points: Vec<(i32, i32)>,
    pub color:  (u8, u8, u8),
    pub size:   u32,
}

#[derive(Clone, Debug)]
pub enum DrawObject {
    Pen(PenStroke),
    Line(ShapeObj),
    Rect(ShapeObj),
    Circle(ShapeObj),
    Arrow(ShapeObj),
    Erase(EraseObj),
    Highlight(HighlightStroke),
    Text(TextObj),
}

impl DrawObject {
    /// Paint this object into a canvas.
    pub fn render(&self, canvas: &mut Canvas) {
        match self {
            DrawObject::Pen(s) => {
                let (r, g, b) = s.color;
                let rad = (s.size as i32 / 2).max(1);
                if s.points.is_empty() { return; }
                draw_circle_fill(canvas, s.points[0].0, s.points[0].1, rad, r, g, b, 255);
                for i in 1..s.points.len() {
                    let (x0, y0) = s.points[i - 1];
                    let (x1, y1) = s.points[i];
                    draw_line(canvas, x0, y0, x1, y1, r, g, b, 255, s.size as i32);
                }
            }
            DrawObject::Line(s) => {
                let (r, g, b) = s.color;
                draw_line(canvas, s.x0, s.y0, s.x1, s.y1, r, g, b, 255, s.size as i32);
            }
            DrawObject::Rect(s) => {
                let (r, g, b) = s.color;
                draw_rect_outline(canvas, s.x0, s.y0, s.x1, s.y1, r, g, b, 255, s.size as i32);
            }
            DrawObject::Circle(s) => {
                let (r, g, b) = s.color;
                draw_ellipse_outline(canvas, s.x0, s.y0, s.x1, s.y1, r, g, b, 255, s.size as i32);
            }
            DrawObject::Arrow(s) => {
                let (r, g, b) = s.color;
                draw_arrow(canvas, s.x0, s.y0, s.x1, s.y1, r, g, b, 255, s.size as i32);
            }
            DrawObject::Erase(e) => {
                for &(x, y) in &e.points {
                    draw_erase(canvas, x, y, e.size as i32);
                }
            }
            DrawObject::Highlight(h) => {
                let (r, g, b) = h.color;
                let rad = h.size as i32 * 2;
                for &(x, y) in &h.points {
                    draw_highlight(canvas, x, y, rad, r, g, b);
                }
            }
            DrawObject::Text(_) => {
                // Text is blitted separately via blit_text_to_canvas in main.rs
                // because it needs SDL TTF; rendering here is a no-op.
            }
        }
    }

    /// Axis-aligned bounding box (x, y, w, h).
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        match self {
            DrawObject::Pen(s) => points_bounds(&s.points, s.size as i32),
            DrawObject::Highlight(h) => points_bounds(&h.points, h.size as i32 * 2),
            DrawObject::Erase(e) => points_bounds(&e.points, e.size as i32),
            DrawObject::Line(s) | DrawObject::Rect(s) | DrawObject::Circle(s) | DrawObject::Arrow(s) => {
                let pad = s.size as i32;
                let (lx, rx) = (s.x0.min(s.x1) - pad, s.x0.max(s.x1) + pad);
                let (ty, by) = (s.y0.min(s.y1) - pad, s.y0.max(s.y1) + pad);
                (lx, ty, rx - lx, by - ty)
            }
            DrawObject::Text(t) => (t.x, t.y, 300, 40),
        }
    }

    /// Returns true if the point (px, py) is within the object's bounding box
    /// plus a small hit radius.
    pub fn hit_test(&self, px: i32, py: i32) -> bool {
        let (x, y, w, h) = self.bounds();
        px >= x - 8 && py >= y - 8 && px <= x + w + 8 && py <= y + h + 8
    }

    /// Translate (move) by (dx, dy).
    pub fn translate(&mut self, dx: i32, dy: i32) {
        match self {
            DrawObject::Pen(s) => { for p in &mut s.points { p.0 += dx; p.1 += dy; } }
            DrawObject::Highlight(h) => { for p in &mut h.points { p.0 += dx; p.1 += dy; } }
            DrawObject::Erase(e) => { for p in &mut e.points { p.0 += dx; p.1 += dy; } }
            DrawObject::Line(s) | DrawObject::Rect(s) | DrawObject::Circle(s) | DrawObject::Arrow(s) => {
                s.x0 += dx; s.y0 += dy; s.x1 += dx; s.y1 += dy;
            }
            DrawObject::Text(t) => { t.x += dx; t.y += dy; }
        }
    }
}

fn points_bounds(pts: &[(i32, i32)], pad: i32) -> (i32, i32, i32, i32) {
    if pts.is_empty() { return (0, 0, 0, 0); }
    let (mut lx, mut ty) = pts[0];
    let (mut rx, mut by) = pts[0];
    for &(x, y) in pts { lx = lx.min(x); ty = ty.min(y); rx = rx.max(x); by = by.max(y); }
    (lx - pad, ty - pad, rx - lx + pad * 2, by - ty + pad * 2)
}

// ── Scene ─────────────────────────────────────────────────────────────────────

pub struct Scene {
    pub objects:    Vec<DrawObject>,
    pub undo_stack: VecDeque<Vec<DrawObject>>,
    pub redo_stack: VecDeque<Vec<DrawObject>>,
    pub undo_limit: usize,
    /// Index of the selected object, if any.
    pub selected:   Option<usize>,
    /// Set whenever the scene is modified; cleared by render_to().
    pub dirty:      bool,
}

impl Scene {
    pub fn new(undo_limit: usize) -> Self {
        Self { objects: Vec::new(), undo_stack: VecDeque::new(), redo_stack: VecDeque::new(), undo_limit, selected: None, dirty: true }
    }

    pub fn push_undo(&mut self) {
        self.undo_stack.push_back(self.objects.clone());
        if self.undo_stack.len() > self.undo_limit { self.undo_stack.pop_front(); }
        self.redo_stack.clear();
        self.dirty = true;
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop_back() {
            self.redo_stack.push_back(self.objects.clone());
            self.objects = prev;
            self.selected = None;
            self.dirty = true;
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop_back() {
            self.undo_stack.push_back(self.objects.clone());
            self.objects = next;
            self.selected = None;
            self.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        self.push_undo();
        self.objects.clear();
        self.selected = None;
        self.dirty = true;
    }

    pub fn add(&mut self, obj: DrawObject) {
        self.objects.push(obj);
        self.dirty = true;
    }

    /// Render all objects into `canvas` (clear first). Only re-renders when dirty.
    pub fn render_to(&mut self, canvas: &mut Canvas) {
        if !self.dirty { return; }
        canvas.clear();
        for obj in &self.objects {
            obj.render(canvas);
        }
        canvas.dirty = true;
        self.dirty = false;
    }

    /// Hit-test from the top (last-drawn) object down. Returns Some(index).
    pub fn hit_test(&self, px: i32, py: i32) -> Option<usize> {
        for i in (0..self.objects.len()).rev() {
            if self.objects[i].hit_test(px, py) {
                return Some(i);
            }
        }
        None
    }
}
