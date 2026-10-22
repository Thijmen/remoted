//! Windows platform for the rd agent.
//!
//! All `unsafe` FFI lives in this crate. Modules:
//! - `capture`     — the screen pipeline (Desktop Duplication -> pointer -> scaler -> encoder)
//!   and a synthetic test-pattern pipeline
//! - `duplication` — DXGI Desktop Duplication of one output (desktop image, pointer)
//! - `gdi`         — GDI screen capture, for displays without a usable D3D11 device
//! - `cursor`      — drawing the pointer into the captured image
//! - `convert`     — D3D11 video processor: BGRA desktop -> letterboxed BT.709 NV12 frame
//! - `encoder`     — Media Foundation H.264 (hardware MFT from textures, software fallback)
//! - `d3d`, `mf`   — devices, adapters, COM / Media Foundation lifetime, encoder discovery
//! - `geometry`    — where the captured display sits in the video frame (capture ⇄ input)
//! - `input`       — `SendInput` injection (absolute/relative mouse, wheel, scan-code keys)
//! - `display`     — monitors, DPI scale, wire display ids
//! - `files`       — file browser places (known folders, drives), Recycle Bin
//! - `power`       — power requests (keep the display awake, declare user activity)
//! - `pty`         — the terminal's pseudo console (ConPTY)
//! - `session`     — lock state, OS description, permissions

#![cfg(windows)]

pub mod capture;
pub mod convert;
pub mod cursor;
pub mod d3d;
pub mod display;
pub mod duplication;
pub mod encoder;
pub mod files;
pub mod gdi;
pub mod geometry;
pub mod input;
pub mod mf;
pub mod power;
pub mod pty;
pub mod secure;
pub mod session;

use std::sync::Arc;

use rd_agent_core::{
    DisplayDesc, Guard, InputInjector, LockState, Perms, Place, Platform, PtyProcess, PtyRequest,
    VideoPipeline,
};

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
    /// The Windows lock screen / UAC helper (None until the service is installed).
    secure: Arc<secure::SecureDesktop>,
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
            secure: secure::SecureDesktop::new(),
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
    fn session_disconnected(&self) -> bool {
        session::disconnected()
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
        Ok(Box::new(secure::SecureInputInjector::new(
            self.secure.clone(),
            self.geometry.clone(),
        )?))
    }
    fn secure_state(&self) -> rd_agent_core::platform::SecureState {
        self.secure.state()
    }
    fn secure_video_pipeline(&self) -> Option<Box<dyn VideoPipeline>> {
        Some(Box::new(secure::SecurePipeline::new(self.secure.clone())))
    }
    fn secure_input_ready(&self) -> bool {
        self.secure.input_ready()
    }
    fn send_sas(&self) -> anyhow::Result<()> {
        self.secure.send_sas()
    }
    fn file_places(&self) -> Vec<Place> {
        files::places()
    }
    fn can_trash(&self) -> bool {
        true
    }
    fn trash(&self, path: &std::path::Path) -> std::io::Result<()> {
        files::trash(path)
    }
    /// ConPTY exists (Windows 10 1809 or later).
    fn pty_supported(&self) -> bool {
        pty::supported()
    }
    fn spawn_pty(&self, req: &PtyRequest) -> std::io::Result<PtyProcess> {
        pty::spawn(req)
    }
}
