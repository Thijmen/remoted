//! macOS platform for the rd agent.
//!
//! All `unsafe` FFI lives in this crate. Modules:
//! - `capture`  — ScreenCaptureKit stream (NV12 420v, scaled output, frame gate on the capture queue)
//! - `encoder`  — VideoToolbox low-latency H.264 (AverageBitRate, on-demand IDR, avcC extraction)
//! - `input`    — CGEvent injection (mouse abs/rel/buttons/click-state, wheel, keys via HID->kVK)
//! - `power`    — IOPM assertions (keep display awake, declare user activity)
//! - `session`  — lock state, permission preflight, OS description, displays
//! - `keychain` — agent token storage
//! - `selftest` — capture+encode benchmark, SPS/VUI assertions, optional `.rdv` recording

#![cfg(target_os = "macos")]

pub mod capture;
pub mod encoder;
pub mod input;
pub mod keychain;
pub mod power;
pub mod selftest;
pub mod session;

use std::sync::Arc;

use rd_agent_core::{DisplayDesc, Guard, InputInjector, LockState, Perms, Platform, VideoPipeline};

/// The macOS [`Platform`].
pub struct MacPlatform {
    _priv: (),
}

impl MacPlatform {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(MacPlatform { _priv: () }))
    }
}

impl Platform for MacPlatform {
    fn os(&self) -> &'static str {
        "macos"
    }
    fn os_description(&self) -> String {
        session::os_description()
    }
    fn displays(&self) -> Vec<DisplayDesc> {
        session::displays()
    }
    fn native_size(&self, display: u32) -> Option<(u32, u32)> {
        session::native_size(display)
    }
    fn permissions(&self) -> Perms {
        session::permissions()
    }
    fn lock_state(&self) -> LockState {
        session::lock_state()
    }
    fn keep_awake(&self, reason: &str) -> Box<dyn Guard> {
        power::keep_display_awake(reason)
    }
    fn declare_user_activity(&self) {
        power::declare_user_activity()
    }
    fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>> {
        Ok(Box::new(capture::ScreenPipeline::new()?))
    }
    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(input::CgInjector::new()?))
    }
}
