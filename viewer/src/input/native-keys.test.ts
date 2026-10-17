import { describe, expect, it } from 'vitest';
import { parse } from '../proto/bin';
import type { ChordAction } from './chords';
import { HID_CAPS_LOCK, type KeyInput, KeyboardForwarder } from './keyboard';
import type { ModifierMapping } from './modifiers';
import { NativeKeys } from './native-keys';

const HID = {
  KeyA: 0x00070004,
  KeyC: 0x00070006,
  KeyE: 0x00070008,
  Tab: 0x0007002b,
  ControlLeft: 0x000700e0,
  ShiftLeft: 0x000700e1,
  AltLeft: 0x000700e2,
  MetaLeft: 0x000700e3,
  ShiftRight: 0x000700e5,
  AltRight: 0x000700e6,
} as const;

const down = (hid: number) => `down 0x${hid.toString(16)}`;
const up = (hid: number) => `up 0x${hid.toString(16)}`;
const rep = (hid: number) => `repeat 0x${hid.toString(16)}`;
const capsTap = [down(HID_CAPS_LOCK), up(HID_CAPS_LOCK)];

/** Native keys through the forwarder the way the controller wires them (Windows viewer by default). */
function setup(mapping: ModifierMapping = 'windows-to-mac', windows = true) {
  const wire: string[] = [];
  const chords: ChordAction[] = [];
  let t = 0;
  let localCaps = false;
  const kb = new KeyboardForwarder(
    {
      send: (b) => {
        const m = parse(b);
        if (m.type !== 'key') throw new Error(`unexpected ${m.type}`);
        wire.push(`${m.down ? (m.repeat ? 'repeat' : 'down') : 'up'} 0x${m.hid.toString(16)}`);
        return true;
      },
      chord: (a) => chords.push(a),
      mapping: () => mapping,
    },
    { windows, mac: false },
  );
  const native = new NativeKeys({ held: (c) => kb.isPressed(c), localCaps: () => localCaps, now: () => t });
  /** rdNative.key(code, down, repeat) at time `at` (ms). */
  const key = (code: string, isDown: boolean, o: { repeat?: boolean; at?: number } = {}) => {
    if (o.at !== undefined) t = o.at;
    kb.handle(native.event(code, isDown, o.repeat ?? false), isDown, true);
  };
  return {
    kb,
    native,
    key,
    chords,
    take: () => wire.splice(0),
    setLocalCaps: (on: boolean) => (localCaps = on),
  };
}

describe('NativeKeys: the KeyInput a DOM event would carry', () => {
  it('derives modifier flags from the keys reported down', () => {
    const native = new NativeKeys({ held: () => false, localCaps: () => false, now: () => 42 });
    native.event('ControlLeft', true, false);
    native.event('ShiftRight', true, false);
    const e: KeyInput = native.event('KeyA', true, false);
    expect(e).toMatchObject({
      code: 'KeyA',
      ctrlKey: true,
      shiftKey: true,
      altKey: false,
      timeStamp: 42,
      native: true,
    });
    expect(e.getModifierState('Shift')).toBe(true);
    expect(e.getModifierState('Meta')).toBe(false);
    // The released key no longer counts.
    expect(native.event('ShiftRight', false, false).shiftKey).toBe(false);
  });

  it('counts keys held from before the grab (pressed through the DOM)', () => {
    const native = new NativeKeys({ held: (c) => c === 'AltLeft', localCaps: () => false, now: () => 0 });
    expect(native.event('Tab', true, false).altKey).toBe(true);
    expect(native.event('AltLeft', false, false).altKey).toBe(false);
  });

  it('marks a second press without a release as autorepeat', () => {
    const native = new NativeKeys({ held: () => false, localCaps: () => false, now: () => 0 });
    expect(native.event('KeyA', true, false).repeat).toBe(false);
    expect(native.event('KeyA', true, false).repeat).toBe(true);
    native.reset();
    expect(native.event('KeyA', true, false).repeat).toBe(false);
  });

  it('reports AltGraph for left Ctrl + right Alt only', () => {
    const native = new NativeKeys({ held: () => false, localCaps: () => false, now: () => 0 });
    native.event('ControlLeft', true, false);
    expect(native.event('AltRight', true, false).getModifierState('AltGraph')).toBe(true);
    native.reset();
    native.event('ControlLeft', true, false);
    expect(native.event('AltLeft', true, false).getModifierState('AltGraph')).toBe(false);
  });
});

describe('native keys through the forwarder', () => {
  it('maps modifiers like DOM keys (Ctrl+C → Cmd+C)', () => {
    const { key, take } = setup();
    key('ControlLeft', true, { at: 0 });
    key('KeyC', true, { at: 30 });
    key('KeyC', false, { at: 60 });
    key('ControlLeft', false, { at: 90 });
    expect(take()).toEqual([down(HID.MetaLeft), down(HID.KeyC), up(HID.KeyC), up(HID.MetaLeft)]);
  });

  it('runs the Ctrl+Alt+Shift chords and swallows the chord key', () => {
    const { key, take, chords } = setup();
    key('ControlLeft', true, { at: 0 });
    key('AltLeft', true, { at: 200 });
    key('ShiftLeft', true, { at: 220 });
    key('KeyD', true, { at: 240 });
    key('KeyD', true, { at: 700, repeat: true });
    key('KeyD', false, { at: 750 });
    expect(chords).toEqual(['disconnect']);
    expect(take()).toEqual([down(HID.MetaLeft), down(HID.AltLeft), down(HID.ShiftLeft)]);
  });

  it('drops the fake Ctrl Windows puts in front of AltGr', () => {
    const { key, take, chords } = setup();
    key('ControlLeft', true, { at: 1000 });
    key('AltRight', true, { at: 1001 });
    key('ShiftLeft', true, { at: 1050 });
    key('KeyD', true, { at: 1100 }); // AltGr+Shift+D types; it is not the disconnect chord
    key('KeyD', false, { at: 1150 });
    key('ShiftLeft', false, { at: 1160 });
    key('ControlLeft', false, { at: 1200 });
    key('AltRight', false, { at: 1200 });
    expect(chords).toEqual([]);
    expect(take()).toEqual([
      down(HID.AltRight),
      down(HID.ShiftLeft),
      down(0x00070007),
      up(0x00070007),
      up(HID.ShiftLeft),
      up(HID.AltRight),
    ]);
  });

  it('keeps a Ctrl that is not followed by AltGr in time', () => {
    const { key, take } = setup('passthrough');
    key('ControlLeft', true, { at: 0 });
    key('AltRight', true, { at: 80 });
    expect(take()).toEqual([down(HID.ControlLeft), down(HID.AltRight)]);
  });

  it('turns Alt+Tab into Cmd+Tab', () => {
    const { key, take } = setup();
    key('AltLeft', true);
    key('ShiftLeft', true);
    key('Tab', true);
    key('Tab', false);
    key('Tab', true);
    key('Tab', false);
    key('ShiftLeft', false);
    key('AltLeft', false);
    expect(take()).toEqual([
      down(HID.AltLeft),
      down(HID.ShiftLeft),
      up(HID.AltLeft),
      down(HID.MetaLeft),
      down(HID.Tab),
      up(HID.Tab),
      down(HID.Tab),
      up(HID.Tab),
      up(HID.ShiftLeft),
      up(HID.MetaLeft),
    ]);
  });

  it('releases only the Shift that went up (the hook reports both keyups)', () => {
    const { key, take } = setup();
    key('ShiftLeft', true);
    key('ShiftRight', true);
    key('ShiftLeft', false);
    key('KeyA', true);
    key('ShiftRight', false);
    expect(take()).toEqual([
      down(HID.ShiftLeft),
      down(HID.ShiftRight),
      up(HID.ShiftLeft),
      down(HID.KeyA),
      up(HID.ShiftRight),
    ]);
  });

  it('keeps a Shift pressed through the DOM before the grab', () => {
    const { kb, key, take } = setup();
    const domShift: KeyInput = {
      code: 'ShiftLeft',
      key: 'Shift',
      repeat: false,
      timeStamp: 0,
      ctrlKey: false,
      altKey: false,
      shiftKey: true,
      getModifierState: () => false,
    };
    kb.handle(domShift, true, true);
    key('KeyA', true);
    key('KeyA', false);
    key('ShiftLeft', false);
    expect(take()).toEqual([down(HID.ShiftLeft), down(HID.KeyA), up(HID.KeyA), up(HID.ShiftLeft)]);
  });

  it('autorepeat goes out as repeat', () => {
    const { key, take } = setup();
    key('KeyE', true);
    key('KeyE', true, { repeat: true });
    key('KeyE', false);
    expect(take()).toEqual([down(HID.KeyE), rep(HID.KeyE), up(HID.KeyE)]);
  });
});

describe('native Caps Lock', () => {
  it('toggles the remote lock per press (the hook keeps the local lock unchanged)', () => {
    const { key, take, native } = setup();
    key('CapsLock', true);
    expect(take()).toEqual(capsTap);
    key('CapsLock', true, { repeat: true });
    key('CapsLock', false);
    expect(take()).toEqual([]);
    expect(native.caps(false)).toBe(true); // what a click on the video syncs during the grab
    key('KeyA', true);
    expect(take()).toEqual([down(HID.KeyA)]);
    key('CapsLock', true);
    key('CapsLock', false);
    expect(take()).toEqual(capsTap);
    expect(native.caps(false)).toBe(false);
  });

  it('starts from the local lock', () => {
    const { key, take, setLocalCaps } = setup();
    setLocalCaps(true);
    key('KeyA', true);
    expect(take()).toEqual([...capsTap, down(HID.KeyA)]);
    key('CapsLock', true);
    expect(take()).toEqual(capsTap);
  });

  it('after reset (grab ended, release) and RELEASE_ALL the remote lock follows the local one again', () => {
    const { kb, key, take, native, setLocalCaps } = setup();
    key('CapsLock', true);
    key('CapsLock', false);
    expect(take()).toEqual(capsTap);
    // rdNative.release(): the controller resets the native state and sends RELEASE_ALL (forget).
    native.reset();
    kb.forget();
    expect(native.caps(false)).toBe(false);
    key('KeyA', true);
    expect(take()).toEqual([down(HID.KeyA)]);
    kb.forget();
    native.reset();
    setLocalCaps(true);
    key('KeyA', true);
    expect(take()).toEqual([...capsTap, down(HID.KeyA)]);
  });

  it('forget() also drops Alt+Tab state reached through native keys', () => {
    const { kb, key, take, native } = setup();
    key('AltLeft', true);
    key('Tab', true);
    expect(take()).toEqual([down(HID.AltLeft), up(HID.AltLeft), down(HID.MetaLeft), down(HID.Tab)]);
    native.reset();
    kb.forget();
    key('Tab', false);
    key('AltLeft', false);
    expect(take()).toEqual([]);
  });
});
