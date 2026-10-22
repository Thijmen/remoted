import { describe, expect, it } from 'vitest';
import {
  AckTracker,
  OutputPosition,
  REPLAY_GUESS_MS,
  ReplayFilter,
  binaryStringBytes,
  splitInput,
  textBytes,
} from './output';

const bytes = (s: string) => new TextEncoder().encode(s);
const text = (b: Uint8Array) => new TextDecoder().decode(b);

describe('OutputPosition', () => {
  it('counts the bytes shown', () => {
    const o = new OutputPosition();
    o.start(0);
    expect(text(o.accept(0, bytes('hello')).bytes)).toBe('hello');
    expect(text(o.accept(5, bytes(' world')).bytes)).toBe(' world');
    expect(o.offset).toBe(11);
  });

  it('continues after a jump forward (history overtaken) and reports what was lost', () => {
    const o = new OutputPosition();
    o.start(100);
    expect(o.accept(150, bytes('abc'))).toEqual({ bytes: bytes('abc'), skipped: 50 });
    expect(o.offset).toBe(153);
  });

  it('drops what is already shown', () => {
    const o = new OutputPosition();
    o.start(10);
    const overlap = o.accept(8, bytes('xyz!'));
    expect(text(overlap.bytes)).toBe('z!');
    expect(overlap.skipped).toBe(0);
    expect(o.offset).toBe(12);
    expect(o.accept(4, bytes('old')).bytes.byteLength).toBe(0);
    expect(o.offset).toBe(12);
  });

  it('starts over where the agent says (reset, or an attach)', () => {
    const o = new OutputPosition();
    o.accept(0, bytes('12345'));
    o.start(3_000_000);
    expect(o.offset).toBe(3_000_000);
    expect(o.accept(3_000_000, bytes('a')).skipped).toBe(0);
  });
});

describe('AckTracker', () => {
  it('acknowledges cumulatively, once per new seq', () => {
    const a = new AckTracker();
    expect(a.take()).toBeNull();
    a.processed(1);
    a.processed(2);
    expect(a.pending).toBe(true);
    expect(a.take()).toBe(2);
    expect(a.take()).toBeNull();
    a.processed(1); // late duplicate
    expect(a.take()).toBeNull();
    a.processed(3);
    expect(a.take()).toBe(3);
  });
});

describe('ReplayFilter', () => {
  it('a new shell has no history', () => {
    const r = new ReplayFilter();
    r.begin(false, 0, null, 0);
    expect(r.frame(0, 10, 0)).toBe(false);
  });

  it('uses the end the agent gives', () => {
    const r = new ReplayFilter();
    r.begin(true, 500, 700, 0);
    expect(r.frame(500, 100, 0)).toBe(true);
    expect(r.frame(600, 100, 0)).toBe(true);
    expect(r.frame(700, 5, 0)).toBe(false);
  });

  it('without an end: history until the user types', () => {
    const r = new ReplayFilter();
    r.begin(true, 0, null, 0);
    expect(r.frame(0, 100, 10)).toBe(true);
    r.input();
    expect(r.frame(100, 5, 20)).toBe(false);
    r.input();
    expect(r.frame(105, 5, 30)).toBe(false);
  });

  it('without an end: history for a while after the hello', () => {
    const r = new ReplayFilter();
    r.begin(true, 0, null, 1000);
    expect(r.frame(0, 100, 1000 + REPLAY_GUESS_MS - 1)).toBe(true);
    expect(r.frame(100, 5, 1000 + REPLAY_GUESS_MS)).toBe(false);
    // A later reattach starts guessing again.
    r.begin(true, 105, null, 9000);
    expect(r.frame(105, 5, 9001)).toBe(true);
  });
});

describe('input', () => {
  it('encodes text as UTF-8 and binary strings byte by byte', () => {
    expect(Array.from(textBytes('é\r'))).toEqual([0xc3, 0xa9, 0x0d]);
    expect(Array.from(binaryStringBytes('\x1b[M\xff'))).toEqual([0x1b, 0x5b, 0x4d, 0xff]);
  });

  it('splits large pastes into frames of at most 64 KiB', () => {
    const big = new Uint8Array((64 << 10) * 2 + 3);
    const parts = splitInput(big);
    expect(parts.map((p) => p.byteLength)).toEqual([64 << 10, 64 << 10, 3]);
    expect(splitInput(bytes('ls\r'))).toHaveLength(1);
    expect(splitInput(new Uint8Array(10), 4).map((p) => p.byteLength)).toEqual([4, 4, 2]);
  });
});
