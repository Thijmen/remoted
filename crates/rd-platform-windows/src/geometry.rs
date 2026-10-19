//! Where the captured display sits inside the video frame, shared by the capture pipeline
//! (which decides it) and the input injector (which maps the viewer's normalised frame
//! coordinates back onto the display). Same rules as the macOS agent:
//!
//! The runtime asks for the display's own aspect, but rounding the size to even pixels can
//! leave a sub-pixel bar, so when fitting the display would leave bars of at most
//! [`STRETCH_MAX_PX`] it fills the whole frame (an invisible distortion). Otherwise it is
//! fitted and centred (letterboxed), and the injector maps through the same content rect, so
//! clicks land where the viewer sees them. When the display mode changes mid-session the
//! capture refits the new size into the same frame, and the injector follows the new bounds.

use parking_lot::Mutex;

use crate::display::Rect;

/// Stretch instead of letterboxing when the bars would total at most this many pixels.
pub const STRETCH_MAX_PX: f64 = 2.0;

/// How a capture lays out its display in the video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLayout {
    /// The captured display (wire id).
    pub display: u32,
    /// Video frame size, pixels.
    pub width: u32,
    pub height: u32,
    /// The display fills the whole frame (aspect not preserved). Otherwise it is fitted and
    /// centred, keeping its aspect ratio.
    pub stretch: bool,
}

/// A rect in fractions of the frame (0..=1 on both axes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl UnitRect {
    pub const FULL: UnitRect = UnitRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
}

/// The display's aspect-preserving fit inside a `width x height` frame, centred, in pixels:
/// `(x, y, w, h)`. `None` for degenerate sizes.
pub fn fit(
    display_w: f64,
    display_h: f64,
    width: u32,
    height: u32,
) -> Option<(f64, f64, f64, f64)> {
    let (fw, fh) = (f64::from(width), f64::from(height));
    if !(display_w > 0.0 && display_h > 0.0 && fw > 0.0 && fh > 0.0) {
        return None;
    }
    let scale = (fw / display_w).min(fh / display_h);
    let (w, h) = (display_w * scale, display_h * scale);
    Some(((fw - w) / 2.0, (fh - h) / 2.0, w, h))
}

/// Whether a display of `display_w x display_h` should be stretched over a `width x height`
/// frame instead of letterboxed: true when fitting it would leave at most [`STRETCH_MAX_PX`]
/// of bars.
pub fn stretch_to_fill(display_w: f64, display_h: f64, width: u32, height: u32) -> bool {
    fit(display_w, display_h, width, height).is_some_and(|(_, _, w, h)| {
        f64::from(width) - w <= STRETCH_MAX_PX && f64::from(height) - h <= STRETCH_MAX_PX
    })
}

/// Where a display of `display_w x display_h` appears in the frame of `layout`, as fractions
/// of the frame.
pub fn content_rect(layout: &CaptureLayout, display_w: f64, display_h: f64) -> UnitRect {
    if layout.stretch {
        return UnitRect::FULL;
    }
    match fit(display_w, display_h, layout.width, layout.height) {
        Some((x, y, w, h)) => {
            let (fw, fh) = (f64::from(layout.width), f64::from(layout.height));
            UnitRect {
                x: x / fw,
                y: y / fh,
                w: w / fw,
                h: h / fh,
            }
        }
        None => UnitRect::FULL,
    }
}

/// The content rect in whole frame pixels `(left, top, right, bottom)`, for the scaler.
pub fn content_pixels(
    layout: &CaptureLayout,
    display_w: u32,
    display_h: u32,
) -> (i32, i32, i32, i32) {
    let c = content_rect(layout, f64::from(display_w), f64::from(display_h));
    let (fw, fh) = (f64::from(layout.width), f64::from(layout.height));
    let left = (c.x * fw).round() as i32;
    let top = (c.y * fh).round() as i32;
    let right = ((c.x + c.w) * fw).round() as i32;
    let bottom = ((c.y + c.h) * fh).round() as i32;
    (left, top, right, bottom)
}

/// Maps a point given in fractions of the video frame (`u`, `v` in 0..=1) onto the display
/// `bounds` (virtual-screen pixels), through the content rect of `layout` (the whole frame
/// without one). Points on a letterbox bar map outside the display; the caller clamps.
pub fn frame_to_display(
    u: f64,
    v: f64,
    layout: Option<&CaptureLayout>,
    bounds: Rect,
) -> (f64, f64) {
    let (bw, bh) = (f64::from(bounds.w), f64::from(bounds.h));
    let content = layout.map_or(UnitRect::FULL, |l| content_rect(l, bw, bh));
    let x = (u - content.x) / content.w;
    let y = (v - content.y) / content.h;
    (f64::from(bounds.x) + x * bw, f64::from(bounds.y) + y * bh)
}

/// The layout of the running capture, published by the pipeline and read by the injector.
#[derive(Debug, Default)]
pub struct CaptureGeometry {
    current: Mutex<Option<(u64, CaptureLayout)>>,
}

impl CaptureGeometry {
    /// `owner` identifies the publishing pipeline, so a stopping pipeline never retracts the
    /// layout of its successor.
    pub fn publish(&self, owner: u64, layout: CaptureLayout) {
        *self.current.lock() = Some((owner, layout));
    }

    pub fn retract(&self, owner: u64) {
        let mut current = self.current.lock();
        if current.is_some_and(|(o, _)| o == owner) {
            *current = None;
        }
    }

    pub fn current(&self) -> Option<CaptureLayout> {
        self.current.lock().map(|(_, layout)| layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: u32, height: u32, stretch: bool) -> CaptureLayout {
        CaptureLayout {
            display: 0,
            width,
            height,
            stretch,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn fit_centres_with_bars_on_the_short_axis() {
        let (x, y, w, h) = fit(1920.0, 1080.0, 1668, 1080).unwrap();
        assert!(close(x, 0.0) && close(w, 1668.0));
        assert!(close(h, 938.25) && close(y, 70.875), "{y} {h}");
        let (x, y, w, h) = fit(1024.0, 768.0, 1600, 1000).unwrap();
        assert!(close(y, 0.0) && close(h, 1000.0));
        assert!(close(w, 1000.0 * 4.0 / 3.0) && close(x, (1600.0 - w) / 2.0));
        assert_eq!(fit(0.0, 1080.0, 1668, 1080), None);
    }

    #[test]
    fn stretches_only_when_the_bars_are_negligible() {
        // 2560x1440 and the even sizes the runtime derives from it.
        for (w, h) in [
            (1728, 972),
            (1920, 1080),
            (1280, 720),
            (2560, 1440),
            (1366, 768),
        ] {
            assert!(stretch_to_fill(2560.0, 1440.0, w, h), "{w}x{h}");
        }
        assert!(!stretch_to_fill(2560.0, 1440.0, 1728, 1118));
        assert!(!stretch_to_fill(1920.0, 1200.0, 1920, 1080));
        assert!(!stretch_to_fill(0.0, 0.0, 1728, 1118));
    }

    #[test]
    fn content_pixels_round_the_letterbox() {
        assert_eq!(
            content_pixels(&layout(1728, 1118, false), 2560, 1440),
            (0, 73, 1728, 1045)
        );
        assert_eq!(
            content_pixels(&layout(1728, 972, true), 2560, 1440),
            (0, 0, 1728, 972)
        );
    }

    #[test]
    fn maps_through_the_letterbox() {
        let bounds = rect(0, 0, 1920, 1080);
        let l = layout(1668, 1080, false);
        let map = |px: f64, py: f64| frame_to_display(px / 1668.0, py / 1080.0, Some(&l), bounds);
        let (x, y) = map(0.0, 70.875);
        assert!(close(x, 0.0) && close(y, 0.0), "{x} {y}");
        let (x, y) = map(1668.0, 1009.125);
        assert!(close(x, 1920.0) && close(y, 1080.0), "{x} {y}");
        let (x, y) = map(834.0, 540.0);
        assert!(close(x, 960.0) && close(y, 540.0));
        assert!(map(834.0, 10.0).1 < 0.0);
    }

    #[test]
    fn secondary_displays_map_into_virtual_screen_coordinates() {
        // A display left of the primary has negative coordinates.
        let bounds = rect(-1920, 0, 1920, 1080);
        for l in [Some(layout(1920, 1080, true)), None] {
            assert_eq!(
                frame_to_display(0.0, 0.0, l.as_ref(), bounds),
                (-1920.0, 0.0)
            );
            assert_eq!(
                frame_to_display(1.0, 1.0, l.as_ref(), bounds),
                (0.0, 1080.0)
            );
            assert_eq!(
                frame_to_display(0.5, 0.5, l.as_ref(), bounds),
                (-960.0, 540.0)
            );
        }
    }

    #[test]
    fn retract_only_removes_the_owners_layout() {
        let g = CaptureGeometry::default();
        g.publish(1, layout(100, 100, true));
        g.publish(2, layout(200, 200, false));
        g.retract(1);
        assert_eq!(g.current(), Some(layout(200, 200, false)));
        g.retract(2);
        assert_eq!(g.current(), None);
    }
}
