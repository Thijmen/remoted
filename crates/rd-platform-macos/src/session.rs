//! Lock state, permission preflight, OS and display information.
//!
//! Display ids on the wire: `0` is always the main display; any other display is identified by
//! its `CGDirectDisplayID` (which is never 0).

use std::ffi::CStr;

use objc2_core_foundation::CFString;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode,
    CGGetActiveDisplayList, CGMainDisplayID, CGPreflightPostEventAccess,
    CGPreflightScreenCaptureAccess, CGRequestPostEventAccess, CGRequestScreenCaptureAccess,
    CGSessionCopyCurrentDictionary,
};
use rd_agent_core::{DisplayDesc, LockState, Perms};

use crate::cf;

const MAX_DISPLAYS: usize = 16;

/// e.g. "macOS 26.3 (25D125)".
pub fn os_description() -> String {
    let version = sysctl_string(c"kern.osproductversion").unwrap_or_else(|| "?".into());
    match sysctl_string(c"kern.osversion") {
        Some(build) => format!("macOS {version} ({build})"),
        None => format!("macOS {version}"),
    }
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    // SAFETY: `buf`/`len` describe a writable buffer; no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf[..len.min(buf.len())]).ok()?;
    Some(s.to_string_lossy().into_owned())
}

/// Active displays, main display first (id 0).
pub fn displays() -> Vec<DisplayDesc> {
    let main = CGMainDisplayID();
    let mut list = active_displays();
    list.sort_by_key(|&d| d != main);
    list.into_iter()
        .map(|d| {
            let bounds = CGDisplayBounds(d);
            let (w_pt, h_pt) = (bounds.size.width, bounds.size.height);
            let scale = pixel_size(d)
                .filter(|_| w_pt > 0.0)
                .map_or(1.0, |(w_px, _)| f64::from(w_px) / w_pt);
            DisplayDesc {
                id: if d == main { 0 } else { d },
                w_pt: w_pt.round() as u32,
                h_pt: h_pt.round() as u32,
                scale: scale as f32,
            }
        })
        .collect()
}

/// Backing pixel size of the display's current mode, e.g. (3456, 2234).
pub fn native_size(display: u32) -> Option<(u32, u32)> {
    pixel_size(resolve_display(display)?)
}

/// Maps a wire display id to a `CGDirectDisplayID` of an active display.
pub fn resolve_display(display: u32) -> Option<CGDirectDisplayID> {
    if display == 0 {
        return Some(CGMainDisplayID());
    }
    active_displays().into_iter().find(|&d| d == display)
}

fn active_displays() -> Vec<CGDirectDisplayID> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: `ids` has room for MAX_DISPLAYS entries.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        tracing::warn!(error = err.0, "CGGetActiveDisplayList failed");
        return vec![CGMainDisplayID()];
    }
    ids[..(count as usize).min(MAX_DISPLAYS)].to_vec()
}

fn pixel_size(display: CGDirectDisplayID) -> Option<(u32, u32)> {
    let mode = CGDisplayCopyDisplayMode(display)?;
    let w = CGDisplayMode::pixel_width(Some(&mode));
    let h = CGDisplayMode::pixel_height(Some(&mode));
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// Current TCC grants, without prompting.
pub fn permissions() -> Perms {
    Perms {
        screen: CGPreflightScreenCaptureAccess(),
        input: CGPreflightPostEventAccess(),
    }
}

/// Only called by `rd-agent setup` (in person): shows the standard system permission prompts.
pub fn request_permissions() -> Perms {
    let screen = CGRequestScreenCaptureAccess();
    let input = CGRequestPostEventAccess();
    Perms { screen, input }
}

/// Whether the console session's screen is locked.
pub fn lock_state() -> LockState {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        // No window-server session (e.g. running outside the GUI login session).
        return LockState::Unknown;
    };
    let key = CFString::from_static_str("CGSSessionScreenIsLocked");
    if cf::dict_bool(&session, &key).unwrap_or(false) {
        LockState::Locked
    } else {
        LockState::Unlocked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_description_has_version_and_build() {
        let s = os_description();
        assert!(s.starts_with("macOS "), "{s}");
        assert!(s.contains('(') && s.ends_with(')'), "{s}");
    }

    #[test]
    fn main_display_is_id_zero() {
        let list = displays();
        assert!(!list.is_empty());
        assert_eq!(list[0].id, 0);
        assert!(list[0].w_pt > 0 && list[0].h_pt > 0 && list[0].scale >= 1.0);
        let (w, h) = native_size(0).expect("main display size");
        assert_eq!(w, (list[0].w_pt as f32 * list[0].scale).round() as u32);
        assert!(h > 0);
        assert_eq!(native_size(u32::MAX), None);
    }

    #[test]
    fn lock_state_smoke() {
        // Outside a GUI login session (ssh, CI) this is legitimately Unknown.
        println!("lock state: {:?}", lock_state());
    }
}
