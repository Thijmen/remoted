import type { KeyInput } from './keyboard';

export interface NativeKeysDeps {
  /** The forwarder holds the key down on the remote side (it may have been pressed before the grab, in the DOM). */
  held(code: string): boolean;
  /** The local Caps Lock as the page last saw it in a DOM event. */
  localCaps(): boolean;
  now(): number;
}

const SIDES: Readonly<Record<string, readonly [string, string]>> = {
  Shift: ['ShiftLeft', 'ShiftRight'],
  Control: ['ControlLeft', 'ControlRight'],
  Alt: ['AltLeft', 'AltRight'],
  Meta: ['MetaLeft', 'MetaRight'],
};

/**
 * Keys from the desktop app (`window.rdNative.key`) arrive as bare (code, down, repeat), while the forwarder
 * reads more from a DOM event: modifier flags, Caps Lock, AltGraph. This derives them from the keys the app
 * reported down (plus keys still held from before the grab).
 *
 * Caps Lock: the hook swallows CapsLock, so the local lock does not change during a grab. The remote lock is
 * the local one, toggled by each CapsLock press reported since the grab started (`reset`).
 */
export class NativeKeys {
  private readonly down = new Set<string>();
  /** An odd number of CapsLock presses since reset(). */
  private capsFlipped = false;

  constructor(private readonly deps: NativeKeysDeps) {}

  /** A grab started or ended, or the app released everything: forget what it reported. */
  reset(): void {
    this.down.clear();
    this.capsFlipped = false;
  }

  /** Caps Lock as the remote side should have it, given the local lock. */
  caps(local: boolean): boolean {
    return local !== this.capsFlipped;
  }

  /** Record a reported key and describe it like a DOM KeyboardEvent. */
  event(code: string, down: boolean, repeat: boolean): KeyInput {
    const again = down && this.down.has(code);
    if (down) {
      if (code === 'CapsLock' && !again && !repeat) this.capsFlipped = !this.capsFlipped;
      this.down.add(code);
    } else {
      this.down.delete(code);
    }
    const isDown = (c: string) => this.down.has(c) || (c !== code && this.deps.held(c));
    const mods: Record<string, boolean> = {};
    for (const [name, [left, right]] of Object.entries(SIDES)) mods[name] = isDown(left) || isDown(right);
    mods.CapsLock = this.caps(this.deps.localCaps());
    // Windows reports AltGr as left Ctrl + right Alt (its fake Ctrl comes through the hook as well).
    mods.AltGraph = isDown('AltRight') && isDown('ControlLeft');
    return {
      code,
      key: 'Unidentified',
      repeat: repeat || again,
      timeStamp: this.deps.now(),
      ctrlKey: mods.Control === true,
      altKey: mods.Alt === true,
      shiftKey: mods.Shift === true,
      getModifierState: (k) => mods[k] === true,
      native: true,
    };
  }
}
