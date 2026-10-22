// Where the terminal page lives (docs/terminal.md): `/t/{machine}`, served by the gateway as terminal.html.

/** On a "New terminal" link: start a new shell even though the new tab may get a copy of sessionStorage. */
export const NEW_TERMINAL_HASH = '#new';

/** The terminal page of `machine` (`fresh`: always a new shell). */
export function terminalUrl(machine: string, fresh = false): string {
  return `/t/${encodeURIComponent(machine)}${fresh ? NEW_TERMINAL_HASH : ''}`;
}

/** Machine name from `/t/{machine}` (or `?machine=` when opened as /terminal.html in dev). */
export function machineFromTerminalPath(loc: Pick<Location, 'pathname' | 'search'> = location): string | null {
  const m = /^\/t\/([^/]+)\/?$/.exec(loc.pathname);
  if (m) {
    try {
      return decodeURIComponent(m[1]!);
    } catch {
      return null;
    }
  }
  return new URLSearchParams(loc.search).get('machine');
}

/** Inside rd Desktop, which hands new windows (`target=_blank`, `window.open`) to the system browser. */
export function inDesktopApp(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}
