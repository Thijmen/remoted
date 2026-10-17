import { percentile } from '../util/stats';

export interface Summary {
  n: number;
  min: number;
  p50: number;
  p95: number;
  max: number;
}

export function summarize(values: readonly number[]): Summary {
  return {
    n: values.length,
    min: values.length ? Math.min(...values) : Number.NaN,
    p50: percentile(values, 50),
    p95: percentile(values, 95),
    max: values.length ? Math.max(...values) : Number.NaN,
  };
}

/** A gap longer than this between consecutive arrivals is a stall. */
export const STALL_MS = 100;
/** Arrivals closer than this are "clumped" (delivered back-to-back after being held). */
export const CLUMP_MS = 2;
/** Clumped arrivals right after a stall that make it a burst (a buffering middlebox). */
export const BURST_MIN_FRAMES = 3;

export interface DownSummary {
  frames: number;
  bytes: number;
  durationMs: number;
  /** Delivered rate over the whole run. */
  kbps: number;
  /** Lowest 1-second delivered rate, excluding the first and last second. */
  minSecondKbps: number;
  /** Per-second delivered kbps. */
  series: number[];
  maxGapMs: number;
  stalls: number;
  bursts: number;
  /** One-way delay above the run's minimum (queueing), ms. */
  queueP50Ms: number;
  queueP95Ms: number;
  queueMaxMs: number;
}

/**
 * Online accumulator for a downstream test (WS frames or HTTP chunks). Detects stalls (long gaps),
 * bursts (a stall followed by back-to-back arrivals, typical of a buffering proxy) and queueing
 * delay growth (receive time − send time, relative to the minimum, so clock offsets cancel).
 */
export class DownCollector {
  private frames = 0;
  private bytes = 0;
  private lastMs = Number.NaN;
  private maxGap = 0;
  private stalls = 0;
  private bursts = 0;
  private clump = 0;
  private afterStall = false;
  private readonly seconds: number[] = [];
  private readonly delays: number[] = [];
  private minDelay = Number.POSITIVE_INFINITY;

  constructor(private readonly startMs: number) {}

  add(rxMs: number, bytes: number, serverMs?: number): void {
    if (this.frames > 0) {
      const gap = rxMs - this.lastMs;
      if (gap > this.maxGap) this.maxGap = gap;
      if (gap > STALL_MS) {
        this.stalls++;
        this.afterStall = true;
        this.clump = 0;
      } else if (this.afterStall && gap < CLUMP_MS) {
        if (++this.clump === BURST_MIN_FRAMES - 1) {
          this.bursts++;
          this.afterStall = false;
        }
      } else {
        this.afterStall = false;
      }
    }
    this.frames++;
    this.bytes += bytes;
    this.lastMs = rxMs;
    const sec = Math.max(0, Math.floor((rxMs - this.startMs) / 1000));
    while (this.seconds.length <= sec) this.seconds.push(0);
    this.seconds[sec]! += bytes;
    if (serverMs !== undefined) {
      const d = rxMs - serverMs;
      this.delays.push(d);
      if (d < this.minDelay) this.minDelay = d;
    }
  }

  get count(): number {
    return this.frames;
  }

  /** Delivered kbps over the last full second (for live display). */
  currentKbps(nowMs: number): number {
    const sec = Math.floor((nowMs - this.startMs) / 1000) - 1;
    return sec >= 0 ? ((this.seconds[sec] ?? 0) * 8) / 1000 : 0;
  }

  summary(endMs: number): DownSummary {
    const durationMs = Math.max(1, endMs - this.startMs);
    const series = this.seconds.map((b) => (b * 8) / 1000);
    const inner = series.slice(1, -1);
    const queue = this.delays.map((d) => d - this.minDelay);
    return {
      frames: this.frames,
      bytes: this.bytes,
      durationMs,
      kbps: (this.bytes * 8) / durationMs,
      minSecondKbps: inner.length ? Math.min(...inner) : Number.NaN,
      series,
      maxGapMs: this.maxGap,
      stalls: this.stalls,
      bursts: this.bursts,
      queueP50Ms: percentile(queue, 50),
      queueP95Ms: percentile(queue, 95),
      queueMaxMs: queue.length ? Math.max(...queue) : Number.NaN,
    };
  }
}

// ---------------------------------------------------------------------------
// verdict against the M0 exit criteria

export type Outcome = 'pass' | 'fail' | 'na';

export interface Criterion {
  name: string;
  outcome: Outcome;
  detail: string;
}

export interface VerdictInput {
  secure: boolean;
  wsOk: boolean;
  idleRtt: Summary | null;
  /** Ladder steps: target and achieved kbps, RTT under load. */
  ladder: Array<{ targetKbps: number; kbps: number; rtt: Summary }>;
  soak: { kbps: number; rtt: Summary; minutes: number } | null;
  unexpectedDisconnects: number;
  idleSurvival: Array<{ minutes: number; ok: boolean }>;
}

export const SUSTAINED_KBPS = 10_000;
export const RTT_LOAD_MARGIN_MS = 50;

export function verdict(v: VerdictInput): { overall: Outcome; criteria: Criterion[] } {
  const criteria: Criterion[] = [];

  criteria.push({
    name: 'WebSocket over TLS works',
    outcome: !v.wsOk ? 'fail' : v.secure ? 'pass' : 'na',
    detail: !v.wsOk
      ? 'probe socket failed'
      : v.secure
        ? 'wss:// connected'
        : 'connected over plain ws:// (dev): TLS not exercised',
  });

  const step10 = v.ladder.find((s) => s.targetKbps === SUSTAINED_KBPS);
  if (v.soak) {
    criteria.push({
      name: '≥ 10 Mbps sustained',
      outcome: v.soak.kbps >= SUSTAINED_KBPS ? 'pass' : 'fail',
      detail: `windowed soak delivered ${(v.soak.kbps / 1000).toFixed(1)} Mbps over ${v.soak.minutes} min`,
    });
  } else if (step10) {
    criteria.push({
      name: '≥ 10 Mbps sustained',
      outcome: step10.kbps >= SUSTAINED_KBPS * 0.95 ? 'pass' : 'fail',
      detail: `10 Mbps step delivered ${(step10.kbps / 1000).toFixed(1)} Mbps (no soak)`,
    });
  } else {
    criteria.push({ name: '≥ 10 Mbps sustained', outcome: 'na', detail: 'not measured' });
  }

  const loaded = [step10?.rtt, v.soak?.rtt].filter((r): r is Summary => !!r && r.n > 0);
  if (v.idleRtt && v.idleRtt.n > 0 && loaded.length) {
    const limit = v.idleRtt.p50 + RTT_LOAD_MARGIN_MS;
    const worst = Math.max(...loaded.map((r) => r.p95));
    criteria.push({
      name: 'RTT p95 under load ≤ idle + 50 ms',
      outcome: worst <= limit ? 'pass' : 'fail',
      detail: `p95 under load ${worst.toFixed(0)} ms vs idle ${v.idleRtt.p50.toFixed(0)} ms + 50`,
    });
  } else {
    criteria.push({ name: 'RTT p95 under load ≤ idle + 50 ms', outcome: 'na', detail: 'not measured' });
  }

  const idleFails = v.idleSurvival.filter((s) => !s.ok);
  criteria.push({
    name: 'No unexplained disconnects',
    outcome: v.unexpectedDisconnects === 0 && idleFails.length === 0 ? 'pass' : 'fail',
    detail: [
      `${v.unexpectedDisconnects} unexpected close(s)`,
      v.idleSurvival.length
        ? `idle survival ${v.idleSurvival.map((s) => `${s.minutes} min ${s.ok ? 'ok' : 'FAILED'}`).join(', ')}`
        : '',
    ]
      .filter(Boolean)
      .join('; '),
  });

  const outcomes = criteria.map((c) => c.outcome);
  const overall: Outcome = outcomes.includes('fail') ? 'fail' : outcomes.every((o) => o === 'na') ? 'na' : 'pass';
  return { overall, criteria };
}
