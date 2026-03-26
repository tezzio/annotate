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

pub fn draw_line(canvas: &mut Canvas, mut x0: i32, mut y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8, a: u8, thickness: i32) {
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    loop {
        draw_circle_fill(canvas, x0, y0, thickness / 2, r, g, b, a);
        if x0 == x1 && y0 == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x0 += sx; }
        if e2 <= dx { err += dx; y0 += sy; }
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

pub fn draw_erase(canvas: &mut Canvas, cx: i32, cy: i32, radius: i32) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius { canvas.erase_pixel(cx + dx, cy + dy); }
        }
    }
}


