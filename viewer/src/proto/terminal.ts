// Terminal channel messages — mirror of crates/rd-proto/src/terminal.rs (the source of truth), see
// docs/terminal.md. JSON text frames tagged by "t"; receivers ignore unknown "t" values and fields. The
// shell's output and input travel as TERM_OUT / TERM_ACK / TERM_IN binary frames (./bin.ts).

import { agentStateFrom, type AgentState } from './control';

export const TERMINAL_PROTO_VERSION = 1;

/** Largest message on a terminal channel, either way. */
export const MAX_MESSAGE = 1 << 20;
/** Most output bytes one TERM_OUT frame carries. */
export const MAX_OUT_CHUNK = 64 << 10;
/** Most input bytes one TERM_IN frame carries (the viewer splits a large paste). */
export const MAX_IN_CHUNK = 64 << 10;
/** Output the agent keeps per shell for replay after a reconnect. */
export const HISTORY_BYTES = 1 << 20;
/** Terminal sizes are clamped to this range (columns and rows). */
export const MIN_SIZE = 2;
export const MAX_SIZE = 1000;

/** Error codes of the agent's `err`. */
export const TermErr = {
  /** `attach` names a shell the agent does not have (it ended, was closed, timed out, or the agent restarted). */
  NOT_FOUND: 'not_found',
  /** The agent already runs its maximum number of shells. */
  BUSY: 'busy',
  /** The shell could not be started (`msg` says why). */
  SPAWN: 'spawn',
  BAD_REQUEST: 'bad_request',
  PROTO_MISMATCH: 'proto_mismatch',
} as const;

/** Why the agent detached this viewer from its shell. */
export const DetachReason = {
  /** The same shell was attached from another channel (another tab or browser). */
  TAKEN_OVER: 'taken_over',
} as const;

export type TermViewerMsg =
  | {
      t: 'hello';
      proto: number;
      cols: number;
      rows: number;
      /** Reattach to this shell instead of starting a new one. */
      attach?: string;
      /** Output bytes of the attached shell the viewer already shows. */
      offset?: number;
    }
  | { t: 'resize'; cols: number; rows: number }
  | { t: 'close' };

export interface TermHello {
  t: 'hello';
  proto: number;
  /** The shell's id (32 hex), for `attach` after a reconnect. */
  id: string;
  /** "macos" | "windows" */
  os: string;
  /** What runs in the terminal, for the title (e.g. "zsh", "pwsh"). */
  shell: string;
  /** Where the replay starts. */
  offset: number;
  /** Clear the screen and scrollback before showing what follows. */
  reset: boolean;
  /**
   * Where the replay ends: the shell's output position when it was attached (everything before it is
   * history). Not in the protocol yet; null when the agent does not send it.
   */
  end: number | null;
  state: AgentState | null;
}

export type TermAgentMsg =
  | TermHello
  | { t: 'state'; state: AgentState }
  | { t: 'exit'; code: number | null; signal: number | null }
  | { t: 'detached'; reason: string }
  | { t: 'err'; code: string; msg: string };

type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const optNum = (o: Obj, k: string): number | null => {
  const v = o[k];
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
};
const str = (o: Obj, k: string, def = ''): string => {
  const v = o[k];
  return typeof v === 'string' ? v : def;
};

/** Parse an agent text frame on a terminal channel; null when it is not a message we know. */
export function parseTermAgentMsg(text: string): TermAgentMsg | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObj(v) || typeof v.t !== 'string') return null;
  switch (v.t) {
    case 'hello': {
      const id = str(v, 'id');
      if (!id) return null;
      return {
        t: 'hello',
        proto: optNum(v, 'proto') ?? 0,
        id,
        os: str(v, 'os'),
        shell: str(v, 'shell'),
        offset: Math.max(0, optNum(v, 'offset') ?? 0),
        reset: v.reset === true,
        end: optNum(v, 'end'),
        state: isObj(v.state) ? agentStateFrom(v.state) : null,
      };
    }
    case 'state':
      return { t: 'state', state: agentStateFrom(v) };
    case 'exit':
      return { t: 'exit', code: optNum(v, 'code'), signal: optNum(v, 'signal') };
    case 'detached':
      return { t: 'detached', reason: str(v, 'reason') };
    case 'err':
      return { t: 'err', code: str(v, 'code', 'bad_request'), msg: str(v, 'msg') };
    default:
      return null;
  }
}

/** Clamp a terminal dimension to what the agent accepts. */
export function clampSize(n: number): number {
  if (!Number.isFinite(n)) return 80;
  return Math.min(MAX_SIZE, Math.max(MIN_SIZE, Math.round(n)));
}

/** Human text for an `err` code (docs/terminal.md). */
export function describeTermError(code: string, msg: string): string {
  switch (code) {
    case TermErr.NOT_FOUND:
      return 'The shell has ended.';
    case TermErr.BUSY:
      return 'This machine already runs as many shells as it allows (8). Close one and try again.';
    case TermErr.SPAWN:
      return msg ? `The shell could not be started: ${msg}` : 'The shell could not be started.';
    case TermErr.PROTO_MISMATCH:
      return 'Viewer and agent speak different terminal protocols. Reload the page or update the agent.';
    default:
      return msg ? `The agent refused the terminal (${code}): ${msg}` : `The agent refused the terminal (${code}).`;
  }
}

/** "[Process exited with code 0]" and the like, for the line written into the terminal. */
export function describeExit(code: number | null, signal: number | null): string {
  if (signal !== null) return `Process ended by signal ${signal}`;
  if (code !== null) return `Process exited with code ${code}`;
  return 'Process exited';
}
