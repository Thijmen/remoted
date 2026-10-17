//! Lock state, permission preflight, OS/display info. TO BE IMPLEMENTED.

use rd_agent_core::{DisplayDesc, LockState, Perms};

pub fn os_description() -> String {
    todo!("rd-platform-macos: session::os_description")
}
pub fn displays() -> Vec<DisplayDesc> {
    todo!()
}
pub fn native_size(display: u32) -> Option<(u32, u32)> {
    let _ = display;
    todo!()
}
pub fn permissions() -> Perms {
    todo!()
}
/// Only called by `rd-agent setup` (in person): shows the standard system permission prompts.
pub fn request_permissions() -> Perms {
    todo!()
}
pub fn lock_state() -> LockState {
    todo!()
}
