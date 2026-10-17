//! IOPM power assertions. TO BE IMPLEMENTED.

use rd_agent_core::Guard;

pub fn keep_display_awake(reason: &str) -> Box<dyn Guard> {
    let _ = reason;
    todo!("rd-platform-macos: power::keep_display_awake")
}

pub fn declare_user_activity() {
    todo!()
}
