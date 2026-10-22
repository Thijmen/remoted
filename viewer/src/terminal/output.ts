// The bookkeeping between TERM_OUT frames and the terminal (docs/terminal.md): which bytes the terminal
// shows, what to acknowledge, which output is replayed history, and how input is cut into TERM_IN frames.
// No DOM, no xterm: unit-tested in output.test.ts.

import { MAX_IN_CHUNK } from '../proto/terminal';

/** What to do with one TERM_OUT frame. */
export interface Accepted {
  /** The bytes to hand to the terminal (a tail of the payload when it overlaps what is shown). */
  bytes: Uint8Array;
  /** Output lost before this frame (the agent's history overtook the channel). */
  skipped: number;
}

/**
 * The position in the shell's output: how many bytes the terminal shows (handed to it, counting from the
 * shell's start). Survives reconnects within the page, so a reattach continues where it left off.
 */
export class OutputPosition {
  private pos = 0;

  /** Bytes of the shell's output the terminal shows (the `offset` of a reattach). */
  get offset(): number {
    return this.pos;
  }

  /** The agent's `hello`: output continues at `offset` (after `reset`, or the start of a new shell). */
  start(offset: number): void {
    this.pos = offset;
  }

  /**
   * A TERM_OUT frame at `offset`. A jump forward just continues with the newer output (what was in between
   * is gone); bytes the terminal already shows are dropped.
   */
  accept(offset: number, payload: Uint8Array): Accepted {
    const end = offset + payload.byteLength;
    if (end <= this.pos) return { bytes: payload.subarray(payload.byteLength), skipped: 0 };
    const skipped = Math.max(0, offset - this.pos);
    const bytes = offset < this.pos ? payload.subarray(this.pos - offset) : payload;
    this.pos = end;
    return { bytes, skipped };
  }
}

/**
 * Cumulative TERM_ACK: the highest TERM_OUT `seq` the terminal has processed, sent at most once per
 * animation frame. One per channel (seq starts at 1 on every channel).
 */
export class AckTracker {
  private done = 0;
  private sent = 0;

  /** The terminal processed every frame up to `seq`. */
  processed(seq: number): void {
    if (seq > this.done) this.done = seq;
  }

  /** The seq to acknowledge now, or null when nothing new was processed since the last ack. */
  take(): number | null {
    if (this.done <= this.sent) return null;
    this.sent = this.done;
    return this.sent;
  }

  get pending(): boolean {
    return this.done > this.sent;
  }
}

/** Without the agent telling where the history ends, output is treated as history for this long. */
export const REPLAY_GUESS_MS = 3000;

/**
 * Which output is replayed history. A terminal answers queries in the output (device attributes, cursor
 * position, colours) by typing the answer; for history those answers would reach a program that asked long
 * ago (or no longer runs), so the terminal must not answer them.
 *
 * The agent's `hello` gives where the replay ends (`end`) when it knows; else everything is history until
 * the user types, or `REPLAY_GUESS_MS` after the hello.
 */
export class ReplayFilter {
  /** Output below this offset is history. */
  private until = 0;
  /** Guessing (no `end`): when the guess runs out. */
  private deadline: number | null = null;
  /** The end of the last frame seen, where live output starts once the guess ends. */
  private seen = 0;

  /**
   * After the agent's hello. `attached`: a reattach (a new shell has no history). `end`: where the history
   * ends, when the agent says.
   */
  begin(attached: boolean, offset: number, end: number | null, now: number): void {
    this.seen = offset;
    this.deadline = null;
    if (!attached) this.until = 0;
    else if (end !== null) this.until = end;
    else {
      this.until = Number.POSITIVE_INFINITY;
      this.deadline = now + REPLAY_GUESS_MS;
    }
  }

  /** Is the frame at `offset` (`len` bytes) history? Call for every frame, in order. */
  frame(offset: number, len: number, now: number): boolean {
    if (this.deadline !== null && now >= this.deadline) this.stopGuessing();
    const replay = offset < this.until;
    this.seen = Math.max(this.seen, offset + len);
    return replay;
  }

  /** The user typed: whatever comes from now on answers to them. */
  input(): void {
    if (this.deadline !== null) this.stopGuessing();
  }

  private stopGuessing(): void {
    this.deadline = null;
    this.until = this.seen;
  }
}

const encoder = new TextEncoder();

/** xterm's `onBinary` data: one byte per UTF-16 code unit (mouse reports in X10 mode). */
export function binaryStringBytes(s: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0xff;
  return out;
}

/** UTF-8 bytes of what the terminal typed (keys, pastes). */
export function textBytes(s: string): Uint8Array<ArrayBuffer> {
  return encoder.encode(s);
}

/** Cut input into TERM_IN payloads of at most `max` bytes (a large paste becomes several frames). */
export function splitInput(bytes: Uint8Array, max = MAX_IN_CHUNK): Uint8Array[] {
  if (bytes.byteLength <= max) return [bytes];
  const out: Uint8Array[] = [];
  for (let i = 0; i < bytes.byteLength; i += max) out.push(bytes.subarray(i, i + max));
  return out;
}
