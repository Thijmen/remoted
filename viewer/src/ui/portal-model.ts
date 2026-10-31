// What the portal's device console shows, derived from `/api/machines`: filters, the summary
// tiles, row status and keyboard selection. Pure, so it can be tested.

import { type MachineInfo, updateInProgress } from '../proto/control';
import { compareVersions } from './agent-update';
import { terminalUrl } from './terminal-links';

export type Filter = 'all' | 'online' | 'offline' | 'update' | 'in-use';

export const FILTERS: readonly Filter[] = ['all', 'online', 'offline', 'update', 'in-use'];

export const FILTER_LABELS: Record<Filter, string> = {
  all: 'All',
  online: 'Online',
  offline: 'Offline',
  update: 'Needs update',
  'in-use': 'In use',
};

/** The agent reported a newer version. */
export function hasUpdate(m: MachineInfo): boolean {
  return m.update?.available != null;
}

export function matchesFilter(m: MachineInfo, f: Filter): boolean {
  switch (f) {
    case 'all':
      return true;
    case 'online':
      return m.online;
    case 'offline':
      return !m.online;
    case 'update':
      return hasUpdate(m);
    case 'in-use':
      return m.viewer !== null;
  }
}

export function filterCounts(ms: readonly MachineInfo[]): Record<Filter, number> {
  const out = { all: 0, online: 0, offline: 0, update: 0, 'in-use': 0 };
  for (const m of ms) for (const f of FILTERS) if (matchesFilter(m, f)) out[f]++;
  return out;
}

export interface VersionCount {
  /** Empty for agents that did not report one. */
  version: string;
  count: number;
}

export interface Summary {
  total: number;
  online: number;
  /** Machines with a viewer session, in listing order. */
  inUse: MachineInfo[];
  /** Machines whose agent reported a newer version. */
  updates: number;
  /** The newest agent version anyone runs or was offered; empty when none is known. */
  newest: string;
  /** Running agent versions, newest first; unknown last. */
  versions: VersionCount[];
}

export function summarize(ms: readonly MachineInfo[]): Summary {
  const counts = new Map<string, number>();
  let newest = '';
  const consider = (v: string) => {
    if (v && (!newest || compareVersions(v, newest) > 0)) newest = v;
  };
  for (const m of ms) {
    counts.set(m.agent_version, (counts.get(m.agent_version) ?? 0) + 1);
    consider(m.agent_version);
    consider(m.update?.available?.version ?? '');
  }
  const versions = [...counts].map(([version, count]) => ({ version, count }));
  versions.sort((a, b) => {
    if (!a.version || !b.version) return a.version ? -1 : b.version ? 1 : 0;
    return compareVersions(b.version, a.version);
  });
  return {
    total: ms.length,
    online: ms.filter((m) => m.online).length,
    inUse: ms.filter((m) => m.viewer !== null),
    updates: ms.filter(hasUpdate).length,
    newest,
    versions,
  };
}

/** Older agents report the bare platform; newer ones the full name ("macOS 26.3"). */
export function osLabel(os: string): string {
  return ({ macos: 'macOS', windows: 'Windows', linux: 'Linux' } as Record<string, string>)[os] ?? os;
}

export type OsFamily = 'mac' | 'windows' | 'linux' | 'other';

export function osFamily(os: string): OsFamily {
  if (/mac\s?os|darwin/i.test(os)) return 'mac';
  if (/windows/i.test(os)) return 'windows';
  if (/linux|ubuntu|debian|fedora|centos|rhel|red hat|alpine|arch|suse|mint/i.test(os)) return 'linux';
  return 'other';
}

/** "Windows 11 Pro 26H2 (26300.9711)" → "Windows 11 Pro 26H2": the build number is for the details panel. */
export function osShort(os: string): string {
  const label = osLabel(os);
  return label.replace(/\s*\([^)]*\)\s*$/, '').replace(/,\s*Linux\s+\S+$/i, '');
}

/** The part of an email before the @, for tight spots. */
export function shortUser(user: string): string {
  const at = user.indexOf('@');
  return at > 0 ? user.slice(0, at) : user;
}

export type StatusLevel = 'ok' | 'warn' | 'bad' | 'accent';

export interface StatusView {
  level: StatusLevel;
  label: string;
  /** Who is connected, for "In use". */
  user: string;
  /** Online since / last seen / session start (Unix ms), for "… ago"; null when unknown. */
  at: number | null;
}

export function statusView(m: MachineInfo): StatusView {
  if (updateInProgress(m.update)) return { level: 'accent', label: 'Updating', user: '', at: null };
  if (!m.online) return { level: 'bad', label: 'Offline', user: '', at: m.since_ms };
  if (m.viewer) return { level: 'warn', label: 'In use', user: m.viewer.user, at: m.viewer.since_ms };
  if (m.screen && m.state?.locked) return { level: 'warn', label: 'Locked', user: '', at: m.since_ms };
  return { level: 'ok', label: 'Online', user: '', at: m.since_ms };
}

export type Action = 'connect' | 'terminal' | 'files';

/** What the machine offers right now; nothing while it is offline. */
export function actions(m: MachineInfo): Action[] {
  if (!m.online) return [];
  const out: Action[] = [];
  if (m.screen) out.push('connect');
  if (m.terminal) out.push('terminal');
  if (m.files) out.push('files');
  return out;
}

/** Connect for a machine with a screen, else its terminal; null when offline. */
export function primaryAction(m: MachineInfo): Action | null {
  return actions(m)[0] ?? null;
}

export function actionUrl(m: MachineInfo, a: Action): string {
  const machine = encodeURIComponent(m.machine);
  switch (a) {
    case 'connect':
      return `/m/${machine}`;
    case 'terminal':
      return terminalUrl(m.machine);
    case 'files':
      return `/m/${machine}#files`;
  }
}

export const ACTION_LABELS: Record<Action, string> = { connect: 'Connect', terminal: 'Terminal', files: 'Files' };

/** The selection after an arrow key: `delta` rows on, clamped; the first or last row when none is selected. */
export function moveSelection(ids: readonly string[], current: string | null, delta: number): string | null {
  if (ids.length === 0) return null;
  const i = current === null ? -1 : ids.indexOf(current);
  if (i < 0) return delta < 0 ? ids[ids.length - 1]! : ids[0]!;
  return ids[Math.min(ids.length - 1, Math.max(0, i + delta))]!;
}

/** Every word of the query appears in the text (case-insensitive). */
export function matchesQuery(query: string, text: string): boolean {
  const hay = text.toLowerCase();
  return query
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => hay.includes(w));
}
