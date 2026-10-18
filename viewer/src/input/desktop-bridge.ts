/** Version of the page ⇄ desktop app contract (docs/desktop-client.md). */
export const BRIDGE_VERSION = 1;

/** What the desktop app calls, with `eval` on the webview. */
export interface RdNative {
  readonly version: number;
  /** A physical key (`KeyboardEvent.code`) from the app's keyboard hook. */
  key(code: string, down: boolean, repeat: boolean): void;
  /** The app stopped grabbing on its own (lost foreground, hook removed, closing). */
  release(): void;
}

/** Injected by Tauri v2 into the pages it loads. */
export interface TauriInternals {
  invoke(cmd: string, args?: Record<string, unknown>): unknown;
}

export interface BridgeHost {
  rdNative?: RdNative;
  __TAURI_INTERNALS__?: TauriInternals;
}

declare global {
  interface Window {
    rdNative?: RdNative;
    __TAURI_INTERNALS__?: TauriInternals;
  }
}

export interface BridgeHandlers {
  key(code: string, down: boolean, repeat: boolean): void;
  release(): void;
}

/** An answer that takes longer than this counts as a refusal (DOM keys keep working). */
export const INVOKE_TIMEOUT_MS = 3000;
/** At most one hook re-install per this interval when DOM keys show the hook is not delivering. */
export const REASSERT_INTERVAL_MS = 5000;

/**
 * Request numbers for `rd_set_grab`, shared by every bridge of the page (a session may build a new one). The
 * app runs commands concurrently, so it applies a request only if its number is newer than the last one; it
 * starts over when a new page loads.
 */
let lastSeq = 0;

/**
 * The page's half of the desktop app bridge. It always defines `rdNative` (unused in a browser); inside the
 * app (`__TAURI_INTERNALS__` present) it also tells the app, on every change, whether to grab the keyboard.
 *
 * Requests are serialized: one `rd_set_grab` at a time, the latest wanted state sent when the previous one
 * is answered, so the app can never end up in an older state than the page.
 */
export class DesktopBridge {
  private wanted = false;
  /** The app answered that its hook delivers the keys for the current `wanted`. */
  private confirmed = false;
  /** The state the app last accepted (null: unknown, e.g. at start or after a failed request). */
  private told: boolean | null = null;
  /** The state the app last refused: not asked again until the next change or `retry()`. */
  private refused: boolean | null = null;
  private inflight: { seq: number; timer: ReturnType<typeof setTimeout> } | null = null;
  /** Re-install the hook: send `false` before the next `true`. */
  private cycle = false;
  private lastReassertMs = Number.NEGATIVE_INFINITY;
  private warned = false;

  constructor(
    private readonly host: BridgeHost,
    handlers: BridgeHandlers,
    private readonly warn: (msg: string, err: unknown) => void = (msg, err) => console.warn(msg, err),
  ) {
    host.rdNative = Object.freeze({
      version: BRIDGE_VERSION,
      key: (code: string, down: boolean, repeat: boolean) => {
        if (typeof code !== 'string' || typeof down !== 'boolean') return;
        handlers.key(code, down, repeat === true);
      },
      release: () => {
        // The app has stopped grabbing; the next eligible state asks again (`setActive(true)`).
        this.wanted = false;
        this.confirmed = false;
        this.told = false;
        this.refused = null;
        this.cycle = false;
        handlers.release();
      },
    });
  }

  /** Keyboard input goes to the remote machine (what the page last asked for). */
  get active(): boolean {
    return this.wanted;
  }

  /** The app grabs the keyboard and forwards keys through `rdNative.key`. */
  get grabbing(): boolean {
    return this.wanted && this.confirmed;
  }

  setActive(active: boolean): void {
    if (active === this.wanted) return;
    this.wanted = active;
    this.confirmed = false;
    this.refused = null;
    this.pump();
  }

  /** The grab was refused (e.g. the window was not in front yet): ask again, e.g. on a click into the video. */
  retry(): void {
    if (!this.wanted || this.confirmed || this.inflight) return;
    this.refused = null;
    this.told = null;
    this.pump();
  }

  /**
   * DOM keys arrive although the app confirmed the grab, so its hook does not see them (Windows dropped the
   * hook, or other software got in the way). Have the app install a fresh one.
   */
  reassert(nowMs: number): void {
    if (!this.grabbing || nowMs - this.lastReassertMs < REASSERT_INTERVAL_MS) return;
    this.lastReassertMs = nowMs;
    this.warnOnce('rd: desktop app: keys bypass its keyboard hook; asking it to grab again', null);
    this.confirmed = false;
    this.cycle = true;
    this.pump();
  }

  private pump(): void {
    const tauri = this.host.__TAURI_INTERNALS__;
    if (!tauri || this.inflight) return;
    let active: boolean;
    if (this.cycle && this.wanted) {
      this.cycle = false;
      active = false;
    } else {
      this.cycle = false;
      if (this.told === this.wanted) {
        if (this.wanted) this.confirmed = true;
        return;
      }
      if (this.refused === this.wanted) return;
      active = this.wanted;
    }
    const seq = ++lastSeq;
    const settle = (ok: boolean, err?: unknown) => {
      if (this.inflight?.seq !== seq) return; // already timed out
      clearTimeout(this.inflight.timer);
      this.inflight = null;
      if (ok) {
        this.told = active;
        this.refused = null;
      } else {
        // Unknown: a late answer may still apply it. Keys the hook forwards anyway are handled (rdNative.key).
        this.told = null;
        this.refused = active;
        this.warnOnce('rd: desktop app: rd_set_grab failed; keys stay with the page', err);
      }
      this.pump();
    };
    this.inflight = { seq, timer: setTimeout(() => settle(false, new Error('no answer')), INVOKE_TIMEOUT_MS) };
    let answer: unknown;
    try {
      answer = tauri.invoke('rd_set_grab', { active, seq });
    } catch (err) {
      settle(false, err);
      return;
    }
    // Until the app answers (and when it refuses, e.g. an older app), DOM key events keep working.
    Promise.resolve(answer).then(
      () => settle(true),
      (err: unknown) => settle(false, err),
    );
  }

  private warnOnce(msg: string, err: unknown): void {
    if (this.warned) return;
    this.warned = true;
    this.warn(msg, err);
  }
}
