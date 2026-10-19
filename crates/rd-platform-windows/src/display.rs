//! Displays: enumeration, DPI scale, wire ids.
//!
//! Display ids on the wire: `0` is always the primary display; any other display is identified
//! by the number in its GDI device name (`\\.\DISPLAY3` -> 3), which is never 0 and stays the
//! same while the agent runs.
//!
//! Everything here is in physical pixels: the process is made per-monitor DPI aware
//! ([`ensure_dpi_aware`]) before any display or input API is used, so Windows reports real
//! monitor rects (virtual-screen coordinates, origin at the primary display's top left) and
//! does not scale pointer positions.

use std::sync::Once;

use rd_agent_core::DisplayDesc;
use tracing::{debug, warn};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

/// A rect in physical pixels: `x`, `y` is the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Rect {
            x: r.left,
            y: r.top,
            w: r.right.saturating_sub(r.left).max(0) as u32,
            h: r.bottom.saturating_sub(r.top).max(0) as u32,
        }
    }
}

/// An attached monitor.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Wire id (see the module docs).
    pub id: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`; also `DXGI_OUTPUT_DESC::DeviceName`.
    pub device: String,
    /// Virtual-screen rect, physical pixels.
    pub rect: Rect,
    /// Effective DPI / 96 (1.5 at 150 %).
    pub scale: f32,
    pub primary: bool,
}

/// Makes the process per-monitor DPI aware (v2). Idempotent; a process that is already DPI
/// aware (e.g. through its manifest) keeps its setting.
pub fn ensure_dpi_aware() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: no preconditions; fails harmlessly when the awareness is already set.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            debug!("SetProcessDpiAwarenessContext: {e} (already set?)");
        }
    });
}

/// Attached monitors, primary first (id 0).
pub fn monitors() -> Vec<Monitor> {
    ensure_dpi_aware();
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `handles`, which outlives the call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut handles as isize),
        )
    };
    if !ok.as_bool() {
        warn!("EnumDisplayMonitors failed");
    }
    let mut list: Vec<Monitor> = handles.into_iter().filter_map(describe).collect();
    list.sort_by_key(|m| (!m.primary, m.id));
    list
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `Vec<HMONITOR>` passed by `monitors`, alive for the enumeration.
    let handles = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    handles.push(monitor);
    BOOL(1)
}

fn describe(monitor: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is a MONITORINFOEXW with cbSize set, as the extended form requires.
    let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let len = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    let device = String::from_utf16_lossy(&info.szDevice[..len]);
    let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    // SAFETY: valid monitor handle and out pointers.
    if let Err(e) = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    {
        debug!(%device, "GetDpiForMonitor: {e}");
        dpi_x = 96;
    }
    let id = if primary {
        0
    } else {
        device_number(&device).unwrap_or(u32::MAX)
    };
    Some(Monitor {
        id,
        device,
        rect: info.monitorInfo.rcMonitor.into(),
        scale: dpi_x.max(1) as f32 / 96.0,
        primary,
    })
}

/// `\\.\DISPLAY12` -> 12.
pub fn device_number(device: &str) -> Option<u32> {
    let digits = device.strip_prefix(r"\\.\DISPLAY")?;
    digits.parse().ok().filter(|&n| n > 0)
}

/// The monitor with wire id `display`.
pub fn resolve(display: u32) -> Option<Monitor> {
    monitors().into_iter().find(|m| m.id == display)
}

/// Wire descriptions: logical size (`w_pt` = pixels / scale) and scale, primary first.
pub fn displays() -> Vec<DisplayDesc> {
    monitors().iter().map(desc).collect()
}

pub fn desc(m: &Monitor) -> DisplayDesc {
    DisplayDesc {
        id: m.id,
        w_pt: (m.rect.w as f32 / m.scale).round() as u32,
        h_pt: (m.rect.h as f32 / m.scale).round() as u32,
        scale: m.scale,
    }
}

/// Pixel size of a display, e.g. (3840, 2160).
pub fn native_size(display: u32) -> Option<(u32, u32)> {
    resolve(display)
        .map(|m| (m.rect.w, m.rect.h))
        .filter(|&(w, h)| w > 0 && h > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_numbers() {
        assert_eq!(device_number(r"\\.\DISPLAY1"), Some(1));
        assert_eq!(device_number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(device_number(r"\\.\DISPLAY0"), None);
        assert_eq!(device_number(r"\\.\DISPLAY"), None);
        assert_eq!(device_number("DISPLAY1"), None);
    }

    #[test]
    fn rect_from_win32() {
        let r = Rect::from(RECT {
            left: -1920,
            top: 100,
            right: 0,
            bottom: 1180,
        });
        assert_eq!(
            r,
            Rect {
                x: -1920,
                y: 100,
                w: 1920,
                h: 1080
            }
        );
    }

    #[test]
    fn primary_display_is_id_zero() {
        let list = displays();
        // A headless CI runner still has one (virtual) display.
        assert!(!list.is_empty());
        assert_eq!(list[0].id, 0);
        assert!(list[0].w_pt > 0 && list[0].h_pt > 0 && list[0].scale >= 1.0);
        let (w, h) = native_size(0).expect("primary display size");
        assert_eq!(w, (list[0].w_pt as f32 * list[0].scale).round() as u32);
        assert!(h > 0);
        assert_eq!(native_size(u32::MAX - 1), None);
        for d in &list[1..] {
            assert_ne!(d.id, 0);
        }
    }
}
