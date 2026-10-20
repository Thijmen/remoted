import { describe, expect, it } from 'vitest';
import vectors from '../../../crates/rd-proto/testvectors/binary.json';
import {
  type BinMsg,
  type EncodableMsg,
  MsgType,
  ProtoError,
  clampI16,
  encode,
  encodeReleaseAll,
  parse,
  satU16,
} from './bin';

interface Vector {
  name: string;
  hex: string;
  fields: Record<string, unknown>;
}

const hexToBytes = (hex: string): Uint8Array => new Uint8Array((hex.match(/../g) ?? []).map((b) => parseInt(b, 16)));

const bytesToHex = (b: Uint8Array): string => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');

/** Translate a golden vector's snake_case fields (u64 as strings) into the TS message shape. */
function expected(f: Record<string, unknown>): EncodableMsg {
  const n = (k: string) => f[k] as number;
  const big = (k: string) => BigInt(f[k] as string);
  const payload = () => hexToBytes(f.payload_hex as string);
  switch (f.type) {
    case 'video':
      return {
        type: 'video',
        flags: n('flags'),
        epoch: n('epoch'),
        seq: n('seq'),
        captureUs: big('capture_us'),
        encodeUs: n('encode_us'),
        payload: payload(),
      };
    case 'audio':
      return {
        type: 'audio',
        flags: n('flags'),
        stream: n('stream'),
        seq: n('seq'),
        captureUs: big('capture_us'),
        payload: payload(),
      };
    case 'mouse_abs':
      return { type: 'mouse_abs', buttons: n('buttons'), x: n('x'), y: n('y') };
    case 'mouse_rel':
      return { type: 'mouse_rel', dx: n('dx'), dy: n('dy') };
    case 'mouse_btn':
      return { type: 'mouse_btn', button: n('button') as 0, down: f.down as boolean, x: n('x'), y: n('y') };
    case 'wheel':
      return { type: 'wheel', mode: n('mode') as 0, dx: n('dx'), dy: n('dy'), phase: n('phase') as 0 };
    case 'key':
      return { type: 'key', down: f.down as boolean, repeat: f.repeat as boolean, hid: n('hid') };
    case 'release_all':
      return { type: 'release_all' };
    case 'video_ack':
      return {
        type: 'video_ack',
        flags: n('flags'),
        epoch: n('epoch'),
        seq: n('seq'),
        decodeUs: n('decode_us'),
        queueUs: n('queue_us'),
      };
    case 'ping':
      return { type: 'ping', id: n('id'), tOriginUs: big('t_origin_us') };
    case 'pong':
      return { type: 'pong', id: n('id'), tOriginUs: big('t_origin_us'), tReplyUs: big('t_reply_us') };
    case 'file_data':
      return {
        type: 'file_data',
        flags: n('flags'),
        id: n('id'),
        seq: n('seq'),
        offset: Number(f.offset as string),
        payload: payload(),
      };
    case 'file_ack':
      return { type: 'file_ack', seq: n('seq') };
    case 'probe_up':
      return { type: 'probe_up', payload: payload() };
    default:
      throw new Error(`unhandled vector type ${String(f.type)}`);
  }
}

/** Payload views compare by content, not by backing buffer. */
function normalise(m: BinMsg): unknown {
  return 'payload' in m ? { ...m, payload: bytesToHex(m.payload) } : m;
}

describe('golden vectors (crates/rd-proto/testvectors/binary.json)', () => {
  const all = vectors as Vector[];

  it('covers every message type', () => {
    const types = new Set(all.map((v) => v.fields.type));
    for (const t of [
      'video',
      'audio',
      'mouse_abs',
      'mouse_rel',
      'mouse_btn',
      'wheel',
      'key',
      'release_all',
      'video_ack',
      'ping',
      'pong',
      'file_data',
      'file_ack',
      'probe_up',
    ]) {
      expect(types.has(t), t).toBe(true);
    }
  });

  for (const v of all) {
    it(`${v.name}: encode`, () => {
      expect(bytesToHex(encode(expected(v.fields)))).toBe(v.hex);
    });
    it(`${v.name}: decode`, () => {
      const bytes = hexToBytes(v.hex);
      expect(normalise(parse(bytes))).toEqual(normalise(expected(v.fields)));
      // Also from an ArrayBuffer and from an offset view (as received from a WebSocket / slice).
      expect(normalise(parse(bytes.slice().buffer))).toEqual(normalise(expected(v.fields)));
      const padded = new Uint8Array(bytes.byteLength + 3);
      padded.set(bytes, 3);
      expect(normalise(parse(padded.subarray(3)))).toEqual(normalise(expected(v.fields)));
    });
  }
});

describe('parse errors and forward compatibility', () => {
  it('rejects empty and short messages', () => {
    expect(() => parse(new Uint8Array())).toThrow(ProtoError);
    expect(() => parse(new Uint8Array([MsgType.VIDEO, 0, 1]))).toThrow(ProtoError);
    expect(() => parse(new Uint8Array([MsgType.KEY, 1]))).toThrow(ProtoError);
  });

  it('rejects invalid enum values', () => {
    expect(() => parse(new Uint8Array([MsgType.MOUSE_BTN, 9, 1, 0, 0, 0, 0, 0]))).toThrow(ProtoError);
    expect(() => parse(new Uint8Array([MsgType.WHEEL, 2, 0, 0, 0, 0, 0, 0]))).toThrow(ProtoError);
    expect(() => parse(new Uint8Array([MsgType.WHEEL, 0, 0, 0, 0, 0, 4, 0]))).toThrow(ProtoError);
  });

  it('returns unknown for unknown and reserved types', () => {
    expect(parse(new Uint8Array([0x7f, 0, 1, 2]))).toEqual({ type: 'unknown', ty: 0x7f });
    expect(parse(new Uint8Array([MsgType.CURSOR]))).toEqual({ type: 'unknown', ty: MsgType.CURSOR });
  });

  it('handles a 1-byte PROBE_UP and RELEASE_ALL', () => {
    expect(normalise(parse(new Uint8Array([MsgType.PROBE_UP])))).toEqual({ type: 'probe_up', payload: '' });
    expect(parse(encodeReleaseAll())).toEqual({ type: 'release_all' });
  });
});

describe('saturation helpers', () => {
  it('satU16', () => {
    expect(satU16(-5)).toBe(0);
    expect(satU16(1500.4)).toBe(1500);
    expect(satU16(70_000)).toBe(0xffff);
  });
  it('clampI16', () => {
    expect(clampI16(40_000)).toBe(32767);
    expect(clampI16(-40_000)).toBe(-32768);
    expect(clampI16(-3.7)).toBe(-3);
  });
});
