/**
 * Terminal keys for the on-screen key bar. A phone keyboard has no Ctrl, Esc, Tab, arrows or
 * function keys, so the bar sends the xterm control sequences straight to the shell. Sticky
 * Ctrl/Alt apply to the next character typed on the phone's keyboard (Ctrl+C, Alt+B).
 */

/** Which sticky modifiers the terminal is holding for the next typed character. */
export interface TerminalMods {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export const NO_MODS: TerminalMods = { ctrl: false, alt: false, shift: false };

/** The bytes a key bar key sends to the shell (xterm's own sequences). */
const SEQUENCES: Readonly<Record<string, string>> = {
  Escape: '\x1b',
  Tab: '\t',
  Backspace: '\x7f',
  Enter: '\r',
  ArrowLeft: '\x1b[D',
  ArrowRight: '\x1b[C',
  ArrowUp: '\x1b[A',
  ArrowDown: '\x1b[B',
  Home: '\x1b[H',
  End: '\x1b[F',
  PageUp: '\x1b[5~',
  PageDown: '\x1b[6~',
  Insert: '\x1b[2~',
  Delete: '\x1b[3~',
  F1: '\x1bOP',
  F2: '\x1bOQ',
  F3: '\x1bOR',
  F4: '\x1bOS',
  F5: '\x1b[15~',
  F6: '\x1b[17~',
  F7: '\x1b[18~',
  F8: '\x1b[19~',
  F9: '\x1b[20~',
  F10: '\x1b[21~',
  F11: '\x1b[23~',
  F12: '\x1b[24~',
};

/** The sequence for a `KeyboardEvent.code`, or null for a key the terminal has no use for. */
export function terminalKey(code: string): string | null {
  return SEQUENCES[code] ?? null;
}

/** Ctrl turns a character into its control code (`c` -> 0x03); Alt prefixes ESC. */
export function applyMods(data: string, mods: TerminalMods): string {
  let out = data;
  if (mods.ctrl) {
    // Ctrl+? is DEL, not 0x1f (the mask alone would give the latter).
    out = out.replace(/[a-zA-Z@[\\\]^_?]/g, (c) =>
      c === '?' ? '\x7f' : String.fromCharCode(c.toUpperCase().charCodeAt(0) & 0x1f),
    );
  }
  if (mods.alt) out = `\x1b${out}`;
  return out;
}

/** Modifier keys on the key bar, by their `KeyboardEvent.code`. */
export function modForCode(code: string): keyof TerminalMods | null {
  switch (code) {
    case 'ControlLeft':
    case 'ControlRight':
      return 'ctrl';
    case 'AltLeft':
    case 'AltRight':
      return 'alt';
    case 'ShiftLeft':
    case 'ShiftRight':
      return 'shift';
    default:
      return null;
  }
}
