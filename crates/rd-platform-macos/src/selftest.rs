//! `rd-agent selftest`: capture (or synthesize) and encode for N seconds, report frame rates,
//! encode latency, frame/IDR sizes and the codec string, and assert that the SPS VUI carries
//! `bitstream_restriction_flag == 1 && max_num_reorder_frames == 0` (without it browsers'
//! decoders hold frames back). Optionally records an `.rdv` file (format: docs/protocol.md).
//!
//! [`run`] blocks for the duration of the test; call it outside an async context.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, ensure};
use base64::Engine as _;
use h264_reader::avcc::AvcDecoderConfigurationRecord;
use h264_reader::nal::sps::{PicOrderCntType, SeqParameterSet};
use h264_reader::nal::{Nal, RefNal};
use rd_agent_core::clock::now_us;
use rd_agent_core::h264::avcc_nals;
use rd_agent_core::{Admitted, CodecConfig, EncodedFrame, FrameGate, VideoConfig, VideoPipeline};
use rd_proto::bin::{VideoHeader, video_flags};
use rd_proto::control::AgentMsg;

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

/// Soft target from the M1 exit criteria; reported, not asserted.
const ENCODE_P50_TARGET_MS: f64 = 10.0;
/// The selftest forces an IDR this often to sample keyframe sizes.
const IDR_EVERY_SECONDS: u64 = 2;

/// Runs the selftest and prints a report to stdout. Returns Ok(true) if every assertion passed.
pub fn run(opts: &SelftestOptions) -> anyhow::Result<bool> {
    ensure!(opts.seconds >= 1, "--seconds must be at least 1");
    ensure!(
        opts.bitrate_kbps >= 100,
        "bitrate must be at least 100 kbps"
    );
    let mut pipeline: Box<dyn VideoPipeline> = if opts.synthetic {
        Box::new(SyntheticPipeline::with_rate_control(opts.rate_control)?)
    } else {
        ensure!(
            session::permissions().screen,
            "Screen Recording permission is not granted to this process; grant it in System \
             Settings > Privacy & Security, or run the synthetic selftest"
        );
        Box::new(ScreenPipeline::with_rate_control(opts.rate_control)?)
    };
    let cfg = VideoConfig {
        display: 0,
        width: opts.width,
        height: opts.height,
        fps: opts.fps,
        bitrate_bps: opts.bitrate_kbps.saturating_mul(1000),
    };
    let recorder = opts.record.as_deref().map(Recorder::create).transpose()?;
    let gate = Arc::new(SelftestGate::new(u64::from(opts.fps) * IDR_EVERY_SECONDS));
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let consumer = std::thread::Builder::new()
        .name("rd-selftest-consumer".into())
        .spawn(move || collect(rx, recorder))
        .context("spawning consumer thread")?;

    pipeline.start(cfg, gate.clone(), tx)?;
    let started = Instant::now();
    let mut rss = Vec::new();
    for _ in 0..opts.seconds {
        std::thread::sleep(Duration::from_secs(1));
        rss.extend(resident_bytes());
    }
    let elapsed = started.elapsed().as_secs_f64();
    pipeline.stop();
    // Dropping the pipeline drops the last FrameSink, which ends the consumer.
    drop(pipeline);
    let collected = consumer
        .join()
        .map_err(|_| anyhow!("selftest consumer panicked"))??;

    let report = Report::new(opts, elapsed, &gate, collected, &rss);
    report.print(opts);
    Ok(report.passed())
}

// ---------------------------------------------------------------------------

/// Admits every frame; forces an IDR every `key_every` frames and after a post-encode drop.
struct SelftestGate {
    key_every: u64,
    admitted: AtomicU64,
    cancelled: AtomicU64,
    post_encode_drops: AtomicU64,
    force_key: AtomicBool,
}

impl SelftestGate {
    fn new(key_every: u64) -> Self {
        SelftestGate {
            key_every: key_every.max(1),
            admitted: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            post_encode_drops: AtomicU64::new(0),
            force_key: AtomicBool::new(false),
        }
    }
}

impl FrameGate for SelftestGate {
    fn admit(&self, _capture_us: u64) -> Option<Admitted> {
        let n = self.admitted.fetch_add(1, Ordering::Relaxed);
        let periodic = n > 0 && n.is_multiple_of(self.key_every);
        Some(Admitted {
            seq: (n as u32).wrapping_add(1),
            force_keyframe: self.force_key.swap(false, Ordering::Relaxed) || periodic,
        })
    }

    fn cancel(&self, _seq: u32) {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
    }

    fn post_encode_drop(&self, _seq: u32) {
        self.post_encode_drops.fetch_add(1, Ordering::Relaxed);
        self.force_key.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct Collected {
    frames: u64,
    bytes: u64,
    encode_us: Vec<u32>,
    idr_sizes: Vec<usize>,
    delta_bytes: u64,
    delta_frames: u64,
    configs: Vec<CodecConfig>,
    first_frame: Option<FirstFrame>,
    bad_framing: u64,
    recording: Option<(PathBuf, u64, u64)>,
}

struct FirstFrame {
    key: bool,
    has_config: bool,
}

fn collect(
    mut rx: tokio::sync::mpsc::Receiver<EncodedFrame>,
    mut recorder: Option<Recorder>,
) -> anyhow::Result<Collected> {
    let mut c = Collected::default();
    let mut epoch: u16 = 0;
    while let Some(frame) = rx.blocking_recv() {
        if c.first_frame.is_none() {
            c.first_frame = Some(FirstFrame {
                key: frame.key,
                has_config: frame.config.is_some(),
            });
        }
        let mut flags = if frame.key { video_flags::KEY } else { 0 };
        if let Some(config) = &frame.config {
            epoch = epoch.wrapping_add(1);
            flags |= video_flags::FIRST_OF_EPOCH;
            if let Some(r) = recorder.as_mut() {
                r.video_config(epoch, config)?;
            }
            c.configs.push(config.clone());
        }
        if let Some(r) = recorder.as_mut() {
            let header = VideoHeader {
                flags,
                epoch,
                seq: frame.seq,
                capture_us: frame.capture_us,
                encode_us: frame.encode_us,
            };
            r.binary(&header.to_message(&frame.data))?;
        }
        if !avcc_framing_ok(&frame.data) {
            c.bad_framing += 1;
        }
        c.frames += 1;
        c.bytes += frame.data.len() as u64;
        c.encode_us.push(frame.encode_us);
        if frame.key {
            c.idr_sizes.push(frame.data.len());
        } else {
            c.delta_frames += 1;
            c.delta_bytes += frame.data.len() as u64;
        }
    }
    if let Some(r) = recorder {
        c.recording = Some(r.finish()?);
    }
    Ok(c)
}

/// The payload is a non-empty sequence of 4-byte length-prefixed NAL units, nothing else.
fn avcc_framing_ok(data: &[u8]) -> bool {
    let (mut count, mut consumed) = (0usize, 0usize);
    for nal in avcc_nals(data) {
        if nal.is_empty() {
            return false;
        }
        count += 1;
        consumed += 4 + nal.len();
    }
    count > 0 && consumed == data.len()
}

// ---------------------------------------------------------------------------

/// `.rdv` writer: `RDV1`, then `[kind u8][t_us u64 LE][len u32 LE][bytes]` records.
struct Recorder {
    path: PathBuf,
    out: BufWriter<File>,
    records: u64,
    bytes: u64,
}

impl Recorder {
    const TEXT: u8 = 0;
    const BINARY: u8 = 1;

    fn create(path: &Path) -> anyhow::Result<Self> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        out.write_all(b"RDV1")?;
        Ok(Recorder {
            path: path.to_owned(),
            out,
            records: 0,
            bytes: 4,
        })
    }

    fn video_config(&mut self, epoch: u16, config: &CodecConfig) -> anyhow::Result<()> {
        let msg = AgentMsg::VideoConfig {
            epoch,
            codec: config.codec.clone(),
            width: config.width,
            height: config.height,
            description: base64::engine::general_purpose::STANDARD.encode(&config.avcc),
        };
        self.record(Self::TEXT, serde_json::to_string(&msg)?.as_bytes())
    }

    fn binary(&mut self, message: &[u8]) -> anyhow::Result<()> {
        self.record(Self::BINARY, message)
    }

    fn record(&mut self, kind: u8, payload: &[u8]) -> anyhow::Result<()> {
        let len = u32::try_from(payload.len()).context("record too large")?;
        self.out.write_all(&[kind])?;
        self.out.write_all(&now_us().to_le_bytes())?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(payload)?;
        self.records += 1;
        self.bytes += 13 + u64::from(len);
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<(PathBuf, u64, u64)> {
        self.out
            .flush()
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok((self.path, self.records, self.bytes))
    }
}

// ---------------------------------------------------------------------------

struct SpsInfo {
    profile: String,
    profile_idc: u8,
    constraint_flags: u8,
    level: String,
    level_idc: u8,
    poc_type: u8,
    max_num_ref_frames: u32,
    size: Option<(u32, u32)>,
    bitstream_restriction: bool,
    max_num_reorder_frames: Option<u32>,
    max_dec_frame_buffering: Option<u32>,
}

/// Parses the first SPS of an avcC record.
fn parse_sps(avcc: &[u8]) -> anyhow::Result<SpsInfo> {
    let record = AvcDecoderConfigurationRecord::try_from(avcc)
        .map_err(|e| anyhow!("invalid avcC record: {e:?}"))?;
    let sps = record
        .sequence_parameter_sets()
        .next()
        .context("avcC record has no SPS")?
        .map_err(|e| anyhow!("invalid SPS in avcC: {e:?}"))?;
    let nal = RefNal::new(sps, &[], true);
    let sps = SeqParameterSet::from_bits(nal.rbsp_bits())
        .map_err(|e| anyhow!("cannot parse SPS: {e:?}"))?;
    let restrictions = sps
        .vui_parameters
        .as_ref()
        .and_then(|vui| vui.bitstream_restrictions.as_ref());
    Ok(SpsInfo {
        profile: format!("{:?}", sps.profile()),
        profile_idc: sps.profile_idc.into(),
        constraint_flags: sps.constraint_flags.into(),
        level: format!("{:?}", sps.level()),
        level_idc: sps.level_idc,
        poc_type: match sps.pic_order_cnt {
            PicOrderCntType::TypeZero { .. } => 0,
            PicOrderCntType::TypeOne { .. } => 1,
            PicOrderCntType::TypeTwo => 2,
        },
        max_num_ref_frames: sps.max_num_ref_frames,
        size: sps.pixel_dimensions().ok(),
        bitstream_restriction: restrictions.is_some(),
        max_num_reorder_frames: restrictions.map(|r| r.max_num_reorder_frames),
        max_dec_frame_buffering: restrictions.map(|r| r.max_dec_frame_buffering),
    })
}

// ---------------------------------------------------------------------------

struct Check {
    name: String,
    ok: bool,
    /// Soft checks are reported but do not fail the selftest.
    hard: bool,
}

struct Report {
    elapsed_s: f64,
    admitted: u64,
    cancelled: u64,
    post_encode_drops: u64,
    collected: Collected,
    encode_ms: Option<[f64; 4]>,
    sps: Option<anyhow::Result<SpsInfo>>,
    rss_mb: Option<(f64, f64, f64)>,
    checks: Vec<Check>,
}

impl Report {
    fn new(
        opts: &SelftestOptions,
        elapsed_s: f64,
        gate: &SelftestGate,
        mut collected: Collected,
        rss: &[u64],
    ) -> Self {
        let encode_ms = percentiles_ms(&mut collected.encode_us);
        let sps = collected.configs.first().map(|c| parse_sps(&c.avcc));
        let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
        let rss_mb = match (rss.first(), rss.last(), rss.iter().max()) {
            (Some(&first), Some(&last), Some(&max)) => Some((mb(first), mb(last), mb(max))),
            _ => None,
        };

        let mut checks = Vec::new();
        let mut check = |name: String, ok: bool, hard: bool| checks.push(Check { name, ok, hard });
        check(
            "encoded at least one frame".into(),
            collected.frames > 0,
            true,
        );
        let first = collected.first_frame.as_ref();
        check(
            "first frame carries the CodecConfig".into(),
            first.is_some_and(|f| f.has_config),
            true,
        );
        check(
            "first frame is an IDR".into(),
            first.is_some_and(|f| f.key),
            true,
        );
        check(
            "every payload is valid AVCC (4-byte length-prefixed NAL units)".into(),
            collected.bad_framing == 0,
            true,
        );
        check(
            "no post-encode drops".into(),
            gate.post_encode_drops.load(Ordering::Relaxed) == 0,
            true,
        );
        match &sps {
            Some(Ok(info)) => {
                check(
                    "SPS VUI bitstream_restriction_flag = 1".into(),
                    info.bitstream_restriction,
                    true,
                );
                check(
                    "SPS VUI max_num_reorder_frames = 0".into(),
                    info.max_num_reorder_frames == Some(0),
                    true,
                );
                check(
                    format!("SPS picture size is {}x{}", opts.width, opts.height),
                    info.size == Some((opts.width, opts.height)),
                    true,
                );
            }
            Some(Err(_)) | None => check("SPS parses".into(), false, true),
        }
        if let Some([p50, ..]) = encode_ms {
            check(
                format!("encode p50 <= {ENCODE_P50_TARGET_MS} ms"),
                p50 <= ENCODE_P50_TARGET_MS,
                false,
            );
        }
        if opts.synthetic {
            let fps = collected.frames as f64 / elapsed_s;
            check(
                format!("encoded >= 90 % of {} fps", opts.fps),
                fps >= 0.9 * f64::from(opts.fps),
                false,
            );
        }

        Report {
            elapsed_s,
            admitted: gate.admitted.load(Ordering::Relaxed),
            cancelled: gate.cancelled.load(Ordering::Relaxed),
            post_encode_drops: gate.post_encode_drops.load(Ordering::Relaxed),
            collected,
            encode_ms,
            sps,
            rss_mb,
            checks,
        }
    }

    fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok || !c.hard)
    }

    fn print(&self, opts: &SelftestOptions) {
        let c = &self.collected;
        let secs = self.elapsed_s;
        let kb = |b: f64| b / 1024.0;
        println!(
            "rd selftest: {} {}x{} @ {} fps, {} kbps, {} s, {} rate control",
            if opts.synthetic {
                "synthetic"
            } else {
                "screen"
            },
            opts.width,
            opts.height,
            opts.fps,
            opts.bitrate_kbps,
            opts.seconds,
            opts.rate_control.as_str()
        );
        println!("  duration        {secs:.2} s");
        println!(
            "  captured        {} frames ({:.1} fps)",
            self.admitted,
            self.admitted as f64 / secs
        );
        println!(
            "  encoded         {} frames ({:.1} fps), {} dropped by the encoder, {} post-encode drops",
            c.frames,
            c.frames as f64 / secs,
            self.cancelled,
            self.post_encode_drops
        );
        if let Some([p50, p90, p99, max]) = self.encode_ms {
            println!(
                "  encode latency  p50 {p50:.2} ms, p90 {p90:.2} ms, p99 {p99:.2} ms, max {max:.2} ms"
            );
        }
        if c.delta_frames > 0 {
            println!(
                "  frame size      avg {:.1} KiB (non-IDR)",
                kb(c.delta_bytes as f64 / c.delta_frames as f64)
            );
        }
        if !c.idr_sizes.is_empty() {
            let avg = c.idr_sizes.iter().sum::<usize>() as f64 / c.idr_sizes.len() as f64;
            let max = c.idr_sizes.iter().max().copied().unwrap_or(0);
            println!(
                "  IDR size        avg {:.1} KiB, max {:.1} KiB ({} IDRs; forced every {IDR_EVERY_SECONDS} s)",
                kb(avg),
                kb(max as f64),
                c.idr_sizes.len()
            );
        }
        println!(
            "  bitrate         {:.0} kbps",
            c.bytes as f64 * 8.0 / 1000.0 / secs
        );
        for config in &c.configs {
            println!(
                "  codec           {} {}x{} (avcC {} bytes)",
                config.codec,
                config.width,
                config.height,
                config.avcc.len()
            );
        }
        match &self.sps {
            Some(Ok(s)) => {
                println!(
                    "  SPS             profile {} (profile_idc {}, constraint flags 0x{:02x}), level {} (level_idc {}), {}, poc type {}, {} ref frames",
                    s.profile,
                    s.profile_idc,
                    s.constraint_flags,
                    s.level,
                    s.level_idc,
                    s.size.map_or("size ?".into(), |(w, h)| format!("{w}x{h}")),
                    s.poc_type,
                    s.max_num_ref_frames
                );
                println!(
                    "  SPS VUI         bitstream_restriction_flag={} max_num_reorder_frames={} max_dec_frame_buffering={}",
                    u8::from(s.bitstream_restriction),
                    opt(s.max_num_reorder_frames),
                    opt(s.max_dec_frame_buffering)
                );
            }
            Some(Err(e)) => println!("  SPS             parse error: {e:#}"),
            None => println!("  SPS             no codec config received"),
        }
        if let Some((first, last, max)) = self.rss_mb {
            println!(
                "  memory (RSS)    first {first:.1} MiB, last {last:.1} MiB, max {max:.1} MiB"
            );
        }
        if let Some((path, records, bytes)) = &c.recording {
            println!(
                "  recording       {} ({records} records, {:.1} MiB)",
                path.display(),
                *bytes as f64 / (1024.0 * 1024.0)
            );
        }
        println!("checks:");
        for check in &self.checks {
            let status = match (check.ok, check.hard) {
                (true, _) => "PASS",
                (false, true) => "FAIL",
                (false, false) => "WARN",
            };
            println!("  {status}  {}", check.name);
        }
        println!("result: {}", if self.passed() { "PASS" } else { "FAIL" });
    }
}

fn opt(v: Option<u32>) -> String {
    v.map_or_else(|| "-".into(), |v| v.to_string())
}

/// p50, p90, p99 and max in milliseconds.
fn percentiles_ms(samples_us: &mut [u32]) -> Option<[f64; 4]> {
    if samples_us.is_empty() {
        return None;
    }
    samples_us.sort_unstable();
    let at = |p: f64| {
        let i = ((samples_us.len() - 1) as f64 * p).round() as usize;
        f64::from(samples_us[i]) / 1000.0
    };
    Some([at(0.5), at(0.9), at(0.99), at(1.0)])
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
    use super::*;

    #[test]
    fn avcc_framing() {
        assert!(avcc_framing_ok(&[0, 0, 0, 2, 0x65, 0x88]));
        assert!(avcc_framing_ok(&[0, 0, 0, 1, 0x06, 0, 0, 0, 1, 0x65]));
        assert!(!avcc_framing_ok(&[]));
        assert!(!avcc_framing_ok(&[0, 0, 0, 3, 0x65, 0x88]));
        assert!(!avcc_framing_ok(&[0, 0, 0, 0]));
        assert!(!avcc_framing_ok(&[0, 0, 0, 1, 0x65, 0xff]));
    }

    #[test]
    fn percentiles() {
        let mut v: Vec<u32> = (1..=100).map(|ms| ms * 1000).collect();
        let [p50, p90, p99, max] = percentiles_ms(&mut v).unwrap();
        assert_eq!((p50.round(), p90, p99, max), (51.0, 90.0, 99.0, 100.0));
        assert!(percentiles_ms(&mut []).is_none());
    }

    #[test]
    fn gate_forces_periodic_keyframes() {
        let gate = SelftestGate::new(3);
        let keys: Vec<bool> = (0..7)
            .map(|_| gate.admit(0).unwrap().force_keyframe)
            .collect();
        assert_eq!(keys, [false, false, false, true, false, false, true]);
        gate.post_encode_drop(5);
        assert!(gate.admit(0).unwrap().force_keyframe);
        assert_eq!(gate.admit(0).unwrap().seq, 9);
    }

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
