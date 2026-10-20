// The send window of a file transfer — mirror of crates/rd-flow/src/bulk.rs (keep them in step;
// docs/files.md, Flow control). A delay-based, low-priority window in the spirit of LEDBAT: it
// grows while the queue a transfer causes stays under the target and shrinks above it.

export interface BulkConfig {
  minWindow: number;
  maxWindow: number;
  initialWindow: number;
  minChunk: number;
  maxChunk: number;
  baseHistoryUs: number;
  baseBucketUs: number;
  recentSamples: number;
}

export const DEFAULT_BULK_CONFIG: Readonly<BulkConfig> = {
  minWindow: 16 << 10,
  maxWindow: 16 << 20,
  initialWindow: 64 << 10,
  minChunk: 16 << 10,
  maxChunk: 256 << 10,
  baseHistoryUs: 120_000_000,
  baseBucketUs: 10_000_000,
  recentSamples: 4,
};

/** Queueing a transfer may cause while a screen session is open. */
export const TARGET_GENTLE_US = 10_000;
/** Queueing a transfer may cause otherwise. */
export const TARGET_FULL_US = 80_000;

interface InFlight {
  seq: number;
  bytes: number;
  sentUs: number;
}

export interface BulkStats {
  window: number;
  inflightBytes: number;
  chunk: number;
  baseRttUs: number | null;
  rttUs: number | null;
  queueUs: number | null;
  targetUs: number;
  slowStart: boolean;
}

export class BulkWindow {
  private targetUs: number;
  private window: number;
  private slowStart = true;
  private nextSeq = 1;
  private readonly inflight: InFlight[] = [];
  private inflightBytes = 0;
  /** [bucket start, lowest rtt], oldest first. */
  private readonly base: Array<[number, number]> = [];
  private readonly recent: number[] = [];
  private lastCutUs: number | null = null;
  private peakInflight = 0;
  private peakUs = 0;

  constructor(
    private readonly cfg: BulkConfig = DEFAULT_BULK_CONFIG,
    targetUs = TARGET_FULL_US,
  ) {
    this.targetUs = Math.max(1, targetUs);
    this.window = cfg.initialWindow;
  }

  setTarget(targetUs: number): void {
    this.targetUs = Math.max(1, targetUs);
  }

  /** Bytes the next frame should carry: a quarter of the window, 16–256 KiB, in whole KiB. */
  chunkSize(): number {
    const quarter = Math.floor(this.window / 4 / 1024) * 1024;
    return Math.min(this.cfg.maxChunk, Math.max(this.cfg.minChunk, quarter));
  }

  /** Whether a frame of `bytes` may go now. Always when nothing is in flight. */
  canSend(bytes: number): boolean {
    return this.inflight.length === 0 || this.inflightBytes + bytes <= Math.floor(this.window);
  }

  get idle(): boolean {
    return this.inflight.length === 0;
  }

  get bytesInFlight(): number {
    return this.inflightBytes;
  }

  /** A frame of `bytes` was handed to the socket; returns its seq. */
  onSent(bytes: number, nowUs: number): number {
    const seq = this.nextSeq;
    this.nextSeq = this.nextSeq >= 0xffff_ffff ? 1 : this.nextSeq + 1;
    this.inflightBytes += bytes;
    if (this.inflightBytes >= this.peakInflight) {
      this.peakInflight = this.inflightBytes;
      this.peakUs = nowUs;
    }
    this.inflight.push({ seq, bytes, sentUs: nowUs });
    return seq;
  }

  /** A cumulative FILE_ACK. Returns the bytes it released (0 for duplicates and unknown seqs). */
  onAck(seq: number, nowUs: number): number {
    if (!this.inflight.some((f) => f.seq === seq)) return 0;
    let released = 0;
    for (;;) {
      const f = this.inflight.shift();
      if (!f) break;
      this.inflightBytes -= f.bytes;
      released += f.bytes;
      this.onSample(f, nowUs);
      if (f.seq === seq) break;
    }
    return released;
  }

  /** The connection was replaced: start over (frames in flight will never be acknowledged). */
  reset(): void {
    this.window = this.cfg.initialWindow;
    this.slowStart = true;
    this.nextSeq = 1;
    this.inflight.length = 0;
    this.inflightBytes = 0;
    this.base.length = 0;
    this.recent.length = 0;
    this.lastCutUs = null;
    this.peakInflight = 0;
    this.peakUs = 0;
  }

  stats(): BulkStats {
    const base = this.baseRtt();
    const rtt = this.recent.length ? Math.min(...this.recent) : null;
    return {
      window: Math.floor(this.window),
      inflightBytes: this.inflightBytes,
      chunk: this.chunkSize(),
      baseRttUs: base,
      rttUs: rtt,
      queueUs: base !== null && rtt !== null ? Math.max(0, rtt - base) : null,
      targetUs: this.targetUs,
      slowStart: this.slowStart,
    };
  }

  private baseRtt(): number | null {
    return this.base.length ? Math.min(...this.base.map(([, rtt]) => rtt)) : null;
  }

  private onSample(f: InFlight, nowUs: number): void {
    const rtt = Math.max(1, nowUs - f.sentUs);
    this.addBase(rtt, nowUs);
    this.recent.push(rtt);
    while (this.recent.length > this.cfg.recentSamples) this.recent.shift();
    const current = Math.min(...this.recent);
    const queue = Math.max(0, current - (this.baseRtt() ?? current));
    const target = this.targetUs;
    if (nowUs - this.peakUs > 2 * current) {
      this.peakInflight = this.inflightBytes;
      this.peakUs = nowUs;
    }
    const limited = this.peakInflight >= this.window * 0.5;

    if (queue > 2 * target && (this.lastCutUs === null || nowUs - this.lastCutUs >= current)) {
      this.window /= 2;
      this.slowStart = false;
      this.lastCutUs = nowUs;
    } else if (this.slowStart) {
      if (queue > target / 2) this.slowStart = false;
      else if (limited) this.window += f.bytes;
    } else {
      const off = Math.min(1, Math.max(-1, (target - queue) / target));
      if (off < 0 || limited) this.window += (off * f.bytes * this.chunkSize()) / this.window;
    }
    this.window = Math.min(this.cfg.maxWindow, Math.max(this.cfg.minWindow, this.window));
  }

  private addBase(rtt: number, nowUs: number): void {
    const bucketUs = Math.max(1, this.cfg.baseBucketUs);
    const bucket = nowUs - (nowUs % bucketUs);
    const last = this.base[this.base.length - 1];
    if (last && last[0] === bucket) last[1] = Math.min(last[1], rtt);
    else this.base.push([bucket, rtt]);
    while (this.base.length && this.base[0]![0] + this.cfg.baseHistoryUs <= bucket) this.base.shift();
  }
}
