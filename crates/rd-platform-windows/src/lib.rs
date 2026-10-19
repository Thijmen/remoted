//! Windows platform for the rd agent.
//!
//! All `unsafe` FFI lives in this crate. Modules:
//! - `capture`     — the screen pipeline (Desktop Duplication -> pointer -> scaler -> encoder)
//!   and a synthetic test-pattern pipeline
//! - `duplication` — DXGI Desktop Duplication of one output (desktop image, pointer)
//! - `cursor`      — drawing the pointer into the captured image
//! - `convert`     — D3D11 video processor: BGRA desktop -> letterboxed BT.709 NV12 frame
//! - `encoder`     — Media Foundation H.264 (hardware MFT from textures, software fallback)
//! - `d3d`, `mf`   — devices, adapters, COM / Media Foundation lifetime, encoder discovery
//! - `geometry`    — where the captured display sits in the video frame (capture ⇄ input)
//! - `input`       — `SendInput` injection (absolute/relative mouse, wheel, scan-code keys)
//! - `display`     — monitors, DPI scale, wire display ids
//! - `power`       — power requests (keep the display awake, declare user activity)
//! - `session`     — lock state, OS description, permissions

#![cfg(windows)]

pub mod capture;
pub mod convert;
pub mod cursor;
pub mod d3d;
pub mod display;
pub mod duplication;
pub mod encoder;
pub mod geometry;
pub mod input;
pub mod mf;
pub mod power;
pub mod session;

use std::sync::Arc;

use rd_agent_core::{DisplayDesc, Guard, InputInjector, LockState, Perms, Platform, VideoPipeline};

pub use encoder::EncoderPreference;
use geometry::CaptureGeometry;

/// Where session video comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoSource {
    /// Desktop Duplication of a display.
    #[default]
    Screen,
    /// Moving test pattern; for end-to-end tests without a desktop.
    Synthetic,
}

/// The Windows [`Platform`].
pub struct WinPlatform {
    video: VideoSource,
    encoder: EncoderPreference,
    /// Published by the screen pipeline, followed by the input injector, so pointer
    /// coordinates map onto the captured display through the same content rect.
    geometry: Arc<CaptureGeometry>,
}

impl WinPlatform {
    /// Screen capture; encoder choice from [`EncoderPreference::from_env`].
    pub fn new() -> anyhow::Result<Arc<Self>> {
        Self::with_options(VideoSource::Screen, EncoderPreference::from_env())
    }

    pub fn with_options(
        video: VideoSource,
        encoder: EncoderPreference,
    ) -> anyhow::Result<Arc<Self>> {
        // Physical pixels everywhere (monitor rects, pointer positions, duplication).
        display::ensure_dpi_aware();
        mf::ensure_started()?;
        Ok(Arc::new(WinPlatform {
            video,
            encoder,
            geometry: Arc::default(),
        }))
    }
}

impl Platform for WinPlatform {
    fn os(&self) -> &'static str {
        "windows"
    }
    fn os_description(&self) -> String {
        session::os_description()
    }
    fn displays(&self) -> Vec<DisplayDesc> {
        display::displays()
    }
    fn native_size(&self, display: u32) -> Option<(u32, u32)> {
        display::native_size(display)
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
        Ok(match self.video {
            VideoSource::Screen => Box::new(
                capture::ScreenPipeline::new(self.encoder)
                    .with_capture_geometry(self.geometry.clone()),
            ),
            VideoSource::Synthetic => Box::new(capture::SyntheticPipeline::new(self.encoder)),
        })
    }
    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(
            input::SendInputInjector::new()?.with_capture_geometry(self.geometry.clone()),
        ))
    }
}
