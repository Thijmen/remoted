import { describe, expect, it } from 'vitest';
import { ClockEstimator } from './clock';

describe('ClockEstimator', () => {
  // Agent clock = viewer clock + 5 s. One-way delays vary; the min-RTT sample is the most accurate.
  const OFFSET = 5_000_000;
  const sample = (c: ClockEstimator, sendUs: number, upUs: number, downUs: number) =>
    c.addSample(sendUs, sendUs + upUs + OFFSET, sendUs + upUs + downUs);

  it('has no estimate before the first sample', () => {
    const c = new ClockEstimator();
    expect(c.offsetUs).toBeNull();
    expect(c.toViewerUs(123)).toBeNull();
    expect(c.rttMinUs).toBeNull();
  });

  it('uses the minimum-RTT sample', () => {
    const c = new ClockEstimator();
    sample(c, 1_000_000, 30_000, 10_000); // rtt 40 ms, asymmetric: error 10 ms
    sample(c, 2_000_000, 2_000, 2_000); // rtt 4 ms, symmetric: exact
    sample(c, 3_000_000, 50_000, 5_000);
    expect(c.rttMinUs).toBe(4_000);
    expect(c.offsetUs).toBe(OFFSET);
    expect(c.toViewerUs(OFFSET + 42)).toBe(42);
    expect(c.rttP50Us).toBe(40_000);
  });

  it('forgets samples older than the window', () => {
    const c = new ClockEstimator(10_000_000);
    sample(c, 0, 1_000, 1_000); // great sample, then the path changes
    sample(c, 11_000_000, 20_000, 0); // only sample in the window now
    expect(c.rttMinUs).toBe(20_000);
    expect(c.offsetUs).toBe(OFFSET + 10_000);
  });

  it('rejects negative RTTs', () => {
    const c = new ClockEstimator();
    expect(c.addSample(100, 5, 50)).toBe(false);
    expect(c.offsetUs).toBeNull();
  });

  it('keeps the last estimate when the window empties', () => {
    const c = new ClockEstimator(1_000);
    sample(c, 0, 100, 100);
    expect(c.offsetUs).toBe(OFFSET);
    c.reset();
    expect(c.offsetUs).toBeNull();
  });
});
