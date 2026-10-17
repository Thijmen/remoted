//! Traits implemented by platform crates. The runtime owns one [`Platform`] and creates a
//! [`VideoPipeline`] / [`InputInjector`] (/ [`AudioSource`]) per session.

use std::sync::Arc;

use bytes::Bytes;
pub use rd_flow::Admitted;
pub use rd_proto::bin::InputEvent;
pub use rd_proto::control::{AgentState, DisplayDesc, Perms};

/// Requested video output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    /// Display id (0 = main display).
    pub display: u32,
    /// Output size in pixels (even numbers; already clamped by the runtime).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Initial average bitrate, bits/s.
    pub bitrate_bps: u32,
}

/// Decoder configuration for an epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    /// WebCodecs codec string, e.g. "avc1.640033" (see [`crate::h264::codec_string_from_avcc`]).
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// avcC record (AVCDecoderConfigurationRecord) -> `VideoDecoderConfig.description`.
    pub avcc: Bytes,
}

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// From [`FrameGate::admit`].
    pub seq: u32,
    /// Agent clock ([`crate::clock::now_us`]) when the frame was captured.
    pub capture_us: u64,
    /// Time spent in the encoder, µs.
    pub encode_us: u32,
    pub key: bool,
    /// AVCC: 4-byte big-endian length-prefixed NAL units, no in-band SPS/PPS required.
    pub data: Bytes,
    /// Set on the first frame after the encoder (re)started or its parameter sets changed.
    /// The runtime sends `video_config` (new epoch) before this frame.
    pub config: Option<CodecConfig>,
}

/// Admission control, called by the platform capture path for every captured frame.
/// Implemented by the runtime on top of [`rd_flow::FlowGate`].
pub trait FrameGate: Send + Sync + 'static {
    /// `capture_us`: agent clock at capture. `None` = drop this frame before encoding.
    /// If `Some(a)` and `a.force_keyframe`, encode as IDR.
    fn admit(&self, capture_us: u64) -> Option<Admitted>;
    /// The encoder dropped admitted frame `seq` (produced no output). Keeps the chain valid.
    fn cancel(&self, seq: u32);
    /// An already-ENCODED frame could not be delivered (e.g. `FrameSink::try_send` failed).
    /// Releases `seq` and forces the next admitted frame to be an IDR (the reference chain
    /// is broken).
    fn post_encode_drop(&self, seq: u32);
    /// True while a keyframe is wanted but no frame has been admitted to carry it (resume,
    /// keyframe_req, a new viewer). Screen capture only delivers frames when something
    /// changes, so on a static screen the pipeline should re-submit its most recent frame
    /// (through `admit` as usual) when this is true. Polling every ~50-100 ms is enough.
    fn refresh_wanted(&self) -> bool {
        false
    }
}

/// Encoded frames go to the runtime through this bounded channel (capacity 8). The encoder
/// callback uses `try_send`; on failure it must call [`FrameGate::post_encode_drop`].
pub type FrameSink = tokio::sync::mpsc::Sender<EncodedFrame>;

/// Capture + encode, fused (platforms can keep frames on the GPU end to end).
pub trait VideoPipeline: Send {
    /// Start capturing `cfg.display` at `cfg.width`x`cfg.height`, `cfg.fps`. For each captured
    /// frame call `gate.admit(capture_us)`; encode admitted frames; deliver to `sink`.
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<()>;
    /// Change size/fps/display. Must emit a new `CodecConfig` on the next frame.
    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()>;
    /// Change the encoder's average bitrate (bits/s). Cheap; may be called every 250 ms.
    fn set_bitrate(&self, bps: u32);
    /// Stop capture and encoding; no more frames are delivered after this returns.
    fn stop(&mut self);
}

/// Injects viewer input. Called from a dedicated input thread (never the network task).
pub trait InputInjector: Send {
    fn inject(&mut self, ev: InputEvent);
    /// Release every key/button currently held (session end, blur, RELEASE_ALL).
    fn release_all(&mut self);
}

/// Receives captured audio: `sink(samples, capture_us)`, 48 kHz stereo interleaved f32.
pub type AudioSink = Box<dyn FnMut(&[f32], u64) + Send>;

/// System audio capture. `sink(samples, capture_us)` receives 48 kHz stereo interleaved f32.
pub trait AudioSource: Send {
    fn start(&mut self, sink: AudioSink) -> anyhow::Result<()>;
    fn stop(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Unlocked,
    Locked,
    Unknown,
}

/// Opaque guard; dropping it releases the resource (e.g. a power assertion).
pub trait Guard: Send {}
impl<T: Send> Guard for T {}

/// A host platform.
pub trait Platform: Send + Sync + 'static {
    /// "macos" | "windows"
    fn os(&self) -> &'static str;
    /// Human description, e.g. "macOS 26.3 (25D125)".
    fn os_description(&self) -> String;
    fn displays(&self) -> Vec<DisplayDesc>;
    /// Native pixel size of a display (for clamping), e.g. (3456, 2234).
    fn native_size(&self, display: u32) -> Option<(u32, u32)>;
    fn permissions(&self) -> Perms;
    fn lock_state(&self) -> LockState;
    /// Keep the display awake while the guard lives.
    fn keep_awake(&self, reason: &str) -> Box<dyn Guard>;
    /// Tell the OS the user is active (wakes a sleeping display).
    fn declare_user_activity(&self);
    fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>>;
    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>>;
    fn audio_source(&self) -> Option<Box<dyn AudioSource>> {
        None
    }
}
