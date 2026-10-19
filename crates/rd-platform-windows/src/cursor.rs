//! The mouse pointer. Desktop Duplication delivers the desktop image *without* the pointer,
//! plus its shape and position separately, so the capture draws it into its copy of the
//! desktop before scaling (the macOS capture includes the pointer as well).
//!
//! Drawing happens on the CPU over the few pixels under the pointer, which keeps the exact
//! semantics of every shape type, including the XOR (inverting) parts of monochrome and
//! masked-colour pointers such as the text I-beam.

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// 1 bpp AND mask followed by a 1 bpp XOR mask (each `height` rows).
    Monochrome,
    /// 32 bpp BGRA with (straight) alpha.
    Color,
    /// 32 bpp BGR plus a mask byte: 0 = replace the screen pixel, 0xFF = XOR with it.
    MaskedColor,
}

impl ShapeKind {
    pub fn from_dxgi(ty: u32) -> Option<Self> {
        match ty {
            1 => Some(ShapeKind::Monochrome),
            2 => Some(ShapeKind::Color),
            4 => Some(ShapeKind::MaskedColor),
            _ => None,
        }
    }
}

/// A pointer shape as Desktop Duplication reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub kind: ShapeKind,
    pub width: u32,
    /// Visible height (half the bitmap's height for monochrome shapes).
    pub height: u32,
    /// Bytes per row of `data`.
    pub pitch: u32,
    pub data: Vec<u8>,
}

impl Shape {
    /// `DXGI_OUTDUPL_POINTER_SHAPE_INFO` fields plus the shape buffer.
    pub fn new(ty: u32, width: u32, height: u32, pitch: u32, data: Vec<u8>) -> Option<Self> {
        let kind = ShapeKind::from_dxgi(ty)?;
        let height = match kind {
            ShapeKind::Monochrome => height / 2,
            _ => height,
        };
        let rows = match kind {
            ShapeKind::Monochrome => height * 2,
            _ => height,
        };
        let min_pitch = match kind {
            ShapeKind::Monochrome => width.div_ceil(8),
            _ => width * 4,
        };
        let enough = pitch >= min_pitch && data.len() >= (pitch * rows) as usize;
        (width > 0 && height > 0 && enough).then_some(Shape {
            kind,
            width,
            height,
            pitch,
            data,
        })
    }

    /// Draws the pointer onto `dst`, a BGRA region of `dst_w x dst_h` pixels with rows of
    /// `dst_pitch` bytes. The shape's top-left corner lands at `(x, y)` in region coordinates
    /// (possibly negative or past the edge: the shape is clipped).
    pub fn draw(&self, dst: &mut [u8], dst_pitch: usize, dst_w: u32, dst_h: u32, x: i32, y: i32) {
        for sy in 0..self.height as i32 {
            let dy = y + sy;
            if dy < 0 || dy >= dst_h as i32 {
                continue;
            }
            for sx in 0..self.width as i32 {
                let dx = x + sx;
                if dx < 0 || dx >= dst_w as i32 {
                    continue;
                }
                let at = dy as usize * dst_pitch + dx as usize * 4;
                let px = &mut dst[at..at + 4];
                self.blend(sx as usize, sy as usize, px);
            }
        }
    }

    fn blend(&self, sx: usize, sy: usize, px: &mut [u8]) {
        let pitch = self.pitch as usize;
        match self.kind {
            ShapeKind::Monochrome => {
                let bit = 0x80u8 >> (sx % 8);
                let and = self.data[sy * pitch + sx / 8] & bit != 0;
                let xor = self.data[(sy + self.height as usize) * pitch + sx / 8] & bit != 0;
                for c in &mut px[..3] {
                    let v = if and { *c } else { 0 };
                    *c = if xor { v ^ 0xff } else { v };
                }
            }
            ShapeKind::Color => {
                let s = &self.data[sy * pitch + sx * 4..sy * pitch + sx * 4 + 4];
                let a = u32::from(s[3]);
                for i in 0..3 {
                    let v = (u32::from(s[i]) * a + u32::from(px[i]) * (255 - a) + 127) / 255;
                    px[i] = v as u8;
                }
            }
            ShapeKind::MaskedColor => {
                let s = &self.data[sy * pitch + sx * 4..sy * pitch + sx * 4 + 4];
                if s[3] == 0 {
                    px[..3].copy_from_slice(&s[..3]);
                } else {
                    for i in 0..3 {
                        px[i] ^= s[i];
                    }
                }
            }
        }
        px[3] = 0xff;
    }
}

/// Where the pointer is, as Desktop Duplication last reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pointer {
    pub visible: bool,
    /// Top-left corner of the shape, desktop (output) pixels.
    pub x: i32,
    pub y: i32,
    pub shape: Option<Shape>,
}

impl Pointer {
    /// The desktop rect the pointer covers, clipped to a `w x h` desktop: `(x, y, w, h)`.
    pub fn bounds(&self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let shape = self.shape.as_ref().filter(|_| self.visible)?;
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + shape.width as i32).min(w as i32);
        let y1 = (self.y + shape.height as i32).min(h as i32);
        (x1 > x0 && y1 > y0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    fn pixel(buf: &[u8], pitch: usize, x: usize, y: usize) -> [u8; 4] {
        buf[y * pitch + x * 4..y * pitch + x * 4 + 4]
            .try_into()
            .unwrap()
    }

    #[test]
    fn monochrome_and_xor_semantics() {
        // 8x1 visible: AND row then XOR row. Pixels: 0 black, 1 white, 2 transparent, 3 invert.
        let and = 0b0011_0000u8;
        let xor = 0b0101_0000u8;
        let shape = Shape::new(1, 8, 2, 1, vec![and, xor]).unwrap();
        assert_eq!(shape.height, 1);
        let mut dst = region(4, 1, [10, 20, 30, 255]);
        shape.draw(&mut dst, 16, 4, 1, 0, 0);
        assert_eq!(pixel(&dst, 16, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&dst, 16, 1, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&dst, 16, 2, 0), [10, 20, 30, 255]);
        assert_eq!(pixel(&dst, 16, 3, 0), [245, 235, 225, 255]);
    }

    #[test]
    fn colour_blends_with_alpha() {
        let data = [
            [0, 0, 255, 255], // opaque red
            [0, 0, 255, 0],   // transparent
            [255, 0, 0, 128], // half blue
        ]
        .concat();
        let shape = Shape::new(2, 3, 1, 12, data).unwrap();
        let mut dst = region(3, 1, [0, 255, 0, 255]);
        shape.draw(&mut dst, 12, 3, 1, 0, 0);
        assert_eq!(pixel(&dst, 12, 0, 0), [0, 0, 255, 255]);
        assert_eq!(pixel(&dst, 12, 1, 0), [0, 255, 0, 255]);
        assert_eq!(pixel(&dst, 12, 2, 0), [128, 127, 0, 255]);
    }

    #[test]
    fn masked_colour_replaces_or_inverts() {
        let data = [[1, 2, 3, 0], [0xff, 0xff, 0xff, 0xff]].concat();
        let shape = Shape::new(4, 2, 1, 8, data).unwrap();
        let mut dst = region(2, 1, [10, 20, 30, 255]);
        shape.draw(&mut dst, 8, 2, 1, 0, 0);
        assert_eq!(pixel(&dst, 8, 0, 0), [1, 2, 3, 255]);
        assert_eq!(pixel(&dst, 8, 1, 0), [245, 235, 225, 255]);
    }

    #[test]
    fn draws_clipped_at_every_edge() {
        let shape = Shape::new(2, 2, 2, 8, [[0, 0, 255, 255]; 4].concat()).unwrap();
        let mut dst = region(3, 3, [0, 0, 0, 255]);
        // Top-left corner partly outside: only (0,0) is covered.
        shape.draw(&mut dst, 12, 3, 3, -1, -1);
        assert_eq!(pixel(&dst, 12, 0, 0), [0, 0, 255, 255]);
        assert_eq!(pixel(&dst, 12, 1, 0), [0, 0, 0, 255]);
        // Bottom-right corner partly outside.
        shape.draw(&mut dst, 12, 3, 3, 2, 2);
        assert_eq!(pixel(&dst, 12, 2, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&dst, 12, 1, 2), [0, 0, 0, 255]);
    }

    #[test]
    fn rejects_short_or_unknown_shapes() {
        assert!(Shape::new(2, 4, 4, 16, vec![0; 63]).is_none());
        assert!(Shape::new(3, 4, 4, 16, vec![0; 64]).is_none());
        assert!(Shape::new(1, 8, 2, 1, vec![0; 1]).is_none());
        assert!(Shape::new(2, 4, 4, 16, vec![0; 64]).is_some());
    }

    #[test]
    fn pointer_bounds_are_clipped_to_the_desktop() {
        let shape = Shape::new(2, 32, 32, 128, vec![0; 128 * 32]);
        let mut p = Pointer {
            visible: true,
            x: -10,
            y: 1430,
            shape,
        };
        assert_eq!(p.bounds(2560, 1440), Some((0, 1430, 22, 10)));
        p.x = 2600;
        assert_eq!(p.bounds(2560, 1440), None);
        p.x = 0;
        p.visible = false;
        assert_eq!(p.bounds(2560, 1440), None);
    }
}
