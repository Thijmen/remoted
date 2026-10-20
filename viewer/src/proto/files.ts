// File channel messages — mirror of crates/rd-proto/src/files.rs (the source of truth), see
// docs/files.md. JSON text frames tagged by "t"; receivers ignore unknown "t" values and fields.

import { agentStateFrom, type AgentState } from './control';

export const FILES_PROTO_VERSION = 1;

/** Capabilities in the agent's file `hello`. */
export const FileCaps = { TRASH: 'trash', ZIP: 'zip', RESUME: 'resume' } as const;

export type EntryKind = 'file' | 'dir' | 'other';

export interface Entry {
  name: string;
  /** After following a symbolic link (`link`); a broken link is `other`. */
  kind: EntryKind;
  size: number;
  /** Unix ms; 0 when unknown. */
  mtime_ms: number;
  hidden: boolean;
  link: boolean;
}

export type PlaceKind = 'home' | 'desktop' | 'documents' | 'downloads' | 'drive' | 'volume' | 'other';

export interface Place {
  kind: PlaceKind;
  name: string;
  path: string;
}

export type Conflict = 'fail' | 'replace' | 'rename';

export type FilesViewerMsg =
  | { t: 'hello'; proto: number }
  | { t: 'list'; id: number; path: string }
  | { t: 'stat'; id: number; path: string }
  | { t: 'mkdir'; id: number; path: string; exist_ok?: boolean }
  | { t: 'rename'; id: number; from: string; to: string }
  | { t: 'delete'; id: number; paths: string[]; trash: boolean }
  | {
      t: 'read';
      id: number;
      path?: string;
      paths?: string[];
      offset?: number;
      zip?: boolean;
      name?: string;
      expect?: { size: number; mtime_ms: number };
    }
  | {
      t: 'write';
      id: number;
      path: string;
      size: number;
      mtime_ms?: number;
      conflict: Conflict;
      token: string;
      resume?: boolean;
    }
  | { t: 'cancel'; id: number };

export interface FilesHello {
  t: 'hello';
  proto: number;
  os: string;
  sep: string;
  home: string;
  places: Place[];
  caps: string[];
  state: AgentState | null;
}

export type FilesAgentMsg =
  | FilesHello
  | { t: 'state'; state: AgentState }
  | { t: 'ok'; id: number }
  | { t: 'err'; id: number; code: string; msg: string }
  | { t: 'list'; id: number; path: string; parent: string | null; entries: Entry[]; truncated: boolean }
  | { t: 'stat'; id: number; entry: Entry | null }
  | {
      t: 'read';
      id: number;
      name: string;
      size: number | null;
      mtime_ms: number | null;
      total: number | null;
      files: number | null;
    }
  | { t: 'progress'; id: number; done: number }
  | { t: 'skipped'; id: number; paths: string[] }
  | { t: 'write'; id: number; offset: number }
  | { t: 'done'; id: number; path: string; size: number };

// ---------------------------------------------------------------------------
// defensive decoding

type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const num = (o: Obj, k: string, def = 0): number => {
  const v = o[k];
  return typeof v === 'number' && Number.isFinite(v) ? v : def;
};
const optNum = (o: Obj, k: string): number | null => {
  const v = o[k];
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
};
const str = (o: Obj, k: string, def = ''): string => {
  const v = o[k];
  return typeof v === 'string' ? v : def;
};
const bool = (o: Obj, k: string): boolean => o[k] === true;
const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);

function agentState(v: unknown): AgentState | null {
  return isObj(v) ? agentStateFrom(v) : null;
}

export function entryFrom(v: unknown): Entry | null {
  if (!isObj(v) || typeof v.name !== 'string' || v.name === '') return null;
  const kind = v.kind === 'file' || v.kind === 'dir' ? v.kind : 'other';
  return {
    name: v.name,
    kind,
    size: Math.max(0, num(v, 'size')),
    mtime_ms: num(v, 'mtime_ms'),
    hidden: bool(v, 'hidden'),
    link: bool(v, 'link'),
  };
}

const PLACE_KINDS: readonly PlaceKind[] = ['home', 'desktop', 'documents', 'downloads', 'drive', 'volume'];

function placeFrom(v: unknown): Place | null {
  if (!isObj(v) || typeof v.path !== 'string' || v.path === '') return null;
  const kind = PLACE_KINDS.find((k) => k === v.kind) ?? 'other';
  return { kind, name: str(v, 'name') || v.path, path: v.path };
}

/** Parse an agent text frame on a file channel; null when it is not a message we know. */
export function parseFilesAgentMsg(text: string): FilesAgentMsg | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObj(v) || typeof v.t !== 'string') return null;
  const id = num(v, 'id');
  switch (v.t) {
    case 'hello':
      return {
        t: 'hello',
        proto: num(v, 'proto'),
        os: str(v, 'os'),
        sep: v.sep === '\\' ? '\\' : '/',
        home: str(v, 'home'),
        places: (Array.isArray(v.places) ? v.places : []).map(placeFrom).filter((p): p is Place => p !== null),
        caps: strings(v.caps),
        state: agentState(v.state),
      };
    case 'state': {
      const state = agentState(v);
      return state ? { t: 'state', state } : null;
    }
    case 'ok':
      return { t: 'ok', id };
    case 'err':
      return { t: 'err', id, code: str(v, 'code', 'io'), msg: str(v, 'msg') };
    case 'list':
      return {
        t: 'list',
        id,
        path: str(v, 'path'),
        parent: typeof v.parent === 'string' ? v.parent : null,
        entries: (Array.isArray(v.entries) ? v.entries : []).map(entryFrom).filter((e): e is Entry => e !== null),
        truncated: bool(v, 'truncated'),
      };
    case 'stat':
      return { t: 'stat', id, entry: entryFrom(v.entry) };
    case 'read':
      return {
        t: 'read',
        id,
        name: str(v, 'name'),
        size: optNum(v, 'size'),
        mtime_ms: optNum(v, 'mtime_ms'),
        total: optNum(v, 'total'),
        files: optNum(v, 'files'),
      };
    case 'progress':
      return { t: 'progress', id, done: num(v, 'done') };
    case 'skipped':
      return { t: 'skipped', id, paths: strings(v.paths) };
    case 'write':
      return { t: 'write', id, offset: num(v, 'offset') };
    case 'done':
      return { t: 'done', id, path: str(v, 'path'), size: num(v, 'size') };
    default:
      return null;
  }
}

/** Human text for an `err` code (docs/files.md). */
export function describeFileError(code: string, msg: string): string {
  const known: Record<string, string> = {
    not_found: 'Not found',
    exists: 'Already exists',
    denied: 'Access denied',
    not_dir: 'Not a folder',
    is_dir: 'Is a folder',
    bad_path: 'Invalid path',
    bad_name: 'Invalid name',
    bad_request: 'Request refused',
    changed: 'The file changed on the remote machine',
    no_space: 'The remote disk is full',
    unsupported: 'Not supported by this agent',
    proto_mismatch: 'Viewer and agent speak different file protocols',
  };
  const head = known[code];
  if (!head) return msg || code;
  return msg && !msg.toLowerCase().startsWith(head.toLowerCase()) ? `${head}: ${msg}` : head;
}
