// Remote paths as the agent spells them (docs/files.md): absolute, with the agent's separator.

/** `dir` + `name`, without a doubled separator (`C:\` + `a` → `C:\a`, `/` + `a` → `/a`). */
export function joinPath(dir: string, name: string, sep: string): string {
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
}

/** The last component (`/a/b.txt` → `b.txt`; a root stays itself). */
export function baseName(path: string, sep: string): string {
  // Roots name themselves: `/`, `C:\`.
  if (path === sep || /^[A-Za-z]:\\?$/.test(path)) return path;
  const trimmed = path.endsWith(sep) ? path.slice(0, -1) : path;
  const i = trimmed.lastIndexOf(sep);
  return i < 0 ? trimmed : trimmed.slice(i + 1);
}

/** `report.pdf` → [`report`, `.pdf`]; dot files and folders have no extension. */
export function splitExt(name: string, isDir = false): [string, string] {
  const dot = name.lastIndexOf('.');
  if (isDir || dot <= 0) return [name, ''];
  return [name.slice(0, dot), name.slice(dot)];
}

/**
 * The name "keep both" gives an upload: the first of `name (2).ext`, `name (3).ext`, … that is not
 * taken. `taken` holds the folder's names; compare case-insensitively unless the remote file system
 * is case-sensitive (macOS and Windows are not, by default).
 */
export function keepBothName(name: string, taken: Iterable<string>, isDir = false, caseSensitive = false): string {
  const norm = (s: string) => (caseSensitive ? s : s.toLowerCase());
  const names = new Set([...taken].map(norm));
  if (!names.has(norm(name))) return name;
  const [stem, ext] = splitExt(name, isDir);
  for (let n = 2; ; n++) {
    const candidate = `${stem} (${n})${ext}`;
    if (!names.has(norm(candidate))) return candidate;
  }
}

/** Why `name` cannot be a file name on the agent's OS, or null when it can. */
export function invalidName(name: string, os: string): string | null {
  if (name.trim() === '') return 'Enter a name.';
  if (name === '.' || name === '..') return `“${name}” is not a valid name.`;
  if (name.includes('/')) return 'A name cannot contain “/”.';
  if (os === 'windows') {
    if (/[<>:"\\|?*\u0000-\u001f]/.test(name)) return 'A name cannot contain < > : " \\ | ? * on Windows.';
    if (/[. ]$/.test(name)) return 'A name cannot end with a dot or a space on Windows.';
    if (/^(con|prn|aux|nul|com\d|lpt\d)(\..*)?$/i.test(name)) return `“${name}” is reserved on Windows.`;
  } else if (name.includes('\u0000')) return 'A name cannot contain a NUL character.';
  return null;
}

/** Remote file systems compare names case-insensitively on macOS and Windows (the default setup). */
export function caseSensitive(os: string): boolean {
  return os !== 'macos' && os !== 'windows';
}
