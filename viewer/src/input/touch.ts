import { ScrollPhase } from '../proto/bin';

/** Finger movement (CSS px) that turns a tap into a drag. */
export const TAP_SLOP_PX = 10;
/** A finger held still this long on a touch screen is a right click. */
export const LONG_PRESS_MS = 500;

export type TouchButton = 'left' | 'right';

export interface TouchPoint {
  /** Pointer id, so multi-touch gestures can track each finger. */
  id: number;
  /** Client coordinates; the controller maps them to the video frame. */
  x: number;
  y: number;
}

/** What a gesture became, in wire terms; the input controller turns these into mouse and wheel messages. */
export type TouchAction =
  | { type: 'tap'; button: TouchButton; x: number; y: number }
  | { type: 'press'; x: number; y: number }
  | { type: 'move'; x: number; y: number }
  | { type: 'release' }
  | { type: 'scroll'; dx: number; dy: number; phase: ScrollPhase };

type Mode = 'idle' | 'pending' | 'drag' | 'long' | 'scroll' | 'done';

/**
 * Touch gestures over the video: a tap is a left click (two quick taps are a double click), a
 * long press a right click, and a drag holds the left button. Two fingers scroll with the content
 * following them (fingers up scroll the remote content up), sent as pixel wheel deltas with
 * Began/Changed/Ended phases. A third finger does not move the scroll.
 *
 * Pure state machine (no DOM, no timers): the controller feeds events and calls `tick` when its
 * long-press timer fires, mostly so the behaviour is unit tested like `wheel.ts`.
 */
export class TouchGestures {
  private readonly pointers = new Map<number, TouchPoint>();
  private mode: Mode = 'idle';
  /** Where the current tap or drag started. */
  private start: TouchPoint = { id: 0, x: 0, y: 0 };
  private startMs = 0;
  /** The two fingers that scroll; a third is ignored so the centroid does not jump. */
  private scrollIds: [number, number] | null = null;
  private lastX = 0;
  private lastY = 0;
  /** Began went out for the current scroll gesture (its Ended follows on lift or cancel). */
  private scrolling = false;
  /** Sub-pixel remainders: a slow drag would otherwise lose every event to truncation. */
  private restX = 0;
  private restY = 0;

  /** Fingers currently down. */
  get pointerCount(): number {
    return this.pointers.size;
  }

  down(p: TouchPoint, nowMs: number): TouchAction[] {
    this.pointers.set(p.id, { ...p });
    switch (this.mode) {
      case 'idle':
        this.mode = 'pending';
        this.start = { ...p };
        this.startMs = nowMs;
        return [];
      case 'pending':
      case 'drag': {
        // A second finger turns the tap or drag into a scroll (a held drag is let go first).
        const out: TouchAction[] = this.mode === 'drag' ? [{ type: 'release' }] : [];
        this.beginScroll(p.id);
        return out;
      }
      default:
        // A third finger, or one added after a long press: it does not change the gesture.
        return [];
    }
  }

  move(p: TouchPoint): TouchAction[] {
    const tracked = this.pointers.get(p.id);
    if (!tracked) return [];
    tracked.x = p.x;
    tracked.y = p.y;
    if (this.mode === 'pending') {
      if (Math.hypot(p.x - this.start.x, p.y - this.start.y) <= TAP_SLOP_PX) return [];
      // The button went down where the finger did: the remote cursor does not jump on the first move.
      this.mode = 'drag';
      return [
        { type: 'press', x: this.start.x, y: this.start.y },
        { type: 'move', x: p.x, y: p.y },
      ];
    }
    if (this.mode === 'drag') return [{ type: 'move', x: p.x, y: p.y }];
    if (this.mode !== 'scroll') return [];
    const c = this.centroid();
    // Natural scrolling: the content follows the fingers, so a finger moving up scrolls down.
    this.restX += this.lastX - c.x;
    this.restY += this.lastY - c.y;
    this.lastX = c.x;
    this.lastY = c.y;
    const dx = Math.trunc(this.restX);
    const dy = Math.trunc(this.restY);
    if (dx === 0 && dy === 0) return [];
    this.restX -= dx;
    this.restY -= dy;
    const phase = this.scrolling ? ScrollPhase.Changed : ScrollPhase.Began;
    this.scrolling = true;
    return [{ type: 'scroll', dx, dy, phase }];
  }

  up(p: TouchPoint): TouchAction[] {
    this.pointers.delete(p.id);
    const left = this.pointers.size === 0;
    switch (this.mode) {
      case 'pending':
        this.mode = left ? 'idle' : 'done';
        // Tap where the finger went down, not where it lifted.
        return [{ type: 'tap', button: 'left', x: this.start.x, y: this.start.y }];
      case 'drag':
        this.mode = left ? 'idle' : 'done';
        return [{ type: 'release' }];
      case 'long':
        this.mode = left ? 'idle' : 'done';
        return [];
      case 'scroll': {
        if (!this.scrollIds?.includes(p.id)) return [];
        const out: TouchAction[] = this.scrolling ? [{ type: 'scroll', dx: 0, dy: 0, phase: ScrollPhase.Ended }] : [];
        this.scrolling = false;
        this.scrollIds = null;
        this.mode = left ? 'idle' : 'done';
        return out;
      }
      default:
        if (left) this.mode = 'idle';
        return [];
    }
  }

  /** The pointer was cancelled (the page scrolled, the app lost focus, the session ended). */
  cancel(): TouchAction[] {
    const out: TouchAction[] = [];
    if (this.mode === 'drag') out.push({ type: 'release' });
    if (this.mode === 'scroll' && this.scrolling) {
      out.push({ type: 'scroll', dx: 0, dy: 0, phase: ScrollPhase.Ended });
    }
    this.pointers.clear();
    this.mode = 'idle';
    this.scrollIds = null;
    this.scrolling = false;
    this.restX = this.restY = 0;
    return out;
  }

  /** The long-press timer fired: a finger still down and still is a right click. */
  tick(nowMs: number): TouchAction[] {
    if (this.mode !== 'pending' || this.pointers.size !== 1) return [];
    if (nowMs - this.startMs < LONG_PRESS_MS) return [];
    this.mode = 'long';
    return [{ type: 'tap', button: 'right', x: this.start.x, y: this.start.y }];
  }

  private beginScroll(secondId: number): void {
    this.scrollIds = [this.start.id, secondId];
    this.mode = 'scroll';
    this.scrolling = false;
    this.restX = this.restY = 0;
    const c = this.centroid();
    this.lastX = c.x;
    this.lastY = c.y;
  }

  private centroid(): { x: number; y: number } {
    const ids = this.scrollIds ?? [];
    let x = 0;
    let y = 0;
    let n = 0;
    for (const id of ids) {
      const p = this.pointers.get(id);
      if (!p) continue;
      x += p.x;
      y += p.y;
      n++;
    }
    return n > 0 ? { x: x / n, y: y / n } : { x: this.lastX, y: this.lastY };
  }
}
