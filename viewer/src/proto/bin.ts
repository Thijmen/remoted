// Binary frames — mirror of crates/rd-proto/src/bin.rs (the source of truth).
// Every binary WebSocket message starts with [type u8][flags u8]; integers are little-endian.
// u64 fields are BigInt so that every value round-trips exactly, except file offsets: plain numbers
// (exact up to 2^53 bytes, 8 PiB).

export const MsgType = {
  VIDEO: 0x01,
  AUDIO: 0x02,
  /** Reserved for a future cursor channel. */
  CURSOR: 0x03,
  MOUSE_ABS: 0x10,
  MOUSE_REL: 0x11,
  MOUSE_BTN: 0x12,
  WHEEL: 0x13,
  KEY: 0x14,
  RELEASE_ALL: 0x15,
  VIDEO_ACK: 0x20,
  PING: 0x30,
  PONG: 0x31,
  /** File channels only (docs/files.md). */
  FILE_DATA: 0x40,
  FILE_ACK: 0x41,
  PROBE_UP: 0x80,
} as const;

/** An ArrayBuffer-backed byte array (what `WebSocket.send` accepts). */
export type Bytes = Uint8Array<ArrayBuffer>;

export const VideoFlags = { KEY: 0x01, REFINE: 0x02, FIRST_OF_EPOCH: 0x04 } as const;
export const AckFlags = { DISCARDED: 0x01 } as const;
export const KeyFlags = { REPEAT: 0x01 } as const;
/** FILE_DATA flags. */
export const FileFlags = { END: 0x01 } as const;

/**
 * Wire button ids. The MOUSE_ABS flags byte is a bitmask of `1 << MouseButton` (left=1, middle=2,
 * right=4, back=8, forward=16), which differs from DOM `MouseEvent.buttons` (right=2, middle=4).
 */
export const MouseButton = { Left: 0, Middle: 1, Right: 2, Back: 3, Forward: 4 } as const;
export type MouseButton = (typeof MouseButton)[keyof typeof MouseButton];

export const WheelMode = { Pixel: 0, Line: 1 } as const;
export type WheelMode = (typeof WheelMode)[keyof typeof WheelMode];

export const ScrollPhase = { None: 0, Began: 1, Changed: 2, Ended: 3 } as const;
export type ScrollPhase = (typeof ScrollPhase)[keyof typeof ScrollPhase];

export const VIDEO_HEADER_LEN = 20;
export const AUDIO_HEADER_LEN = 16;
export const MOUSE_ABS_LEN = 6;
export const MOUSE_REL_LEN = 6;
export const MOUSE_BTN_LEN = 8;
export const WHEEL_LEN = 8;
export const KEY_LEN = 8;
export const RELEASE_ALL_LEN = 2;
export const VIDEO_ACK_LEN = 12;
export const PING_LEN = 24;
export const PONG_LEN = 24;
export const FILE_DATA_HEADER_LEN = 20;
export const FILE_ACK_LEN = 8;

export interface VideoMsg {
  type: 'video';
  flags: number;
  epoch: number;
  seq: number;
  /** Agent monotonic clock, µs. */
  captureUs: bigint;
  encodeUs: number;
  /** AVCC access unit (view into the received buffer). */
  payload: Uint8Array;
}

export interface AudioMsg {
  type: 'audio';
  flags: number;
  stream: number;
  seq: number;
  captureUs: bigint;
  /** One Opus packet. */
  payload: Uint8Array;
}

export interface MouseAbsMsg {
  type: 'mouse_abs';
  buttons: number;
  x: number;
  y: number;
}
export interface MouseRelMsg {
  type: 'mouse_rel';
  dx: number;
  dy: number;
}
export interface MouseBtnMsg {
  type: 'mouse_btn';
  button: MouseButton;
  down: boolean;
  x: number;
  y: number;
}
export interface WheelMsg {
  type: 'wheel';
  mode: WheelMode;
  dx: number;
  dy: number;
  phase: ScrollPhase;
}
export interface KeyMsg {
  type: 'key';
  down: boolean;
  repeat: boolean;
  /** USB HID usage, (page << 16) | usage. */
  hid: number;
}
export interface ReleaseAllMsg {
  type: 'release_all';
}
export interface VideoAckMsg {
  type: 'video_ack';
  flags: number;
  epoch: number;
  seq: number;
  decodeUs: number;
  queueUs: number;
}
export interface PingMsg {
  type: 'ping';
  id: number;
  tOriginUs: bigint;
}
export interface PongMsg {
  type: 'pong';
  id: number;
  tOriginUs: bigint;
  tReplyUs: bigint;
}
export interface FileDataMsg {
  type: 'file_data';
  flags: number;
  /** The transfer (the request's `id`). */
  id: number;
  seq: number;
  offset: number;
  payload: Uint8Array;
}
export interface FileAckMsg {
  type: 'file_ack';
  seq: number;
}
export interface ProbeUpMsg {
  type: 'probe_up';
  payload: Uint8Array;
}
export interface UnknownMsg {
  type: 'unknown';
  ty: number;
}

export type InputMsg = MouseAbsMsg | MouseRelMsg | MouseBtnMsg | WheelMsg | KeyMsg | ReleaseAllMsg;

export type BinMsg =
  VideoMsg | AudioMsg | InputMsg | VideoAckMsg | PingMsg | PongMsg | FileDataMsg | FileAckMsg | ProbeUpMsg | UnknownMsg;

/** Messages this side can build. */
export type EncodableMsg = Exclude<BinMsg, UnknownMsg>;

export class ProtoError extends Error {
  override name = 'ProtoError';
}

// ---------------------------------------------------------------------------
// decode

function need(b: Uint8Array, ty: number, n: number): void {
  if (b.byteLength < n) {
    throw new ProtoError(`message type 0x${ty.toString(16).padStart(2, '0')} too short: ${b.byteLength} < ${n}`);
  }
}

function enumValue<T extends number>(field: string, v: number, max: number): T {
  if (v > max) throw new ProtoError(`invalid value for ${field}: ${v}`);
  return v as T;
}

function asBytes(data: ArrayBuffer | Uint8Array): Uint8Array {
  return data instanceof Uint8Array ? data : new Uint8Array(data);
}

/** Parse one binary WebSocket message. Throws {@link ProtoError} on malformed input. */
export function parse(data: ArrayBuffer | Uint8Array): BinMsg {
  const b = asBytes(data);
  if (b.byteLength === 0) throw new ProtoError('empty message');
  const t = b[0]!;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  switch (t) {
    case MsgType.VIDEO:
      need(b, t, VIDEO_HEADER_LEN);
      return {
        type: 'video',
        flags: b[1]!,
        epoch: v.getUint16(2, true),
        seq: v.getUint32(4, true),
        captureUs: v.getBigUint64(8, true),
        encodeUs: v.getUint32(16, true),
        payload: b.subarray(VIDEO_HEADER_LEN),
      };
    case MsgType.AUDIO:
      need(b, t, AUDIO_HEADER_LEN);
      return {
        type: 'audio',
        flags: b[1]!,
        stream: v.getUint16(2, true),
        seq: v.getUint32(4, true),
        captureUs: v.getBigUint64(8, true),
        payload: b.subarray(AUDIO_HEADER_LEN),
      };
    case MsgType.MOUSE_ABS:
      need(b, t, MOUSE_ABS_LEN);
      return { type: 'mouse_abs', buttons: b[1]!, x: v.getUint16(2, true), y: v.getUint16(4, true) };
    case MsgType.MOUSE_REL:
      need(b, t, MOUSE_REL_LEN);
      return { type: 'mouse_rel', dx: v.getInt16(2, true), dy: v.getInt16(4, true) };
    case MsgType.MOUSE_BTN:
      need(b, t, MOUSE_BTN_LEN);
      return {
        type: 'mouse_btn',
        button: enumValue<MouseButton>('button', b[1]!, MouseButton.Forward),
        down: b[2] !== 0,
        x: v.getUint16(4, true),
        y: v.getUint16(6, true),
      };
    case MsgType.WHEEL:
      need(b, t, WHEEL_LEN);
      return {
        type: 'wheel',
        mode: enumValue<WheelMode>('wheel_mode', b[1]!, WheelMode.Line),
        dx: v.getInt16(2, true),
        dy: v.getInt16(4, true),
        phase: enumValue<ScrollPhase>('scroll_phase', b[6]!, ScrollPhase.Ended),
      };
    case MsgType.KEY:
      need(b, t, KEY_LEN);
      return {
        type: 'key',
        down: b[1] !== 0,
        repeat: (b[2]! & KeyFlags.REPEAT) !== 0,
        hid: v.getUint32(4, true),
      };
    case MsgType.RELEASE_ALL:
      return { type: 'release_all' };
    case MsgType.VIDEO_ACK:
      need(b, t, VIDEO_ACK_LEN);
      return {
        type: 'video_ack',
        flags: b[1]!,
        epoch: v.getUint16(2, true),
        seq: v.getUint32(4, true),
        decodeUs: v.getUint16(8, true),
        queueUs: v.getUint16(10, true),
      };
    case MsgType.PING:
      need(b, t, PING_LEN);
      return { type: 'ping', id: v.getUint32(4, true), tOriginUs: v.getBigUint64(8, true) };
    case MsgType.PONG:
      need(b, t, PONG_LEN);
      return {
        type: 'pong',
        id: v.getUint32(4, true),
        tOriginUs: v.getBigUint64(8, true),
        tReplyUs: v.getBigUint64(16, true),
      };
    case MsgType.FILE_DATA:
      need(b, t, FILE_DATA_HEADER_LEN);
      return {
        type: 'file_data',
        flags: b[1]!,
        id: v.getUint32(4, true),
        seq: v.getUint32(8, true),
        offset: Number(v.getBigUint64(12, true)),
        payload: b.subarray(FILE_DATA_HEADER_LEN),
      };
    case MsgType.FILE_ACK:
      need(b, t, FILE_ACK_LEN);
      return { type: 'file_ack', seq: v.getUint32(4, true) };
    case MsgType.PROBE_UP:
      return { type: 'probe_up', payload: b.subarray(Math.min(2, b.byteLength)) };
    default:
      return { type: 'unknown', ty: t };
  }
}

// ---------------------------------------------------------------------------
// encode

function frame(ty: number, flags: number, len: number): [Bytes, DataView] {
  const b = new Uint8Array(len);
  b[0] = ty;
  b[1] = flags;
  return [b, new DataView(b.buffer)];
}

function withPayload(header: Uint8Array, payload: Uint8Array): Bytes {
  const out = new Uint8Array(header.byteLength + payload.byteLength);
  out.set(header);
  out.set(payload, header.byteLength);
  return out;
}

/** Encode a message. Integer fields wrap to their wire width; callers clamp beforehand. */
export function encode(m: EncodableMsg): Bytes {
  switch (m.type) {
    case 'video': {
      const [b, v] = frame(MsgType.VIDEO, m.flags, VIDEO_HEADER_LEN);
      v.setUint16(2, m.epoch, true);
      v.setUint32(4, m.seq, true);
      v.setBigUint64(8, m.captureUs, true);
      v.setUint32(16, m.encodeUs, true);
      return withPayload(b, m.payload);
    }
    case 'audio': {
      const [b, v] = frame(MsgType.AUDIO, m.flags, AUDIO_HEADER_LEN);
      v.setUint16(2, m.stream, true);
      v.setUint32(4, m.seq, true);
      v.setBigUint64(8, m.captureUs, true);
      return withPayload(b, m.payload);
    }
    case 'mouse_abs':
      return encodeMouseAbs(m.buttons, m.x, m.y);
    case 'mouse_rel':
      return encodeMouseRel(m.dx, m.dy);
    case 'mouse_btn':
      return encodeMouseBtn(m.button, m.down, m.x, m.y);
    case 'wheel':
      return encodeWheel(m.mode, m.dx, m.dy, m.phase);
    case 'key':
      return encodeKey(m.down, m.repeat, m.hid);
    case 'release_all':
      return encodeReleaseAll();
    case 'video_ack':
      return encodeVideoAck(m.flags, m.epoch, m.seq, m.decodeUs, m.queueUs);
    case 'ping':
      return encodePing(m.id, m.tOriginUs);
    case 'pong':
      return encodePong(m.id, m.tOriginUs, m.tReplyUs);
    case 'file_data':
      return encodeFileData(m.flags, m.id, m.seq, m.offset, m.payload);
    case 'file_ack':
      return encodeFileAck(m.seq);
    case 'probe_up':
      return withPayload(new Uint8Array([MsgType.PROBE_UP, 0]), m.payload);
  }
}

/** A FILE_DATA frame: header and bytes in one buffer. */
export function encodeFileData(flags: number, id: number, seq: number, offset: number, payload: Uint8Array): Bytes {
  const out = new Uint8Array(FILE_DATA_HEADER_LEN + payload.byteLength);
  const v = new DataView(out.buffer);
  out[0] = MsgType.FILE_DATA;
  out[1] = flags;
  v.setUint32(4, id, true);
  v.setUint32(8, seq, true);
  v.setBigUint64(12, BigInt(offset), true);
  out.set(payload, FILE_DATA_HEADER_LEN);
  return out;
}

export function encodeFileAck(seq: number): Bytes {
  const [b, v] = frame(MsgType.FILE_ACK, 0, FILE_ACK_LEN);
  v.setUint32(4, seq, true);
  return b;
}

export function encodeMouseAbs(buttons: number, x: number, y: number): Bytes {
  const [b, v] = frame(MsgType.MOUSE_ABS, buttons, MOUSE_ABS_LEN);
  v.setUint16(2, x, true);
  v.setUint16(4, y, true);
  return b;
}

export function encodeMouseRel(dx: number, dy: number): Bytes {
  const [b, v] = frame(MsgType.MOUSE_REL, 0, MOUSE_REL_LEN);
  v.setInt16(2, dx, true);
  v.setInt16(4, dy, true);
  return b;
}

export function encodeMouseBtn(button: MouseButton, down: boolean, x: number, y: number): Bytes {
  const [b, v] = frame(MsgType.MOUSE_BTN, button, MOUSE_BTN_LEN);
  b[2] = down ? 1 : 0;
  v.setUint16(4, x, true);
  v.setUint16(6, y, true);
  return b;
}

export function encodeWheel(mode: WheelMode, dx: number, dy: number, phase: ScrollPhase): Bytes {
  const [b, v] = frame(MsgType.WHEEL, mode, WHEEL_LEN);
  v.setInt16(2, dx, true);
  v.setInt16(4, dy, true);
  b[6] = phase;
  return b;
}

export function encodeKey(down: boolean, repeat: boolean, hid: number): Bytes {
  const [b, v] = frame(MsgType.KEY, down ? 1 : 0, KEY_LEN);
  b[2] = repeat ? KeyFlags.REPEAT : 0;
  v.setUint32(4, hid, true);
  return b;
}

export function encodeReleaseAll(): Bytes {
  return frame(MsgType.RELEASE_ALL, 0, RELEASE_ALL_LEN)[0];
}

export function encodeVideoAck(flags: number, epoch: number, seq: number, decodeUs: number, queueUs: number): Bytes {
  const [b, v] = frame(MsgType.VIDEO_ACK, flags, VIDEO_ACK_LEN);
  v.setUint16(2, epoch, true);
  v.setUint32(4, seq, true);
  v.setUint16(8, decodeUs, true);
  v.setUint16(10, queueUs, true);
  return b;
}

export function encodePing(id: number, tOriginUs: bigint): Bytes {
  const [b, v] = frame(MsgType.PING, 0, PING_LEN);
  v.setUint32(4, id, true);
  v.setBigUint64(8, tOriginUs, true);
  return b;
}

export function encodePong(id: number, tOriginUs: bigint, tReplyUs: bigint): Bytes {
  const [b, v] = frame(MsgType.PONG, 0, PONG_LEN);
  v.setUint32(4, id, true);
  v.setBigUint64(8, tOriginUs, true);
  v.setBigUint64(16, tReplyUs, true);
  return b;
}

export function encodeProbeUp(payloadLen: number): Bytes {
  const b = new Uint8Array(2 + payloadLen);
  b[0] = MsgType.PROBE_UP;
  return b;
}

/** Saturate to the u16 range (VIDEO_ACK timing fields). */
export function satU16(v: number): number {
  return v <= 0 ? 0 : v >= 0xffff ? 0xffff : Math.round(v);
}

/** Clamp to the i16 range. */
export function clampI16(v: number): number {
  return v <= -0x8000 ? -0x8000 : v >= 0x7fff ? 0x7fff : Math.trunc(v);
}
