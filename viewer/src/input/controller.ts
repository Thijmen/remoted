import {
  type Bytes,
  MouseButton,
  clampI16,
  encodeKey,
  encodeMouseAbs,
  encodeMouseBtn,
  encodeMouseRel,
  encodeReleaseAll,
  encodeWheel,
} from '../proto/bin';
import type { ChordAction } from './chords';
import { type Rect, normalizePoint } from './coords';
import { DesktopBridge } from './desktop-bridge';
import { KeyboardForwarder } from './keyboard';
import { type ModifierMapping, isMacViewer, isWindowsViewer } from './modifiers';
import { NativeKeys } from './native-keys';
import { WheelNormalizer } from './wheel';

export interface InputSink {
  /** Send a binary frame; false when not connected. */
  send(b: Bytes): boolean;
  readonly bufferedAmount: number;
}

export interface InputOptions {
  mapping: ModifierMapping;
  invertScroll: boolean;
}

export interface InputCallbacks {
  chord(action: ChordAction): void;
  pointerLockChanged(locked: boolean): void;
}

/** ~250 Hz cap for absolute moves and the relative-motion flush period. */
const MOVE_INTERVAL_MS = 4;
/** Above this many unsent bytes, moves/wheel coalesce instead of queueing. */
const BACKPRESSURE_BYTES = 64 * 1024;

/** DOM `buttons` bit → wire button. */
const DOM_BUTTON_BITS: ReadonlyArray<[number, MouseButton]> = [
  [1, MouseButton.Left],
  [2, MouseButton.Right],
  [4, MouseButton.Middle],
  [8, MouseButton.Back],
  [16, MouseButton.Forward],
];

/**
 * Captures mouse/keyboard/wheel input over the video surface and sends it as wire messages.
 * Moves and wheel are coalesced (rate cap, backpressure); buttons and keys are never dropped.
 */
export class InputController {
  private enabled = false;
  private opts: InputOptions;
  private videoW = 0;
  private videoH = 0;
  private rect: Rect = { left: 0, top: 0, width: 0, height: 0 };
  /** Wire button mask (MOUSE_ABS flags) and the DOM mask it was derived from. */
  private buttons = 0;
  private domButtons = 0;
  private readonly keys: KeyboardForwarder;
  /** Keys from the desktop app's keyboard hook (docs/desktop-client.md), and the grab it is told about. */
  private readonly native: NativeKeys;
  private readonly bridge: DesktopBridge;
  /** Local Caps Lock as of the last key or pointer event anywhere on the page (null: not seen yet). */
  private localCaps: boolean | null = null;
  private pos = { x: 0x8000, y: 0x8000 };
  private pendingMove: { x: number; y: number } | null = null;
  private lastMoveMs = Number.NEGATIVE_INFINITY;
  private moveTimer = 0;
  private relX = 0;
  private relY = 0;
  private relTimer = 0;
  private locked = false;
  private readonly wheel: WheelNormalizer;
  private wheelRaf = 0;

  constructor(
    /** Element covering the video area (the canvas, CSS `object-fit: contain`). */
    private readonly surface: HTMLElement,
    private readonly sink: InputSink,
    private readonly cb: InputCallbacks,
    opts: InputOptions,
  ) {
    this.opts = { ...opts };
    this.keys = new KeyboardForwarder(
      { send: (b) => this.sink.send(b), chord: (a) => this.cb.chord(a), mapping: () => this.opts.mapping },
      { windows: isWindowsViewer(navigator), mac: isMacViewer(navigator) },
    );
    this.native = new NativeKeys({
      held: (code) => this.keys.isPressed(code),
      localCaps: () => this.localCaps ?? false,
      now: () => performance.now(),
    });
    this.bridge = new DesktopBridge(window, {
      key: (code, down, repeat) => this.onNativeKey(code, down, repeat),
      release: () => this.onNativeRelease(),
    });
    this.wheel = new WheelNormalizer(window.devicePixelRatio, opts.invertScroll);
    new ResizeObserver(() => this.updateRect()).observe(surface);
    this.attach();
  }

  /** Forward input only while streaming. Disabling releases everything. */
  setEnabled(on: boolean): void {
    if (this.enabled && !on) this.releaseAll();
    const enabling = on && !this.enabled;
    this.enabled = on;
    // A new session's agent starts with Caps Lock off; match the local lock right away.
    if (enabling && this.localCaps !== null) this.keys.syncCaps(this.localCaps);
    this.updateGrab();
  }

  setOptions(opts: InputOptions): void {
    this.opts = { ...opts };
    this.wheel.invert = opts.invertScroll;
  }

  setVideoSize(w: number, h: number): void {
    this.videoW = w;
    this.videoH = h;
  }

  /** Last absolute pointer position (normalised 0..65535). */
  get pointer(): { x: number; y: number } {
    return this.pos;
  }

  get pointerLocked(): boolean {
    return this.locked;
  }

  /** Release every key and button on the remote side (this also clears its Caps Lock) and forget local state. */
  releaseAll(): void {
    this.sink.send(encodeReleaseAll());
    this.forget();
  }

  /** Drop local key/button state without telling the agent (it already released on disconnect). */
  forget(): void {
    this.keys.forget();
    this.buttons = 0;
    this.domButtons = 0;
    this.pendingMove = null;
    this.relX = this.relY = 0;
    this.wheel.reset();
  }

  /** Send a key press+release (used by the latency test). */
  tapKey(hid: number): void {
    this.sink.send(encodeKey(true, false, hid));
    this.sink.send(encodeKey(false, false, hid));
  }

  async togglePointerLock(): Promise<void> {
    if (document.pointerLockElement === this.surface) {
      document.exitPointerLock();
      return;
    }
    try {
      await this.surface.requestPointerLock({ unadjustedMovement: true });
    } catch {
      // unadjustedMovement is not available everywhere (e.g. some OSes): plain lock.
      try {
        await this.surface.requestPointerLock();
      } catch {
        // Denied (no user activation, or blocked): pointerlockerror covers it.
      }
    }
  }

  // -------------------------------------------------------------------------
  // wiring

  private attach(): void {
    const s = this.surface;
    // pointerrawupdate delivers every hardware event (not rAF-aligned) where supported.
    const moveEvent = 'onpointerrawupdate' in window ? 'pointerrawupdate' : 'pointermove';
    s.addEventListener(moveEvent, (e) => this.onPointerMove(e as PointerEvent));
    s.addEventListener('pointerdown', (e) => this.onPointerButton(e));
    s.addEventListener('pointerup', (e) => this.onPointerButton(e));
    s.addEventListener('pointercancel', () => this.releaseButtons());
    s.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    // Browser default actions that would interfere: context menu, back/forward, middle-click scroll.
    for (const type of ['contextmenu', 'auxclick', 'mousedown', 'mouseup'] as const) {
      s.addEventListener(type, (e) => this.enabled && e.preventDefault());
    }
    window.addEventListener('keydown', (e) => this.onKey(e, true), { capture: true });
    window.addEventListener('keyup', (e) => this.onKey(e, false), { capture: true });
    // Also before a session runs (the Connect click), so Caps Lock can be synced as soon as it starts.
    window.addEventListener('pointerdown', (e) => (this.localCaps = e.getModifierState('CapsLock')), {
      capture: true,
      passive: true,
    });
    window.addEventListener('blur', () => {
      if (this.enabled) this.releaseAll();
      this.updateGrab();
    });
    document.addEventListener('visibilitychange', () => {
      if (document.hidden && this.enabled) this.releaseAll();
      this.updateGrab();
    });
    window.addEventListener('focus', () => this.updateGrab());
    document.addEventListener('focusin', () => this.updateGrab());
    document.addEventListener('focusout', () => this.updateGrab());
    window.addEventListener('pageshow', () => this.updateGrab());
    window.addEventListener('pagehide', () => this.bridge.setActive(false));
    document.addEventListener('pointerlockchange', () => this.onPointerLockChange());
  }

  /**
   * Keys go to the remote machine: streaming, page visible and focused, and no viewer control has focus. The
   * desktop app grabs the keyboard (system shortcuts included) exactly while this holds.
   */
  private updateGrab(): void {
    const on = this.enabled && !document.hidden && document.hasFocus() && this.forwardsKeysFrom(document.activeElement);
    if (on === this.bridge.active) return;
    this.native.reset();
    this.bridge.setActive(on);
  }

  /** Keys typed into viewer UI (settings, banner buttons) stay local. */
  private forwardsKeysFrom(t: EventTarget | null): boolean {
    return t === null || t === window || t === this.surface || t === document.body || t === document.documentElement;
  }

  private updateRect(): void {
    const r = this.surface.getBoundingClientRect();
    this.rect = { left: r.left, top: r.top, width: r.width, height: r.height };
  }

  private backpressured(): boolean {
    return this.sink.bufferedAmount > BACKPRESSURE_BYTES;
  }

  // -------------------------------------------------------------------------
  // mouse

  private normalize(e: PointerEvent): { x: number; y: number } {
    if (this.videoW === 0) return this.pos;
    return normalizePoint(e.clientX, e.clientY, this.rect, this.videoW, this.videoH);
  }

  private onPointerMove(e: PointerEvent): void {
    if (!this.enabled) return;
    if (this.locked) {
      this.relX += e.movementX;
      this.relY += e.movementY;
      this.syncButtons(e.buttons);
      return;
    }
    const p = this.normalize(e);
    this.pendingMove = p;
    this.pos = p;
    // Chorded buttons (a second button while one is held) arrive as moves.
    if (e.buttons !== this.domButtons) {
      this.syncButtons(e.buttons);
      return;
    }
    this.scheduleMove();
  }

  private onPointerButton(e: PointerEvent): void {
    if (!this.enabled) return;
    if (e.type === 'pointerdown') {
      e.preventDefault();
      this.surface.focus({ preventScroll: true });
      if (!this.locked) this.surface.setPointerCapture(e.pointerId);
    } else {
      e.preventDefault();
    }
    if (!this.locked) {
      this.pos = this.normalize(e);
      this.pendingMove = this.pos;
    }
    this.keys.syncCaps(this.native.caps(e.getModifierState('CapsLock')));
    this.syncButtons(e.buttons);
  }

  /** Emit MOUSE_BTN for every changed button (after the modifiers and the motion they apply to). */
  private syncButtons(dom: number): void {
    const changed = dom ^ this.domButtons;
    if (changed === 0) return;
    this.keys.flushPending();
    // Buttons are never dropped, so the motion before them must not be either (not even under backpressure).
    if (this.locked) this.flushRel(true);
    else this.flushMove();
    for (const [bit, button] of DOM_BUTTON_BITS) {
      if ((changed & bit) === 0) continue;
      const down = (dom & bit) !== 0;
      const mask = 1 << button; // MOUSE_ABS bitmask bit
      this.buttons = down ? this.buttons | mask : this.buttons & ~mask;
      this.sink.send(encodeMouseBtn(button, down, this.pos.x, this.pos.y));
    }
    this.domButtons = dom;
  }

  private releaseButtons(): void {
    if (this.enabled) this.syncButtons(0);
  }

  private scheduleMove(): void {
    if (this.moveTimer) return;
    const wait = MOVE_INTERVAL_MS - (performance.now() - this.lastMoveMs);
    if (wait <= 0 && !this.backpressured()) {
      this.flushMove();
      return;
    }
    this.moveTimer = window.setTimeout(
      () => {
        this.moveTimer = 0;
        if (this.backpressured()) this.scheduleMove();
        else this.flushMove();
      },
      Math.max(wait, MOVE_INTERVAL_MS / 2),
    );
  }

  private flushMove(): void {
    const p = this.pendingMove;
    if (!p) return;
    this.pendingMove = null;
    this.lastMoveMs = performance.now();
    this.sink.send(encodeMouseAbs(this.buttons, p.x, p.y));
  }

  private onPointerLockChange(): void {
    const locked = document.pointerLockElement === this.surface;
    if (locked === this.locked) return;
    this.locked = locked;
    clearInterval(this.relTimer);
    this.relTimer = 0;
    if (locked) {
      // The last absolute position goes out now: no MOUSE_ABS may follow relative motion.
      clearTimeout(this.moveTimer);
      this.moveTimer = 0;
      if (this.enabled) this.flushMove();
      this.pendingMove = null;
      this.relTimer = window.setInterval(() => this.flushRel(), MOVE_INTERVAL_MS);
    } else if (this.enabled) {
      this.releaseAll();
    }
    this.cb.pointerLockChanged(locked);
  }

  /** Summed pointer-lock motion, sent every 4 ms; coalesces further under backpressure unless `force`d. */
  private flushRel(force = false): void {
    if (!this.enabled || (!force && this.backpressured())) return;
    let dx = Math.trunc(this.relX);
    let dy = Math.trunc(this.relY);
    this.relX -= dx;
    this.relY -= dy;
    while (dx !== 0 || dy !== 0) {
      const sx = clampI16(dx);
      const sy = clampI16(dy);
      this.sink.send(encodeMouseRel(sx, sy));
      dx -= sx;
      dy -= sy;
    }
  }

  // -------------------------------------------------------------------------
  // wheel

  private onWheel(e: WheelEvent): void {
    if (!this.enabled) return;
    e.preventDefault();
    this.keys.flushPending(); // Ctrl+wheel
    this.wheel.push(e, performance.now());
    this.scheduleWheel();
  }

  private scheduleWheel(): void {
    if (this.wheelRaf) return;
    this.wheelRaf = requestAnimationFrame((t) => {
      this.wheelRaf = 0;
      if (!this.enabled) return;
      if (!this.backpressured()) {
        for (const w of this.wheel.flush(t)) this.sink.send(encodeWheel(w.mode, w.dx, w.dy, w.phase));
      }
      if (this.wheel.active) this.scheduleWheel();
    });
  }

  // -------------------------------------------------------------------------
  // keyboard

  private onKey(e: KeyboardEvent, down: boolean): void {
    this.localCaps = e.getModifierState('CapsLock');
    // While the desktop app grabs the keyboard its hook delivers every key (rdNative.key): never send twice.
    if (!this.enabled || this.bridge.grabbing) return;
    if (this.keys.handle(e, down, this.forwardsKeysFrom(e.target)) === 'consume') {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  /** A key from the desktop app: the same path as a DOM key event aimed at the video. */
  private onNativeKey(code: string, down: boolean, repeat: boolean): void {
    const e = this.native.event(code, down, repeat);
    if (this.enabled) this.keys.handle(e, down, this.bridge.active);
  }

  /** The app stopped grabbing on its own: like a window blur. */
  private onNativeRelease(): void {
    this.native.reset();
    if (this.enabled) this.releaseAll();
  }
}
