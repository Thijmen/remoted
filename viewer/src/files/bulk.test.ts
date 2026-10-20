import { describe, expect, it } from 'vitest';
import { BulkWindow, DEFAULT_BULK_CONFIG, TARGET_FULL_US, TARGET_GENTLE_US } from './bulk';

const MS = 1000;

// The same cases as crates/rd-flow/src/bulk.rs, so both copies behave alike.
describe('BulkWindow', () => {
  it('sends one frame even when it is larger than the window', () => {
    const w = new BulkWindow();
    expect(w.canSend(1 << 20)).toBe(true);
    w.onSent(1 << 20, 0);
    expect(w.canSend(1)).toBe(false);
    expect(w.onAck(1, 10 * MS)).toBe(1 << 20);
    expect(w.idle).toBe(true);
  });

  it('acknowledges cumulatively and ignores duplicates', () => {
    const w = new BulkWindow();
    const a = w.onSent(100, 0);
    const b = w.onSent(100, 0);
    const c = w.onSent(100, 0);
    expect([a, b, c]).toEqual([1, 2, 3]);
    expect(w.onAck(b, MS)).toBe(200);
    expect(w.onAck(b, MS)).toBe(0);
    expect(w.onAck(77, MS)).toBe(0);
    expect(w.bytesInFlight).toBe(100);
    expect(w.onAck(c, MS)).toBe(100);
  });

  it('doubles per round trip in slow start while there is no queue', () => {
    const w = new BulkWindow();
    const start = w.stats().window;
    let sent = 0;
    while (w.canSend(w.chunkSize())) {
      w.onSent(w.chunkSize(), 0);
      sent++;
    }
    for (let seq = 1; seq <= sent; seq++) w.onAck(seq, 20 * MS);
    expect(w.stats().window).toBe(2 * start);
    expect(w.stats().slowStart).toBe(true);
  });

  it('shrinks to the minimum when the queue keeps growing', () => {
    const w = new BulkWindow(DEFAULT_BULK_CONFIG, TARGET_GENTLE_US);
    let t = 0;
    let seq = 0;
    for (let rtt = 10 * MS; rtt <= 300 * MS; rtt += 10 * MS) {
      const chunk = w.chunkSize();
      while (w.canSend(chunk)) seq = w.onSent(chunk, t);
      t += rtt;
      w.onAck(seq, t);
    }
    expect(w.stats().slowStart).toBe(false);
    expect(w.stats().window).toBe(DEFAULT_BULK_CONFIG.minWindow);
  });

  it('does not grow while the sender is not using the window', () => {
    const w = new BulkWindow();
    const start = w.stats().window;
    for (let i = 1; i <= 1000; i++) {
      const seq = w.onSent(1024, i * 30 * MS);
      w.onAck(seq, i * 30 * MS + 10 * MS);
    }
    expect(w.stats().window).toBe(start);
  });

  it('forgets the base delay after two minutes', () => {
    const w = new BulkWindow(DEFAULT_BULK_CONFIG, TARGET_FULL_US);
    w.onAck(w.onSent(10, 0), 5 * MS);
    for (let i = 1; i <= 13; i++) {
      const t = i * 10_000 * MS;
      w.onAck(w.onSent(10, t), t + 50 * MS);
    }
    expect(w.stats().baseRttUs).toBe(50 * MS);
  });

  it('fills an idle link and keeps its queue near the target', () => {
    // 50 Mbps, 30 ms one way: the same bottleneck as crates/rd-flow/tests/bulk_sim.rs.
    const rate = (50e6 / 8) * 1e-6; // bytes per µs
    const delay = 30 * MS;
    const w = new BulkWindow(DEFAULT_BULK_CONFIG, TARGET_FULL_US);
    const queue: Array<{ left: number; seq: number }> = [];
    const acks: Array<{ at: number; seq: number }> = [];
    let queued = 0;
    let delivered = 0;
    const qs: number[] = [];
    const step = 250;
    for (let t = 0; t < 30_000 * MS; t += step) {
      while (acks.length && acks[0]!.at <= t) w.onAck(acks.shift()!.seq, t);
      while (w.canSend(w.chunkSize())) {
        const chunk = w.chunkSize();
        queue.push({ left: chunk, seq: w.onSent(chunk, t) });
        queued += chunk;
      }
      let budget = rate * step;
      while (budget > 0 && queue.length) {
        const f = queue[0]!;
        const served = Math.min(f.left, budget);
        f.left -= served;
        budget -= served;
        queued -= served;
        if (t >= 10_000 * MS) delivered += served;
        if (f.left <= 0) acks.push({ at: t + 2 * delay, seq: queue.shift()!.seq });
      }
      if (t >= 10_000 * MS && t % MS === 0) qs.push(queued / rate / MS);
    }
    const share = delivered / (rate * 20_000 * MS);
    const mean = qs.reduce((a, b) => a + b, 0) / qs.length;
    expect(share).toBeGreaterThan(0.9);
    expect(mean).toBeLessThan(2 * (TARGET_FULL_US / MS));
  });
});
