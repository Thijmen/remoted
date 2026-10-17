//! Where the captured display sits inside the video frame, shared by the capture pipeline
//! (which decides it) and the input injector (which maps the viewer's normalised frame
//! coordinates back onto the display).
//!
//! ScreenCaptureKit scales the display into the `width x height` output. With
//! `preservesAspectRatio` (the default) it keeps the display's aspect ratio and centres it,
//! leaving black bars when the aspects differ. The runtime asks for the display's own aspect,
//! but rounding the size to even pixels can still leave a sub-pixel bar, so when the bars
//! would be at most [`STRETCH_MAX_PX`] the pipeline turns aspect preservation off and the
//! display fills the whole frame (an invisible distortion). Otherwise it letterboxes, and the
//! injector maps through the same centred content rect, so clicks land where the viewer sees
//! them. That also holds when the display mode changes mid-session: the injector re-reads the
//! display bounds and fits the new aspect into the frame exactly as ScreenCaptureKit does.

use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::CGDirectDisplayID;
use parking_lot::Mutex;

/// Stretch instead of letterboxing when the bars would total at most this many pixels.
pub const STRETCH_MAX_PX: f64 = 2.0;

/// How a capture lays out its display in the video frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureLayout {
    /// The captured display.
    pub display: CGDirectDisplayID,
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

/// Whether a display of `display_w x display_h` (any unit) should be stretched over a
/// `width x height` frame instead of letterboxed: true when fitting it would leave at most
/// [`STRETCH_MAX_PX`] of bars.
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

/// Maps a point given in fractions of the video frame (`u`, `v` in 0..=1) onto the display
/// `bounds` (global points), through the content rect of `layout` (the whole frame without
/// one). Points on a letterbox bar map outside the display; the caller clamps.
pub fn frame_to_display(u: f64, v: f64, layout: Option<&CaptureLayout>, bounds: CGRect) -> CGPoint {
    let CGRect { origin, size } = bounds;
    let content = layout.map_or(UnitRect::FULL, |l| content_rect(l, size.width, size.height));
    let x = (u - content.x) / content.w;
    let y = (v - content.y) / content.h;
    CGPoint::new(origin.x + x * size.width, origin.y + y * size.height)
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
    use objc2_core_foundation::CGSize;

    use super::*;

    fn layout(width: u32, height: u32, stretch: bool) -> CaptureLayout {
        CaptureLayout {
            display: 1,
            width,
            height,
            stretch,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn fit_centres_with_bars_on_the_short_axis() {
        // A 16:9 display in the 1668x1080 frame of the M1 finding: ~71 px bars top and bottom.
        let (x, y, w, h) = fit(1920.0, 1080.0, 1668, 1080).unwrap();
        assert!(close(x, 0.0) && close(w, 1668.0));
        assert!(close(h, 938.25) && close(y, 70.875), "{y} {h}");
        // A 4:3 display in a 16:10 frame: pillarbox.
        let (x, y, w, h) = fit(1024.0, 768.0, 1600, 1000).unwrap();
        assert!(close(y, 0.0) && close(h, 1000.0));
        assert!(close(w, 1000.0 * 4.0 / 3.0) && close(x, (1600.0 - w) / 2.0));
        assert_eq!(fit(0.0, 1080.0, 1668, 1080), None);
        assert_eq!(fit(1920.0, 1080.0, 0, 1080), None);
    }

    #[test]
    fn stretches_only_when_the_bars_are_negligible() {
        // MacBook Pro 14": 1728x1117 pt; even-rounded requests leave at most ~1 px.
        for (w, h) in [
            (1728, 1118),
            (1280, 828),
            (2592, 1676),
            (3456, 2234),
            (1726, 1116),
        ] {
            assert!(stretch_to_fill(1728.0, 1117.0, w, h), "{w}x{h}");
        }
        // Same aspect at a different scale (native pixels vs points) is the same decision.
        assert!(stretch_to_fill(3456.0, 2234.0, 1728, 1118));
        // Real aspect mismatches letterbox.
        assert!(!stretch_to_fill(1920.0, 1080.0, 1668, 1080));
        assert!(!stretch_to_fill(1728.0, 1117.0, 1920, 1080));
        assert!(!stretch_to_fill(1728.0, 1117.0, 1728, 1122));
        assert!(!stretch_to_fill(0.0, 0.0, 1728, 1118));
    }

    #[test]
    fn maps_through_the_letterbox() {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1920.0, 1080.0));
        let l = layout(1668, 1080, false);
        let map = |px: f64, py: f64| frame_to_display(px / 1668.0, py / 1080.0, Some(&l), bounds);
        // The content's corners are the display's corners.
        let p = map(0.0, 70.875);
        assert!(close(p.x, 0.0) && close(p.y, 0.0), "{p:?}");
        let p = map(1668.0, 1009.125);
        assert!(close(p.x, 1920.0) && close(p.y, 1080.0), "{p:?}");
        // The centre stays the centre.
        let p = map(834.0, 540.0);
        assert!(close(p.x, 960.0) && close(p.y, 540.0), "{p:?}");
        // A click on the menu bar as seen in the video (~10 pt below the top) lands there.
        let p = map(834.0, 80.0);
        assert!((p.y - 10.5).abs() < 0.01, "{p:?}");
        // The bars map off the display (the injector clamps them onto the edge).
        assert!(map(834.0, 10.0).y < 0.0);
        assert!(map(834.0, 1075.0).y > 1080.0);
    }

    #[test]
    fn stretch_and_no_layout_map_the_whole_frame() {
        let bounds = CGRect::new(CGPoint::new(-1728.0, 100.0), CGSize::new(1728.0, 1117.0));
        for l in [Some(layout(1728, 1118, true)), None] {
            let p = frame_to_display(0.0, 0.0, l.as_ref(), bounds);
            assert_eq!(p, CGPoint::new(-1728.0, 100.0));
            let p = frame_to_display(1.0, 1.0, l.as_ref(), bounds);
            assert_eq!(p, CGPoint::new(0.0, 1217.0));
            let p = frame_to_display(0.5, 0.5, l.as_ref(), bounds);
            assert_eq!(p, CGPoint::new(-864.0, 658.5));
        }
    }

    #[test]
    fn display_mode_change_mid_session_is_followed() {
        // Captured at 1728x1118 (letterbox mode chosen for some other aspect), then the display
        // switches to 16:9: the content rect follows the new bounds.
        let l = layout(1728, 1118, false);
        let wide = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1920.0, 1080.0));
        let c = content_rect(&l, wide.size.width, wide.size.height);
        assert!(close(c.x, 0.0) && close(c.w, 1.0));
        let bar = (1118.0 - 1728.0 * 1080.0 / 1920.0) / 2.0 / 1118.0;
        assert!(close(c.y, bar) && close(c.h, 1.0 - 2.0 * bar));
        let top = frame_to_display(0.25, c.y, Some(&l), wide);
        assert!(close(top.y, 0.0) && close(top.x, 480.0), "{top:?}");
    }

    #[test]
    fn retract_only_removes_the_owners_layout() {
        let g = CaptureGeometry::default();
        assert_eq!(g.current(), None);
        g.publish(1, layout(100, 100, true));
        g.publish(2, layout(200, 200, false));
        g.retract(1);
        assert_eq!(g.current(), Some(layout(200, 200, false)));
        g.retract(2);
        assert_eq!(g.current(), None);
    }
}
