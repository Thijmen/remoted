//! Synthetic test pattern for [`crate::capture::SyntheticPipeline`]: IOSurface-backed NV12
//! (`420v`, BT.709 video range) pixel buffers from a `CVPixelBufferPool`, so the whole
//! capture → encode → viewer path can be exercised without screen-recording permission.
//!
//! Layout: colour bars scrolling sideways with a white band moving down over them; below that,
//! pseudo-text scrolling upwards like a terminal (realistic desktop-like content for the
//! encoder). Top left: the frame counter and the agent clock in ms as two rows of 32 binary
//! blocks (MSB first, white = 1).

use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use anyhow::{Context as _, bail, ensure};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferPoolMinimumBufferCountKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

use crate::cf;

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
const Y_BLACK: u8 = 16;
const Y_WHITE: u8 = 235;
const C_NEUTRAL: u8 = 128;
const TEXT_TILE_ROWS: usize = 512;

pub(crate) struct TestPattern {
    pool: CFRetained<CVPixelBufferPool>,
    width: usize,
    height: usize,
    bar_colours: [(u8, u8, u8); 8],
    /// `TEXT_TILE_ROWS` rows of `width` luma bytes, scrolled vertically.
    text: Vec<u8>,
    bar_row_y: Vec<u8>,
    bar_row_uv: Vec<u8>,
}

// SAFETY: CVPixelBufferPool is thread-safe; everything else is plain owned data.
unsafe impl Send for TestPattern {}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        ensure!(
            width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "test pattern needs an even size of at least 64x64, got {width}x{height}"
        );
        let (width, height) = (width as usize, height as usize);
        Ok(TestPattern {
            pool: create_pool(width, height)?,
            width,
            height,
            bar_colours: BARS.map(|[r, g, b]| ycbcr709(r, g, b)),
            text: text_tile(width),
            bar_row_y: vec![0; width],
            bar_row_uv: vec![0; width],
        })
    }

    /// Renders frame number `frame`, stamped with `clock_us`, into a fresh pool buffer.
    pub fn render(
        &mut self,
        frame: u64,
        clock_us: u64,
    ) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: valid pool; `out` receives a +1 pixel buffer on success.
        let rc = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out))
        };
        if rc != 0 {
            bail!("CVPixelBufferPoolCreatePixelBuffer failed: {rc}");
        }
        let out = NonNull::new(out).context("pool returned no pixel buffer")?;
        // SAFETY: +1 reference from the pool.
        let buffer = unsafe { CFRetained::from_raw(out) };
        {
            let mut lock = LockedPlanes::lock(&buffer)?;
            let (mut luma, mut chroma) = lock.planes();
            self.draw(&mut luma, &mut chroma, frame, clock_us);
        }
        tag_bt709(&buffer);
        Ok(buffer)
    }

    fn draw(&mut self, luma: &mut Plane<'_>, chroma: &mut Plane<'_>, frame: u64, clock_us: u64) {
        let (w, h) = (self.width, self.height);
        let bars_h = (h * 3 / 5) & !1;

        // Colour bars, scrolling right by 4 px per frame.
        let shift = (frame as usize * 4) % w;
        for x in 0..w {
            let (y, cb, cr) = self.bar_colours[((x + w - shift) % w) * 8 / w];
            self.bar_row_y[x] = y;
            self.bar_row_uv[x] = if x % 2 == 0 { cb } else { cr };
        }
        for row in 0..bars_h {
            luma.row(row)[..w].copy_from_slice(&self.bar_row_y);
        }
        for row in 0..bars_h / 2 {
            chroma.row(row)[..w].copy_from_slice(&self.bar_row_uv);
        }

        // White band moving down over the bars.
        let band_h = ((h / 40).max(2)) & !1;
        let travel = bars_h.saturating_sub(band_h).max(1);
        let band_top = ((frame as usize * 6) % travel) & !1;
        fill(luma, chroma, (0, band_top), (w, band_top + band_h), Y_WHITE);

        // Pseudo-text scrolling up by 2 px per frame.
        let scroll = (frame as usize * 2) % TEXT_TILE_ROWS;
        for row in bars_h..h {
            let src = (row - bars_h + scroll) % TEXT_TILE_ROWS * w;
            luma.row(row)[..w].copy_from_slice(&self.text[src..src + w]);
        }
        for row in bars_h / 2..h / 2 {
            chroma.row(row)[..w].fill(C_NEUTRAL);
        }

        // Frame counter and clock (ms), truncated to 32 bits.
        let block = ((w / 96).max(8)) & !1;
        draw_bits(luma, chroma, block, 0, frame as u32);
        draw_bits(luma, chroma, block, 1, (clock_us / 1000) as u32);
    }
}

/// One locked plane: `rows` rows of `stride` bytes, writable for `'a`.
struct Plane<'a> {
    base: *mut u8,
    stride: usize,
    rows: usize,
    _locked: PhantomData<&'a mut [u8]>,
}

impl Plane<'_> {
    fn row(&mut self, row: usize) -> &mut [u8] {
        assert!(row < self.rows, "row {row} out of {}", self.rows);
        // SAFETY: the plane is locked and spans `rows * stride` bytes; `&mut self` makes the
        // returned row the only live reference into the plane.
        unsafe { std::slice::from_raw_parts_mut(self.base.add(row * self.stride), self.stride) }
    }
}

/// Base-address lock on a pixel buffer, released on drop.
struct LockedPlanes<'a> {
    buffer: &'a CVPixelBuffer,
}

impl<'a> LockedPlanes<'a> {
    fn lock(buffer: &'a CVPixelBuffer) -> anyhow::Result<Self> {
        // SAFETY: valid pixel buffer; balanced by the unlock in Drop.
        let rc = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        ensure!(rc == 0, "CVPixelBufferLockBaseAddress failed: {rc}");
        Ok(LockedPlanes { buffer })
    }

    /// The (disjoint) luma and interleaved-chroma planes.
    fn planes(&mut self) -> (Plane<'_>, Plane<'_>) {
        let plane = |index| {
            let base = CVPixelBufferGetBaseAddressOfPlane(self.buffer, index).cast::<u8>();
            assert!(
                !base.is_null(),
                "pixel buffer plane {index} has no base address"
            );
            Plane {
                base,
                stride: CVPixelBufferGetBytesPerRowOfPlane(self.buffer, index),
                rows: CVPixelBufferGetHeightOfPlane(self.buffer, index),
                _locked: PhantomData,
            }
        };
        (plane(0), plane(1))
    }
}

impl Drop for LockedPlanes<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `lock` with the same flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags(0)) };
    }
}

/// Fills the luma rect from `(x0, y0)` to `(x1, y1)` (exclusive, even coordinates) with `y`
/// and neutral chroma, clipped to the planes.
fn fill(
    luma: &mut Plane<'_>,
    chroma: &mut Plane<'_>,
    (x0, y0): (usize, usize),
    (x1, y1): (usize, usize),
    y: u8,
) {
    let x1 = x1.min(luma.stride).min(chroma.stride);
    if x0 >= x1 {
        return;
    }
    for row in y0..y1.min(luma.rows) {
        luma.row(row)[x0..x1].fill(y);
    }
    for row in y0 / 2..(y1 / 2).min(chroma.rows) {
        chroma.row(row)[x0..x1].fill(C_NEUTRAL);
    }
}

/// Draws `value` as 32 blocks in strip `strip` (0 = top) at the top-left corner.
fn draw_bits(luma: &mut Plane<'_>, chroma: &mut Plane<'_>, block: usize, strip: usize, value: u32) {
    let margin = (block / 2) & !1;
    let y0 = margin + strip * block;
    fill(
        luma,
        chroma,
        (margin, y0),
        (margin + 32 * block, y0 + block),
        Y_BLACK,
    );
    for bit in 0..32 {
        if value & (1 << (31 - bit)) != 0 {
            let x0 = margin + bit * block;
            fill(
                luma,
                chroma,
                (x0 + 2, y0 + 2),
                (x0 + block - 2, y0 + block - 2),
                Y_WHITE,
            );
        }
    }
}

/// BT.709 video-range Y'CbCr for gamma-encoded RGB in 0..=1.
fn ycbcr709(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
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

fn create_pool(width: usize, height: usize) -> anyhow::Result<CFRetained<CVPixelBufferPool>> {
    let io_surface = CFDictionary::<CFString, CFType>::empty();
    // SAFETY (statics): extern constants provided by CoreVideo.
    let buffer_attrs = cf::dict([
        (
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            &CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32),
        ),
        (
            unsafe { kCVPixelBufferWidthKey },
            &CFNumber::new_i32(width as i32),
        ),
        (
            unsafe { kCVPixelBufferHeightKey },
            &CFNumber::new_i32(height as i32),
        ),
        (unsafe { kCVPixelBufferIOSurfacePropertiesKey }, &io_surface),
    ]);
    let pool_attrs = cf::dict([(
        unsafe { kCVPixelBufferPoolMinimumBufferCountKey },
        &CFNumber::new_i32(4),
    )]);
    let mut out: *mut CVPixelBufferPool = ptr::null_mut();
    // SAFETY: valid attribute dictionaries; `out` receives a +1 pool on success.
    let rc = unsafe {
        CVPixelBufferPool::create(
            None,
            Some(pool_attrs.as_opaque()),
            Some(buffer_attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    if rc != 0 {
        bail!("CVPixelBufferPoolCreate({width}x{height}, 420v) failed: {rc}");
    }
    let out = NonNull::new(out).context("CVPixelBufferPoolCreate returned no pool")?;
    // SAFETY: +1 reference from Create.
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// Same colour tags ScreenCaptureKit attaches to its `420v` frames.
fn tag_bt709(buffer: &CVPixelBuffer) {
    // SAFETY: valid buffer; keys and values are the documented CoreVideo constants.
    unsafe {
        let mode = CVAttachmentMode::ShouldPropagate;
        buffer.set_attachment(
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
            mode,
        );
        buffer.set_attachment(
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_ITU_R_709_2,
            mode,
        );
        buffer.set_attachment(
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            mode,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::{
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    };

    #[test]
    fn bt709_reference_colours() {
        assert_eq!(ycbcr709(0.0, 0.0, 0.0), (16, 128, 128));
        assert_eq!(ycbcr709(1.0, 1.0, 1.0), (235, 128, 128));
        let (y, cb, cr) = ycbcr709(1.0, 0.0, 0.0);
        assert_eq!((y, cb, cr), (63, 102, 240));
    }

    #[test]
    fn renders_nv12_frames_with_counter() {
        let mut pattern = TestPattern::new(320, 180).unwrap();
        for frame in 0..3 {
            let buffer = pattern.render(frame, 5_000_000).unwrap();
            assert_eq!(CVPixelBufferGetWidth(&buffer), 320);
            assert_eq!(CVPixelBufferGetHeight(&buffer), 180);
            assert_eq!(
                CVPixelBufferGetPixelFormatType(&buffer),
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            );
            let mut lock = LockedPlanes::lock(&buffer).unwrap();
            let (mut luma, _) = lock.planes();
            // Block 0 is the counter's MSB (0 here); block 31 is its LSB.
            let (block, margin) = (8, 4);
            let row = luma.row(margin + block / 2);
            let centre = |bit: usize| row[margin + bit * block + block / 2];
            assert_eq!(centre(0), Y_BLACK);
            assert_eq!(centre(31), if frame & 1 == 1 { Y_WHITE } else { Y_BLACK });
        }
    }

    #[test]
    fn rejects_odd_sizes() {
        assert!(TestPattern::new(321, 180).is_err());
    }
}
