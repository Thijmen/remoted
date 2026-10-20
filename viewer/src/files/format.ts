/** Bytes for people: `999 B`, `12.3 KB`, `4.56 GB` (decimal units, as Finder and Explorer's transfer dialogs). */
export function formatSize(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n < 1000) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let v = n;
  let i = -1;
  do {
    v /= 1000;
    i++;
  } while (v >= 999.5 && i < units.length - 1);
  return `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${units[i]}`;
}

export function formatRate(bytesPerSecond: number): string {
  return `${formatSize(Math.round(bytesPerSecond))}/s`;
}

/** `5 s`, `3 min`, `1 h 20 min`. */
export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '';
  const s = Math.round(seconds);
  if (s < 60) return `${s} s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h} h ${m % 60} min` : `${h} h`;
}

/** A modification time: `14:05` today, `12 Mar` this year, `12 Mar 2023` before. */
export function formatMtime(ms: number, now = Date.now(), locale?: string): string {
  if (!ms) return '—';
  const d = new Date(ms);
  const today = new Date(now);
  if (d.toDateString() === today.toDateString()) {
    return d.toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' });
  }
  const sameYear = d.getFullYear() === today.getFullYear();
  return d.toLocaleDateString(locale, {
    day: 'numeric',
    month: 'short',
    ...(sameYear ? {} : { year: 'numeric' }),
  });
}

/** Bytes per second, smoothed over about `windowMs`. */
export class RateMeter {
  private samples: Array<[number, number]> = [];

  constructor(private readonly windowMs = 3000) {}

  /** Record the running total of bytes at time `nowMs`. */
  add(total: number, nowMs = performance.now()): void {
    this.samples.push([nowMs, total]);
    while (this.samples.length > 2 && nowMs - this.samples[0]![0] > this.windowMs) this.samples.shift();
  }

  rate(nowMs = performance.now()): number {
    if (this.samples.length < 2) return 0;
    const [t0, b0] = this.samples[0]!;
    const [t1, b1] = this.samples[this.samples.length - 1]!;
    // Stalled: nothing new for a while means no speed, not the old speed.
    if (nowMs - t1 > this.windowMs) return 0;
    const dt = Math.max(nowMs, t1) - t0;
    return dt > 0 ? ((b1 - b0) * 1000) / dt : 0;
  }

  reset(): void {
    this.samples = [];
  }
}
