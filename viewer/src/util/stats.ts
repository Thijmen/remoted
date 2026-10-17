/** Linear-interpolated percentile (p in 0..100) of an unsorted list; NaN when empty. */
export function percentile(values: readonly number[], p: number): number {
  if (values.length === 0) return Number.NaN;
  const s = [...values].sort((a, b) => a - b);
  const rank = (Math.min(100, Math.max(0, p)) / 100) * (s.length - 1);
  const lo = Math.floor(rank);
  const hi = Math.ceil(rank);
  return s[lo]! + (s[hi]! - s[lo]!) * (rank - lo);
}

/** Samples kept for a fixed time span (timestamps in any monotonic unit). */
export class TimeWindow {
  private readonly ts: number[] = [];
  private readonly vs: number[] = [];

  constructor(private readonly span: number) {}

  push(t: number, v: number): void {
    this.ts.push(t);
    this.vs.push(v);
    this.prune(t);
  }

  prune(now: number): void {
    let drop = 0;
    while (drop < this.ts.length && this.ts[drop]! < now - this.span) drop++;
    if (drop > 0) {
      this.ts.splice(0, drop);
      this.vs.splice(0, drop);
    }
  }

  values(): readonly number[] {
    return this.vs;
  }

  get size(): number {
    return this.vs.length;
  }

  clear(): void {
    this.ts.length = 0;
    this.vs.length = 0;
  }
}

/** Monotonic viewer clock in microseconds (protocol timestamp convention). */
export function nowUs(): number {
  return Math.round(performance.now() * 1000);
}

export function fmt(v: number, digits = 0): string {
  return Number.isFinite(v) ? v.toFixed(digits) : '–';
}
