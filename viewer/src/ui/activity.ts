// The portal's Recent activity list: gateway events (`/api/activity`) as short sentences. Pure,
// so it can be tested.

import type { ActivityEvent } from '../proto/control';
import { shortUser } from './portal-model';

export type ActivityLevel = 'ok' | 'warn' | 'bad' | 'accent' | 'muted';

/** Plain text, or a name shown in bold. */
export type Part = string | { strong: string };

export interface ActivityLine {
  level: ActivityLevel;
  parts: Part[];
  at_ms: number;
}

/** Machines that come online or go offline this close together share a line (a gateway restart). */
export const GROUP_MS = 60_000;

export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min`;
  const hours = Math.floor(m / 60);
  const rest = m % 60;
  return rest ? `${hours} h ${rest} min` : `${hours} h`;
}

/** "A", "A and B", "A, B and C", "A, B and 3 more". */
function names(list: string[]): Part[] {
  const [a = '', b = '', c = ''] = list;
  switch (list.length) {
    case 1:
      return [{ strong: a }];
    case 2:
      return [{ strong: a }, ' and ', { strong: b }];
    case 3:
      return [{ strong: a }, ', ', { strong: b }, ' and ', { strong: c }];
    default:
      return [{ strong: a }, ', ', { strong: b }, ` and ${list.length - 2} more`];
  }
}

function line(e: ActivityEvent, name: string): ActivityLine | null {
  const who: Part = { strong: e.user ? shortUser(e.user) : 'Someone' };
  const on: Part = { strong: name };
  const at = e.at_ms;
  switch (e.event) {
    case 'session_start':
      return { level: 'warn', at_ms: at, parts: [who, ' connected to ', on] };
    case 'session_end':
      return {
        level: 'muted',
        at_ms: at,
        parts: [who, ' left ', on, e.duration_ms !== null ? ` after ${formatDuration(e.duration_ms)}` : ''],
      };
    case 'terminal_open':
      return { level: 'muted', at_ms: at, parts: [who, ' opened a terminal on ', on] };
    case 'files_open':
      return { level: 'muted', at_ms: at, parts: [who, ' opened the files of ', on] };
    case 'agent_update':
      return { level: 'accent', at_ms: at, parts: [who, ' started updating ', on, e.to ? ` to ${e.to}` : ''] };
    case 'agent_updated':
      return {
        level: 'accent',
        at_ms: at,
        parts: [on, ' was updated', e.from ? ` from ${e.from}` : '', e.to ? ` to ${e.to}` : ''],
      };
    case 'device_add':
      return { level: 'ok', at_ms: at, parts: [who, ' added ', on] };
    case 'device_token':
      return { level: 'warn', at_ms: at, parts: [who, ' gave ', on, ' a new token'] };
    case 'device_remove':
      return { level: 'bad', at_ms: at, parts: [who, ' removed ', on] };
    default:
      return null;
  }
}

/** At most `max` lines for `events` (newest first); `nameOf` turns a machine id into its name. */
export function activityLines(
  events: readonly ActivityEvent[],
  nameOf: (machine: string) => string,
  max: number,
): ActivityLine[] {
  const out: ActivityLine[] = [];
  let i = 0;
  while (i < events.length && out.length < max) {
    const e = events[i]!;
    if (e.event === 'agent_online' || e.event === 'agent_offline') {
      const machines: string[] = [];
      let j = i;
      for (; j < events.length; j++) {
        const next = events[j]!;
        if (next.event !== e.event || e.at_ms - next.at_ms > GROUP_MS) break;
        const name = nameOf(next.machine);
        if (!machines.includes(name)) machines.push(name);
      }
      const online = e.event === 'agent_online';
      out.push({
        level: online ? 'ok' : 'bad',
        at_ms: e.at_ms,
        parts: [...names(machines), online ? ' came online' : ' went offline'],
      });
      i = j;
      continue;
    }
    const l = line(e, nameOf(e.machine));
    if (l) out.push(l);
    i++;
  }
  return out;
}

/** The text of a line, for tests and titles. */
export function lineText(l: ActivityLine): string {
  return l.parts.map((p) => (typeof p === 'string' ? p : p.strong)).join('');
}
