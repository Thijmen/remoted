import { type MachineInfo, type Me, parseMachines, parseMe } from '../proto/control';

export class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

async function getJson(path: string, signal?: AbortSignal): Promise<unknown> {
  const res = await fetch(path, { cache: 'no-store', credentials: 'same-origin', signal: signal ?? null });
  if (!res.ok) throw new HttpError(res.status, `${path}: HTTP ${res.status}`);
  return res.json();
}

export async function fetchMachines(signal?: AbortSignal): Promise<MachineInfo[]> {
  return parseMachines(await getJson('/api/machines', signal));
}

export async function fetchMachine(machine: string, signal?: AbortSignal): Promise<MachineInfo | null> {
  return (await fetchMachines(signal)).find((m) => m.machine === machine) ?? null;
}

export async function fetchMe(signal?: AbortSignal): Promise<Me | null> {
  return parseMe(await getJson('/api/me', signal));
}

/** Whether this browser is still signed in and allowed: `unknown` when the check itself failed. */
export type Access = 'ok' | 'denied' | 'unknown';

/**
 * Classify the answer to `fetch('/api/me', { redirect: 'manual' })`. Authentik forward auth redirects to
 * its sign-in page (an opaque redirect) or answers 401; the gateway answers 403 when the user is not allowed.
 */
export function accessFromResponse(res: { type: ResponseType; status: number }): Access {
  if (res.type === 'opaqueredirect' || res.status === 401 || res.status === 403) return 'denied';
  return res.status >= 200 && res.status < 300 ? 'ok' : 'unknown';
}

/**
 * A rejected WebSocket upgrade (sign-in redirect, 403) only shows up as close code 1006, exactly like a
 * network failure; the HTTP API tells them apart. Never throws.
 */
export async function probeAccess(signal?: AbortSignal): Promise<Access> {
  try {
    const res = await fetch('/api/me', {
      redirect: 'manual',
      cache: 'no-store',
      credentials: 'same-origin',
      signal: signal ?? null,
    });
    return accessFromResponse(res);
  } catch {
    return 'unknown'; // network error, timeout or abort
  }
}

/** ws(s):// URL for a path on this origin. */
export function wsUrl(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}${path}`;
}
