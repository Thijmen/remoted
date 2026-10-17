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

/** ws(s):// URL for a path on this origin. */
export function wsUrl(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}${path}`;
}
