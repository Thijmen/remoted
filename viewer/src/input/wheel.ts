import { ScrollPhase, WheelMode, clampI16 } from '../proto/bin';

export interface WheelInput {
  deltaX: number;
  deltaY: number;
  /** WheelEvent.deltaMode: 0 pixel, 1 line, 2 page. */
  deltaMode: number;
}

export interface WheelOutput {
  mode: WheelMode;
  dx: number;
  dy: number;
  phase: ScrollPhase;
}

/** One wheel notch scrolls this many lines (Windows / Chromium default). */
export const LINES_PER_NOTCH = 3;
/** DOM_DELTA_PAGE is rare (Firefox "one screen at a time"); approximate a page. */
export const LINES_PER_PAGE = 15;
/** Pixel deltas per notch that Chromium reports for wheel mice (100 on Windows, 120 elsewhere). */
const NOTCH_PX = [100, 120];
/** More notches than this in one event is not a wheel mouse (e.g. a synthetic or fling delta). */
const MAX_NOTCHES_PER_EVENT = 50;
/** A pixel gesture ends after this long without wheel events. */
export const GESTURE_END_MS = 150;

const DOM_DELTA_LINE = 1;
const DOM_DELTA_PAGE = 2;

/** Number of notches if `d` is a whole multiple of a notch size, else null. 0 for 0. */
function notches(d: number, sizes: readonly number[]): number | null {
  if (d === 0) return 0;
  for (const n of sizes) {
    const k = d / n;
    const r = Math.round(k);
    if (r !== 0 && Math.abs(r) <= MAX_NOTCHES_PER_EVENT && Math.abs(k - r) < 0.01) return r;
  }
  return null;
}

/**
 * Turns DOM wheel events into wire WHEEL messages: discrete mouse notches become Line mode,
 * smooth (trackpad) deltas stay Pixel mode with Began/Changed/Ended phases. Deltas accumulate
 * between flushes (one per animation frame) and fractional remainders carry over.
 * Sign convention: DOM — positive dy scrolls down (content moves up).
 */
export class WheelNormalizer {
  private lineX = 0;
  private lineY = 0;
  private pxX = 0;
  private pxY = 0;
  private inGesture = false;
  private lastPixelMs = Number.NEGATIVE_INFINITY;
  private readonly notchSizes: number[];

  constructor(
    devicePixelRatio = 1,
    public invert = false,
  ) {
    // Some Chromium builds scale wheel deltas by the zoom/DPR factor.
    const sizes = new Set<number>(NOTCH_PX);
    if (devicePixelRatio > 0 && devicePixelRatio !== 1) for (const n of NOTCH_PX) sizes.add(n / devicePixelRatio);
    this.notchSizes = [...sizes];
  }

  push(e: WheelInput, nowMs: number): void {
    const s = this.invert ? -1 : 1;
    const dx = e.deltaX * s;
    const dy = e.deltaY * s;
    if (e.deltaMode === DOM_DELTA_LINE) {
      this.lineX += dx;
      this.lineY += dy;
      return;
    }
    if (e.deltaMode === DOM_DELTA_PAGE) {
      this.lineX += dx * LINES_PER_PAGE;
      this.lineY += dy * LINES_PER_PAGE;
      return;
    }
    const nx = notches(dx, this.notchSizes);
    const ny = notches(dy, this.notchSizes);
    // A smooth-scroll gesture in progress keeps pixel mode even if a delta happens to be round.
    if (nx !== null && ny !== null && !this.inGesture) {
      this.lineX += nx * LINES_PER_NOTCH;
      this.lineY += ny * LINES_PER_NOTCH;
      return;
    }
    this.pxX += dx;
    this.pxY += dy;
    this.lastPixelMs = nowMs;
  }

  /** Messages to send now (call once per animation frame). */
  flush(nowMs: number): WheelOutput[] {
    const out: WheelOutput[] = [];
    // Whole units (clamped to i16) go out now; the rest carries over to the next flush.
    const lx = clampI16(this.lineX);
    const ly = clampI16(this.lineY);
    if (lx !== 0 || ly !== 0) {
      out.push({ mode: WheelMode.Line, dx: lx, dy: ly, phase: ScrollPhase.None });
      this.lineX -= lx;
      this.lineY -= ly;
    }
    const px = clampI16(this.pxX);
    const py = clampI16(this.pxY);
    if (px !== 0 || py !== 0) {
      out.push({
        mode: WheelMode.Pixel,
        dx: px,
        dy: py,
        phase: this.inGesture ? ScrollPhase.Changed : ScrollPhase.Began,
      });
      this.inGesture = true;
      this.pxX -= px;
      this.pxY -= py;
    } else if (this.inGesture && nowMs - this.lastPixelMs >= GESTURE_END_MS) {
      out.push({ mode: WheelMode.Pixel, dx: 0, dy: 0, phase: ScrollPhase.Ended });
      this.inGesture = false;
      this.pxX = 0;
      this.pxY = 0;
    }
    return out;
  }

  /** True while flushes are still needed (a gesture to end or whole units pending). */
  get active(): boolean {
    return (
      this.inGesture ||
      Math.abs(this.lineX) >= 1 ||
      Math.abs(this.lineY) >= 1 ||
      Math.abs(this.pxX) >= 1 ||
      Math.abs(this.pxY) >= 1
    );
  }

  reset(): void {
    this.lineX = this.lineY = this.pxX = this.pxY = 0;
    this.inGesture = false;
  }
}
