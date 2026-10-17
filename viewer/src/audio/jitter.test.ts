import { describe, expect, it } from 'vitest';
import { DEFAULT_JITTER, JitterPolicy } from './jitter';
import { AudioRing } from './ring';

const Q = 128;
const MS = 48; // frames per ms at 48 kHz

describe('JitterPolicy', () => {
  it('waits until the start level (40 ms) is buffered', () => {
    const p = new JitterPolicy();
    expect(p.next(39 * MS, Q)).toEqual({ play: false, skip: 0 });
    expect(p.next(40 * MS, Q)).toEqual({ play: true, skip: 0 });
    expect(p.stats().playing).toBe(true);
  });

  it('re-primes and raises the target after an underrun', () => {
    const p = new JitterPolicy();
    p.next(40 * MS, Q);
    expect(p.next(Q - 1, Q)).toEqual({ play: false, skip: 0 });
    expect(p.stats()).toMatchObject({ underruns: 1, playing: false, targetMs: 50 });
    expect(p.next(45 * MS, Q).play).toBe(false);
    expect(p.next(50 * MS, Q).play).toBe(true);
  });

  it('caps the target at 120 ms', () => {
    const p = new JitterPolicy();
    for (let i = 0; i < 20; i++) {
      p.next(200 * MS, Q);
      p.next(0, Q);
    }
    expect(p.stats().targetMs).toBe(DEFAULT_JITTER.maxMs);
  });

  it('drops a 10 ms chunk while more than target + 60 ms is buffered', () => {
    const p = new JitterPolicy();
    p.next(40 * MS, Q);
    expect(p.next(100 * MS, Q)).toEqual({ play: true, skip: 0 });
    expect(p.next(101 * MS, Q)).toEqual({ play: true, skip: 10 * MS });
    expect(p.stats().drops).toBe(1);
  });

  it('lowers the target towards 30 ms while playback is stable', () => {
    const p = new JitterPolicy();
    p.next(40 * MS, Q);
    const quantaPer10s = Math.ceil((10_000 * MS) / Q);
    for (let i = 0; i < quantaPer10s * 3; i++) p.next(60 * MS, Q);
    expect(p.stats().targetMs).toBe(DEFAULT_JITTER.minMs);
    expect(p.stats().underruns).toBe(0);
  });
});

describe('AudioRing', () => {
  const stereo = (frames: number, start = 0) => {
    const a = new Float32Array(frames * 2);
    for (let i = 0; i < frames; i++) {
      a[i * 2] = start + i;
      a[i * 2 + 1] = -(start + i);
    }
    return a;
  };

  it('requires a power-of-two capacity', () => {
    expect(() => AudioRing.create(1000, false)).toThrow(RangeError);
  });

  it('round-trips interleaved frames across the wrap point', () => {
    const r = AudioRing.create(8, false);
    expect(r.write(stereo(6))).toBe(6);
    const l = new Float32Array(4);
    const rt = new Float32Array(4);
    expect(r.read(l, rt)).toBe(4);
    expect(Array.from(l)).toEqual([0, 1, 2, 3]);
    expect(r.write(stereo(6, 6))).toBe(6); // wraps
    expect(r.available()).toBe(8);
    const l2 = new Float32Array(8);
    const r2 = new Float32Array(8);
    expect(r.read(l2, r2)).toBe(8);
    expect(Array.from(l2)).toEqual([4, 5, 6, 7, 8, 9, 10, 11]);
    expect(Array.from(r2)).toEqual([-4, -5, -6, -7, -8, -9, -10, -11]);
  });

  it('drops what does not fit and supports skip/clear', () => {
    const r = AudioRing.create(4, true);
    expect(r.write(stereo(6))).toBe(4);
    expect(r.skip(3)).toBe(3);
    expect(r.available()).toBe(1);
    r.clear();
    expect(r.available()).toBe(0);
  });

  it('shares state through the buffer', () => {
    const a = AudioRing.create(16, true);
    const b = new AudioRing(a.buffer, 16);
    a.write(stereo(5));
    expect(b.available()).toBe(5);
  });
});
