import { describe, expect, it } from 'vitest';
import { chordFor } from './chords';
import { defaultMapping, isModifierCode, mapCode } from './modifiers';

describe('mapCode', () => {
  it('swaps Ctrl and Win/Meta for a Windows keyboard driving a Mac', () => {
    expect(mapCode('ControlLeft', 'windows-to-mac')).toBe('MetaLeft');
    expect(mapCode('ControlRight', 'windows-to-mac')).toBe('MetaRight');
    expect(mapCode('MetaLeft', 'windows-to-mac')).toBe('ControlLeft');
    expect(mapCode('MetaRight', 'windows-to-mac')).toBe('ControlRight');
  });

  it('keeps Alt (= Option) and ordinary keys', () => {
    for (const code of ['AltLeft', 'AltRight', 'KeyA', 'ShiftLeft', 'F13']) {
      expect(mapCode(code, 'windows-to-mac')).toBe(code);
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

describe('defaultMapping', () => {
  it('uses windows-to-mac on Windows', () => {
    expect(defaultMapping({ userAgentData: { platform: 'Windows' } })).toBe('windows-to-mac');
    expect(defaultMapping({ platform: 'Win32' })).toBe('windows-to-mac');
  });
  it('passes through elsewhere', () => {
    expect(defaultMapping({ userAgentData: { platform: 'macOS' }, platform: 'MacIntel' })).toBe('passthrough');
    expect(defaultMapping({ platform: 'Linux x86_64' })).toBe('passthrough');
    expect(defaultMapping({})).toBe('passthrough');
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
  });
  it('ignores other keys and incomplete chords', () => {
    expect(chordFor({ ...base, code: 'KeyA' })).toBeNull();
    expect(chordFor({ ...base, shiftKey: false, code: 'KeyF' })).toBeNull();
    expect(chordFor({ ctrlKey: true, altKey: false, shiftKey: true, code: 'KeyL' })).toBeNull();
  });
});
