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

/**
 * The page's half of the desktop app bridge. It always defines `rdNative` (unused in a browser); inside the
 * app (`__TAURI_INTERNALS__` present) it also tells the app, on every change, whether to grab the keyboard.
 */
export class DesktopBridge {
  private wanted = false;
  /** The app accepted the current grab: its hook delivers the keys, so DOM key events are ignored. */
  private confirmed = false;
  /** Bumped by every grab change and release, so a late answer cannot confirm a grab that already ended. */
  private generation = 0;
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
        this.generation++;
        this.confirmed = false;
        handlers.release();
      },
    });
  }

  /** Keyboard input goes to the remote machine (what the page last told the app). */
  get active(): boolean {
    return this.wanted;
  }

  /** The app grabs the keyboard and forwards keys through `rdNative.key`: DOM key events are duplicates. */
  get grabbing(): boolean {
    return this.wanted && this.confirmed;
  }

  setActive(active: boolean): void {
    if (active === this.wanted) return;
    this.wanted = active;
    this.confirmed = false;
    const generation = ++this.generation;
    const tauri = this.host.__TAURI_INTERNALS__;
    if (!tauri) return;
    let answer: unknown;
    try {
      answer = tauri.invoke('rd_set_grab', { active });
    } catch (err) {
      this.fail(err);
      return;
    }
    // Until the app answers (and when it refuses, e.g. an older app), DOM key events keep working.
    Promise.resolve(answer).then(
      () => {
        if (active && generation === this.generation) this.confirmed = true;
      },
      (err: unknown) => this.fail(err),
    );
  }

  private fail(err: unknown): void {
    if (this.warned) return;
    this.warned = true;
    this.warn('rd: desktop app: rd_set_grab failed; keys stay with the page', err);
  }
}
