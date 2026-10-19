//! Synthetic NV12 test pattern (BT.709 video range) for the platforms' synthetic video
//! sources, so the whole capture -> encode -> viewer path can be exercised without screen
//! capture. Platforms render it into their own frame buffers through [`Nv12Planes`].
//!
//! Layout: colour bars scrolling sideways with a white band moving down over them; below that,
//! pseudo-text scrolling upwards like a terminal (realistic desktop-like content for the
//! encoder). Top left: the frame counter and the agent clock in ms as two rows of 32 binary
//! blocks (MSB first, white = 1).

use anyhow::ensure;

/// 75 % colour bars (white, yellow, cyan, green, magenta, red, blue, black) as RGB.
const BARS: [[f32; 3]; 8] = [
    [0.75, 0.75, 0.75],
    [0.75, 0.75, 0.0],
    [0.0, 0.75, 0.75],
    [0.0, 0.75, 0.0],
    [0.75, 0.0, 0.75],
    [0.75, 0.0, 0.0],
    [0.0, 0.0, 0.75],
    [0.0, 0.0, 0.0],
];
pub const Y_BLACK: u8 = 16;
pub const Y_WHITE: u8 = 235;
pub const C_NEUTRAL: u8 = 128;
const TEXT_TILE_ROWS: usize = 512;

/// Writable NV12 planes: `height` rows of luma and `height / 2` rows of interleaved CbCr, each
/// row at least `width` bytes.
pub struct Nv12Planes<'a> {
    pub luma: &'a mut [u8],
    pub luma_stride: usize,
    pub chroma: &'a mut [u8],
    pub chroma_stride: usize,
}

impl Nv12Planes<'_> {
    fn luma_row(&mut self, row: usize, width: usize) -> &mut [u8] {
        let start = row * self.luma_stride;
        &mut self.luma[start..start + width]
    }

    fn chroma_row(&mut self, row: usize, width: usize) -> &mut [u8] {
        let start = row * self.chroma_stride;
        &mut self.chroma[start..start + width]
    }
}

/// Renders the pattern; keeps the per-size tables between frames.
pub struct TestPattern {
    width: usize,
    height: usize,
    bar_colours: [(u8, u8, u8); 8],
    /// `TEXT_TILE_ROWS` rows of `width` luma bytes, scrolled vertically.
    text: Vec<u8>,
    bar_row_y: Vec<u8>,
    bar_row_uv: Vec<u8>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        ensure!(
            width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "test pattern needs an even size of at least 64x64, got {width}x{height}"
        );
        let (width, height) = (width as usize, height as usize);
        Ok(TestPattern {
            width,
            height,
            bar_colours: BARS.map(|[r, g, b]| ycbcr709(r, g, b)),
            text: text_tile(width),
            bar_row_y: vec![0; width],
            bar_row_uv: vec![0; width],
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// Renders frame number `frame`, stamped with `clock_us`, into `planes`.
    ///
    /// # Panics
    /// If the planes are too small for the pattern's size.
    pub fn draw(&mut self, planes: &mut Nv12Planes<'_>, frame: u64, clock_us: u64) {
        let (w, h) = (self.width, self.height);
        assert!(
            planes.luma_stride >= w
                && planes.chroma_stride >= w
                && planes.luma.len() >= planes.luma_stride * (h - 1) + w
                && planes.chroma.len() >= planes.chroma_stride * (h / 2 - 1) + w,
            "NV12 planes too small for {w}x{h}"
        );
        let bars_h = (h * 3 / 5) & !1;

        // Colour bars, scrolling right by 4 px per frame.
        let shift = (frame as usize * 4) % w;
        for x in 0..w {
            let (y, cb, cr) = self.bar_colours[((x + w - shift) % w) * 8 / w];
            self.bar_row_y[x] = y;
            self.bar_row_uv[x] = if x % 2 == 0 { cb } else { cr };
        }
        for row in 0..bars_h {
            planes.luma_row(row, w).copy_from_slice(&self.bar_row_y);
        }
        for row in 0..bars_h / 2 {
            planes.chroma_row(row, w).copy_from_slice(&self.bar_row_uv);
        }

        // White band moving down over the bars.
        let band_h = ((h / 40).max(2)) & !1;
        let travel = bars_h.saturating_sub(band_h).max(1);
        let band_top = ((frame as usize * 6) % travel) & !1;
        fill(planes, w, h, (0, band_top), (w, band_top + band_h), Y_WHITE);

        // Pseudo-text scrolling up by 2 px per frame.
        let scroll = (frame as usize * 2) % TEXT_TILE_ROWS;
        for row in bars_h..h {
            let src = (row - bars_h + scroll) % TEXT_TILE_ROWS * w;
            planes
                .luma_row(row, w)
                .copy_from_slice(&self.text[src..src + w]);
        }
        for row in bars_h / 2..h / 2 {
            planes.chroma_row(row, w).fill(C_NEUTRAL);
        }

        // Frame counter and clock (ms), truncated to 32 bits.
        let block = ((w / 96).max(8)) & !1;
        draw_bits(planes, w, h, block, 0, frame as u32);
        draw_bits(planes, w, h, block, 1, (clock_us / 1000) as u32);
    }
}

/// Fills the luma rect from `(x0, y0)` to `(x1, y1)` (exclusive, even coordinates) with `y`
/// and neutral chroma, clipped to the `w x h` frame.
fn fill(
    planes: &mut Nv12Planes<'_>,
    w: usize,
    h: usize,
    (x0, y0): (usize, usize),
    (x1, y1): (usize, usize),
    y: u8,
) {
    let x1 = x1.min(w);
    if x0 >= x1 {
        return;
    }
    for row in y0..y1.min(h) {
        planes.luma_row(row, w)[x0..x1].fill(y);
    }
    for row in y0 / 2..(y1 / 2).min(h / 2) {
        planes.chroma_row(row, w)[x0..x1].fill(C_NEUTRAL);
    }
}

/// Draws `value` as 32 blocks in strip `strip` (0 = top) at the top-left corner.
fn draw_bits(
    planes: &mut Nv12Planes<'_>,
    w: usize,
    h: usize,
    block: usize,
    strip: usize,
    value: u32,
) {
    let margin = (block / 2) & !1;
    let y0 = margin + strip * block;
    fill(
        planes,
        w,
        h,
        (margin, y0),
        (margin + 32 * block, y0 + block),
        Y_BLACK,
    );
    for bit in 0..32 {
        if value & (1 << (31 - bit)) != 0 {
            let x0 = margin + bit * block;
            fill(
                planes,
                w,
                h,
                (x0 + 2, y0 + 2),
                (x0 + block - 2, y0 + block - 2),
                Y_WHITE,
            );
        }
    }
}

/// BT.709 video-range Y'CbCr for gamma-encoded RGB in 0..=1.
pub fn ycbcr709(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (
        q(16.0 + 219.0 * y),
        q(128.0 + 224.0 * cb),
        q(128.0 + 224.0 * cr),
    )
}

/// Dark "glyphs" on a light background: lines of random 6x12 bitmaps with word gaps and a
/// ragged right edge, scaled 2x on Retina-sized frames.
fn text_tile(width: usize) -> Vec<u8> {
    let scale = (width / 1280).max(1);
    let (cell_w, line_h) = (8 * scale, 16 * scale);
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut tile = vec![Y_WHITE; TEXT_TILE_ROWS * width];
    for line_top in (0..TEXT_TILE_ROWS).step_by(line_h) {
        let line_cells = width / cell_w * (30 + rng.below(70)) / 100;
        for cell in 0..line_cells {
            if rng.below(100) < 15 {
                continue; // word gap
            }
            let glyph = rng.next();
            for gy in 0..12 {
                for gx in 0..6 {
                    if glyph >> ((gy * 6 + gx) % 64) & 1 == 0 || rng.below(100) < 35 {
                        continue;
                    }
                    for sy in 0..scale {
                        let row = line_top + (2 + gy) * scale + sy;
                        if row >= TEXT_TILE_ROWS {
                            continue;
                        }
                        let x = cell * cell_w + gx * scale;
                        tile[row * width + x..row * width + x + scale].fill(40);
                    }
                }
            }
        }
    }
    tile
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_reference_colours() {
        assert_eq!(ycbcr709(0.0, 0.0, 0.0), (16, 128, 128));
        assert_eq!(ycbcr709(1.0, 1.0, 1.0), (235, 128, 128));
        assert_eq!(ycbcr709(1.0, 0.0, 0.0), (63, 102, 240));
    }

    #[test]
    fn renders_the_counter_into_strided_planes() {
        let (w, h, stride) = (320usize, 180usize, 384usize);
        let mut luma = vec![0u8; stride * h];
        let mut chroma = vec![0u8; stride * h / 2];
        let mut pattern = TestPattern::new(w as u32, h as u32).unwrap();
        for frame in 0..3 {
            let mut planes = Nv12Planes {
                luma: &mut luma,
                luma_stride: stride,
                chroma: &mut chroma,
                chroma_stride: stride,
            };
            pattern.draw(&mut planes, frame, 5_000_000);
            // Block 0 is the counter's MSB (0 here); block 31 is its LSB.
            let (block, margin) = (8, 4);
            let row = &luma[(margin + block / 2) * stride..];
            let centre = |bit: usize| row[margin + bit * block + block / 2];
            assert_eq!(centre(0), Y_BLACK);
            assert_eq!(centre(31), if frame & 1 == 1 { Y_WHITE } else { Y_BLACK });
        }
        // Nothing is written past each row's width.
        assert!(luma[w..stride].iter().all(|&b| b == 0));
        assert!(chroma[w..stride].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_odd_sizes() {
        assert!(TestPattern::new(321, 180).is_err());
        assert!(TestPattern::new(32, 32).is_err());
    }
}
