// What a portal device card shows about agent updates (docs/updates.md), from the agent's
// `update_status` in `/api/machines` and the update this page asked for. Pure, so it can be tested.

import { type AvailableUpdate, type MachineInfo, type UpdateStatus, updateInProgress } from '../proto/control';

/** An update click shows "Updating…" at most this long without an answer from the agent. */
export const PENDING_MAX_MS = 4 * 60_000;
/** An agent that has not started the update this long after the click did not take it up. */
export const PENDING_START_MS = 30_000;
/**
 * A machine that went offline while installing is shown as updating this long. The update helper
 * waits 2 minutes for the new version, then puts the old one back; after that it is just offline.
 */
export const INSTALL_OFFLINE_MS = 5 * 60_000;
/** A successful update is mentioned this long. */
export const UPDATED_SHOWN_MS = 24 * 3600_000;

/** An update this page asked for; kept across the portal's re-renders. */
export interface PendingUpdate {
  to: string;
  /** The agent's version at the click. */
  from: string;
  /** `Date.now()` at the click. */
  at: number;
  /** The status's `last` and `error` at the click: a change is the agent's answer. */
  last: string;
  error: string;
  /** The agent was seen working on it (downloading or installing, or gone to restart). */
  started: boolean;
}

export interface UpdateOutcomeView {
  level: 'muted' | 'warn' | 'bad';
  text: string;
  /** When it ended (Unix ms), for "… ago"; 0 when not shown. */
  at: number;
}

export interface UpdateCardView {
  /** Installing this version (may be empty): shown instead of the available update and its buttons. */
  updating: string | null;
  available: AvailableUpdate | null;
  canInstall: boolean;
  cannotInstall: string;
  outcome: UpdateOutcomeView | null;
  /** Why the last check or update request failed. */
  error: string;
}

/** Compare dotted versions numerically (`1.10.0` > `1.9.2`); pre-release tags are ignored. */
export function compareVersions(a: string, b: string): number {
  const parts = (v: string) =>
    v
      .trim()
      .replace(/^v/i, '')
      .split(/[-+]/, 1)[0]!
      .split('.')
      .map((p) => Number.parseInt(p, 10) || 0);
  const x = parts(a);
  const y = parts(b);
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d !== 0) return Math.sign(d);
  }
  return 0;
}

function outcomeKey(u: UpdateStatus | null): string {
  return JSON.stringify(u?.last ?? null);
}

export function pendingUpdate(m: MachineInfo, to: string, now: number): PendingUpdate {
  return {
    to,
    from: m.agent_version,
    at: now,
    last: outcomeKey(m.update),
    error: m.update?.error ?? '',
    started: false,
  };
}

/**
 * The pending update after a new listing of its machine, or null once the agent answered: a new
 * version, a new outcome or a new error, or back to idle after working on it. Also null when the
 * agent never started it, or nothing happened for too long.
 */
export function advancePending(p: PendingUpdate, m: MachineInfo, now: number): PendingUpdate | null {
  if (now - p.at > PENDING_MAX_MS) return null;
  const u = m.update;
  if (!m.online || updateInProgress(u)) return p.started ? p : { ...p, started: true };
  // Back after a restart, but the new connection has not reported yet.
  if (!u) return p;
  if (m.agent_version !== p.from || outcomeKey(u) !== p.last || (u.error !== '' && u.error !== p.error)) return null;
  if (p.started) return null;
  return now - p.at > PENDING_START_MS ? null : p;
}

function outcomeView(m: MachineInfo, u: UpdateStatus, now: number): UpdateOutcomeView | null {
  const last = u.last;
  if (!last) return null;
  const why = last.error ? `: ${last.error}` : '';
  const still = m.agent_version || last.from;
  switch (last.result) {
    case 'ok':
      return last.at_ms > 0 && now - last.at_ms < UPDATED_SHOWN_MS
        ? { level: 'muted', text: `Updated to ${last.to}`, at: last.at_ms }
        : null;
    case 'rolled_back':
    case 'failed':
      // Until a newer outcome replaces it, or the agent got there anyway (installed by hand).
      if (m.agent_version && compareVersions(m.agent_version, last.to) >= 0) return null;
      return last.result === 'failed'
        ? { level: 'bad', text: `Update to ${last.to} failed${why}; still on ${still}`, at: 0 }
        : { level: 'warn', text: `Update to ${last.to} was rolled back${why}; still on ${still}`, at: 0 };
    case 'unknown':
      return { level: 'muted', text: `Last update to ${last.to}${why || ': unknown result'}`, at: 0 };
  }
}

/** What the card shows; null when there is nothing to show (agents before 1.5.0). */
export function updateCardView(m: MachineInfo, pending: PendingUpdate | null, now: number): UpdateCardView | null {
  const u = m.update;
  if (!u && !pending) return null;
  const installing =
    updateInProgress(u) && (m.online || (m.since_ms !== null && now - m.since_ms < INSTALL_OFFLINE_MS))
      ? (u?.available?.version ?? '')
      : null;
  const updating = pending ? pending.to : installing;
  return {
    updating,
    available: updating === null ? (u?.available ?? null) : null,
    canInstall: u?.can_install ?? false,
    cannotInstall: u?.cannot_install ?? '',
    outcome: updating === null && u ? outcomeView(m, u, now) : null,
    error: updating === null ? (u?.error ?? '') : '',
  };
}
