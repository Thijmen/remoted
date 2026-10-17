//! CGEvent input injection. TO BE IMPLEMENTED.

use rd_agent_core::{InputEvent, InputInjector};

pub struct CgInjector {}

impl CgInjector {
    pub fn new() -> anyhow::Result<Self> {
        Ok(CgInjector {})
    }
}

impl InputInjector for CgInjector {
    fn inject(&mut self, ev: InputEvent) {
        let _ = ev;
        todo!("rd-platform-macos: CgInjector::inject")
    }
    fn release_all(&mut self) {
        todo!()
    }
}
