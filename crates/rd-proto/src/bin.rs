//! Binary frames. Every binary WebSocket message starts with `[type u8][flags u8]`.
//! All multi-byte integers are little-endian. Unknown types must be ignored.
//!
//! | type | name        | dir | layout (after `[type][flags]`)                                   | len |
//! |------|-------------|-----|------------------------------------------------------------------|-----|
//! | 0x01 | VIDEO       | A→V | epoch u16, seq u32, capture_us u64, encode_us u32, then AVCC     | 20+ |
//! | 0x02 | AUDIO       | A→V | stream u16, seq u32, capture_us u64, then one Opus packet        | 16+ |
//! | 0x10 | MOUSE_ABS   | V→A | (flags=buttons bitmask) x u16, y u16 (0..=65535 over the frame)  | 6   |
//! | 0x11 | MOUSE_REL   | V→A | dx i16, dy i16                                                    | 6   |
//! | 0x12 | MOUSE_BTN   | V→A | (flags=button) down u8, pad u8, x u16, y u16                      | 8   |
//! | 0x13 | WHEEL       | V→A | (flags=mode) dx i16, dy i16, phase u8, pad u8                     | 8   |
//! | 0x14 | KEY         | V→A | (flags=down) key_flags u8, pad u8, hid u32                        | 8   |
//! | 0x15 | RELEASE_ALL | V→A | –                                                                 | 2   |
//! | 0x20 | VIDEO_ACK   | V→A | epoch u16, seq u32, decode_us u16, queue_us u16                   | 12  |
//! | 0x30 | PING        | both| pad u16, id u32, t_origin_us u64, pad u64                         | 24  |
//! | 0x31 | PONG        | both| pad u16, id u32, t_origin_us u64, t_reply_us u64                  | 24  |
//! | 0x80 | PROBE_UP    | V→G | opaque payload (gateway network probe only)                      | 2+  |
//!
//! Mouse button bitmask (MOUSE_ABS flags): bit0 = left, bit1 = middle, bit2 = right,
//! bit3 = back, bit4 = forward. NOTE: this differs from DOM `MouseEvent.buttons`
//! (left=1, right=2, middle=4); the viewer converts.

use crate::ProtoError;

/// Message type bytes.
pub mod ty {
    pub const VIDEO: u8 = 0x01;
    pub const AUDIO: u8 = 0x02;
    /// Reserved for a future cursor shape/position channel.
    pub const CURSOR: u8 = 0x03;
    pub const MOUSE_ABS: u8 = 0x10;
    pub const MOUSE_REL: u8 = 0x11;
    pub const MOUSE_BTN: u8 = 0x12;
    pub const WHEEL: u8 = 0x13;
    pub const KEY: u8 = 0x14;
    pub const RELEASE_ALL: u8 = 0x15;
    pub const VIDEO_ACK: u8 = 0x20;
    pub const PING: u8 = 0x30;
    pub const PONG: u8 = 0x31;
    pub const PROBE_UP: u8 = 0x80;

    /// True for viewer->agent input messages (used by the gateway for idle tracking).
    pub fn is_input(t: u8) -> bool {
        (MOUSE_ABS..=RELEASE_ALL).contains(&t)
    }
}

/// VIDEO flags.
pub mod video_flags {
    /// IDR / keyframe.
    pub const KEY: u8 = 0x01;
    /// Static-screen quality refinement of an unchanged frame.
    pub const REFINE: u8 = 0x02;
    /// First frame of a new epoch (a `video_config` was sent just before).
    pub const FIRST_OF_EPOCH: u8 = 0x04;
}

/// VIDEO_ACK flags.
pub mod ack_flags {
    /// Viewer discarded the frame without decoding (wrong epoch / waiting for keyframe).
    pub const DISCARDED: u8 = 0x01;
}

/// KEY `key_flags`.
pub mod key_flags {
    /// Browser auto-repeat.
    pub const REPEAT: u8 = 0x01;
}

/// Mouse button bitmask bits (MOUSE_ABS flags byte).
pub mod buttons {
    pub const LEFT: u8 = 1 << 0;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const BACK: u8 = 1 << 3;
    pub const FORWARD: u8 = 1 << 4;
}

pub const VIDEO_HEADER_LEN: usize = 20;
pub const AUDIO_HEADER_LEN: usize = 16;
pub const MOUSE_ABS_LEN: usize = 6;
pub const MOUSE_REL_LEN: usize = 6;
pub const MOUSE_BTN_LEN: usize = 8;
pub const WHEEL_LEN: usize = 8;
pub const KEY_LEN: usize = 8;
pub const RELEASE_ALL_LEN: usize = 2;
pub const VIDEO_ACK_LEN: usize = 12;
pub const PING_LEN: usize = 24;
pub const PONG_LEN: usize = 24;

// ---------------------------------------------------------------------------
// little-endian helpers

#[inline]
fn need(b: &[u8], ty: u8, n: usize) -> Result<(), ProtoError> {
    if b.len() < n {
        Err(ProtoError::TooShort {
            ty,
            len: b.len(),
            need: n,
        })
    } else {
        Ok(())
    }
}
#[inline]
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn i16_at(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// VIDEO / AUDIO headers

/// Header of a VIDEO message; the AVCC payload (4-byte length-prefixed NAL units) follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoHeader {
    pub flags: u8,
    pub epoch: u16,
    pub seq: u32,
    /// Agent monotonic clock, microseconds.
    pub capture_us: u64,
    /// Encoder latency for this frame, microseconds.
    pub encode_us: u32,
}

impl VideoHeader {
    pub fn is_key(&self) -> bool {
        self.flags & video_flags::KEY != 0
    }

    /// Append the 20-byte header to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(VIDEO_HEADER_LEN);
        out.push(ty::VIDEO);
        out.push(self.flags);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.encode_us.to_le_bytes());
    }

    /// Build a complete VIDEO message (header + payload).
    pub fn to_message(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(VIDEO_HEADER_LEN + payload.len());
        self.write(&mut v);
        v.extend_from_slice(payload);
        v
    }

    fn parse_body(b: &[u8]) -> Result<(Self, &[u8]), ProtoError> {
        need(b, ty::VIDEO, VIDEO_HEADER_LEN)?;
        Ok((
            VideoHeader {
                flags: b[1],
                epoch: u16_at(b, 2),
                seq: u32_at(b, 4),
                capture_us: u64_at(b, 8),
                encode_us: u32_at(b, 16),
            },
            &b[VIDEO_HEADER_LEN..],
        ))
    }
}

/// Header of an AUDIO message; one Opus packet follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioHeader {
    pub flags: u8,
    pub stream: u16,
    pub seq: u32,
    pub capture_us: u64,
}

impl AudioHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(AUDIO_HEADER_LEN);
        out.push(ty::AUDIO);
        out.push(self.flags);
        out.extend_from_slice(&self.stream.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
    }

    pub fn to_message(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(AUDIO_HEADER_LEN + payload.len());
        self.write(&mut v);
        v.extend_from_slice(payload);
        v
    }

    fn parse_body(b: &[u8]) -> Result<(Self, &[u8]), ProtoError> {
        need(b, ty::AUDIO, AUDIO_HEADER_LEN)?;
        Ok((
            AudioHeader {
                flags: b[1],
                stream: u16_at(b, 2),
                seq: u32_at(b, 4),
                capture_us: u64_at(b, 8),
            },
            &b[AUDIO_HEADER_LEN..],
        ))
    }
}

// ---------------------------------------------------------------------------
// input

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    Back = 3,
    Forward = 4,
}

impl MouseButton {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        Ok(match v {
            0 => Self::Left,
            1 => Self::Middle,
            2 => Self::Right,
            3 => Self::Back,
            4 => Self::Forward,
            _ => {
                return Err(ProtoError::InvalidValue {
                    field: "button",
                    value: v as u32,
                });
            }
        })
    }
    /// Bit in the MOUSE_ABS buttons bitmask.
    pub fn mask(self) -> u8 {
        1 << (self as u8)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WheelMode {
    /// Continuous (trackpad / smooth) scrolling in CSS pixels.
    Pixel = 0,
    /// Discrete wheel notches, in lines.
    Line = 1,
}

impl WheelMode {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        Ok(match v {
            0 => Self::Pixel,
            1 => Self::Line,
            _ => {
                return Err(ProtoError::InvalidValue {
                    field: "wheel_mode",
                    value: v as u32,
                });
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScrollPhase {
    None = 0,
    Began = 1,
    Changed = 2,
    Ended = 3,
}

impl ScrollPhase {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        Ok(match v {
            0 => Self::None,
            1 => Self::Began,
            2 => Self::Changed,
            3 => Self::Ended,
            _ => {
                return Err(ProtoError::InvalidValue {
                    field: "scroll_phase",
                    value: v as u32,
                });
            }
        })
    }
}

/// Viewer -> agent input event. Coordinates are normalised 0..=65535 over the video frame.
/// Keys are USB HID usages: `(page << 16) | usage`, e.g. KeyA = 0x0007_0004 (see [`crate::keymap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    MouseAbs {
        buttons: u8,
        x: u16,
        y: u16,
    },
    MouseRel {
        dx: i16,
        dy: i16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        x: u16,
        y: u16,
    },
    Wheel {
        mode: WheelMode,
        dx: i16,
        dy: i16,
        phase: ScrollPhase,
    },
    Key {
        down: bool,
        repeat: bool,
        hid: u32,
    },
    ReleaseAll,
}

impl InputEvent {
    pub fn write(&self, out: &mut Vec<u8>) {
        match *self {
            InputEvent::MouseAbs { buttons, x, y } => {
                out.extend_from_slice(&[ty::MOUSE_ABS, buttons]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            InputEvent::MouseRel { dx, dy } => {
                out.extend_from_slice(&[ty::MOUSE_REL, 0]);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            InputEvent::MouseButton { button, down, x, y } => {
                out.extend_from_slice(&[ty::MOUSE_BTN, button as u8, down as u8, 0]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            InputEvent::Wheel {
                mode,
                dx,
                dy,
                phase,
            } => {
                out.extend_from_slice(&[ty::WHEEL, mode as u8]);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out.extend_from_slice(&[phase as u8, 0]);
            }
            InputEvent::Key { down, repeat, hid } => {
                let kf = if repeat { key_flags::REPEAT } else { 0 };
                out.extend_from_slice(&[ty::KEY, down as u8, kf, 0]);
                out.extend_from_slice(&hid.to_le_bytes());
            }
            InputEvent::ReleaseAll => out.extend_from_slice(&[ty::RELEASE_ALL, 0]),
        }
    }

    pub fn to_message(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8);
        self.write(&mut v);
        v
    }

    fn parse_body(b: &[u8]) -> Result<Self, ProtoError> {
        let t = b[0];
        Ok(match t {
            ty::MOUSE_ABS => {
                need(b, t, MOUSE_ABS_LEN)?;
                InputEvent::MouseAbs {
                    buttons: b[1],
                    x: u16_at(b, 2),
                    y: u16_at(b, 4),
                }
            }
            ty::MOUSE_REL => {
                need(b, t, MOUSE_REL_LEN)?;
                InputEvent::MouseRel {
                    dx: i16_at(b, 2),
                    dy: i16_at(b, 4),
                }
            }
            ty::MOUSE_BTN => {
                need(b, t, MOUSE_BTN_LEN)?;
                InputEvent::MouseButton {
                    button: MouseButton::from_u8(b[1])?,
                    down: b[2] != 0,
                    x: u16_at(b, 4),
                    y: u16_at(b, 6),
                }
            }
            ty::WHEEL => {
                need(b, t, WHEEL_LEN)?;
                InputEvent::Wheel {
                    mode: WheelMode::from_u8(b[1])?,
                    dx: i16_at(b, 2),
                    dy: i16_at(b, 4),
                    phase: ScrollPhase::from_u8(b[6])?,
                }
            }
            ty::KEY => {
                need(b, t, KEY_LEN)?;
                InputEvent::Key {
                    down: b[1] != 0,
                    repeat: b[2] & key_flags::REPEAT != 0,
                    hid: u32_at(b, 4),
                }
            }
            ty::RELEASE_ALL => InputEvent::ReleaseAll,
            _ => unreachable!("parse_body called with non-input type"),
        })
    }
}

// ---------------------------------------------------------------------------
// ack / ping / pong

/// Cumulative ACK: the viewer has finished (decoded or discarded) every frame with seq <= `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoAck {
    pub flags: u8,
    pub epoch: u16,
    pub seq: u32,
    /// Decoder time for this frame (saturating), microseconds.
    pub decode_us: u16,
    /// Time the chunk waited in the viewer before being submitted to the decoder (saturating), µs.
    pub queue_us: u16,
}

impl VideoAck {
    pub fn discarded(&self) -> bool {
        self.flags & ack_flags::DISCARDED != 0
    }
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[ty::VIDEO_ACK, self.flags]);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.decode_us.to_le_bytes());
        out.extend_from_slice(&self.queue_us.to_le_bytes());
    }
    pub fn to_message(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(VIDEO_ACK_LEN);
        self.write(&mut v);
        v
    }
    fn parse_body(b: &[u8]) -> Result<Self, ProtoError> {
        need(b, ty::VIDEO_ACK, VIDEO_ACK_LEN)?;
        Ok(VideoAck {
            flags: b[1],
            epoch: u16_at(b, 2),
            seq: u32_at(b, 4),
            decode_us: u16_at(b, 8),
            queue_us: u16_at(b, 10),
        })
    }
}

/// PING: either side may send; the receiver must answer with a PONG echoing `id` and `t_origin_us`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ping {
    pub id: u32,
    /// Sender's clock, microseconds.
    pub t_origin_us: u64,
}

impl Ping {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[ty::PING, 0, 0, 0]);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.t_origin_us.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }
    pub fn to_message(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(PING_LEN);
        self.write(&mut v);
        v
    }
    /// The PONG answering this ping.
    pub fn pong(&self, t_reply_us: u64) -> Pong {
        Pong {
            id: self.id,
            t_origin_us: self.t_origin_us,
            t_reply_us,
        }
    }
    fn parse_body(b: &[u8]) -> Result<Self, ProtoError> {
        need(b, ty::PING, PING_LEN)?;
        Ok(Ping {
            id: u32_at(b, 4),
            t_origin_us: u64_at(b, 8),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pong {
    pub id: u32,
    /// Echo of the ping's `t_origin_us` (pinger's clock).
    pub t_origin_us: u64,
    /// Responder's clock when replying, microseconds.
    pub t_reply_us: u64,
}

impl Pong {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[ty::PONG, 0, 0, 0]);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.t_origin_us.to_le_bytes());
        out.extend_from_slice(&self.t_reply_us.to_le_bytes());
    }
    pub fn to_message(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(PONG_LEN);
        self.write(&mut v);
        v
    }
    fn parse_body(b: &[u8]) -> Result<Self, ProtoError> {
        need(b, ty::PONG, PONG_LEN)?;
        Ok(Pong {
            id: u32_at(b, 4),
            t_origin_us: u64_at(b, 8),
            t_reply_us: u64_at(b, 16),
        })
    }
}

// ---------------------------------------------------------------------------
// top-level parse

/// A parsed binary message. Payloads borrow from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinMsg<'a> {
    Video(VideoHeader, &'a [u8]),
    Audio(AudioHeader, &'a [u8]),
    Input(InputEvent),
    VideoAck(VideoAck),
    Ping(Ping),
    Pong(Pong),
    ProbeUp(&'a [u8]),
    /// Unknown or reserved type: must be ignored by receivers (forward compatibility).
    Unknown(u8),
}

/// Parse one binary WebSocket message.
pub fn parse(b: &[u8]) -> Result<BinMsg<'_>, ProtoError> {
    let Some(&t) = b.first() else {
        return Err(ProtoError::Empty);
    };
    Ok(match t {
        ty::VIDEO => {
            let (h, p) = VideoHeader::parse_body(b)?;
            BinMsg::Video(h, p)
        }
        ty::AUDIO => {
            let (h, p) = AudioHeader::parse_body(b)?;
            BinMsg::Audio(h, p)
        }
        ty::MOUSE_ABS | ty::MOUSE_REL | ty::MOUSE_BTN | ty::WHEEL | ty::KEY => {
            BinMsg::Input(InputEvent::parse_body(b)?)
        }
        ty::RELEASE_ALL => BinMsg::Input(InputEvent::ReleaseAll),
        ty::VIDEO_ACK => BinMsg::VideoAck(VideoAck::parse_body(b)?),
        ty::PING => BinMsg::Ping(Ping::parse_body(b)?),
        ty::PONG => BinMsg::Pong(Pong::parse_body(b)?),
        ty::PROBE_UP => BinMsg::ProbeUp(if b.len() >= 2 { &b[2..] } else { &[] }),
        other => BinMsg::Unknown(other),
    })
}

/// Build a PROBE_UP message with `payload_len` filler bytes.
pub fn probe_up_message(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + payload.len());
    v.extend_from_slice(&[ty::PROBE_UP, 0]);
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_roundtrip() {
        let h = VideoHeader {
            flags: video_flags::KEY | video_flags::FIRST_OF_EPOCH,
            epoch: 3,
            seq: 42,
            capture_us: 1_234_567_890_123,
            encode_us: 9000,
        };
        let m = h.to_message(&[0, 0, 0, 1, 0x65]);
        assert_eq!(m.len(), VIDEO_HEADER_LEN + 5);
        match parse(&m).unwrap() {
            BinMsg::Video(h2, p) => {
                assert_eq!(h, h2);
                assert!(h2.is_key());
                assert_eq!(p, &[0, 0, 0, 1, 0x65]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn input_roundtrip() {
        let evs = [
            InputEvent::MouseAbs {
                buttons: buttons::LEFT | buttons::RIGHT,
                x: 65535,
                y: 1,
            },
            InputEvent::MouseRel { dx: -5, dy: 300 },
            InputEvent::MouseButton {
                button: MouseButton::Back,
                down: true,
                x: 10,
                y: 20,
            },
            InputEvent::Wheel {
                mode: WheelMode::Pixel,
                dx: -1,
                dy: 120,
                phase: ScrollPhase::Changed,
            },
            InputEvent::Key {
                down: true,
                repeat: true,
                hid: 0x0007_0004,
            },
            InputEvent::ReleaseAll,
        ];
        for e in evs {
            let m = e.to_message();
            assert!(ty::is_input(m[0]));
            assert_eq!(parse(&m).unwrap(), BinMsg::Input(e));
        }
    }

    #[test]
    fn ack_ping_pong_roundtrip() {
        let a = VideoAck {
            flags: ack_flags::DISCARDED,
            epoch: 7,
            seq: 99,
            decode_us: 1500,
            queue_us: 3,
        };
        assert_eq!(parse(&a.to_message()).unwrap(), BinMsg::VideoAck(a));
        let p = Ping {
            id: 5,
            t_origin_us: 77,
        };
        assert_eq!(parse(&p.to_message()).unwrap(), BinMsg::Ping(p));
        let q = p.pong(88);
        assert_eq!(parse(&q.to_message()).unwrap(), BinMsg::Pong(q));
    }

    #[test]
    fn short_and_unknown() {
        assert_eq!(parse(&[]), Err(ProtoError::Empty));
        assert!(matches!(
            parse(&[ty::VIDEO, 0, 1]),
            Err(ProtoError::TooShort { .. })
        ));
        assert!(matches!(
            parse(&[ty::KEY, 1]),
            Err(ProtoError::TooShort { .. })
        ));
        assert_eq!(parse(&[0x7f, 0, 1, 2]).unwrap(), BinMsg::Unknown(0x7f));
        assert_eq!(parse(&[ty::CURSOR]).unwrap(), BinMsg::Unknown(ty::CURSOR));
        assert!(parse(&[ty::MOUSE_BTN, 9, 1, 0, 0, 0, 0, 0]).is_err());
    }
}
