//! `rd-agent selftest` on macOS: the ScreenCaptureKit (or synthetic) pipeline through the
//! platform-neutral harness in [`rd_agent_core::selftest`] (report, SPS/VUI assertions,
//! optional `.rdv` recording).
//!
//! [`run`] blocks for the duration of the test; call it outside an async context.

use std::path::PathBuf;

use anyhow::ensure;
use rd_agent_core::VideoPipeline;

use crate::capture::{ScreenPipeline, SyntheticPipeline};
use crate::encoder::RateControl;
use crate::session;

#[derive(Debug, Clone)]
pub struct SelftestOptions {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub bitrate_kbps: u32,
    pub record: Option<PathBuf>,
    /// Encode a moving test pattern instead of the screen (needs no Screen Recording grant).
    pub synthetic: bool,
    pub rate_control: RateControl,
}

impl Default for SelftestOptions {
    fn default() -> Self {
        SelftestOptions {
            width: 1728,
            height: 1118,
            fps: 60,
            seconds: 10,
            bitrate_kbps: 8000,
            record: None,
            synthetic: false,
            rate_control: RateControl::from_env(),
        }
    }
}

/// Runs the selftest and prints a report to stdout. Returns Ok(true) if every assertion passed.
pub fn run(opts: &SelftestOptions) -> anyhow::Result<bool> {
    let pipeline: Box<dyn VideoPipeline> = if opts.synthetic {
        Box::new(SyntheticPipeline::with_rate_control(opts.rate_control)?)
    } else {
        ensure!(
            session::permissions().screen,
            "Screen Recording permission is not granted to this process; grant it in System \
             Settings > Privacy & Security, or run the synthetic selftest"
        );
        Box::new(ScreenPipeline::with_rate_control(opts.rate_control)?)
    };
    let core = rd_agent_core::selftest::SelftestOptions {
        width: opts.width,
        height: opts.height,
        fps: opts.fps,
        seconds: opts.seconds,
        bitrate_kbps: opts.bitrate_kbps,
        record: opts.record.clone(),
        synthetic: opts.synthetic,
        encoder: format!("{} rate control", opts.rate_control.as_str()),
    };
    rd_agent_core::selftest::run(pipeline, &core, resident_bytes)
}

/// Current resident set size of this process.
fn resident_bytes() -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` is a writable proc_taskinfo of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (n == size).then_some(info.pti_resident_size)
}

#[cfg(test)]
mod tests {
    use rd_proto::bin::video_flags;

    use super::*;

    #[test]
    fn rss_is_reported() {
        assert!(resident_bytes().is_some_and(|b| b > 0));
    }

    /// End to end through the real VideoToolbox encoder (no permission needed).
    #[test]
    fn synthetic_selftest_passes_and_records() {
        let dir = std::env::temp_dir().join(format!("rd-selftest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.rdv");
        let opts = SelftestOptions {
            width: 640,
            height: 360,
            fps: 30,
            seconds: 1,
            bitrate_kbps: 2000,
            record: Some(path.clone()),
            synthetic: true,
            rate_control: RateControl::LowLatency,
        };
        assert!(run(&opts).unwrap());

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RDV1");
        let (kind, len) = (
            bytes[4],
            u32::from_le_bytes(bytes[13..17].try_into().unwrap()),
        );
        assert_eq!(kind, 0);
        let json: serde_json::Value =
            serde_json::from_slice(&bytes[17..17 + len as usize]).unwrap();
        assert_eq!(json["t"], "video_config");
        assert_eq!(json["width"], 640);
        let next = 17 + len as usize;
        assert_eq!(bytes[next], 1);
        let first_video = &bytes[next + 13..];
        assert_eq!(first_video[0], rd_proto::bin::ty::VIDEO);
        assert_eq!(
            first_video[1],
            video_flags::KEY | video_flags::FIRST_OF_EPOCH
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
