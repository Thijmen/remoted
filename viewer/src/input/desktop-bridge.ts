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
  /** The app accepted or refused a grab (see `grabbing`, `refusal`). */
  changed?(): void;
}

/** An answer that takes longer than this counts as a refusal (DOM keys keep working). */
export const INVOKE_TIMEOUT_MS = 3000;
/** The first hook re-install when DOM keys show the hook is not delivering; doubles each time, up to the max. */
export const REASSERT_INTERVAL_MS = 5000;
export const REASSERT_MAX_INTERVAL_MS = 60_000;
/** Keys came through the hook this recently: it works, and DOM keys are ones it leaves alone on purpose. */
export const NATIVE_RECENT_MS = 2000;

/**
 * Request numbers for `rd_set_grab`, shared by every bridge of the page (a session may build a new one). The
 * app runs commands concurrently, so it applies a request only if its number is newer than the last one. They
 * start at the clock (µs) so that a newer page always outranks a late call of the page before it.
 */
let lastSeq = Date.now() * 1000;

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
  private reassertInterval = REASSERT_INTERVAL_MS;
  private lastNativeMs = Number.NEGATIVE_INFINITY;
  /** Bumped by `release()`: an answer to a call sent before it says nothing about the app's state now. */
  private epoch = 0;
  private warned = false;
  /** Why the app last refused to grab (its error message); null once a grab is accepted. */
  private lastRefusal: string | null = null;

  constructor(
    private readonly host: BridgeHost,
    private readonly handlers: BridgeHandlers,
    private readonly warn: (msg: string, err: unknown) => void = (msg, err) => console.warn(msg, err),
    private readonly now: () => number = () => performance.now(),
  ) {
    host.rdNative = Object.freeze({
      version: BRIDGE_VERSION,
      key: (code: string, down: boolean, repeat: boolean) => {
        if (typeof code !== 'string' || typeof down !== 'boolean') return;
        this.lastNativeMs = this.now();
        this.reassertInterval = REASSERT_INTERVAL_MS;
        handlers.key(code, down, repeat === true);
      },
      release: () => {
        // The app has stopped grabbing; the next eligible state asks again (`setActive(true)`).
        this.epoch++;
        this.wanted = false;
        this.confirmed = false;
        this.told = false;
        this.refused = null;
        this.cycle = false;
        handlers.release();
      },
    });
  }

  /** The page runs inside the desktop app (rd Desktop). */
  get inApp(): boolean {
    return this.host.__TAURI_INTERNALS__ !== undefined;
  }

  /** Why the app last refused to grab the keyboard, e.g. a missing macOS permission; null when it last grabbed. */
  get refusal(): string | null {
    return this.lastRefusal;
  }

  /**
   * Call another app command (the Troubleshoot screen's). Rejects outside the app, when the app refuses (an older
   * app without the command), or without an answer within `INVOKE_TIMEOUT_MS`.
   */
  call(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
    const tauri = this.host.__TAURI_INTERNALS__;
    if (!tauri) return Promise.reject(new Error('not in the desktop app'));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('no answer')), INVOKE_TIMEOUT_MS);
      const done = (f: () => void) => {
        clearTimeout(timer);
        f();
      };
      try {
        Promise.resolve(tauri.invoke(cmd, args)).then(
          (v) => done(() => resolve(v)),
          (err: unknown) => done(() => reject(err)),
        );
      } catch (err) {
        done(() => reject(err));
      }
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
   * A DOM key arrived although the app confirmed the grab. If no key has come through the hook lately, the hook
   * probably sees nothing (Windows dropped it, or other software got in the way): have the app install a fresh
   * one, backing off while that does not help. Keys the hook passes on purpose (injected by other programs)
   * arrive while it delivers the others, so they cause no re-install.
   */
  reassert(): void {
    const nowMs = this.now();
    if (!this.grabbing || nowMs - this.lastNativeMs < NATIVE_RECENT_MS) return;
    if (nowMs - this.lastReassertMs < this.reassertInterval) return;
    if (this.lastReassertMs > Number.NEGATIVE_INFINITY) {
      this.reassertInterval = Math.min(this.reassertInterval * 2, REASSERT_MAX_INTERVAL_MS);
    }
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
    const epoch = this.epoch;
    const settle = (ok: boolean, err?: unknown) => {
      if (this.inflight?.seq !== seq) return; // already timed out
      clearTimeout(this.inflight.timer);
      this.inflight = null;
      if (ok) {
        // After a release() the app is not grabbing, whatever it answered before: ask again if wanted.
        this.told = epoch === this.epoch ? active : null;
        this.refused = null;
        if (active) this.lastRefusal = null;
      } else {
        // Unknown: a late answer may still apply it. Keys the hook forwards anyway are handled (rdNative.key).
        this.told = null;
        this.refused = active;
        if (active) this.lastRefusal = errorText(err);
        this.warnOnce('rd: desktop app: rd_set_grab failed; keys stay with the page', err);
      }
      this.pump();
      this.handlers.changed?.();
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

/** Tauri rejects with the command's error string; anything else is described as well as it can be. */
function errorText(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}
