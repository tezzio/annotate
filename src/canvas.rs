use sdl2::render::Texture;

// ── Canvas ────────────────────────────────────────────────────────────────────

/// RGBA annotation layer.
pub struct Canvas {
    pub width:  u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub dirty:  bool,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, pixels: vec![0u8; (width * height * 4) as usize], dirty: false }
    }

    #[inline]
    pub fn put_pixel(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 { return; }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        self.pixels[idx] = r; self.pixels[idx+1] = g; self.pixels[idx+2] = b; self.pixels[idx+3] = a;
        self.dirty = true;
    }

    #[inline]
    pub fn erase_pixel(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 { return; }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        self.pixels[idx + 3] = 0;
        self.dirty = true;
    }

    pub fn clear(&mut self) { self.pixels.fill(0); self.dirty = true; }

    pub fn upload_texture(&mut self, texture: &mut Texture) {
        if !self.dirty { return; }
        texture.update(None, &self.pixels, (self.width * 4) as usize).expect("texture update");
        self.dirty = false;
    }
}
