//! Video sender: turns the pipeline's [`EncodedFrame`]s into `video_config` + VIDEO messages.
//!
//! Epochs: every [`CodecConfig`] the pipeline attaches to a frame starts a new epoch. Its
//! `video_config` is sent right before the epoch's first frame, which carries
//! `FIRST_OF_EPOCH|KEY`. The viewer cannot decode deltas before that keyframe, so such frames
//! are discarded here (with [`FrameGate::post_encode_drop`], which forces the next IDR).

use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;
use rd_proto::bin::{VideoHeader, video_flags};
use rd_proto::control::AgentMsg;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use super::flow::Flow;
use crate::clock::now_us;
use crate::platform::{CodecConfig, EncodedFrame, FrameGate as _};

/// Sends every frame from `frames` through `out` (the session's video queue) until the
/// pipeline stops.
pub(crate) async fn send_video(
    mut frames: mpsc::Receiver<EncodedFrame>,
    flow: Arc<Flow>,
    out: mpsc::Sender<Message>,
    stats: Arc<Mutex<EncodeWindow>>,
) {
    forward(&mut frames, &flow, &out, &stats).await;
    // The socket is gone, so the session is ending: keep taking frames until the pipeline has
    // stopped, so its final output is not reported as a post-encode drop.
    while frames.recv().await.is_some() {}
}

/// Returns when `out` closes.
async fn forward(
    frames: &mut mpsc::Receiver<EncodedFrame>,
    flow: &Flow,
    out: &mpsc::Sender<Message>,
    stats: &Mutex<EncodeWindow>,
) {
    let mut epochs = Epochs::default();
    let mut warned = false;
    while let Some(mut frame) = frames.recv().await {
        let (config, header) = match epochs.plan(&mut frame) {
            Plan::Discard => {
                if !warned {
                    warn!(
                        seq = frame.seq,
                        "discarding encoded delta frame: its codec config has no keyframe yet"
                    );
                    warned = true;
                }
                flow.post_encode_drop(frame.seq);
                continue;
            }
            Plan::Send { config, header } => (config, header),
        };
        warned = false;
        if let Some(config) = config {
            let text = match serde_json::to_string(&config) {
                Ok(text) => text,
                Err(e) => {
                    warn!("serializing video_config: {e}");
                    return;
                }
            };
            if out.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        let bytes = frame.data.len();
        let now = now_us();
        // Counted as sent once it is queued: time in our own queue is queueing delay too.
        flow.gate.on_sent(frame.seq, bytes, frame.key, now);
        flow.bitrate.lock().on_encoded(bytes, now);
        stats.lock().record(frame.encode_us, bytes);
        let message = header.to_message(&frame.data);
        if out.send(Message::Binary(message.into())).await.is_err() {
            return;
        }
    }
}

/// What to do with one encoded frame.
#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    /// The viewer could not decode it: discard.
    Discard,
    /// Send `config` (a `video_config`) first if present, then the frame with `header`.
    Send {
        config: Option<AgentMsg>,
        header: VideoHeader,
    },
}

/// Epoch bookkeeping of one session's video stream.
#[derive(Debug, Default)]
pub(crate) struct Epochs {
    /// Current epoch; 0 = none yet (epochs start at 1).
    epoch: u16,
    /// A config waiting for its first keyframe.
    pending: Option<CodecConfig>,
}

impl Epochs {
    pub(crate) fn plan(&mut self, frame: &mut EncodedFrame) -> Plan {
        if let Some(config) = frame.config.take() {
            self.pending = Some(config);
        }
        let mut flags = if frame.key { video_flags::KEY } else { 0 };
        let config = match self.pending.take() {
            Some(config) if frame.key => {
                self.epoch = match self.epoch.wrapping_add(1) {
                    0 => 1,
                    e => e,
                };
                flags |= video_flags::FIRST_OF_EPOCH;
                info!(
                    epoch = self.epoch,
                    codec = config.codec,
                    width = config.width,
                    height = config.height,
                    "new video epoch"
                );
                Some(video_config(self.epoch, &config))
            }
            Some(config) => {
                self.pending = Some(config);
                return Plan::Discard;
            }
            None if self.epoch == 0 => return Plan::Discard,
            None => None,
        };
        Plan::Send {
            config,
            header: VideoHeader {
                flags,
                epoch: self.epoch,
                seq: frame.seq,
                capture_us: frame.capture_us,
                encode_us: frame.encode_us,
            },
        }
    }
}

fn video_config(epoch: u16, config: &CodecConfig) -> AgentMsg {
    AgentMsg::VideoConfig {
        epoch,
        codec: config.codec.clone(),
        width: config.width,
        height: config.height,
        description: base64::engine::general_purpose::STANDARD.encode(&config.avcc),
    }
}

/// Encoder statistics since the last [`EncodeWindow::take`] (one stats period).
#[derive(Debug, Default)]
pub(crate) struct EncodeWindow {
    encode_us: Vec<u32>,
    frames: u32,
    bytes: u64,
}

/// Summary of one stats period.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct EncodeSummary {
    pub(crate) frames: u32,
    pub(crate) bytes: u64,
    pub(crate) enc_ms_p50: f32,
    pub(crate) enc_ms_p99: f32,
}

impl EncodeWindow {
    pub(crate) fn record(&mut self, encode_us: u32, bytes: usize) {
        self.encode_us.push(encode_us);
        self.frames += 1;
        self.bytes += bytes as u64;
    }

    /// Returns the summary and starts a new period.
    pub(crate) fn take(&mut self) -> EncodeSummary {
        let mut samples = std::mem::take(&mut self.encode_us);
        samples.sort_unstable();
        let summary = EncodeSummary {
            frames: self.frames,
            bytes: self.bytes,
            enc_ms_p50: percentile(&samples, 50) as f32 / 1000.0,
            enc_ms_p99: percentile(&samples, 99) as f32 / 1000.0,
        };
        self.frames = 0;
        self.bytes = 0;
        // Keep the allocation for the next period.
        samples.clear();
        self.encode_us = samples;
        summary
    }
}

/// Nearest-rank percentile of sorted samples (0 when empty).
fn percentile(sorted: &[u32], p: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn frame(seq: u32, key: bool, config: bool) -> EncodedFrame {
        EncodedFrame {
            seq,
            capture_us: u64::from(seq) * 1000,
            encode_us: 500,
            key,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            config: config.then(|| CodecConfig {
                codec: "avc1.640033".into(),
                width: 1728,
                height: 1118,
                avcc: Bytes::from_static(&[1, 0x64, 0, 0x33, 0xff]),
            }),
        }
    }

    fn sent(plan: Plan) -> (Option<AgentMsg>, VideoHeader) {
        match plan {
            Plan::Send { config, header } => (config, header),
            Plan::Discard => panic!("discarded"),
        }
    }

    #[test]
    fn epochs_and_flags() {
        let mut e = Epochs::default();
        // Nothing decodable before the first config.
        assert_eq!(e.plan(&mut frame(1, true, false)), Plan::Discard);
        let (config, h) = sent(e.plan(&mut frame(2, true, true)));
        match config {
            Some(AgentMsg::VideoConfig {
                epoch, description, ..
            }) => {
                assert_eq!(epoch, 1);
                assert_eq!(description, "AWQAM/8=");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(h.epoch, 1);
        assert_eq!(h.seq, 2);
        assert_eq!(h.flags, video_flags::KEY | video_flags::FIRST_OF_EPOCH);
        let (config, h) = sent(e.plan(&mut frame(3, false, false)));
        assert!(config.is_none());
        assert_eq!((h.epoch, h.flags), (1, 0));
        // A config on a delta frame waits for the next keyframe.
        assert_eq!(e.plan(&mut frame(4, false, true)), Plan::Discard);
        assert_eq!(e.plan(&mut frame(5, false, false)), Plan::Discard);
        let (config, h) = sent(e.plan(&mut frame(6, true, false)));
        assert!(matches!(
            config,
            Some(AgentMsg::VideoConfig { epoch: 2, .. })
        ));
        assert_eq!(h.flags, video_flags::KEY | video_flags::FIRST_OF_EPOCH);
        // A plain keyframe within an epoch.
        let (config, h) = sent(e.plan(&mut frame(7, true, false)));
        assert!(config.is_none());
        assert_eq!((h.epoch, h.flags), (2, video_flags::KEY));
    }

    #[test]
    fn epoch_wraps_past_zero() {
        let mut e = Epochs {
            epoch: u16::MAX,
            pending: None,
        };
        let (_, h) = sent(e.plan(&mut frame(1, true, true)));
        assert_eq!(h.epoch, 1);
    }

    #[test]
    fn encode_window() {
        let mut w = EncodeWindow::default();
        assert_eq!(w.take(), EncodeSummary::default());
        for us in 1..=100 {
            w.record(us * 100, 10);
        }
        let s = w.take();
        assert_eq!((s.frames, s.bytes), (100, 1000));
        assert_eq!(s.enc_ms_p50, 5.0);
        assert_eq!(s.enc_ms_p99, 9.9);
        assert_eq!(w.take().frames, 0);
    }
}
