import { type Bytes, encodeKey } from '../proto/bin';
import { type ChordAction, chordFor } from './chords';
import { hidForCode } from './keymap.gen';
import { type ModifierMapping, isModifierCode, mapCode } from './modifiers';

export const HID_CAPS_LOCK = 0x00070039;

/**
 * Windows sends a fake ControlLeft right before AltRight on AltGr layouts; both carry (nearly) the same
 * timestamp. A real Ctrl followed by AltGr this fast is not humanly distinguishable (noVNC uses 50 ms too).
 */
export const ALTGR_WINDOW_MS = 50;
/** A held-back ControlLeft is sent at the latest after this long (a lone Ctrl press). */
export const ALTGR_HOLD_MS = 100;

/** The parts of a KeyboardEvent the forwarding logic reads (tests pass plain objects). */
export interface KeyInput {
  readonly code: string;
  readonly key: string;
  readonly repeat: boolean;
  readonly timeStamp: number;
  readonly ctrlKey: boolean;
  readonly altKey: boolean;
  readonly shiftKey: boolean;
  getModifierState(key: string): boolean;
}

export interface KeyboardDeps {
  /** Send a binary frame; false when not connected. */
  send(b: Bytes): boolean;
  chord(action: ChordAction): void;
  mapping(): ModifierMapping;
}

export interface KeyboardPlatform {
  /** Windows viewer: AltGr filter and the double-Shift release. */
  windows: boolean;
  /** macOS viewer: Chrome swallows keyups of keys released while Cmd is held. */
  mac: boolean;
}

/**
 * What the caller does with the DOM event: `consume` = preventDefault + stopPropagation, `ignore` = leave it
 * alone. `released` = a key-up was forwarded but the event belongs to a viewer control (focus moved while the
 * key was held), so the control must still see it.
 */
export type KeyVerdict = 'consume' | 'ignore' | 'released';

const SHIFTS = ['ShiftLeft', 'ShiftRight'] as const;
const isShift = (code: string) => code === 'ShiftLeft' || code === 'ShiftRight';

/**
 * Keyboard state between the DOM and the wire: which physical keys are down on the remote side, the
 * remote Caps Lock state, viewer chords, and the Windows quirks (AltGr's fake Ctrl, missing Shift keyups).
 */
export class KeyboardForwarder {
  /** Physical code → HID sent on keydown, so keyup releases the same key even if the mapping changed. */
  private readonly pressed = new Map<string, number>();
  /** Codes whose keydown was consumed (chord, AltGr's fake Ctrl); their autorepeat and keyup are too. */
  private readonly swallowed = new Set<string>();
  /**
   * Caps Lock as the agent last saw it. The agent toggles on each CapsLock down, but browsers deliver
   * CapsLock events unevenly (Chrome on macOS: one event per press, alternating down/up), so the lock
   * state is synced from `getModifierState('CapsLock')` instead of forwarding the key.
   */
  private remoteCaps = false;
  /** Windows: a ControlLeft keydown held back to see whether it is AltGr's fake Ctrl. */
  private pendingCtrl: { timeStamp: number; timer: ReturnType<typeof setTimeout> } | null = null;

  constructor(
    private readonly deps: KeyboardDeps,
    private readonly platform: KeyboardPlatform,
  ) {}

  /**
   * Handle a keydown/keyup. `local` is false when the event targets viewer UI (settings, banner buttons):
   * such keydowns stay local, but a keyup always releases a key that was forwarded.
   */
  handle(e: KeyInput, down: boolean, local: boolean): KeyVerdict {
    // Windows sends no keyup for a Shift released while the other one is held; the OS state knows better.
    if (!isShift(e.code) && !e.shiftKey) this.releaseShifts();
    if (down) return local ? this.keyDown(e) : 'ignore';
    return this.keyUp(e, local);
  }

  /** Make the remote Caps Lock match the local one (a CapsLock tap when they differ). */
  syncCaps(on: boolean): void {
    if (on === this.remoteCaps) return;
    if (!this.deps.send(encodeKey(true, false, HID_CAPS_LOCK))) return;
    this.deps.send(encodeKey(false, false, HID_CAPS_LOCK));
    this.remoteCaps = on;
  }

  /** Send a held-back Ctrl now (before mouse buttons and wheel, so Ctrl+click works). */
  flushPending(): void {
    const p = this.pendingCtrl;
    if (!p) return;
    this.pendingCtrl = null;
    clearTimeout(p.timer);
    this.press('ControlLeft', false);
  }

  /**
   * Forget all key state: the agent released everything (RELEASE_ALL, which also clears its Caps Lock)
   * or a new session starts with nothing pressed and Caps Lock off.
   */
  forget(): void {
    if (this.pendingCtrl) clearTimeout(this.pendingCtrl.timer);
    this.pendingCtrl = null;
    this.pressed.clear();
    this.swallowed.clear();
    this.remoteCaps = false;
  }

  // -------------------------------------------------------------------------

  private keyDown(e: KeyInput): KeyVerdict {
    const pending = this.pendingCtrl;
    if (pending) {
      this.pendingCtrl = null;
      clearTimeout(pending.timer);
      if (e.code === 'AltRight' && isAltGraph(e) && e.timeStamp - pending.timeStamp < ALTGR_WINDOW_MS) {
        // AltGr: the Ctrl was Windows' invention. Never send it, and eat its keyup as well.
        this.swallowed.add('ControlLeft');
      } else {
        this.press('ControlLeft', false);
      }
    }
    if (this.swallowed.has(e.code)) {
      if (e.repeat) return 'consume';
      this.swallowed.delete(e.code); // a new press: its earlier keyup never came
    }
    // Ctrl+Alt+Shift chords. On Windows the event's ctrlKey is also set by AltGr's fake Ctrl, so only a
    // Ctrl that was actually forwarded counts (AltGr+Shift+D must type, not disconnect).
    const chord = chordFor({ code: e.code, ctrlKey: this.ctrlHeld(e), altKey: e.altKey, shiftKey: e.shiftKey });
    if (chord) {
      this.swallowed.add(e.code);
      if (!e.repeat) this.deps.chord(chord);
      return 'consume';
    }
    if (e.code === 'CapsLock') {
      this.syncCaps(e.getModifierState('CapsLock'));
      return 'consume';
    }
    if (this.platform.windows && e.code === 'ControlLeft' && !this.pressed.has('ControlLeft')) {
      this.pendingCtrl = { timeStamp: e.timeStamp, timer: setTimeout(() => this.flushPending(), ALTGR_HOLD_MS) };
      return 'consume';
    }
    if (hidForCode(mapCode(e.code, this.deps.mapping())) === undefined && !this.pressed.has(e.code)) return 'ignore';
    this.syncCaps(e.getModifierState('CapsLock'));
    this.press(e.code, e.repeat);
    return 'consume';
  }

  private keyUp(e: KeyInput, local: boolean): KeyVerdict {
    // A keyup cannot be part of the fake-Ctrl + AltGr pair; a quick Ctrl tap ends up here too.
    this.flushPending();
    if (this.swallowed.delete(e.code)) return local ? 'consume' : 'ignore';
    if (e.code === 'CapsLock') {
      if (!local) return 'ignore';
      this.syncCaps(e.getModifierState('CapsLock'));
      return 'consume';
    }
    const hid = this.pressed.get(e.code);
    if (hid === undefined) {
      return local && hidForCode(mapCode(e.code, this.deps.mapping())) !== undefined ? 'consume' : 'ignore';
    }
    this.syncCaps(e.getModifierState('CapsLock'));
    this.release(e.code);
    if (isShift(e.code)) {
      // Windows: when both Shifts were down, only the last one released sends a keyup.
      if (this.platform.windows) this.releaseShifts();
    } else if (this.platform.mac && (e.code === 'MetaLeft' || e.code === 'MetaRight')) {
      // Chrome on macOS swallows keyups of keys released while Cmd is held.
      this.releaseNonModifiers();
    }
    return local ? 'consume' : 'released';
  }

  /** Send a key-down for a physical code (autorepeat if it is already down). */
  private press(code: string, repeat: boolean): void {
    const held = this.pressed.get(code);
    const hid = held ?? hidForCode(mapCode(code, this.deps.mapping()));
    if (hid === undefined) return;
    this.swallowed.delete(code);
    this.pressed.set(code, hid);
    this.deps.send(encodeKey(true, repeat && held !== undefined, hid));
  }

  private release(code: string): void {
    const hid = this.pressed.get(code);
    if (hid === undefined) return;
    this.pressed.delete(code);
    this.deps.send(encodeKey(false, false, hid));
  }

  private releaseShifts(): void {
    for (const code of SHIFTS) this.release(code);
  }

  private releaseNonModifiers(): void {
    for (const code of [...this.pressed.keys()]) if (!isModifierCode(code)) this.release(code);
  }

  private ctrlHeld(e: KeyInput): boolean {
    if (!e.ctrlKey) return false;
    return !this.platform.windows || this.pressed.has('ControlLeft') || this.pressed.has('ControlRight');
  }
}

function isAltGraph(e: KeyInput): boolean {
  return e.key === 'AltGraph' || e.getModifierState('AltGraph');
}
