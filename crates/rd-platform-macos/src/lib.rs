//! macOS platform for the rd agent.
//!
//! All `unsafe` FFI lives in this crate. Modules:
//! - `capture`  — ScreenCaptureKit stream (NV12 420v, scaled output, frame gate on the capture queue)
//!   and a synthetic test-pattern pipeline (no permission needed)
//! - `encoder`  — VideoToolbox hardware H.264 (AverageBitRate, on-demand IDR, avcC extraction)
//! - `input`    — CGEvent injection (mouse abs/rel/buttons/click-state, wheel, keys via HID->kVK)
//! - `power`    — IOPM assertions (keep display awake, declare user activity)
//! - `session`  — lock state, permission preflight, OS description, displays
//! - `keychain` — agent token storage
//! - `selftest` — capture+encode benchmark, SPS/VUI assertions, optional `.rdv` recording

#![cfg(target_os = "macos")]

pub mod capture;
mod cf;
pub mod encoder;
pub mod input;
pub mod keychain;
mod pattern;
pub mod power;
pub mod selftest;
pub mod session;
mod vui;

use std::sync::Arc;

use rd_agent_core::{DisplayDesc, Guard, InputInjector, LockState, Perms, Platform, VideoPipeline};

pub use encoder::RateControl;

/// Where session video comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoSource {
    /// ScreenCaptureKit display capture (needs Screen Recording permission).
    #[default]
    Screen,
    /// Moving test pattern; for end-to-end tests without Screen Recording permission.
    Synthetic,
}

/// The macOS [`Platform`].
pub struct MacPlatform {
    video: VideoSource,
    rate_control: RateControl,
}

impl MacPlatform {
    /// Screen capture; encoder mode from [`RateControl::from_env`].
    pub fn new() -> anyhow::Result<Arc<Self>> {
        Self::with_options(VideoSource::Screen, RateControl::from_env())
    }

    pub fn with_options(
        video: VideoSource,
        rate_control: RateControl,
    ) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(MacPlatform {
            video,
            rate_control,
        }))
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
        let perms = session::permissions();
        match self.video {
            VideoSource::Screen => perms,
            // The synthetic source needs no Screen Recording grant.
            VideoSource::Synthetic => Perms {
                screen: true,
                ..perms
            },
        }
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
        let rc = self.rate_control;
        Ok(match self.video {
            VideoSource::Screen => Box::new(capture::ScreenPipeline::with_rate_control(rc)?),
            VideoSource::Synthetic => Box::new(capture::SyntheticPipeline::with_rate_control(rc)?),
        })
    }
    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(input::CgInjector::new()?))
    }
}
