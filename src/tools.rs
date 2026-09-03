use crate::canvas::Canvas;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen, Line, Rect, Circle, Arrow, Text, Select, Eraser, Highlight, Laser,
}

impl Tool {
    pub fn index(self) -> usize {
        match self {
            Tool::Pen       => 0,
            Tool::Line      => 1,
            Tool::Rect      => 2,
            Tool::Circle    => 3,
            Tool::Arrow     => 4,
            Tool::Text      => 5,
            Tool::Select    => 6,
            Tool::Eraser    => 7,
            Tool::Highlight => 8,
            Tool::Laser     => 9,
        }
    }

    /// Stable lowercase name used by the REST API (`pen`, `line`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Tool::Pen       => "pen",
            Tool::Line      => "line",
            Tool::Rect      => "rect",
            Tool::Circle    => "circle",
            Tool::Arrow     => "arrow",
            Tool::Text      => "text",
            Tool::Select    => "select",
            Tool::Eraser    => "eraser",
            Tool::Highlight => "highlight",
            Tool::Laser     => "laser",
        }
    }

    /// Inverse of [`Tool::name`]; accepts any ASCII case.  `None` for unknown names.
    // Used by the Phase 2 control endpoints (api.rs) and the api.rs unit tests.
    #[allow(dead_code)]
    pub fn from_name(s: &str) -> Option<Tool> {
        Some(match s.to_ascii_lowercase().as_str() {
            "pen"       => Tool::Pen,
            "line"      => Tool::Line,
            "rect"      => Tool::Rect,
            "circle"    => Tool::Circle,
            "arrow"     => Tool::Arrow,
            "text"      => Tool::Text,
            "select"    => Tool::Select,
            "eraser"    => Tool::Eraser,
            "highlight" => Tool::Highlight,
            "laser"     => Tool::Laser,
            _ => return None,
        })
    }
}

pub const PALETTE: [(u8, u8, u8); 8] = [
    (255, 255, 255), (255,  51,  51), (255, 255,   0), ( 51, 153, 255),
    ( 51, 204,  51), (  0,   0,   0), (255, 153,  51), ( 51, 255, 255),
];

// ── Drawing functions ─────────────────────────────────────────────────────────

pub fn draw_circle_fill(canvas: &mut Canvas, cx: i32, cy: i32, radius: i32, r: u8, g: u8, b: u8, a: u8) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius { canvas.put_pixel(cx + dx, cy + dy, r, g, b, a); }
        }
    }
}

/// Draw a thick line from `(x0, y0)` to `(x1, y1)`.
/// `thickness` is the full width of the line in pixels (radius = thickness/2).
///
/// # TODO 7
/// Previous implementation did a Bresenham walk and stamped a filled disc
/// at every pixel — quadratic in segment length (a 200-pixel line wrote
/// ~200 × π·r² ≈ 8000 `put_pixel` calls per commit, and a 50-stroke
/// scene's commit paid this for every stroke).  The new implementation
/// uses a perpendicular-strip pass — two endpoint discs plus one
/// rectangle between them — linear in segment length regardless of
/// thickness.  For a 200-pixel line at thickness=8 this is ~40× fewer
/// `put_pixel` calls.
pub fn draw_line(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let len = (dx * dx + dy * dy).sqrt().max(0.001);
    // Unit vector along the line + perpendicular.
    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux);
    let half = (thickness as f32 * 0.5).max(0.5);

    // Stamp a disc at each endpoint so segment joints round off cleanly.
    draw_circle_fill(canvas, x0, y0, half as i32, r, g, b, a);
    draw_circle_fill(canvas, x1, y1, half as i32, r, g, b, a);

    // Fill the rotated rectangle between the endpoints.  We do this by
    // stamping `2*half+1` parallel lines perpendicular to the segment,
    // each running the full length — linear in `len`.
    let samples = half.ceil() as i32;
    for k in -samples..=samples {
        let ox = (px * k as f32).round() as i32;
        let oy = (py * k as f32).round() as i32;
        // Parameterised walk along the segment — the existing `draw_line`
        // we just replaced does Bresenham, but calling it recursively here
        // would re-introduce the quadratic cost.  Instead, walk integer
        // steps along the dominant axis.
        let steps = (len.ceil() as i32).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = x0 as f32 + dx * t + ox as f32;
            let y = y0 as f32 + dy * t + oy as f32;
            canvas.put_pixel(x.round() as i32, y.round() as i32, r, g, b, a);
        }
    }
}

pub fn draw_rect_outline(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (lx, rx) = (x0.min(x1), x0.max(x1));
    let (ty, by) = (y0.min(y1), y0.max(y1));
    draw_line(canvas, lx, ty, rx, ty, r, g, b, a, thickness);
    draw_line(canvas, lx, by, rx, by, r, g, b, a, thickness);
    draw_line(canvas, lx, ty, lx, by, r, g, b, a, thickness);
    draw_line(canvas, rx, ty, rx, by, r, g, b, a, thickness);
}

pub fn draw_ellipse_outline(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let (ra, rb) = (((x1 - x0).abs() / 2).max(1) as f64, ((y1 - y0).abs() / 2).max(1) as f64);
    let steps = (2.0 * std::f64::consts::PI * ra.max(rb)) as usize * 2;
    let mut prev: Option<(i32, i32)> = None;
    for i in 0..=steps {
        let t = (i as f64 / steps as f64) * 2.0 * std::f64::consts::PI;
        let (px, py) = ((cx as f64 + ra * t.cos()).round() as i32, (cy as f64 + rb * t.sin()).round() as i32);
        if let Some((px0, py0)) = prev { draw_line(canvas, px0, py0, px, py, r, g, b, a, thickness); }
        prev = Some((px, py));
    }
}

pub fn draw_arrow(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    draw_line(canvas, x0, y0, x1, y1, r, g, b, a, thickness);
    let (dx, dy) = ((x1 - x0) as f64, (y1 - y0) as f64);
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let (ux, uy) = (dx / len, dy / len);
    let (hl, hw) = ((thickness * 6).max(18) as f64, (thickness * 3).max(9) as f64);
    let (bx, bf) = (x1 as f64 - ux * hl, y1 as f64 - uy * hl);
    let p1 = (x1, y1);
    let p2 = ((bx - uy * hw).round() as i32, (bf + ux * hw).round() as i32);
    let p3 = ((bx + uy * hw).round() as i32, (bf - ux * hw).round() as i32);
    fill_triangle(canvas, p1, p2, p3, r, g, b, a);
}

fn fill_triangle(canvas: &mut Canvas, p1: (i32, i32), p2: (i32, i32), p3: (i32, i32), r: u8, g: u8, b: u8, a: u8) {
    let mut pts = [p1, p2, p3];
    pts.sort_by_key(|p| p.1);
    let [a0, a1, a2] = pts;
    let interp = |y: i32, p: (i32, i32), q: (i32, i32)| -> i32 {
        if p.1 == q.1 { return p.0; }
        p.0 + (y - p.1) * (q.0 - p.0) / (q.1 - p.1)
    };
    let row = |canvas: &mut Canvas, y: i32, xa: i32, xb: i32| {
        for x in xa.min(xb)..=xa.max(xb) { canvas.put_pixel(x, y, r, g, b, a); }
    };
    for y in a0.1..=a1.1 { row(canvas, y, interp(y, a0, a2), interp(y, a0, a1)); }
    for y in a1.1..=a2.1 { row(canvas, y, interp(y, a0, a2), interp(y, a1, a2)); }
}

pub fn draw_highlight(canvas: &mut Canvas, cx: i32, cy: i32, radius: i32, r: u8, g: u8, b: u8) {
    draw_circle_fill(canvas, cx, cy, radius, r, g, b, 100);
}

// ── Slice-based drawing (row-parallel, rayon's hot path) ────────────────────
//
// `SliceTarget` is the parallel-friendly equivalent of a sub-range of a
// `Canvas`.  It holds a raw pointer to the first pixel of the slice, the
// row stride in bytes (== full canvas width * 4), and the dimensions of the
// canvas.  All writers bounds-check against the full canvas extent before
// writing — each helper is responsible for skipping pixels outside its own
// row range, but uses the global width for x-bounds checking so that
// partial-row slices (e.g. one slice owns rows 100..200 of a 1000-row
// canvas) still draw correctly.
//
// Functions that take a `SliceTarget` MUST NOT touch pixels outside their
// own row range.  This is the only constraint rayon needs in order to fan
// out work safely.

/// A row-range of a `Canvas::pixels` buffer exposed for parallel writing.
pub struct SliceTarget {
    pub pixels: *mut u8,
    /// Full-canvas row stride in bytes (== canvas.width * 4).
    pub stride: usize,
    /// Canvas width in pixels.
    pub width:  u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// y coordinate of the first row in `pixels`.
    pub y_start: u32,
    /// Number of rows this slice owns.
    pub rows:    u32,
}

impl SliceTarget {
    /// SAFETY: caller must guarantee `pixels` points to at least
    /// `(y_start + rows) * stride` bytes of valid, aligned, writable memory
    /// and remains live for the duration of any borrow.
    #[inline]
    pub unsafe fn put_pixel(&self, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
        if x < 0 || y < 0 { return; }
        let (x, y) = (x as u32, y as u32);
        if x >= self.width || y >= self.height { return; }
        let row = (y - self.y_start) as usize;
        let idx = row * self.stride + x as usize * 4;
        // SAFETY: documented on the function; idx is in-bounds.
        unsafe {
            let p = self.pixels.add(idx);
            *p         = r;
            *p.add(1)  = g;
            *p.add(2)  = b;
            *p.add(3)  = a;
        }
    }
}

/// SAFETY: each `SliceTarget` is constructed per-worker with a non-
/// overlapping row range, so the underlying memory is never shared between
/// threads.  The `Send` impl is just to satisfy rayon's `par_iter`.
unsafe impl Send for SliceTarget {}
unsafe impl Sync for SliceTarget {}

pub fn slice_circle_fill(s: &SliceTarget, cx: i32, cy: i32, radius: i32, r: u8, g: u8, b: u8, a: u8) {
    let y_lo = (cy - radius).max(s.y_start as i32);
    let y_hi = (cy + radius).min((s.y_start + s.rows) as i32 - 1);
    for y in y_lo..=y_hi {
        let dy = y - cy;
        let dy2 = dy * dy;
        let dx_max = ((radius * radius - dy2).max(0) as f64).sqrt() as i32;
        let x_lo = cx - dx_max;
        let x_hi = cx + dx_max;
        for x in x_lo..=x_hi {
            unsafe { s.put_pixel(x, y, r, g, b, a); }
        }
    }
}

pub fn slice_line(s: &SliceTarget, mut x0: i32, mut y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    loop {
        slice_circle_fill(s, x0, y0, thickness / 2, r, g, b, a);
        if x0 == x1 && y0 == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x0 += sx; }
        if e2 <= dx { err += dx; y0 += sy; }
    }
}

pub fn slice_rect_outline(s: &SliceTarget, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (lx, rx) = (x0.min(x1), x0.max(x1));
    let (ty, by) = (y0.min(y1), y0.max(y1));
    slice_line(s, lx, ty, rx, ty, r, g, b, a, thickness);
    slice_line(s, lx, by, rx, by, r, g, b, a, thickness);
    slice_line(s, lx, ty, lx, by, r, g, b, a, thickness);
    slice_line(s, rx, ty, rx, by, r, g, b, a, thickness);
}

pub fn slice_ellipse_outline(s: &SliceTarget, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let (ra, rb) = (((x1 - x0).abs() / 2).max(1) as f64, ((y1 - y0).abs() / 2).max(1) as f64);
    let steps = (2.0 * std::f64::consts::PI * ra.max(rb)) as usize * 2;
    let mut prev: Option<(i32, i32)> = None;
    for i in 0..=steps {
        let t = (i as f64 / steps as f64) * 2.0 * std::f64::consts::PI;
        let (px, py) = ((cx as f64 + ra * t.cos()).round() as i32, (cy as f64 + rb * t.sin()).round() as i32);
        if let Some((px0, py0)) = prev { slice_line(s, px0, py0, px, py, r, g, b, a, thickness); }
        prev = Some((px, py));
    }
}

pub fn slice_arrow(s: &SliceTarget, x0: i32, y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    slice_line(s, x0, y0, x1, y1, r, g, b, a, thickness);
    let (dx, dy) = ((x1 - x0) as f64, (y1 - y0) as f64);
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let (ux, uy) = (dx / len, dy / len);
    let (hl, hw) = ((thickness * 6).max(18) as f64, (thickness * 3).max(9) as f64);
    let (bx, bf) = (x1 as f64 - ux * hl, y1 as f64 - uy * hl);
    let p1 = (x1, y1);
    let p2 = ((bx - uy * hw).round() as i32, (bf + ux * hw).round() as i32);
    let p3 = ((bx + uy * hw).round() as i32, (bf - ux * hw).round() as i32);
    slice_fill_triangle(s, p1, p2, p3, r, g, b, a);
}

fn slice_fill_triangle(s: &SliceTarget, p1: (i32, i32), p2: (i32, i32), p3: (i32, i32), r: u8, g: u8, b: u8, a: u8) {
    let mut pts = [p1, p2, p3];
    pts.sort_by_key(|p| p.1);
    let [a0, a1, a2] = pts;
    let interp = |y: i32, p: (i32, i32), q: (i32, i32)| -> i32 {
        if p.1 == q.1 { return p.0; }
        p.0 + (y - p.1) * (q.0 - p.0) / (q.1 - p.1)
    };
    let row = |s: &SliceTarget, y: i32, xa: i32, xb: i32| {
        for x in xa.min(xb)..=xa.max(xb) { unsafe { s.put_pixel(x, y, r, g, b, a); } }
    };
    for y in a0.1..=a1.1 { row(s, y, interp(y, a0, a2), interp(y, a0, a1)); }
    for y in a1.1..=a2.1 { row(s, y, interp(y, a0, a2), interp(y, a1, a2)); }
}

pub fn slice_highlight(s: &SliceTarget, cx: i32, cy: i32, radius: i32, r: u8, g: u8, b: u8) {
    slice_circle_fill(s, cx, cy, radius, r, g, b, 100);
}


