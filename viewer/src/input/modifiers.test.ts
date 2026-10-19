import { describe, expect, it } from 'vitest';
import { chordFor } from './chords';
import { isModifierCode, mapCode, resolveMapping } from './modifiers';

describe('mapCode', () => {
  it('swaps Ctrl and Win/Meta for a Windows keyboard driving a Mac', () => {
    expect(mapCode('ControlLeft', 'windows-to-mac')).toBe('MetaLeft');
    expect(mapCode('ControlRight', 'windows-to-mac')).toBe('MetaRight');
    expect(mapCode('MetaLeft', 'windows-to-mac')).toBe('ControlLeft');
    expect(mapCode('MetaRight', 'windows-to-mac')).toBe('ControlRight');
  });

  it('swaps Cmd and Ctrl for a Mac keyboard driving Windows', () => {
    expect(mapCode('MetaLeft', 'mac-to-windows')).toBe('ControlLeft');
    expect(mapCode('MetaRight', 'mac-to-windows')).toBe('ControlRight');
    expect(mapCode('ControlLeft', 'mac-to-windows')).toBe('MetaLeft');
    expect(mapCode('ControlRight', 'mac-to-windows')).toBe('MetaRight');
  });

  it('keeps Alt (= Option) and ordinary keys', () => {
    for (const code of ['AltLeft', 'AltRight', 'KeyA', 'ShiftLeft', 'F13']) {
      expect(mapCode(code, 'windows-to-mac')).toBe(code);
      expect(mapCode(code, 'mac-to-windows')).toBe(code);
      expect(mapCode(code, 'passthrough')).toBe(code);
    }
  });

  it('passes modifiers through unchanged in passthrough mode', () => {
    expect(mapCode('ControlLeft', 'passthrough')).toBe('ControlLeft');
    expect(mapCode('MetaLeft', 'passthrough')).toBe('MetaLeft');
  });

  it('normalises legacy OSLeft/OSRight', () => {
    expect(mapCode('OSLeft', 'passthrough')).toBe('MetaLeft');
    expect(mapCode('OSRight', 'windows-to-mac')).toBe('ControlRight');
  });
});

describe('resolveMapping', () => {
  const win = { userAgentData: { platform: 'Windows' } };
  const mac = { userAgentData: { platform: 'macOS' }, platform: 'MacIntel' };
  it('auto: maps a keyboard driving the other OS', () => {
    expect(resolveMapping('auto', win, 'macos')).toBe('windows-to-mac');
    expect(resolveMapping('auto', { platform: 'Win32' }, 'macos')).toBe('windows-to-mac');
    expect(resolveMapping('auto', mac, 'windows')).toBe('mac-to-windows');
    expect(resolveMapping('auto', { platform: 'MacIntel' }, 'windows')).toBe('mac-to-windows');
  });
  it('auto: passes through on the same OS, elsewhere and before the agent is known', () => {
    expect(resolveMapping('auto', win, 'windows')).toBe('passthrough');
    expect(resolveMapping('auto', mac, 'macos')).toBe('passthrough');
    expect(resolveMapping('auto', { platform: 'Linux x86_64' }, 'windows')).toBe('passthrough');
    expect(resolveMapping('auto', win, undefined)).toBe('passthrough');
  });
  it('an explicit mapping wins', () => {
    expect(resolveMapping('passthrough', win, 'macos')).toBe('passthrough');
    expect(resolveMapping('mac-to-windows', win, 'macos')).toBe('mac-to-windows');
  });
});

describe('isModifierCode', () => {
  it('knows modifiers', () => {
    expect(isModifierCode('ShiftRight')).toBe(true);
    expect(isModifierCode('KeyC')).toBe(false);
  });
});

describe('chordFor', () => {
  const base = { ctrlKey: true, altKey: true, shiftKey: true };
  it('matches the reserved chords by physical key', () => {
    expect(chordFor({ ...base, code: 'KeyF' })).toBe('fullscreen');
    expect(chordFor({ ...base, code: 'KeyL' })).toBe('pointer-lock');
    expect(chordFor({ ...base, code: 'KeyH' })).toBe('hud');
    expect(chordFor({ ...base, code: 'KeyD' })).toBe('disconnect');
    expect(chordFor({ ...base, code: 'KeyT' })).toBe('latency-test');
    expect(chordFor({ ...base, code: 'KeyM' })).toBe('next-display');
  });
  it('ignores other keys and incomplete chords', () => {
    expect(chordFor({ ...base, code: 'KeyA' })).toBeNull();
    expect(chordFor({ ...base, shiftKey: false, code: 'KeyF' })).toBeNull();
    expect(chordFor({ ctrlKey: true, altKey: false, shiftKey: true, code: 'KeyL' })).toBeNull();
  });
});
