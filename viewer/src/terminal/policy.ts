// What a terminal does after its channel closed (docs/terminal.md). Pure: unit-tested in policy.test.ts.

import { CloseCode } from '../proto/control';

/** Reconnect delays: 1 → 2 → 4 → 8 → 10 s, then 10 s forever. */
export const TERMINAL_RECONNECT_STEPS_MS = [1000, 2000, 4000, 8000, 10_000] as const;

export type CloseDecision =
  /** Reconnect (and reattach) after the backoff delay. */
  | 'retry'
  /** Too many terminal channels to the machine (1013): retry after the backoff delay, say why. */
  | 'busy'
  /** The machine is offline or updating: wait until it is back, then reconnect. */
  | 'offline'
  /** Closed for idleness (4008): reconnect once the page is visible. */
  | 'idle'
  /** The shell was opened elsewhere (4001): only the user takes it back. */
  | 'taken'
  /** The agent offers no terminal (4011). */
  | 'unsupported'
  /** Signed out or not allowed (4002): a reload goes through the sign-in. */
  | 'signed-out'
  /** Protocol mismatch (4005): retrying cannot help. */
  | 'protocol'
  /** Refused before it opened (1006): signed out, or the network? Ask the HTTP API. */
  | 'probe';

/** What to do after the channel closed with `code` (`neverOpened`: the WebSocket never opened). */
export function closeDecision(code: number, neverOpened: boolean): CloseDecision {
  switch (code) {
    case CloseCode.KICKED:
      return 'taken';
    case CloseCode.FORBIDDEN:
      return 'signed-out';
    case CloseCode.AGENT_OFFLINE:
    case CloseCode.AGENT_UPDATING:
      return 'offline';
    case CloseCode.PROTOCOL:
      return 'protocol';
    case CloseCode.IDLE_TIMEOUT:
      return 'idle';
    case CloseCode.UNSUPPORTED:
      return 'unsupported';
    case CloseCode.TRY_AGAIN_LATER:
      return 'busy';
  }
  // 1000 without an `exit` or `err` first, 1001/1012 (gateway restart), 4004 (agent did not dial in), network.
  return neverOpened && code === 1006 ? 'probe' : 'retry';
}
