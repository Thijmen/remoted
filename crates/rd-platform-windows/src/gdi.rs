//! GDI screen capture: the fallback for a display no D3D11 device can be had on.
//!
//! Desktop Duplication needs a WDDM 1.2 driver and a D3D11 device with video support. A PC
//! without a graphics driver (the Microsoft Basic Display Adapter is WDDM 1.1), a virtual
//! machine's display adapter or an indirect display (USB/dock) offers neither, and
//! `D3D11CreateDevice` fails with `DXGI_ERROR_UNSUPPORTED`: Direct3D would only be created for
//! feature level 9.x there, which [`crate::d3d`] does not ask for.
//!
//! [`Capture`] takes the picture with `BitBlt`, draws the pointer in, fits the display into the
//! frame with `StretchBlt` (bars black) and converts the result to NV12 on the CPU
//! ([`rd_agent_core::nv12`]), so Media Foundation's software encoder can send it. It needs no
//! Direct3D at all. Everything happens on the capture thread, which owns the DCs.
//!
//! This is the slow path: every frame crosses the CPU a few times, so the pipeline captures at
//! most [`MAX_FPS`] frames per second through it.

use std::ffi::c_void;
use std::mem::size_of;

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::nv12;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BLACKNESS, BitBlt, CAPTUREBLT,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
    GetObjectW, HALFTONE, HBITMAP, HDC, HGDIOBJ, PatBlt, ReleaseDC, SRCCOPY, SelectObject,
    SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};

use crate::display::Monitor;
use crate::encoder::Nv12Image;

/// Frames per second at most on this path; it copies the whole desktop through the CPU.
pub const MAX_FPS: u32 = 30;

/// One display, captured with GDI into frame-sized NV12 images.
pub struct Capture {
    monitor: Monitor,
    /// The screen: where `BitBlt` reads the desktop from.
    screen: HDC,
    /// The frame: the desktop fitted into `width x height`, bars black.
    frame: Surface,
    width: u32,
    height: u32,
    /// Where the display sits in the frame, pixels `(left, top, right, bottom)`.
    content: (i32, i32, i32, i32),
    /// The last frame's BGRA pixels, to tell whether anything changed. `None` until the first
    /// capture, which is always new.
    previous: Option<Vec<u8>>,
}

impl Capture {
    /// Captures `monitor` into `width x height` frames, with the display in `content`.
    pub fn new(
        monitor: Monitor,
        width: u32,
        height: u32,
        content: (i32, i32, i32, i32),
    ) -> anyhow::Result<Self> {
        check(width, height, content)?;
        // SAFETY: no preconditions; the DC is released in Drop.
        let screen = unsafe { GetDC(None) };
        if screen.is_invalid() {
            bail!("GetDC(NULL) failed");
        }
        let frame = match Surface::new(screen, width, height) {
            Ok(frame) => frame,
            Err(e) => {
                // SAFETY: balances GetDC above.
                unsafe { ReleaseDC(None, screen) };
                return Err(e);
            }
        };
        Ok(Capture {
            monitor,
            screen,
            frame,
            width,
            height,
            content,
            previous: None,
        })
    }

    pub fn monitor(&self) -> &Monitor {
        &self.monitor
    }

    /// Follows a changed display size or mode: a new frame surface, and the next capture is
    /// new. The old surface stays if the new one cannot be created.
    pub fn resize(
        &mut self,
        monitor: Monitor,
        width: u32,
        height: u32,
        content: (i32, i32, i32, i32),
    ) -> anyhow::Result<()> {
        check(width, height, content)?;
        let frame = Surface::new(self.screen, width, height)?;
        self.frame = frame;
        self.monitor = monitor;
        self.width = width;
        self.height = height;
        self.content = content;
        self.previous = None;
        Ok(())
    }

    /// Takes the picture into `image`; `true` when it differs from the previous one.
    pub fn capture(&mut self, image: &mut Nv12Image) -> anyhow::Result<bool> {
        let (left, top, right, bottom) = self.content;
        let (width, height) = (self.width as i32, self.height as i32);
        let (content_width, content_height) = (right - left, bottom - top);
        let rect = self.monitor.rect;
        let full = (content_width, content_height) == (width, height) && (left, top) == (0, 0);
        let size = (self.width * 4) as usize;
        // SAFETY: the DCs and their bitmaps are valid for as long as `self`; the rectangles
        // lie within the frame (checked in `new`) and the display (the monitor's own rect).
        unsafe {
            if !full {
                // The bars around the display.
                PatBlt(self.frame.dc, 0, 0, width, height, BLACKNESS).ok()?;
            }
            if content_width == rect.w as i32 && content_height == rect.h as i32 {
                BitBlt(
                    self.frame.dc,
                    left,
                    top,
                    content_width,
                    content_height,
                    Some(self.screen),
                    rect.x,
                    rect.y,
                    SRCCOPY | CAPTUREBLT,
                )?;
            } else {
                // Smooth downscaling, as a scaled capture should look like the desktop.
                SetStretchBltMode(self.frame.dc, HALFTONE);
                let _ = SetBrushOrgEx(self.frame.dc, 0, 0, None);
                StretchBlt(
                    self.frame.dc,
                    left,
                    top,
                    content_width,
                    content_height,
                    Some(self.screen),
                    rect.x,
                    rect.y,
                    rect.w as i32,
                    rect.h as i32,
                    SRCCOPY | CAPTUREBLT,
                )
                .ok()?;
            }
        }
        self.draw_pointer()?;
        let pixels = self.frame.pixels();
        if self.previous.as_deref() == Some(pixels) {
            return Ok(false);
        }
        nv12::bgra_to_nv12(pixels, size, self.width, self.height, &mut image.planes())?;
        match &mut self.previous {
            Some(previous) => previous.copy_from_slice(pixels),
            None => self.previous = Some(pixels.to_vec()),
        }
        Ok(true)
    }

    /// Draws the pointer into the frame, scaled with the display, when it is on it.
    ///
    /// GDI does not include the cursor in `BitBlt`; `DrawIconEx` composites the cursor's own
    /// mask and alpha, which is how a capture tool shows it.
    fn draw_pointer(&self) -> anyhow::Result<()> {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a CURSORINFO with cbSize set, as the API requires.
        unsafe { GetCursorInfo(&mut info) }.context("GetCursorInfo")?;
        if info.flags != CURSOR_SHOWING {
            return Ok(());
        }
        let rect = self.monitor.rect;
        let (x, y) = (info.ptScreenPos.x, info.ptScreenPos.y);
        if x < rect.x || y < rect.y || x >= rect.x + rect.w as i32 || y >= rect.y + rect.h as i32 {
            return Ok(());
        }
        let mut icon = ICONINFO::default();
        // SAFETY: a valid cursor handle; GetIconInfo copies the hotspot and creates bitmaps,
        // which `bitmaps` deletes again. The cursor itself belongs to the system.
        unsafe { GetIconInfo(info.hCursor.into(), &mut icon) }.context("GetIconInfo")?;
        let bitmaps = IconBitmaps(icon);
        let (cursor_width, cursor_height) = bitmaps.size().unwrap_or((32, 32));
        // Through the content rect, as the desktop was.
        let (left, top, right, bottom) = self.content;
        let scale_x = f64::from(right - left) / f64::from(rect.w);
        let scale_y = f64::from(bottom - top) / f64::from(rect.h);
        let x = f64::from(left) + (f64::from(x - rect.x) - f64::from(bitmaps.0.xHotspot)) * scale_x;
        let y = f64::from(top) + (f64::from(y - rect.y) - f64::from(bitmaps.0.yHotspot)) * scale_y;
        let cursor_width = ((f64::from(cursor_width) * scale_x).round() as i32).max(1);
        let cursor_height = ((f64::from(cursor_height) * scale_y).round() as i32).max(1);
        // SAFETY: valid DC and cursor; the icon's bitmaps outlive the call.
        unsafe {
            DrawIconEx(
                self.frame.dc,
                x.round() as i32,
                y.round() as i32,
                HICON(info.hCursor.0),
                cursor_width,
                cursor_height,
                0,
                None,
                DI_NORMAL,
            )
        }
        .context("DrawIconEx")
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the DC taken in `new`; released exactly once.
        unsafe { ReleaseDC(None, self.screen) };
    }
}

/// What [`Capture`] needs to be a valid frame.
fn check(width: u32, height: u32, content: (i32, i32, i32, i32)) -> anyhow::Result<()> {
    ensure!(
        width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "frame size {width}x{height} must be even and at least 64x64"
    );
    let (left, top, right, bottom) = content;
    ensure!(
        left >= 0
            && top >= 0
            && right <= width as i32
            && bottom <= height as i32
            && right > left
            && bottom > top,
        "content rect {content:?} does not fit {width}x{height}"
    );
    Ok(())
}

/// The bitmaps `GetIconInfo` creates; deleted when this goes, also on the success path.
struct IconBitmaps(ICONINFO);

impl IconBitmaps {
    /// The cursor's pixel size. A monochrome cursor has only a mask, which is two cursors tall
    /// (the AND mask above the XOR mask).
    fn size(&self) -> Option<(i32, i32)> {
        let mono = self.0.hbmColor.is_invalid();
        let bitmap = if mono {
            self.0.hbmMask
        } else {
            self.0.hbmColor
        };
        if bitmap.is_invalid() {
            return None;
        }
        let mut info = BITMAP::default();
        // SAFETY: a valid bitmap; `info` is a BITMAP for GetObjectW to fill.
        let size = size_of::<BITMAP>() as i32;
        let read = unsafe { GetObjectW(HGDIOBJ(bitmap.0), size, Some((&raw mut info).cast())) };
        (read != 0).then(|| {
            let height = if mono {
                info.bmHeight / 2
            } else {
                info.bmHeight
            };
            (info.bmWidth, height)
        })
    }
}

impl Drop for IconBitmaps {
    fn drop(&mut self) {
        // SAFETY: handles created by GetIconInfo; each deleted once.
        unsafe {
            for bitmap in [self.0.hbmMask, self.0.hbmColor] {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
            }
        }
    }
}

/// A DC with a 32-bit top-down DIB selected: `width * height` BGRA pixels at `bits`.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    /// What was selected before our bitmap, put back before the DC goes.
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl Surface {
    fn new(screen: HDC, width: u32, height: u32) -> anyhow::Result<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Top-down: the first row of pixels is the top row of the desktop.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut::<c_void>();
        // SAFETY: a valid DC and description; GDI allocates and owns the pixels.
        let bitmap =
            unsafe { CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .context("CreateDIBSection")?;
        if bits.is_null() {
            // SAFETY: created just above and not used anywhere.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            };
            bail!("CreateDIBSection returned no pixels");
        }
        // SAFETY: a new memory DC compatible with the screen's.
        let dc = unsafe { CreateCompatibleDC(Some(screen)) };
        if dc.is_invalid() {
            // SAFETY: created just above and not used anywhere.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            };
            bail!("CreateCompatibleDC failed");
        }
        // SAFETY: valid DC and bitmap; both live in `self` now, and the bitmap is put back
        // before the DC is deleted (see Drop).
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Surface {
            dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
        })
    }

    /// The frame's pixels, BGRA, top-down rows of `width * 4` bytes.
    fn pixels(&self) -> &[u8] {
        // SAFETY: the DIB holds `width * height` 32-bit pixels (see `new`), and `&self`
        // covers every access while the slice lives.
        unsafe { std::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: our DC, our bitmap (still selected) and the object selected before it.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop-sized capture of the primary display: the first frame converts, an unchanged
    /// desktop does not. Opt-in (it reads the real desktop): `cargo test -p
    /// rd-platform-windows -- --ignored`.
    #[test]
    #[ignore]
    fn gdi_capture_of_the_primary_display() {
        let Some(monitor) = crate::display::resolve(0) else {
            return;
        };
        let mut capture = Capture::new(monitor, 640, 360, (0, 0, 640, 360)).unwrap();
        let mut image = Nv12Image::new(640, 360);
        assert!(
            capture.capture(&mut image).unwrap(),
            "the first frame is new"
        );
        // The desktop may change on its own (a clock, the pointer); both answers are right.
        let _ = capture.capture(&mut image).unwrap();
        assert!(
            image.data.iter().any(|&v| v != 0),
            "the frame was converted"
        );
    }
}
