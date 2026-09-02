use std::collections::VecDeque;

use crate::canvas::Canvas;
use crate::tools::{
    draw_circle_fill, draw_line, draw_rect_outline, draw_ellipse_outline,
    draw_arrow, draw_highlight,
    slice_circle_fill, slice_line, slice_rect_outline, slice_ellipse_outline,
    slice_arrow, slice_highlight, SliceTarget,
};
use rayon::prelude::*;

/// Newtype around a raw pixel pointer to satisfy rayon's `Sync` bound on
/// `for_each` closures.  Each rayon worker is given a non-overlapping
/// row range to write, so the pointer is logically per-worker — sharing
/// the *base* pointer is safe because no two workers write to the same
/// pixel range.
struct SharedPixelPtr(*mut u8);
unsafe impl Send for SharedPixelPtr {}
unsafe impl Sync for SharedPixelPtr {}

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
}

fn points_bounds(pts: &[(i32, i32)], pad: i32) -> (i32, i32, i32, i32) {
    if pts.is_empty() { return (0, 0, 0, 0); }
    let (mut lx, mut ty) = pts[0];
    let (mut rx, mut by) = pts[0];
    for &(x, y) in pts { lx = lx.min(x); ty = ty.min(y); rx = rx.max(x); by = by.max(y); }
    (lx - pad, ty - pad, rx - lx + pad * 2, by - ty + pad * 2)
}

// ── Scene ─────────────────────────────────────────────────────────────────────

/// One undo/redo snapshot.  Both the live object list AND the baked background
/// must be captured together because an erase commit moves objects out of
/// `objects` and into `baked_bg`; undoing needs to put them back coherently.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) objects:  Vec<DrawObject>,
    pub(crate) baked_bg: Option<BakedLayer>,
}

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
    pub objects:    Vec<DrawObject>,
    pub undo_stack: VecDeque<Snapshot>,
    pub redo_stack: VecDeque<Snapshot>,
    pub undo_limit: usize,
    /// Index of the selected object, if any.
    pub selected:   Option<usize>,
    /// Set whenever the scene is modified; cleared by render_to().
    pub dirty:      bool,
    /// Permanent baked raster set by erase commits.  Rendered under `objects`
    /// so the visible state is always `baked_bg + objects`.
    pub baked_bg:   Option<BakedLayer>,
}

// Clone is cheap-ish for the common path: only `objects` (DrawObject list)
// and `baked_bg` (8.3 MB of pixels at 1080p) are non-trivial.  We accept
// the cost on commit; in the future we can share via Arc<SceneInner>.
impl Clone for Scene {
    fn clone(&self) -> Self {
        Self {
            objects:    self.objects.clone(),
            undo_stack: self.undo_stack.clone(),
            redo_stack: self.redo_stack.clone(),
            undo_limit: self.undo_limit,
            selected:   self.selected,
            dirty:      self.dirty,
            baked_bg:   self.baked_bg.clone(),
        }
    }
}

impl Scene {
    pub fn new(undo_limit: usize) -> Self {
        Self {
            objects:    Vec::new(),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            undo_limit,
            selected:   None,
            dirty:      true,
            baked_bg:   None,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { objects: self.objects.clone(), baked_bg: self.baked_bg.clone() }
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
            self.objects   = prev.objects;
            self.baked_bg  = prev.baked_bg;
            self.selected  = None;
            self.dirty     = true;
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop_back() {
            self.undo_stack.push_back(self.snapshot());
            self.objects   = next.objects;
            self.baked_bg  = next.baked_bg;
            self.selected  = None;
            self.dirty     = true;
        }
    }

    pub fn clear(&mut self) {
        self.push_undo();
        self.objects.clear();
        self.baked_bg  = None;
        self.selected  = None;
        self.dirty     = true;
    }

    pub fn add(&mut self, obj: DrawObject) {
        self.objects.push(obj);
        self.dirty = true;
    }

    /// Bake the current committed-canvas state into a permanent background
    /// layer and clear all live objects.  After this call, nothing in the
    /// pre-erase scene is selectable or movable any more — but undo still
    /// works because [`push_undo`] is expected to have been called just
    /// before (the input handler does this on mouse-down).
    pub fn bake_from_canvas(&mut self, canvas: &Canvas) {
        self.baked_bg = Some(BakedLayer {
            width:  canvas.width,
            height: canvas.height,
            pixels: canvas.pixels.clone(),
        });
        self.objects.clear();
        self.selected = None;
        self.dirty    = true;
    }

    /// Render the visible scene into `canvas` (clear first).  Composition
    /// order: `baked_bg` (direct pixel copy) → live `objects`.  No-op if the
    /// scene isn't dirty.
    pub fn render_to(&mut self, canvas: &mut Canvas) {
        if !self.dirty { return; }
        canvas.clear();
        if let Some(bg) = &self.baked_bg {
            // Defensive: if dimensions changed (e.g. window resize before the
            // next resize-handler pass rebuilt the canvas), the baked bytes
            // are stale.  Discard them so the scene degrades to "objects
            // only" instead of crashing or showing garbled pixels.
            if bg.width == canvas.width && bg.height == canvas.height {
                let n = bg.pixels.len().min(canvas.pixels.len());
                canvas.pixels[..n].copy_from_slice(&bg.pixels[..n]);
            } else {
                self.baked_bg = None;
            }
        }
        for obj in &self.objects {
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
        if let Some(bg) = &self.baked_bg {
            if bg.width == canvas.width && bg.height == canvas.height {
                let n = bg.pixels.len().min(canvas.pixels.len());
                canvas.pixels[..n].copy_from_slice(&bg.pixels[..n]);
            }
            // Note: we don't reset self.baked_bg here because this is a
            // `&self` borrow.  If dimensions drift (window resize between
            // worker scheduling and execution), the next render_to_par call
            // will detect the mismatch via the same gate and the bake will
            // be discarded on the next synchronous render_to.
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
        let objects: &[DrawObject] = &self.objects;
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
        for i in (0..self.objects.len()).rev() {
            if self.objects[i].hit_test(px, py) {
                return Some(i);
            }
        }
        None
    }
}
