import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { type Bytes, parse } from '../proto/bin';
import type { ChordAction } from './chords';
import { ALTGR_HOLD_MS, HID_CAPS_LOCK, type KeyInput, KeyboardForwarder, type KeyboardPlatform } from './keyboard';
import type { ModifierMapping } from './modifiers';

const HID = {
  KeyA: 0x00070004,
  KeyD: 0x00070007,
  KeyE: 0x00070008,
  ShiftLeft: 0x000700e1,
  ShiftRight: 0x000700e5,
  KeyC: 0x00070006,
  ControlLeft: 0x000700e0,
  AltLeft: 0x000700e2,
  AltRight: 0x000700e6,
  MetaLeft: 0x000700e3,
  MetaRight: 0x000700e7,
  Tab: 0x0007002b,
} as const;

interface EvOpts {
  key?: string;
  repeat?: boolean;
  t?: number;
  ctrl?: boolean;
  alt?: boolean;
  shift?: boolean;
  caps?: boolean;
  altGraph?: boolean;
}

function ev(code: string, o: EvOpts = {}): KeyInput {
  return {
    code,
    key: o.key ?? code,
    repeat: o.repeat ?? false,
    timeStamp: o.t ?? 0,
    ctrlKey: o.ctrl ?? false,
    altKey: o.alt ?? false,
    shiftKey: o.shift ?? false,
    getModifierState: (k) => (k === 'CapsLock' ? (o.caps ?? false) : k === 'AltGraph' ? (o.altGraph ?? false) : false),
  };
}

/** A forwarder whose wire output is decoded into readable strings ("down 0x700e0", "up …", "repeat …"). */
function setup(platform: Partial<KeyboardPlatform> = {}, mapping: ModifierMapping = 'passthrough') {
  const wire: string[] = [];
  const chords: ChordAction[] = [];
  let connected = true;
  const kb = new KeyboardForwarder(
    {
      send: (b: Bytes) => {
        if (!connected) return false;
        const m = parse(b);
        if (m.type !== 'key') throw new Error(`unexpected ${m.type}`);
        wire.push(`${m.down ? (m.repeat ? 'repeat' : 'down') : 'up'} 0x${m.hid.toString(16)}`);
        return true;
      },
      chord: (a) => chords.push(a),
      mapping: () => mapping,
    },
    { windows: false, mac: false, ...platform },
  );
  const take = () => wire.splice(0);
  return { kb, wire, chords, take, setConnected: (c: boolean) => (connected = c) };
}

const down = (hid: number) => `down 0x${hid.toString(16)}`;
const up = (hid: number) => `up 0x${hid.toString(16)}`;
const capsTap = [down(HID_CAPS_LOCK), up(HID_CAPS_LOCK)];

describe('KeyboardForwarder basics', () => {
  it('forwards a key press and release by physical code', () => {
    const { kb, take } = setup();
    expect(kb.handle(ev('KeyA'), true, true)).toBe('consume');
    expect(kb.handle(ev('KeyA', { repeat: true }), true, true)).toBe('consume');
    expect(kb.handle(ev('KeyA'), false, true)).toBe('consume');
    expect(take()).toEqual([down(HID.KeyA), `repeat 0x${HID.KeyA.toString(16)}`, up(HID.KeyA)]);
  });

  it('maps Ctrl to Cmd in windows-to-mac and releases what was pressed even if the mapping changes', () => {
    let mapping: ModifierMapping = 'windows-to-mac';
    const wire: number[] = [];
    const kb = new KeyboardForwarder(
      {
        send: (b) => {
          const m = parse(b);
          if (m.type === 'key') wire.push(m.hid);
          return true;
        },
        chord: () => {},
        mapping: () => mapping,
      },
      { windows: false, mac: false },
    );
    kb.handle(ev('ControlLeft', { ctrl: true }), true, true);
    mapping = 'passthrough';
    kb.handle(ev('ControlLeft'), false, true);
    expect(wire).toEqual([HID.MetaLeft, HID.MetaLeft]);
  });

  it('leaves keys without a HID usage alone', () => {
    const { kb, take } = setup();
    expect(kb.handle(ev('Unidentified'), true, true)).toBe('ignore');
    expect(kb.handle(ev('Unidentified'), false, true)).toBe('ignore');
    expect(take()).toEqual([]);
  });
});

describe('keyup release (focus moved to viewer UI)', () => {
  it('keeps keydowns aimed at viewer controls local', () => {
    const { kb, take } = setup();
    expect(kb.handle(ev('KeyA'), true, false)).toBe('ignore');
    expect(kb.handle(ev('KeyA'), false, false)).toBe('ignore');
    expect(take()).toEqual([]);
  });

  it('always releases a forwarded key, without taking the keyup away from the control', () => {
    const { kb, take } = setup();
    kb.handle(ev('ShiftLeft', { shift: true }), true, true);
    // The user clicks the toolbar handle while holding Shift; the keyup targets that button.
    expect(kb.handle(ev('ShiftLeft'), false, false)).toBe('released');
    expect(take()).toEqual([down(HID.ShiftLeft), up(HID.ShiftLeft)]);
    // A later keyup on the canvas has nothing left to release.
    kb.handle(ev('ShiftLeft'), false, true);
    expect(take()).toEqual([]);
  });

  it('drops a stale chord swallow when focus moved before the keyup', () => {
    const { kb, take, chords } = setup();
    kb.handle(ev('KeyD', { ctrl: true, alt: true, shift: true }), true, true);
    expect(chords).toEqual(['disconnect']);
    expect(kb.handle(ev('KeyD'), false, false)).toBe('ignore');
    // The next ordinary D press and release both go through.
    kb.handle(ev('KeyD'), true, true);
    kb.handle(ev('KeyD'), false, true);
    expect(take()).toEqual([down(HID.KeyD), up(HID.KeyD)]);
  });
});

describe('chords', () => {
  it('swallows the chord key, its autorepeat and its keyup', () => {
    const { kb, take, chords } = setup();
    const mods = { ctrl: true, alt: true, shift: true };
    expect(kb.handle(ev('KeyD', mods), true, true)).toBe('consume');
    expect(kb.handle(ev('KeyD', { ...mods, repeat: true }), true, true)).toBe('consume');
    // Modifiers released while D keeps repeating: still not typed on the remote side.
    expect(kb.handle(ev('KeyD', { repeat: true }), true, true)).toBe('consume');
    expect(kb.handle(ev('KeyD'), false, true)).toBe('consume');
    expect(chords).toEqual(['disconnect']);
    expect(take()).toEqual([]);
  });

  it('Ctrl+Alt+Shift+` toggles the terminal', () => {
    const { kb, take, chords } = setup();
    expect(kb.handle(ev('Backquote', { ctrl: true, alt: true, shift: true }), true, true)).toBe('consume');
    expect(kb.handle(ev('Backquote'), false, true)).toBe('consume');
    expect(chords).toEqual(['terminal']);
    expect(take()).toEqual([]);
  });

  it('a new press after a lost keyup is forwarded again', () => {
    const { kb, take } = setup();
    kb.handle(ev('KeyD', { ctrl: true, alt: true, shift: true }), true, true);
    kb.handle(ev('KeyD'), true, true);
    kb.handle(ev('KeyD'), false, true);
    expect(take()).toEqual([down(HID.KeyD), up(HID.KeyD)]);
  });
});

describe('Caps Lock sync', () => {
  it('never forwards CapsLock as such; taps it when the local lock differs from the remote one', () => {
    const { kb, take } = setup();
    // Chrome on macOS: one event per press, keydown when the lock turns on, keyup when it turns off.
    kb.handle(ev('CapsLock', { caps: true }), true, true);
    expect(take()).toEqual(capsTap);
    kb.handle(ev('CapsLock', { caps: false }), false, true);
    expect(take()).toEqual(capsTap);
    kb.handle(ev('CapsLock', { caps: true }), true, true);
    expect(take()).toEqual(capsTap);
  });

  it('handles a Windows press whose keydown still reports the old state', () => {
    const { kb, take } = setup({ windows: true });
    kb.handle(ev('CapsLock', { caps: false }), true, true);
    expect(take()).toEqual([]);
    kb.handle(ev('CapsLock', { caps: true }), false, true);
    expect(take()).toEqual(capsTap);
    // Same press reported post-toggle on both events: one tap only.
    kb.handle(ev('CapsLock', { caps: false }), true, true);
    kb.handle(ev('CapsLock', { caps: false }), false, true);
    expect(take()).toEqual(capsTap);
  });

  it('syncs before any forwarded key (lock toggled while focus was elsewhere)', () => {
    const { kb, take } = setup();
    kb.handle(ev('KeyA', { caps: true }), true, true);
    kb.handle(ev('KeyA', { caps: true }), false, true);
    expect(take()).toEqual([...capsTap, down(HID.KeyA), up(HID.KeyA)]);
  });

  it('re-syncs after forget() (RELEASE_ALL / new session clears the remote lock)', () => {
    const { kb, take } = setup();
    kb.syncCaps(true);
    expect(take()).toEqual(capsTap);
    kb.syncCaps(true);
    expect(take()).toEqual([]);
    kb.forget();
    kb.handle(ev('KeyA', { caps: true }), true, true);
    expect(take()).toEqual([...capsTap, down(HID.KeyA)]);
  });

  it('does not count a tap that could not be sent', () => {
    const { kb, take, setConnected } = setup();
    setConnected(false);
    kb.syncCaps(true);
    setConnected(true);
    kb.syncCaps(true);
    expect(take()).toEqual(capsTap);
  });
});

describe('Windows AltGr filter', () => {
  beforeEach(() => void vi.useFakeTimers());
  afterEach(() => void vi.useRealTimers());

  const altGr = (t: number, extra: EvOpts = {}) =>
    ev('AltRight', { key: 'AltGraph', t, ctrl: true, alt: true, altGraph: true, ...extra });

  it('drops the fake Ctrl in front of AltGr (and its keyup), so Right Alt arrives as Option', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    expect(kb.handle(ev('ControlLeft', { t: 1000, ctrl: true }), true, true)).toBe('consume');
    expect(kb.handle(altGr(1000.4), true, true)).toBe('consume');
    kb.handle(ev('KeyE', { t: 1100, ctrl: true, alt: true, altGraph: true }), true, true);
    kb.handle(ev('KeyE', { t: 1150, ctrl: true, alt: true, altGraph: true }), false, true);
    expect(kb.handle(ev('ControlLeft', { t: 1200, alt: true }), false, true)).toBe('consume');
    kb.handle(ev('AltRight', { key: 'AltGraph', t: 1200 }), false, true);
    vi.advanceTimersByTime(1000);
    expect(take()).toEqual([down(HID.AltRight), down(HID.KeyE), up(HID.KeyE), up(HID.AltRight)]);
  });

  it('handles AltGr autorepeat (fake Ctrl + AltRight repeated)', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    kb.handle(altGr(0), true, true);
    kb.handle(ev('ControlLeft', { t: 500, ctrl: true, alt: true, repeat: true }), true, true);
    kb.handle(altGr(500, { repeat: true }), true, true);
    kb.handle(ev('ControlLeft', { t: 600, alt: true }), false, true);
    kb.handle(ev('AltRight', { t: 600 }), false, true);
    vi.advanceTimersByTime(1000);
    expect(take()).toEqual([down(HID.AltRight), `repeat 0x${HID.AltRight.toString(16)}`, up(HID.AltRight)]);
  });

  it('sends a real Ctrl before the next key (Ctrl+C → Cmd+C without delay)', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    expect(take()).toEqual([]);
    kb.handle(ev('KeyA', { t: 30, ctrl: true }), true, true);
    expect(take()).toEqual([down(HID.MetaLeft), down(HID.KeyA)]);
  });

  it('sends a lone Ctrl after the hold time, and a quick Ctrl tap as down+up', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    vi.advanceTimersByTime(ALTGR_HOLD_MS);
    expect(take()).toEqual([down(HID.MetaLeft)]);
    kb.handle(ev('ControlLeft', { t: 400 }), false, true);
    expect(take()).toEqual([up(HID.MetaLeft)]);

    kb.handle(ev('ControlLeft', { t: 1000, ctrl: true }), true, true);
    kb.handle(ev('ControlLeft', { t: 1020 }), false, true);
    expect(take()).toEqual([down(HID.MetaLeft), up(HID.MetaLeft)]);
  });

  it('flushes a held Ctrl on demand (before a mouse button: Ctrl+click)', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    kb.flushPending();
    expect(take()).toEqual([down(HID.MetaLeft)]);
    vi.advanceTimersByTime(1000);
    expect(take()).toEqual([]);
  });

  it('keeps Ctrl when Right Alt is not AltGr (US layout) or comes too late', () => {
    const plain = setup({ windows: true }, 'passthrough');
    plain.kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    plain.kb.handle(ev('AltRight', { key: 'Alt', t: 1, ctrl: true, alt: true }), true, true);
    expect(plain.take()).toEqual([down(HID.ControlLeft), down(HID.AltRight)]);

    const late = setup({ windows: true }, 'passthrough');
    late.kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    late.kb.handle(altGr(80), true, true);
    expect(late.take()).toEqual([down(HID.ControlLeft), down(HID.AltRight)]);
  });

  it('does not treat AltGr+Shift+D as the disconnect chord', () => {
    const { kb, take, chords } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    kb.handle(altGr(0), true, true);
    kb.handle(ev('ShiftLeft', { t: 50, ctrl: true, alt: true, shift: true, altGraph: true }), true, true);
    kb.handle(ev('KeyD', { t: 90, ctrl: true, alt: true, shift: true, altGraph: true }), true, true);
    expect(chords).toEqual([]);
    expect(take()).toEqual([down(HID.AltRight), down(HID.ShiftLeft), down(HID.KeyD)]);
  });

  it('keeps the Ctrl+Alt+Shift chords working on AltGr layouts (left Ctrl+Alt also reads as AltGraph)', () => {
    const { kb, chords } = setup({ windows: true }, 'windows-to-mac');
    const all = { ctrl: true, alt: true, shift: true, altGraph: true };
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    kb.handle(ev('AltLeft', { t: 40, ctrl: true, alt: true, altGraph: true }), true, true);
    kb.handle(ev('ShiftLeft', { t: 80, ...all }), true, true);
    kb.handle(ev('KeyD', { t: 120, ...all }), true, true);
    expect(chords).toEqual(['disconnect']);
  });

  it('drops a held Ctrl on forget()', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    kb.forget();
    vi.advanceTimersByTime(1000);
    expect(take()).toEqual([]);
  });

  it('is off on other platforms', () => {
    const { kb, take } = setup({ mac: true }, 'passthrough');
    kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
    expect(take()).toEqual([down(HID.ControlLeft)]);
  });
});

describe('Shift release', () => {
  it('Windows: a Shift keyup releases both Shifts (no keyup comes for the one released first)', () => {
    const { kb, take } = setup({ windows: true });
    kb.handle(ev('ShiftLeft', { shift: true }), true, true);
    kb.handle(ev('ShiftRight', { shift: true }), true, true);
    // ShiftLeft released first: Windows sends nothing. Then ShiftRight:
    kb.handle(ev('ShiftRight'), false, true);
    expect(take()).toEqual([down(HID.ShiftLeft), down(HID.ShiftRight), up(HID.ShiftRight), up(HID.ShiftLeft)]);
  });

  it('releases a Shift the OS no longer reports as held', () => {
    const { kb, take } = setup();
    kb.handle(ev('ShiftLeft', { shift: true }), true, true);
    kb.handle(ev('KeyA', { shift: true }), true, true);
    kb.handle(ev('KeyA', { shift: false }), false, true);
    expect(take()).toEqual([down(HID.ShiftLeft), down(HID.KeyA), up(HID.ShiftLeft), up(HID.KeyA)]);
  });
});

describe('macOS viewer', () => {
  it('releases keys whose keyup Chrome swallowed while Cmd was held', () => {
    const { kb, take } = setup({ mac: true });
    kb.handle(ev('MetaLeft'), true, true);
    kb.handle(ev('KeyA'), true, true);
    kb.handle(ev('MetaLeft'), false, true);
    expect(take()).toEqual([down(HID.MetaLeft), down(HID.KeyA), up(HID.MetaLeft), up(HID.KeyA)]);
  });
});

describe('Cmd+Tab → Alt+Tab (mac-to-windows)', () => {
  it('sends Cmd+C as Ctrl+C and Ctrl as the Windows key', () => {
    const { kb, take } = setup({ mac: true }, 'mac-to-windows');
    kb.handle(ev('MetaLeft'), true, true);
    kb.handle(ev('KeyC'), true, true);
    kb.handle(ev('KeyC'), false, true);
    kb.handle(ev('MetaLeft'), false, true);
    kb.handle(ev('ControlLeft', { ctrl: true }), true, true);
    kb.handle(ev('ControlLeft'), false, true);
    expect(take()).toEqual([
      down(HID.ControlLeft),
      down(HID.KeyC),
      up(HID.KeyC),
      up(HID.ControlLeft),
      down(HID.MetaLeft),
      up(HID.MetaLeft),
    ]);
  });

  it('turns the held left Cmd (as Ctrl) into Alt at the first Tab, for the rest of the hold', () => {
    const { kb, take } = setup({ mac: true }, 'mac-to-windows');
    kb.handle(ev('MetaLeft'), true, true);
    kb.handle(ev('Tab'), true, true);
    kb.handle(ev('Tab'), false, true);
    kb.handle(ev('Tab'), true, true);
    kb.handle(ev('MetaLeft'), false, true);
    kb.handle(ev('Tab'), false, true);
    expect(take()).toEqual([
      down(HID.ControlLeft),
      up(HID.ControlLeft),
      down(HID.AltLeft),
      down(HID.Tab),
      up(HID.Tab),
      down(HID.Tab),
      up(HID.AltLeft),
      up(HID.Tab),
    ]);
    // The next Cmd is Ctrl again.
    kb.handle(ev('MetaLeft'), true, true);
    expect(take()).toEqual([down(HID.ControlLeft)]);
  });

  it('leaves Option+Cmd+Tab (Alt already held) alone', () => {
    const { kb, take } = setup({ mac: true }, 'mac-to-windows');
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('MetaLeft', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), true, true);
    expect(take()).toEqual([down(HID.AltLeft), down(HID.ControlLeft), down(HID.Tab)]);
  });
});

describe('Alt+Tab → Cmd+Tab (windows-to-mac)', () => {
  const rep = (hid: number) => `repeat 0x${hid.toString(16)}`;
  const altTab = [down(HID.AltLeft), up(HID.AltLeft), down(HID.MetaLeft), down(HID.Tab)];

  it('turns the held left Alt into Cmd at the first Tab, for the rest of the hold', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    expect(take()).toEqual([down(HID.AltLeft)]);
    kb.handle(ev('Tab', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), false, true);
    expect(take()).toEqual([up(HID.AltLeft), down(HID.MetaLeft), down(HID.Tab), up(HID.Tab)]);
    // Further Tabs (and Alt autorepeat) while Alt stays down: Cmd is already held.
    kb.handle(ev('AltLeft', { alt: true, repeat: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true, repeat: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), false, true);
    kb.handle(ev('AltLeft'), false, true);
    expect(take()).toEqual([rep(HID.MetaLeft), down(HID.Tab), rep(HID.Tab), up(HID.Tab), up(HID.MetaLeft)]);
    // The next Alt is Option again.
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('KeyE', { alt: true }), true, true);
    expect(take()).toEqual([down(HID.AltLeft), down(HID.KeyE)]);
  });

  it('Shift+Alt+Tab → Shift+Cmd+Tab, whichever modifier came first', () => {
    const a = setup({ windows: true }, 'windows-to-mac');
    a.kb.handle(ev('AltLeft', { alt: true }), true, true);
    a.kb.handle(ev('ShiftLeft', { alt: true, shift: true }), true, true);
    a.kb.handle(ev('Tab', { alt: true, shift: true }), true, true);
    expect(a.take()).toEqual([
      down(HID.AltLeft),
      down(HID.ShiftLeft),
      up(HID.AltLeft),
      down(HID.MetaLeft),
      down(HID.Tab),
    ]);

    const b = setup({ windows: true }, 'windows-to-mac');
    b.kb.handle(ev('ShiftLeft', { shift: true }), true, true);
    b.kb.handle(ev('AltLeft', { alt: true, shift: true }), true, true);
    b.kb.handle(ev('Tab', { alt: true, shift: true }), true, true);
    b.kb.handle(ev('Tab', { alt: true, shift: true }), false, true);
    b.kb.handle(ev('ShiftLeft', { alt: true }), false, true);
    b.kb.handle(ev('Tab', { alt: true }), true, true);
    b.kb.handle(ev('AltLeft'), false, true);
    expect(b.take()).toEqual([
      down(HID.ShiftLeft),
      down(HID.AltLeft),
      up(HID.AltLeft),
      down(HID.MetaLeft),
      down(HID.Tab),
      up(HID.Tab),
      up(HID.ShiftLeft),
      down(HID.Tab),
      up(HID.MetaLeft),
    ]);
  });

  it('leaves passthrough, Right Alt, a plain Tab and Ctrl+Alt+Tab (Cmd already held) alone', () => {
    const pass = setup({ windows: true }, 'passthrough');
    pass.kb.handle(ev('AltLeft', { alt: true }), true, true);
    pass.kb.handle(ev('Tab', { alt: true }), true, true);
    pass.kb.handle(ev('AltLeft'), false, true);
    expect(pass.take()).toEqual([down(HID.AltLeft), down(HID.Tab), up(HID.AltLeft)]);

    const right = setup({ windows: true }, 'windows-to-mac');
    right.kb.handle(ev('AltRight', { alt: true }), true, true);
    right.kb.handle(ev('Tab', { alt: true }), true, true);
    right.kb.handle(ev('AltRight'), false, true);
    expect(right.take()).toEqual([down(HID.AltRight), down(HID.Tab), up(HID.AltRight)]);

    const plain = setup({ windows: true }, 'windows-to-mac');
    plain.kb.handle(ev('Tab'), true, true);
    plain.kb.handle(ev('AltLeft', { alt: true }), true, true);
    plain.kb.handle(ev('Tab', { alt: true }), false, true);
    expect(plain.take()).toEqual([down(HID.Tab), down(HID.AltLeft), up(HID.Tab)]);

    const ctrl = setup({ mac: false }, 'windows-to-mac');
    ctrl.kb.handle(ev('ControlLeft', { ctrl: true }), true, true);
    ctrl.kb.handle(ev('AltLeft', { ctrl: true, alt: true }), true, true);
    ctrl.kb.handle(ev('Tab', { ctrl: true, alt: true }), true, true);
    ctrl.kb.handle(ev('AltLeft', { ctrl: true }), false, true);
    expect(ctrl.take()).toEqual([down(HID.MetaLeft), down(HID.AltLeft), down(HID.Tab), up(HID.AltLeft)]);
  });

  it('releases Cmd (not Option) even if the mapping changes during the hold', () => {
    let mapping: ModifierMapping = 'windows-to-mac';
    const wire: string[] = [];
    const kb = new KeyboardForwarder(
      {
        send: (b) => {
          const m = parse(b);
          if (m.type === 'key') wire.push(`${m.down ? 'down' : 'up'} 0x${m.hid.toString(16)}`);
          return true;
        },
        chord: () => {},
        mapping: () => mapping,
      },
      { windows: true, mac: false },
    );
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), true, true);
    mapping = 'passthrough';
    kb.handle(ev('AltLeft'), false, true);
    expect(wire).toEqual([...altTab, up(HID.MetaLeft)]);
  });

  it('RELEASE_ALL / blur (forget) ends the switch: the stale Alt keyup sends nothing, the next Alt+Tab works', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), true, true);
    expect(take()).toEqual(altTab);
    kb.forget(); // the agent released Cmd and Tab
    expect(kb.handle(ev('Tab', { alt: true }), false, true)).toBe('consume');
    expect(kb.handle(ev('AltLeft'), false, true)).toBe('consume');
    expect(take()).toEqual([]);
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true }), true, true);
    kb.handle(ev('AltLeft'), false, true);
    expect(take()).toEqual([...altTab, up(HID.MetaLeft)]);
  });

  it('syncs Caps Lock first, also after forget() cleared the remote lock', () => {
    const { kb, take } = setup({ windows: true }, 'windows-to-mac');
    kb.handle(ev('AltLeft', { alt: true }), true, true);
    kb.handle(ev('Tab', { alt: true, caps: true }), true, true);
    expect(take()).toEqual([down(HID.AltLeft), ...capsTap, up(HID.AltLeft), down(HID.MetaLeft), down(HID.Tab)]);
    kb.forget();
    kb.handle(ev('AltLeft', { alt: true, caps: true }), true, true);
    kb.handle(ev('Tab', { alt: true, caps: true }), true, true);
    kb.handle(ev('AltLeft', { caps: true }), false, true);
    expect(take()).toEqual([...capsTap, ...altTab, up(HID.MetaLeft)]);
  });

  it('does not disturb the AltGr filter or the chords', () => {
    vi.useFakeTimers();
    try {
      const { kb, take, chords } = setup({ windows: true }, 'windows-to-mac');
      // AltGr (fake Ctrl + Right Alt) + Tab: Right Alt stays Option, the fake Ctrl never goes out.
      kb.handle(ev('ControlLeft', { t: 0, ctrl: true }), true, true);
      kb.handle(ev('AltRight', { key: 'AltGraph', t: 1, ctrl: true, alt: true, altGraph: true }), true, true);
      kb.handle(ev('Tab', { t: 20, ctrl: true, alt: true, altGraph: true }), true, true);
      expect(take()).toEqual([down(HID.AltRight), down(HID.Tab)]);
      kb.forget();
      // Ctrl+Alt+Shift+T still runs the latency test.
      const all = { ctrl: true, alt: true, shift: true };
      kb.handle(ev('ControlLeft', { t: 100, ctrl: true }), true, true);
      kb.handle(ev('AltLeft', { t: 300, ctrl: true, alt: true }), true, true);
      kb.handle(ev('ShiftLeft', { t: 320, ...all }), true, true);
      kb.handle(ev('KeyT', { t: 340, ...all }), true, true);
      expect(chords).toEqual(['latency-test']);
    } finally {
      vi.useRealTimers();
    }
  });
});
