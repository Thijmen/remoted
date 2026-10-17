import { percentile } from '../util/stats';

interface Sample {
  recvUs: number;
  rttUs: number;
  offsetUs: number;
}

/**
 * Agent−viewer clock offset from PING/PONG exchanges: the sample with the smallest RTT in the
 * last `windowUs` gives the tightest bound, `offset = t_reply − (t_origin + t_recv) / 2`.
 * All arguments are microseconds; `t_origin`/`t_recv` are viewer clock, `t_reply` agent clock.
 */
export class ClockEstimator {
  private samples: Sample[] = [];
  private best: Sample | null = null;

  constructor(private readonly windowUs = 10_000_000) {}

  /** Record a PONG. Returns false (and ignores it) for impossible samples. */
  addSample(tOriginUs: number, tReplyUs: number, tRecvUs: number): boolean {
    const rttUs = tRecvUs - tOriginUs;
    if (!(rttUs >= 0) || rttUs > 60_000_000) return false;
    const s: Sample = { recvUs: tRecvUs, rttUs, offsetUs: tReplyUs - (tOriginUs + tRecvUs) / 2 };
    this.samples.push(s);
    this.prune(tRecvUs);
    return true;
  }

  private prune(nowUs: number): void {
    const cutoff = nowUs - this.windowUs;
    this.samples = this.samples.filter((s) => s.recvUs >= cutoff);
    let best: Sample | null = null;
    for (const s of this.samples) if (!best || s.rttUs < best.rttUs) best = s;
    // Keep the previous estimate if the window emptied (e.g. a pause in pings).
    if (best) this.best = best;
  }

  /** Agent clock minus viewer clock, µs; null before the first sample. */
  get offsetUs(): number | null {
    return this.best?.offsetUs ?? null;
  }

  get rttMinUs(): number | null {
    return this.samples.length ? Math.min(...this.samples.map((s) => s.rttUs)) : null;
  }

  get rttP50Us(): number | null {
    return this.samples.length
      ? percentile(
          this.samples.map((s) => s.rttUs),
          50,
        )
      : null;
  }

  /** Convert an agent timestamp to the viewer clock (µs). */
  toViewerUs(agentUs: number): number | null {
    const o = this.offsetUs;
    return o === null ? null : agentUs - o;
  }

  reset(): void {
    this.samples = [];
    this.best = null;
  }
}
