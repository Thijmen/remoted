//! IOPM power assertions: keep the display awake during a session and declare user activity
//! (wakes a sleeping display).

use objc2_core_foundation::CFString;
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionDeclareUserActivity, IOPMAssertionID,
    IOPMAssertionRelease, IOPMUserActiveType, kIOPMAssertionLevelOn,
};
use parking_lot::Mutex;
use rd_agent_core::Guard;
use tracing::warn;

/// `kIOPMAssertPreventUserIdleDisplaySleep` (a `CFSTR` macro in IOPMLib.h).
const PREVENT_USER_IDLE_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";
/// `kIOPMNullAssertionID`.
const NULL_ASSERTION: IOPMAssertionID = 0;

/// The `UserIsActive` assertion from [`declare_user_activity`]. IOKit asks callers to pass the
/// previous id back on every call and to release it when done; it is released together with the
/// session's [`DisplayAwake`] guard.
static USER_ACTIVITY: Mutex<IOPMAssertionID> = Mutex::new(NULL_ASSERTION);

/// Holds `PreventUserIdleDisplaySleep` until dropped.
pub struct DisplayAwake {
    id: IOPMAssertionID,
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        release(self.id);
        end_user_activity();
    }
}

/// Keep the display awake while the returned guard lives. If the assertion cannot be created
/// the failure is logged and the guard does nothing.
pub fn keep_display_awake(reason: &str) -> Box<dyn Guard> {
    let kind = CFString::from_static_str(PREVENT_USER_IDLE_DISPLAY_SLEEP);
    let name = CFString::from_str(reason);
    let mut id = NULL_ASSERTION;
    // SAFETY: valid CFStrings and an out pointer to a local.
    let rc = unsafe {
        IOPMAssertionCreateWithName(Some(&kind), kIOPMAssertionLevelOn, Some(&name), &mut id)
    };
    if rc != 0 {
        warn!(
            rc,
            "IOPMAssertionCreateWithName(PreventUserIdleDisplaySleep) failed"
        );
        id = NULL_ASSERTION;
    }
    Box::new(DisplayAwake { id })
}

/// Tell the power manager a local user is active: wakes the display and postpones display
/// sleep (up to the display-sleep setting).
pub fn declare_user_activity() {
    let name = CFString::from_static_str("rd remote input");
    let mut id = USER_ACTIVITY.lock();
    // SAFETY: valid CFString; `id` is passed in (previous assertion) and out (current one).
    let rc = unsafe {
        IOPMAssertionDeclareUserActivity(Some(&name), IOPMUserActiveType::Local, &mut *id)
    };
    if rc != 0 {
        warn!(rc, "IOPMAssertionDeclareUserActivity failed");
    }
}

/// Release the user-activity assertion, if any.
pub fn end_user_activity() {
    let id = std::mem::replace(&mut *USER_ACTIVITY.lock(), NULL_ASSERTION);
    release(id);
}

fn release(id: IOPMAssertionID) {
    if id != NULL_ASSERTION {
        let rc = IOPMAssertionRelease(id);
        if rc != 0 {
            warn!(rc, id, "IOPMAssertionRelease failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn assertions_of_this_process() -> String {
        let out = Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .unwrap();
        let pid = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&pid))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn assertions_are_released_with_the_guard() {
        let guard = keep_display_awake("rd-platform-macos test");
        declare_user_activity();
        declare_user_activity(); // reuses the same assertion
        let held = assertions_of_this_process();
        assert!(held.contains("PreventUserIdleDisplaySleep"), "{held}");
        assert_eq!(held.matches("UserIsActive").count(), 1, "{held}");
        drop(guard);
        let after = assertions_of_this_process();
        assert!(!after.contains("PreventUserIdleDisplaySleep"), "{after}");
        assert!(!after.contains("UserIsActive"), "{after}");
    }
}
