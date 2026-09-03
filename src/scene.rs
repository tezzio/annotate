use std::collections::VecDeque;
use std::sync::Arc;

use crate::canvas::Canvas;
use crate::tools::{
    draw_circle_fill, draw_line, draw_rect_outline, draw_ellipse_outline,
    draw_arrow, draw_highlight,
    slice_circle_fill, slice_line, slice_rect_outline, slice_ellipse_outline,
    slice_arrow, slice_highlight, SliceTarget,
};
use rayon::prelude::*;

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

/// Text annotation.  Currently dead because text rendering goes through
/// `blit_text_to_canvas` in `main.rs` (which uses SDL TTF directly), but
/// the type is kept here so the REST API can model text objects in the
/// scene graph.  See `DrawObject::Text`.
#[allow(dead_code)]
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

/// Live (still-editable) drawing object.  Note that the eraser is *not*
/// represented here — an erase commit "bakes" the current canvas into
/// `Scene::baked_bg` and clears the live objects.  See [`Scene::bake_from_canvas`].
#[derive(Clone, Debug)]
pub enum DrawObject {
    Pen(PenStroke),
    Line(ShapeObj),
    Rect(ShapeObj),
    Circle(ShapeObj),
    Arrow(ShapeObj),
    Highlight(HighlightStroke),
    /// Text annotations — currently rendered via `blit_text_to_canvas`
    /// outside the scene graph, but the variant is reserved for future
    /// API-driven text editing.
    #[allow(dead_code)]
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

    /// Paint this object into a `SliceTarget` (rayon's row-parallel path).
    /// Functionally identical to [`render`] but writes directly into the
    /// slice's raw pointer instead of through `Canvas::put_pixel`.  Each
    /// helper inside `tools::slice_*` bounds-checks against the global
    /// canvas dimensions, so pixels that fall outside the slice's row
    /// range are simply skipped — no per-call branching required.
    pub fn render_into_slice(&self, s: &SliceTarget) {
        match self {
            DrawObject::Pen(st) => {
                let (r, g, b) = st.color;
                let rad = (st.size as i32 / 2).max(1);
                if st.points.is_empty() { return; }
                slice_circle_fill(s, st.points[0].0, st.points[0].1, rad, r, g, b, 255);
                for i in 1..st.points.len() {
                    let (x0, y0) = st.points[i - 1];
                    let (x1, y1) = st.points[i];
                    slice_line(s, x0, y0, x1, y1, r, g, b, 255, st.size as i32);
                }
            }
            DrawObject::Line(st) => {
                let (r, g, b) = st.color;
                slice_line(s, st.x0, st.y0, st.x1, st.y1, r, g, b, 255, st.size as i32);
            }
            DrawObject::Rect(st) => {
                let (r, g, b) = st.color;
                slice_rect_outline(s, st.x0, st.y0, st.x1, st.y1, r, g, b, 255, st.size as i32);
            }
            DrawObject::Circle(st) => {
                let (r, g, b) = st.color;
                slice_ellipse_outline(s, st.x0, st.y0, st.x1, st.y1, r, g, b, 255, st.size as i32);
            }
            DrawObject::Arrow(st) => {
                let (r, g, b) = st.color;
                slice_arrow(s, st.x0, st.y0, st.x1, st.y1, r, g, b, 255, st.size as i32);
            }
            DrawObject::Highlight(h) => {
                let (r, g, b) = h.color;
                let rad = h.size as i32 * 2;
                for &(x, y) in &h.points {
                    slice_highlight(s, x, y, rad, r, g, b);
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
            DrawObject::Line(s) | DrawObject::Rect(s) | DrawObject::Circle(s) | DrawObject::Arrow(s) => {
                s.x0 += dx; s.y0 += dy; s.x1 += dx; s.y1 += dy;
            }
            DrawObject::Text(t) => { t.x += dx; t.y += dy; }
        }
    }

    /// True if any erase disc centered on a point in `eraser_path` with
    /// radius `r` touches this object's pixels.  Cheap AABB pre-test plus
    /// a per-shape geometric test.  Used by the layer-aware eraser to
    /// skip objects that are nowhere near the brush.
    pub fn intersects_eraser(&self, eraser_path: &[(i32, i32)], r: i32) -> bool {
        let (bx, by, bw, bh) = self.bounds();
        // Padding must be at least the eraser radius for the AABB test to
        // catch brush-near-edge cases; each per-shape helper adds its own
        // shape-specific padding on top.
        let pad = r + 2;
        for &(cx, cy) in eraser_path {
            let nx = cx.max(bx - pad);
            let ny = cy.max(by - pad);
            // Path point is inside (expanded) bounds.
            if cx >= bx - pad && cx <= bx + bw + pad
                && cy >= by - pad && cy <= by + bh + pad
            {
                return true;
            }
            // Segment from a previous path point to this one — covers fast
            // strokes where consecutive eraser points straddle the edge.
            let _ = (nx, ny);
        }
        // Also check segments of the eraser path against the bbox so fast
        // drags (where consecutive points are 30+ px apart) still register.
        if eraser_path.len() >= 2 {
            let (bl, br, bt, bb) = (bx - pad, bx + bw + pad, by - pad, by + bh + pad);
            for w in eraser_path.windows(2) {
                let (x0, y0) = w[0];
                let (x1, y1) = w[1];
                if segment_intersects_rect(x0, y0, x1, y1, bl, bt, br, bb) {
                    return true;
                }
            }
        }
        let _ = (bw, bh); // keep naming symmetric with `bounds()`
        false
    }

    /// Trim the erase disc out of this object.  Returns Some(replacement)
    /// if the object was modified (the caller should swap it in place),
    /// Some(removed) via `Some(empty)` if the object is gone, or None if
    /// no change.  See [`Scene::erase_path`] for the high-level flow.
    pub fn apply_eraser(&self, eraser_path: &[(i32, i32)], r: i32) -> EraseResult {
        match self {
            DrawObject::Pen(s) => {
                // Split the polyline at points that lie inside the eraser
                // disc; emit one sub-stroke per contiguous surviving run.
                let mut out: Vec<DrawObject> = Vec::new();
                let mut cur: Vec<(i32, i32)> = Vec::new();
                for &p in &s.points {
                    if point_in_eraser(p, eraser_path, r) {
                        if cur.len() >= 2 {
                            out.push(DrawObject::Pen(PenStroke {
                                points: std::mem::take(&mut cur),
                                color:  s.color,
                                size:   s.size,
                            }));
                        } else {
                            cur.clear();
                        }
                        // open a new sub-stroke after the gap
                    } else {
                        cur.push(p);
                    }
                }
                if cur.len() >= 2 {
                    out.push(DrawObject::Pen(PenStroke {
                        points: cur,
                        color:  s.color,
                        size:   s.size,
                    }));
                }
                erase_outcome(out)
            }
            DrawObject::Highlight(h) => {
                // Same logic as Pen, with Highlight's wider disc.
                let mut out: Vec<DrawObject> = Vec::new();
                let mut cur: Vec<(i32, i32)> = Vec::new();
                for &p in &h.points {
                    if point_in_eraser(p, eraser_path, r) {
                        if cur.len() >= 2 {
                            out.push(DrawObject::Highlight(HighlightStroke {
                                points: std::mem::take(&mut cur),
                                color:  h.color,
                                size:   h.size,
                            }));
                        } else {
                            cur.clear();
                        }
                    } else {
                        cur.push(p);
                    }
                }
                if cur.len() >= 2 {
                    out.push(DrawObject::Highlight(HighlightStroke {
                        points: cur,
                        color:  h.color,
                        size:   h.size,
                    }));
                }
                erase_outcome(out)
            }
            DrawObject::Line(s) => {
                // Lines are thin primitives: any wipe inside the segment
                // trims it.  Compute the closest point on the segment to
                // any eraser disc; if within r, chop at that point.
                if let Some(t) = segment_eraser_t(s.x0, s.y0, s.x1, s.y1, eraser_path, r) {
                    if t < 0.02 { EraseResult::Removed }
                    else if t > 0.98 { EraseResult::Removed }
                    else {
                        // Split: keep the (0..t) sub-segment.
                        let nx = s.x0 + ((s.x1 - s.x0) as f32 * t).round() as i32;
                        let ny = s.y0 + ((s.y1 - s.y0) as f32 * t).round() as i32;
                        EraseResult::Replaced(vec![DrawObject::Line(ShapeObj {
                            x0: s.x0, y0: s.y0, x1: nx, y1: ny,
                            color: s.color, size: s.size,
                        })])
                    }
                } else { EraseResult::Unchanged }
            }
            // Rect/Circle/Arrow: anything but a single, very precise corner
            // wipe is going to visibly damage the shape.  Match GIMP's
            // behaviour on small primitives: if the erase path comes within
            // `r` of any edge sample, drop the entire shape.  The user can
            // re-draw it (they had a snapshot for undo).
            DrawObject::Rect(s) | DrawObject::Circle(s) | DrawObject::Arrow(s) => {
                if shape_touched(s, eraser_path, r) {
                    EraseResult::Removed
                } else {
                    EraseResult::Unchanged
                }
            }
            DrawObject::Text(_) => EraseResult::Unchanged,
        }
    }
}

/// Outcome of [`DrawObject::apply_eraser`].
#[derive(Debug)]
pub enum EraseResult {
    /// No overlap with the eraser path; keep the original.
    Unchanged,
    /// Object fully consumed by the eraser; drop it from the scene.
    Removed,
    /// Object replaced by these (zero or more) replacement objects.
    Replaced(Vec<DrawObject>),
}

fn erase_outcome(out: Vec<DrawObject>) -> EraseResult {
        match out.len() {
            0 => EraseResult::Removed,
        _ => EraseResult::Replaced(out),
    }
}

/// True if a point lies inside any eraser disc along `path`.
fn point_in_eraser(p: (i32, i32), path: &[(i32, i32)], r: i32) -> bool {
    let r2 = (r * r) as i32;
    for &(cx, cy) in path {
        let dx = p.0 - cx;
        let dy = p.1 - cy;
        if dx * dx + dy * dy <= r2 { return true; }
    }
    // Cover the case where consecutive path points are far apart (fast
    // drag): also check the segment between path points.
    if path.len() >= 2 {
        for w in path.windows(2) {
            if point_near_segment(p, w[0], w[1], r) { return true; }
        }
    }
    false
}

/// Distance from point `p` to segment (a,b), in pixels.  Uses squared
/// distance for the comparison; `r` is in pixels.
fn point_near_segment(p: (i32, i32), a: (i32, i32), b: (i32, i32), r: i32) -> bool {
    let (px, py) = (p.0 as f32, p.1 as f32);
    let (ax, ay) = (a.0 as f32, a.1 as f32);
    let (bx, by) = (b.0 as f32, b.1 as f32);
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 < 1.0 {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey <= (r * r) as f32;
    }
    let mut t = ((px - ax) * dx + (py - ay) * dy) / len2;
    t = t.clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey <= (r * r) as f32
}

/// True if a segment intersects an axis-aligned rect.
fn segment_intersects_rect(
    x0: i32, y0: i32, x1: i32, y1: i32,
    rl: i32, rt: i32, rr: i32, rb: i32,
) -> bool {
    // Cohen–Sutherland outcodes
    fn outcode(x: i32, y: i32, rl: i32, rt: i32, rr: i32, rb: i32) -> i32 {
        let mut c = 0;
        if x <  rl { c |= 1; }
        if x >  rr { c |= 2; }
        if y <  rt { c |= 4; }
        if y >  rb { c |= 8; }
        c
    }
    let mut x0 = x0; let mut y0 = y0; let mut x1 = x1; let mut y1 = y1;
    let mut c0 = outcode(x0, y0, rl, rt, rr, rb);
    let mut c1 = outcode(x1, y1, rl, rt, rr, rb);
    loop {
        if (c0 | c1) == 0 { return true; }
        if (c0 & c1) != 0 { return false; }
        let c = if c0 != 0 { c0 } else { c1 };
        let (mx, my);
        if c & 1 != 0 {
            mx = rl; my = y0 + (y1 - y0) * (rl - x0) / (x1 - x0).max(1);
        } else if c & 2 != 0 {
            mx = rr; my = y0 + (y1 - y0) * (rr - x0) / (x1 - x0).max(1);
        } else if c & 4 != 0 {
            my = rt; mx = x0 + (x1 - x0) * (rt - y0) / (y1 - y0).max(1);
        } else {
            my = rb; mx = x0 + (x1 - x0) * (rb - y0) / (y1 - y0).max(1);
        }
        if c == c0 {
            x0 = mx; y0 = my;
            c0 = outcode(x0, y0, rl, rt, rr, rb);
        } else {
            x1 = mx; y1 = my;
            c1 = outcode(x1, y1, rl, rt, rr, rb);
        }
    }
}

/// Find parameter t in [0, 1] along the segment (s.x0,s.y0)->(s.x1,s.y1)
/// at which the eraser disc first touches it, if any.  Returned t is the
/// earliest erase point along the segment direction.
fn segment_eraser_t(
    x0: i32, y0: i32, x1: i32, y1: i32,
    path: &[(i32, i32)], r: i32,
) -> Option<f32> {
    let mut best_t: Option<f32> = None;
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let len2 = dx * dx + dy * dy;
    if len2 < 1.0 { return None; }
    for _w in path.windows(2) {
        // Iterate samples along the segment and find the first one that
        // is within r of any eraser disc centre.
        let steps = 12;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let sx = x0 as f32 + dx * t;
            let sy = y0 as f32 + dy * t;
            if best_t.is_some_and(|bt| t >= bt) { continue; }
            if point_in_eraser((sx.round() as i32, sy.round() as i32), path, r) {
                best_t = Some(t);
            }
        }
    }
    best_t
}

fn shape_touched(s: &ShapeObj, path: &[(i32, i32)], r: i32) -> bool {
    // Sample a few points along the bounding-box perimeter plus the two
    // endpoints.  Plenty for an eraser hit-test (the user sees the
    // preview outline, not a sub-pixel accurate hit).
    let (lx, rx) = (s.x0.min(s.x1), s.x0.max(s.x1));
    let (ty, by) = (s.y0.min(s.y1), s.y0.max(s.y1));
    let corners = [(lx, ty), (rx, ty), (rx, by), (lx, by)];
    for &c in &corners {
        if point_in_eraser(c, path, r) { return true; }
    }
    // Mid-edges.
    for &(cx, cy) in &[
        ((lx + rx) / 2, ty),
        (rx, (ty + by) / 2),
        ((lx + rx) / 2, by),
        (lx, (ty + by) / 2),
    ] {
        if point_in_eraser((cx, cy), path, r) { return true; }
    }
    // And the segment between consecutive eraser path points — fast drags.
    if path.len() >= 2 {
        for w in path.windows(2) {
            if segment_intersects_rect(w[0].0, w[0].1, w[1].0, w[1].1,
                                       lx - r, ty - r, rx + r, by + r)
            {
                return true;
            }
        }
    }
    false
}

fn points_bounds(pts: &[(i32, i32)], pad: i32) -> (i32, i32, i32, i32) {
    if pts.is_empty() { return (0, 0, 0, 0); }
    let (mut lx, mut ty) = pts[0];
    let (mut rx, mut by) = pts[0];
    for &(x, y) in pts { lx = lx.min(x); ty = ty.min(y); rx = rx.max(x); by = by.max(y); }
    (lx - pad, ty - pad, rx - lx + pad * 2, by - ty + pad * 2)
}

// ── Scene ─────────────────────────────────────────────────────────────────────

/// Shared, cheaply-cloneable scene payload.
///
/// The 8.3 MB baked raster (`baked_bg.pixels`) plus the (potentially long)
/// `objects` Vec make a per-commit `Scene::clone()` expensive.  We hold the
/// payload behind an `Arc<SceneInner>` and use `Arc::make_mut` for in-place
/// mutation (clone-on-write).  A `Scene` clone becomes two pointer bumps
/// instead of a deep copy.
#[derive(Clone)]
pub struct SceneInner {
    pub objects:  Vec<DrawObject>,
    /// Wrapped in `Arc` so the (potentially 8.3 MB) pixel buffer is
    /// reference-counted; only `bake_from_canvas` actually owns the pixels.
    pub baked_bg: Option<Arc<BakedLayer>>,
}

/// One undo/redo snapshot — just an alias for `Arc<SceneInner>` so the
/// `undo_stack`/`redo_stack` clone cheaply when the user clicks undo/redo
/// repeatedly.
pub(crate) type Snapshot = Arc<SceneInner>;

/// Permanent raster layer captured by an erase commit.  Once an erase has
/// happened the previous strokes are baked here and the live `objects` list
/// is cleared — so they can't be selected/moved anymore, but undo still works.
#[derive(Clone)]
pub struct BakedLayer {
    pub(crate) width:  u32,
    pub(crate) height: u32,
    /// RGBA bytes; length = `width * height * 4`.
    pub(crate) pixels: Vec<u8>,
}

pub struct Scene {
    /// Cheaply cloneable payload — `Arc::make_mut` provides clone-on-write
    /// mutation semantics so undo/redo/clear are O(1) in the common case
    /// (no other `Arc` holds a reference) and O(payload) only when the
    /// payload is shared with the undo or redo stack.
    pub(crate) inner:     Arc<SceneInner>,
    pub     undo_stack: VecDeque<Snapshot>,
    pub     redo_stack: VecDeque<Snapshot>,
    pub     undo_limit: usize,
    /// Index of the selected object, if any.
    /// Indices of all currently-selected live objects.  Always sorted and
    /// de-duplicated.  An empty vec means "nothing selected".  The Select
    /// tool can hold multiple indices simultaneously — see input.rs.
    pub     selected:   Vec<usize>,
    /// Set whenever the scene is modified; cleared by render_to().
    pub     dirty:      bool,
}

// `Scene: Clone` is now an `Arc::clone` plus two `VecDeque::clone`s (the
// latter only contain `Arc` clones).  Cost is essentially the size of three
// pointers — no 8.3 MB pixel copy.
//
// Even better: when the live `Arc<SceneInner>` is *not* shared with any
// undo or redo entry, `Arc::make_mut` mutations (e.g. `add`) avoid
// cloning the inner at all.
impl Clone for Scene {
    fn clone(&self) -> Self {
        Self {
            inner:      Arc::clone(&self.inner),
            undo_stack: self.undo_stack.clone(),
            redo_stack: self.redo_stack.clone(),
            undo_limit: self.undo_limit,
            selected:   self.selected.clone(),
            dirty:      self.dirty,
        }
    }
}

impl Scene {
    pub fn new(undo_limit: usize) -> Self {
        Self {
            inner:      Arc::new(SceneInner {
                objects:  Vec::new(),
                baked_bg: None,
            }),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            undo_limit,
            selected:   Vec::new(),
            dirty:      true,
        }
    }

    /// Live object list.  Returns `&mut Vec<DrawObject>` via `Arc::make_mut`,
    /// which clones the inner Arc's payload only when other Arc handles
    /// exist (e.g. on the undo stack).
    pub fn objects_mut(&mut self) -> &mut Vec<DrawObject> {
        &mut Arc::make_mut(&mut self.inner).objects
    }

    /// Live baked background.  Returns `&mut Option<Arc<BakedLayer>>` via
    /// `Arc::make_mut` (clone-on-write).  Reserved for future callers
    /// that need to manipulate the bake directly; not used today.
    #[allow(dead_code)]
    pub fn baked_bg_mut(&mut self) -> &mut Option<Arc<BakedLayer>> {
        &mut Arc::make_mut(&mut self.inner).baked_bg
    }

    pub fn objects(&self) -> &[DrawObject] { &self.inner.objects }
    /// Read-only access to the baked background.  Reserved for future
    /// callers (e.g. screenshot utilities that want to export just the
    /// background layer).
    #[allow(dead_code)]
    pub fn baked_bg(&self) -> Option<&BakedLayer> { self.inner.baked_bg.as_deref() }

    fn snapshot(&self) -> Snapshot {
        Arc::clone(&self.inner)
    }

    pub fn push_undo(&mut self) {
        self.undo_stack.push_back(self.snapshot());
        if self.undo_stack.len() > self.undo_limit { self.undo_stack.pop_front(); }
        self.redo_stack.clear();
        self.dirty = true;
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop_back() {
            self.redo_stack.push_back(self.snapshot());
            // `prev` is itself an `Arc<SceneInner>` — assign directly.  No
            // pixel copy, no object clone.
            self.inner    = prev;
            self.selected = Vec::new();
            self.dirty    = true;
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop_back() {
            self.undo_stack.push_back(self.snapshot());
            self.inner    = next;
            self.selected = Vec::new();
            self.dirty    = true;
        }
    }

    pub fn clear(&mut self) {
        self.push_undo();
        // Drop the live inner; subsequent mutations will rebuild via
        // Arc::make_mut.  For the common case (no other Arc shares this
        // inner) this is just a write — the old payload is freed.
        self.inner    = Arc::new(SceneInner {
            objects:  Vec::new(),
            baked_bg: None,
        });
        self.selected = Vec::new();
        self.dirty    = true;
    }

    pub fn add(&mut self, obj: DrawObject) {
        self.objects_mut().push(obj);
        self.dirty = true;
    }

    /// Layer-aware eraser: walk the live `objects` list, drop / trim each
    /// one that the eraser path crosses.  Any part of the eraser that
    /// doesn't intersect a live object falls through to `baked_bg` (the
    /// legacy "wipe to the layer below" path) — but since live objects
    /// always render on top of baked_bg, users see the more specific
    /// stroke-trim behaviour whenever there's something to trim.
    ///
    /// Mirrors Photoshop's "Eraser tool" on a vector layer: each stroked
    /// path is split at the eraser-disc boundary, keeping the surviving
    /// sub-strokes as separate objects so they remain selectable / movable.
    /// Returns `true` if anything was actually erased.
    pub fn erase_path(&mut self, eraser_path: &[(i32, i32)], radius: u32, canvas: &mut Canvas) -> bool {
        let r = radius as i32;
        if r <= 0 || eraser_path.is_empty() { return false; }
        self.push_undo();

        // 1. Walk the live objects list; replace in place with the (zero or
        //    more) surviving sub-objects.  Vector strokes get split into
        //    segments on either side of the eraser; small shapes (Rect,
        //    Circle, Arrow) get dropped entirely if the brush touches them.
        let mut touched_live = false;
        {
            let inner = Arc::make_mut(&mut self.inner);
            let mut new_objects: Vec<DrawObject> = Vec::with_capacity(inner.objects.len());
            for obj in inner.objects.drain(..) {
                if !obj.intersects_eraser(eraser_path, r) {
                    new_objects.push(obj);
                    continue;
                }
                match obj.apply_eraser(eraser_path, r) {
                    EraseResult::Unchanged => new_objects.push(obj),
                    EraseResult::Removed   => { touched_live = true; }
                    EraseResult::Replaced(repls) => {
                        touched_live = true;
                        new_objects.extend(repls);
                    }
                }
            }
            inner.objects = new_objects;
        }
        // Drop any selected indices that no longer exist (the trim above
        // removed some objects from the live list).
        self.selected.retain(|&i| i < self.inner.objects.len());

        // 2. Update baked_bg: alpha-zero at the brush path.  This wipes
        //    the brush footprint from anything that was already committed
        //    to baked_bg — most importantly the "cut" pixels between
        //    surviving sub-strokes that previously existed only as raster
        //    data.  Without this, those pixels would be ghost-visible
        //    because we no longer re-render the live scene into the bake.
        //
        //    We DELIBERATELY do NOT call `bake_from_canvas` here.  The
        //    previous implementation baked the trimmed live scene into a
        //    new baked_bg and cleared `objects`, which destroyed the
        //    sub-strokes' object identity and made them un-selectable /
        //    un-movable afterwards.  The pipeline already does the
        //    standard `baked_bg → live objects on top` composition every
        //    frame, so the visible result is identical — but the trimmed
        //    sub-strokes stay in `scene.objects` and remain fully
        //    selectable / movable post-erase (Photoshop "Erase to
        //    history" on a vector layer).
        let w = canvas.width as i32;
        let h = canvas.height as i32;
        let r2 = r * r;
        let mut touched_baked = false;
        let inner_mut = Arc::make_mut(&mut self.inner);
        if let Some(bg_outer) = inner_mut.baked_bg.as_mut() {
            if let Some(bg) = Arc::get_mut(bg_outer) {
                for &(px_, py_) in eraser_path {
                    let xa = (px_ - r).max(0);
                    let xb = (px_ + r).min(w - 1);
                    let ya = (py_ - r).max(0);
                    let yb = (py_ + r).min(h - 1);
                    for y in ya..=yb {
                        let dy = y - py_;
                        for x in xa..=xb {
                            let dx = x - px_;
                            if dx * dx + dy * dy > r2 { continue; }
                            let idx = ((y as u32 * bg.width + x as u32) * 4) as usize;
                            bg.pixels[idx + 3] = 0;
                        }
                    }
                }
                // Mirror the post-wipe bake into the caller's `canvas`
                // so the pipeline's next `submit_scene` copies a correct
                // base layer (the trimmed live objects will be drawn on
                // top of this by the worker).  `committed_canvas` is a
                // passive mirror of state; the worker re-rasterizes the
                // final scene on top anyway, so this copy is defensive
                // rather than load-bearing.
                canvas.pixels.copy_from_slice(&bg.pixels);
                canvas.dirty = true;
                touched_baked = true;
            }
        } else {
            // No prior bake exists — every visible pixel was a live
            // object.  The trim rewrote those live objects, so there are
            // no orphan pixels in the bake to wipe.  Re-render the
            // trimmed scene into canvas so the caller sees the new state
            // immediately (e.g. for a screenshot capture this frame).
            self.dirty = true;
            self.render_to(canvas);
        }

        self.dirty = true;
        touched_live || touched_baked
    }

    /// Bake the current committed-canvas state into a permanent background
    /// layer and clear all live objects.  After this call, nothing in the
    /// pre-erase scene is selectable or movable any more — but undo still
    /// works because [`push_undo`] is expected to have been called just
    /// before (the input handler does this on mouse-down).
    ///
    /// NOTE: `erase_path` no longer calls this — see the comment there
    /// for why the unconditional bake would have destroyed the trimmed
    /// sub-strokes' identity.  Kept as `#[allow(dead_code)]` for callers
    /// that explicitly want the collapse (e.g. a future "Flatten" tool).
    #[allow(dead_code)]
    pub fn bake_from_canvas(&mut self, canvas: &Canvas) {
        let bg = Arc::new(BakedLayer {
            width:  canvas.width,
            height: canvas.height,
            pixels: canvas.pixels.clone(),
        });
        // COW: if no other Arc shares the inner, this just overwrites the
        // Option in place.  If shared (e.g. on the undo stack), Arc::make_mut
        // clones the 8.3 MB once — that's the only time we pay the full
        // pixel copy cost in this code path.
        let inner = Arc::make_mut(&mut self.inner);
        inner.baked_bg = Some(bg);
        inner.objects.clear();
        self.selected = Vec::new();
        self.dirty    = true;
    }

    /// Render the visible scene into `canvas` (clear first).  Composition
    /// order: `baked_bg` (direct pixel copy) → live `objects`.  No-op if the
    /// scene isn't dirty.
    pub fn render_to(&mut self, canvas: &mut Canvas) {
        if !self.dirty { return; }
        canvas.clear();
        let baked_to_drop = if let Some(bg) = &self.inner.baked_bg {
            if bg.width == canvas.width && bg.height == canvas.height {
                let n = bg.pixels.len().min(canvas.pixels.len());
                canvas.pixels[..n].copy_from_slice(&bg.pixels[..n]);
                None
            } else {
                Some(Arc::clone(bg))
            }
        } else {
            None
        };
        if baked_to_drop.is_some() {
            Arc::make_mut(&mut self.inner).baked_bg = None;
        }
        // After potential `make_mut` invalidation above, walk objects freshly.
        for obj in &self.inner.objects {
            obj.render(canvas);
        }
        canvas.dirty = true;
        self.dirty = false;
    }

    /// Row-parallel render into `canvas`.  Splits the canvas height into
    /// chunks and fans out across rayon's worker pool.  Returns `true` if
    /// the scene was rendered, `false` if it was a no-op (not dirty).
    ///
    /// SAFETY contract for callers:
    /// * The `canvas.pixels` buffer MUST NOT be touched by any other thread
    ///   for the duration of this call (no main-thread blitting in parallel).
    /// * The caller must invoke `canvas.dirty = true` and `self.dirty = false`
    ///   after this returns — this function does NOT set them, because they
    ///   are `&mut` operations on `Canvas` / `Scene` and we want callers to
    ///   decide when to commit the work (e.g. after a worker thread joins).
    pub fn render_to_par(&self, canvas: &mut Canvas) -> bool {
        if !self.dirty { return false; }

        // 1. Clear + baked_bg copy are cheap (~8.3 MB memset + 8.3 MB memcpy)
        //    and don't benefit from parallelization — do them inline on the
        //    caller thread.
        canvas.clear();
        if let Some(bg) = &self.inner.baked_bg {
            if bg.width == canvas.width && bg.height == canvas.height {
                let n = bg.pixels.len().min(canvas.pixels.len());
                canvas.pixels[..n].copy_from_slice(&bg.pixels[..n]);
            }
        }

        // 2. Parallel object rasterization.  Split the canvas rows into
        //    roughly-equal chunks; each rayon worker walks every object and
        //    bounds-checks against its own row range, so non-overlapping
        //    objects touch only their owning chunk.
        let width     = canvas.width;
        let height    = canvas.height;
        let stride    = (width as usize) * 4;
        let rows_total = (canvas.pixels.len() / stride) as u32;
        let n_workers  = rayon::current_num_threads().max(1) as u32;
        let chunk_rows = (rows_total / n_workers).max(64);

        // `par_chunks_mut` gives each worker an exclusive `&mut [u8]` slice,
        // which sidesteps the `Sync` requirement for raw pointers.  We
        // borrow `self.objects` immutably across the call — safe because
        // the caller has guaranteed no concurrent mutation.
        let objects: &[DrawObject] = &self.inner.objects;
        let (w, h, stride_u, chunk_rows_u) = (width, height, stride, chunk_rows);
        canvas
            .pixels
            .par_chunks_mut(chunk_rows as usize * stride)
            .enumerate()
            .for_each(|(chunk_idx, chunk)| {
                let y_start = (chunk_idx as u32 * chunk_rows_u) as u32;
                let rows    = (chunk.len() / stride_u) as u32;
                // SAFETY: `chunk` is an exclusive `&mut [u8]` whose length
                // is `rows * stride_u`.  We never read outside this range
                // and `put_pixel` bounds-checks against `height`.
                let slice = SliceTarget {
                    pixels: chunk.as_mut_ptr(),
                    stride: stride_u,
                    width:  w,
                    height: h,
                    y_start,
                    rows,
                };
                for obj in objects {
                    obj.render_into_slice(&slice);
                }
            });

        true
    }

    /// Hit-test from the top (last-drawn) live object down.  Returns
    /// `Some(index)`.  The baked background is never hit-tested — it's
    /// intentionally not selectable.
    pub fn hit_test(&self, px: i32, py: i32) -> Option<usize> {
        for i in (0..self.inner.objects.len()).rev() {
            if self.inner.objects[i].hit_test(px, py) {
                return Some(i);
            }
        }
        None
    }

    // ── Multi-select helpers ────────────────────────────────────────────────

    /// Replace the selection with a single index.  Out-of-bounds indices
    /// are silently dropped (defensive against a stale UI snapshot).
    pub fn select_one(&mut self, idx: usize) {
        if idx < self.inner.objects.len() {
            self.selected = vec![idx];
        } else {
            self.selected.clear();
        }
    }

    //// Add an index to the selection if absent; do nothing if it's already
    /// present (so shift-clicking an already-selected object is a no-op
    /// for selection state, but `select_toggle` will *remove* it).
    #[allow(dead_code)]
    pub fn select_add(&mut self, idx: usize) {
        if idx >= self.inner.objects.len() { return; }
        if !self.selected.contains(&idx) {
            self.selected.push(idx);
            self.selected.sort_unstable();
        }
    }

    /// Toggle an index: add if absent, remove if present.  The selection
    /// stays sorted after a removal.
    pub fn select_toggle(&mut self, idx: usize) {
        if idx >= self.inner.objects.len() { return; }
        match self.selected.iter().position(|&i| i == idx) {
            Some(pos) => { self.selected.swap_remove(pos); }
            None      => { self.selected.push(idx); self.selected.sort_unstable(); }
        }
    }

    /// Add every object whose bounding box overlaps the rect (x0,y0)..(x1,y1).
    /// Coordinates are inclusive min / exclusive max.  Used by the marquee
    /// selection on mouse-up.  Returns the indices that were newly added
    /// so callers can decide whether to push an undo snapshot.
    pub fn select_add_in_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<usize> {
        let lx = x0.min(x1);
        let rx = x0.max(x1);
        let ty = y0.min(y1);
        let by = y0.max(y1);
        let mut added = Vec::new();
        for (i, obj) in self.inner.objects.iter().enumerate() {
            let (bx, by_, bw, bh) = obj.bounds();
            if bx + bw < lx || bx > rx || by_ + bh < ty || by_ > by { continue; }
            if !self.selected.contains(&i) {
                self.selected.push(i);
                added.push(i);
            }
        }
        self.selected.sort_unstable();
        added
    }

    /// Clear the selection.
    pub fn select_clear(&mut self) {
        self.selected.clear();
    }

    /// True if `idx` is in the current selection.
    pub fn is_selected(&self, idx: usize) -> bool {
        self.selected.contains(&idx)
    }

    /// Number of selected objects.  `0` is treated the same as "nothing
    /// selected" by all UI code.
    #[allow(dead_code)]
    pub fn selection_count(&self) -> usize {
        self.selected.len()
    }
}
