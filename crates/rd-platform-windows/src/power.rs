//! Power requests: keep the display awake during a session and declare user activity (wakes
//! a sleeping display, which Desktop Duplication needs: a dark display delivers no frames).

use rd_agent_core::Guard;
use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, PowerClearRequest, PowerCreateRequest,
    PowerRequestDisplayRequired, PowerRequestSystemRequired, PowerSetRequest,
    SetThreadExecutionState,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::core::PWSTR;

/// Holds display- and system-required power requests until dropped.
pub struct DisplayAwake {
    request: Option<HANDLE>,
}

// SAFETY: a power request handle may be used and closed from any thread.
unsafe impl Send for DisplayAwake {}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        if let Some(h) = self.request.take() {
            // SAFETY: the request created in `keep_display_awake`, cleared and closed once.
            unsafe {
                let _ = PowerClearRequest(h, PowerRequestDisplayRequired);
                let _ = PowerClearRequest(h, PowerRequestSystemRequired);
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Keep the display (and the system) awake while the returned guard lives. If the request
/// cannot be created the failure is logged and the guard does nothing.
pub fn keep_display_awake(reason: &str) -> Box<dyn Guard> {
    let mut text: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
    let context = REASON_CONTEXT {
        Version: 0, // POWER_REQUEST_CONTEXT_VERSION
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: REASON_CONTEXT_0 {
            SimpleReasonString: PWSTR(text.as_mut_ptr()),
        },
    };
    // SAFETY: the reason string outlives the call (Windows copies it).
    let request = match unsafe { PowerCreateRequest(&context) } {
        Ok(h) => {
            // SAFETY: a valid power request handle.
            unsafe {
                if let Err(e) = PowerSetRequest(h, PowerRequestDisplayRequired) {
                    warn!("PowerSetRequest(DisplayRequired): {e}");
                }
                if let Err(e) = PowerSetRequest(h, PowerRequestSystemRequired) {
                    warn!("PowerSetRequest(SystemRequired): {e}");
                }
            }
            Some(h)
        }
        Err(e) => {
            warn!("PowerCreateRequest: {e}");
            None
        }
    };
    Box::new(DisplayAwake { request })
}

/// Tell the power manager the user is active: wakes the display and restarts the idle timers
/// (up to the display-off setting).
pub fn declare_user_activity() {
    // SAFETY: no ES_CONTINUOUS, so nothing stays set on this thread.
    unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_created_and_released() {
        let guard = keep_display_awake("rd-platform-windows test");
        declare_user_activity();
        drop(guard);
    }
}
