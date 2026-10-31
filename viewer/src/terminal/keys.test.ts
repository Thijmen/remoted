import { describe, expect, it } from 'vitest';
import { NO_MODS, applyMods, modForCode, terminalKey } from './keys';

describe('terminalKey', () => {
  it('sends the sequences a phone keyboard cannot produce', () => {
    expect(terminalKey('Escape')).toBe('\x1b');
    expect(terminalKey('Tab')).toBe('\t');
    expect(terminalKey('Backspace')).toBe('\x7f');
    expect(terminalKey('Enter')).toBe('\r');
    expect(terminalKey('ArrowUp')).toBe('\x1b[A');
    expect(terminalKey('ArrowLeft')).toBe('\x1b[D');
    expect(terminalKey('Home')).toBe('\x1b[H');
    expect(terminalKey('PageDown')).toBe('\x1b[6~');
    expect(terminalKey('Delete')).toBe('\x1b[3~');
    expect(terminalKey('F1')).toBe('\x1bOP');
    expect(terminalKey('F12')).toBe('\x1b[24~');
  });

  it('has nothing for a key the shell does not need', () => {
    expect(terminalKey('KeyA')).toBeNull();
    expect(terminalKey('CapsLock')).toBeNull();
  });
});

describe('applyMods', () => {
  it('turns Ctrl+letter into its control code', () => {
    expect(applyMods('c', { ...NO_MODS, ctrl: true })).toBe('\x03');
    expect(applyMods('C', { ...NO_MODS, ctrl: true })).toBe('\x03');
    expect(applyMods('[', { ...NO_MODS, ctrl: true })).toBe('\x1b');
    expect(applyMods('?', { ...NO_MODS, ctrl: true })).toBe('\x7f');
  });

  it('prefixes Alt with ESC', () => {
    expect(applyMods('b', { ...NO_MODS, alt: true })).toBe('\x1bb');
    expect(applyMods('b', { ...NO_MODS, ctrl: true, alt: true })).toBe('\x1b\x02');
  });

  it('leaves text alone without modifiers', () => {
    expect(applyMods('x', NO_MODS)).toBe('x');
    expect(applyMods('é', NO_MODS)).toBe('é');
  });
});

describe('modForCode', () => {
  it('names the key bar modifiers', () => {
    expect(modForCode('ControlLeft')).toBe('ctrl');
    expect(modForCode('AltRight')).toBe('alt');
    expect(modForCode('ShiftLeft')).toBe('shift');
    expect(modForCode('KeyA')).toBeNull();
  });
});
