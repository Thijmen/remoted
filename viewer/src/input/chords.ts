/** Viewer shortcuts on Ctrl+Alt+Shift+<key>; never forwarded to the remote machine. */
export type ChordAction = 'fullscreen' | 'pointer-lock' | 'hud' | 'disconnect' | 'latency-test' | 'next-display';

const CHORDS: Readonly<Record<string, ChordAction>> = {
  KeyF: 'fullscreen',
  KeyL: 'pointer-lock',
  KeyH: 'hud',
  KeyD: 'disconnect',
  KeyT: 'latency-test',
  KeyM: 'next-display',
};

export const CHORD_LABEL = 'Ctrl+Alt+Shift';

export interface ChordKeyEvent {
  code: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

/** Matched by physical key (`code`), so it works on every layout. */
export function chordFor(e: ChordKeyEvent): ChordAction | null {
  if (!(e.ctrlKey && e.altKey && e.shiftKey)) return null;
  return CHORDS[e.code] ?? null;
}
