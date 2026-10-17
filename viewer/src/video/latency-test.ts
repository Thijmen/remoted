import { toVideoPixels } from '../input/coords';
import { percentile } from '../util/stats';

/** USB HID usage of F13 (keyboard page 0x07, usage 0x68); /latency-target flips its box on it. */
export const HID_F13 = 0x0007_0068;

const RUNS = 20;
const TIMEOUT_MS = 2000;
const GAP_MS = 300;
/** Sum of absolute RGB differences that counts as "the box flipped". */
const FLIP_THRESHOLD = 90;
/** Sample a small square up and left of the cursor hotspot (the arrow covers down-right). */
const REGION = 8;
const REGION_OFFSET = 16;

export interface LatencyTestDeps {
  canvas: HTMLCanvasElement;
  /** Press + release a key on the remote machine. */
  tapKey(hid: number): void;
  /** Remote pointer position, normalised 0..65535. */
  pointer(): { x: number; y: number };
  report(text: string, done: boolean): void;
}

export interface LatencyResult {
  samplesMs: number[];
  p50: number;
  p95: number;
  min: number;
  max: number;
}

/**
 * Automated input→photon test: with /latency-target open on the host and the remote cursor
 * resting on its box, send F13 and time until the decoded video shows the box changing colour.
 */
export class LatencyTest {
  private running = false;
  private waiting = false;
  private samples: number[] = [];
  private ref: [number, number, number] = [0, 0, 0];
  private region = { x: 0, y: 0 };
  private t0 = 0;
  private timer = 0;
  lastResult: LatencyResult | null = null;

  constructor(private readonly deps: LatencyTestDeps) {}

  get active(): boolean {
    return this.running;
  }

  start(): void {
    this.cancel();
    const { canvas } = this.deps;
    const ctx = canvas.getContext('2d');
    if (!ctx || canvas.width < REGION_OFFSET * 2) {
      this.deps.report('Latency test needs a running video stream.', true);
      return;
    }
    const p = this.deps.pointer();
    const px = toVideoPixels(p.x, p.y, canvas.width, canvas.height);
    this.region = {
      x: Math.max(0, Math.min(canvas.width - REGION, Math.round(px.x) - REGION_OFFSET)),
      y: Math.max(0, Math.min(canvas.height - REGION, Math.round(px.y) - REGION_OFFSET)),
    };
    this.ref = this.sample(ctx);
    this.samples = [];
    this.running = true;
    this.deps.report(`Latency test: 0/${RUNS}…`, false);
    this.fire();
  }

  cancel(): void {
    clearTimeout(this.timer);
    this.running = false;
    this.waiting = false;
  }

  /** Call after every painted frame. */
  onDrawn(ctx: CanvasRenderingContext2D): void {
    if (!this.waiting) return;
    const c = this.sample(ctx);
    const diff = Math.abs(c[0] - this.ref[0]) + Math.abs(c[1] - this.ref[1]) + Math.abs(c[2] - this.ref[2]);
    if (diff < FLIP_THRESHOLD) return;
    this.samples.push(performance.now() - this.t0);
    this.ref = c;
    this.waiting = false;
    clearTimeout(this.timer);
    if (this.samples.length >= RUNS) {
      this.finish();
      return;
    }
    this.deps.report(
      `Latency test: ${this.samples.length}/${RUNS}, last ${this.samples.at(-1)!.toFixed(0)} ms…`,
      false,
    );
    this.timer = window.setTimeout(() => this.fire(), GAP_MS + Math.random() * GAP_MS);
  }

  private fire(): void {
    if (!this.running) return;
    this.waiting = true;
    this.t0 = performance.now();
    this.deps.tapKey(HID_F13);
    this.timer = window.setTimeout(() => {
      this.cancel();
      this.deps.report(
        'Latency test: no flip seen. Open /latency-target on the host, rest the cursor on the box, keep that window focused, then try again.',
        true,
      );
    }, TIMEOUT_MS);
  }

  private finish(): void {
    this.cancel();
    const s = this.samples;
    const r: LatencyResult = {
      samplesMs: s.map((v) => Math.round(v * 10) / 10),
      p50: percentile(s, 50),
      p95: percentile(s, 95),
      min: Math.min(...s),
      max: Math.max(...s),
    };
    this.lastResult = r;
    this.deps.report(
      `Input→photon over ${s.length} runs: p50 ${r.p50.toFixed(0)} ms · p95 ${r.p95.toFixed(0)} ms · min ${r.min.toFixed(0)} · max ${r.max.toFixed(0)} ms`,
      true,
    );
  }

  private sample(ctx: CanvasRenderingContext2D): [number, number, number] {
    const d = ctx.getImageData(this.region.x, this.region.y, REGION, REGION).data;
    let r = 0;
    let g = 0;
    let b = 0;
    for (let i = 0; i < d.length; i += 4) {
      r += d[i]!;
      g += d[i + 1]!;
      b += d[i + 2]!;
    }
    const n = d.length / 4;
    return [r / n, g / n, b / n];
  }
}
