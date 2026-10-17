import { CloseCode } from '../proto/control';

/** Reconnect delays: 0.5 → 1 → 2 → 4 → 10 s, then 10 s forever. */
export const RECONNECT_STEPS_MS = [500, 1000, 2000, 4000, 10_000] as const;

export class Backoff {
  private attempt = 0;

  constructor(private readonly steps: readonly number[] = RECONNECT_STEPS_MS) {}

  /** Delay before the next attempt (advances the schedule). */
  next(): number {
    const d = this.steps[Math.min(this.attempt, this.steps.length - 1)]!;
    this.attempt++;
    return d;
  }

  /** Call once a session got established. */
  reset(): void {
    this.attempt = 0;
  }

  get attempts(): number {
    return this.attempt;
  }
}

export type CloseAction =
  /** Don't reconnect automatically; the user has to click. */
  | 'stop'
  /** Machine offline: poll /api/machines until it is back, then reconnect. */
  | 'poll'
  /** Reconnect with backoff. */
  | 'retry';

/**
 * What to do after the session socket closed with `code`.
 * 4001 (taken over) must not reconnect, or two viewers would kick each other forever. 4002
 * (forbidden), 4005 (protocol mismatch) and 4008 (idle timeout) also stop: retrying cannot help
 * or would defeat the idle timeout.
 */
export function closeAction(code: number): CloseAction {
  switch (code) {
    case CloseCode.KICKED:
    case CloseCode.FORBIDDEN:
    case CloseCode.PROTOCOL:
    case CloseCode.IDLE_TIMEOUT:
      return 'stop';
    case CloseCode.AGENT_OFFLINE:
      return 'poll';
    default:
      return 'retry';
  }
}

export const OFFLINE_POLL_MS = 3000;
