//! Synthetic test pattern for [`crate::capture::SyntheticPipeline`]: IOSurface-backed NV12
//! (`420v`, BT.709 video range) pixel buffers from a `CVPixelBufferPool`, so the whole
//! capture → encode → viewer path can be exercised without screen-recording permission.
//!
//! The picture itself (colour bars, pseudo-text, the frame counter and clock as binary
//! blocks) is drawn by [`rd_agent_core::pattern`], shared with the Windows agent.

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
use rd_agent_core::pattern::{self, Nv12Planes};

use crate::cf;

pub(crate) struct TestPattern {
    pool: CFRetained<CVPixelBufferPool>,
    pattern: pattern::TestPattern,
}

// SAFETY: CVPixelBufferPool is thread-safe; everything else is plain owned data.
unsafe impl Send for TestPattern {}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        ensure!(
            width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "test pattern needs an even size of at least 64x64, got {width}x{height}"
        );
        Ok(TestPattern {
            pool: create_pool(width as usize, height as usize)?,
            pattern: pattern::TestPattern::new(width, height)?,
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
            let mut planes = Nv12Planes {
                luma_stride: luma.stride,
                chroma_stride: chroma.stride,
                luma: luma.bytes(),
                chroma: chroma.bytes(),
            };
            self.pattern.draw(&mut planes, frame, clock_us);
        }
        tag_bt709(&buffer);
        Ok(buffer)
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
    /// The whole plane.
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the plane is locked and spans `rows * stride` bytes; `&mut self` makes the
        // returned slice the only live reference into the plane.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.rows * self.stride) }
    }

    #[cfg(test)]
    fn row(&mut self, row: usize) -> &mut [u8] {
        assert!(row < self.rows, "row {row} out of {}", self.rows);
        let stride = self.stride;
        &mut self.bytes()[row * stride..(row + 1) * stride]
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
    use objc2_core_video::{
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    };
    use rd_agent_core::pattern::{Y_BLACK, Y_WHITE};

    use super::*;

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
