import {
  type DeviceIssued,
  type MachineInfo,
  type Me,
  parseDeviceIssued,
  parseMachines,
  parseMe,
} from '../proto/control';

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

/**
 * A JSON request to the devices API. Errors carry the gateway's own message (`{"error": "…"}`)
 * when it sent one.
 */
async function deviceRequest(method: 'POST' | 'DELETE', path: string, body?: unknown): Promise<unknown> {
  const res = await fetch(path, {
    method,
    cache: 'no-store',
    credentials: 'same-origin',
    // JSON on purpose: a cross-site page cannot send it without a preflight.
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? null : JSON.stringify(body),
  });
  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    try {
      const err: unknown = await res.json();
      if (typeof err === 'object' && err !== null && typeof (err as { error?: unknown }).error === 'string') {
        message = (err as { error: string }).error;
      }
    } catch {
      // not JSON: keep the status
    }
    throw new HttpError(res.status, message);
  }
  return res.status === 204 ? null : res.json();
}

function issued(v: unknown): DeviceIssued {
  const out = parseDeviceIssued(v);
  if (!out) throw new HttpError(500, 'the gateway sent no setup code');
  return out;
}

export async function addDevice(machine: string, name: string): Promise<DeviceIssued> {
  return issued(await deviceRequest('POST', '/api/devices', { machine, name }));
}

export async function replaceDeviceToken(machine: string): Promise<DeviceIssued> {
  return issued(await deviceRequest('POST', `/api/devices/${encodeURIComponent(machine)}/token`, {}));
}

export async function removeDevice(machine: string): Promise<void> {
  await deviceRequest('DELETE', `/api/devices/${encodeURIComponent(machine)}`);
}

/** A machine id suggestion from a display name: lower case, `[a-z0-9_-]`, at most 64. */
export function machineIdFrom(name: string): string {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64);
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
