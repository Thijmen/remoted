import { describe, expect, it } from 'vitest';
import { DownCollector, summarize, verdict } from './analysis';

describe('DownCollector', () => {
  it('measures throughput and per-second series for a smooth stream', () => {
    const c = new DownCollector(0);
    // 60 fps × 2 s × 5000 B = 2.4 Mbps.
    for (let i = 0; i < 120; i++) c.add(i * (1000 / 60), 5000, i * (1000 / 60) - 20);
    const s = c.summary(2000);
    expect(s.frames).toBe(120);
    expect(s.kbps).toBeCloseTo((120 * 5000 * 8) / 2000, 5);
    expect(s.series).toHaveLength(2);
    expect(s.stalls).toBe(0);
    expect(s.bursts).toBe(0);
    expect(s.queueP95Ms).toBeCloseTo(0, 5);
  });

  it('flags a stall followed by a clump as a burst and measures queueing', () => {
    const c = new DownCollector(0);
    let t = 0;
    for (let i = 0; i < 10; i++, t += 16) c.add(t, 1000, t);
    // Proxy holds 300 ms worth of frames, then releases them back to back.
    const held = t + 300;
    for (let i = 0; i < 18; i++) c.add(held + i * 0.1, 1000, t + i * 16);
    const s = c.summary(held + 10);
    expect(s.stalls).toBe(1);
    expect(s.bursts).toBe(1);
    // Last paced frame at t=144, released clump at 460.
    expect(s.maxGapMs).toBeCloseTo(316, 5);
    expect(s.queueMaxMs).toBeGreaterThan(250);
  });

  it('does not call evenly paced frames after a pause a burst', () => {
    const c = new DownCollector(0);
    c.add(0, 1);
    c.add(500, 1);
    c.add(516, 1);
    c.add(532, 1);
    const s = c.summary(600);
    expect(s.stalls).toBe(1);
    expect(s.bursts).toBe(0);
  });
});

describe('verdict', () => {
  const rtt = (p50: number, p95: number) => ({ n: 10, min: p50, p50, p95, max: p95 });
  const good = {
    secure: true,
    wsOk: true,
    idleRtt: rtt(30, 35),
    ladder: [{ targetKbps: 10_000, kbps: 9900, rtt: rtt(40, 60) }],
    soak: { kbps: 14_000, rtt: rtt(40, 70), minutes: 30 },
    unexpectedDisconnects: 0,
    idleSurvival: [{ minutes: 2, ok: true }],
  };

  it('passes a healthy network', () => {
    const v = verdict(good);
    expect(v.overall).toBe('pass');
    expect(v.criteria.map((c) => c.outcome)).toEqual(['pass', 'pass', 'pass', 'pass']);
  });

  it('fails on bufferbloat under load', () => {
    expect(verdict({ ...good, soak: { ...good.soak, rtt: rtt(60, 200) } }).criteria[2]!.outcome).toBe('fail');
  });

  it('fails on low sustained throughput, disconnects and idle cuts', () => {
    expect(verdict({ ...good, soak: { ...good.soak, kbps: 6000 } }).criteria[1]!.outcome).toBe('fail');
    expect(verdict({ ...good, unexpectedDisconnects: 1 }).overall).toBe('fail');
    expect(verdict({ ...good, idleSurvival: [{ minutes: 5, ok: false }] }).overall).toBe('fail');
  });

  it('falls back to the 10 Mbps ladder step without a soak and marks plain ws as n/a', () => {
    const v = verdict({ ...good, secure: false, soak: null });
    expect(v.criteria[0]!.outcome).toBe('na');
    expect(v.criteria[1]!.outcome).toBe('pass');
    expect(v.overall).toBe('pass');
  });

  it('fails when the WebSocket does not work', () => {
    const v = verdict({ ...good, wsOk: false, idleRtt: null, ladder: [], soak: null, idleSurvival: [] });
    expect(v.overall).toBe('fail');
  });
});

describe('summarize', () => {
  it('handles empty input', () => {
    expect(summarize([]).n).toBe(0);
    expect(Number.isNaN(summarize([]).p50)).toBe(true);
  });
  it('computes percentiles', () => {
    const s = summarize([10, 20, 30, 40, 50]);
    expect(s).toEqual({ n: 5, min: 10, p50: 30, p95: 48, max: 50 });
  });
});
