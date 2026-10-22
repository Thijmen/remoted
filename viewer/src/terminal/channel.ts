import { type Bytes, type TermOutMsg, ProtoError, encodePing, parse } from '../proto/bin';
import type { AgentState } from '../proto/control';
import {
  DetachReason,
  type TermHello,
  type TermViewerMsg,
  TermErr,
  describeTermError,
  parseTermAgentMsg,
} from '../proto/terminal';
import { fetchMachine, probeAccess, wsUrl } from '../net/api';
import { Backoff, OFFLINE_POLL_MS } from '../net/backoff';
import { TERMINAL_RECONNECT_STEPS_MS, closeDecision } from './policy';

/** Chromium only gives up on a hanging opening handshake after 240 s. */
const OPEN_TIMEOUT_MS = 10_000;
/** The gateway gives the agent 10 s to dial in, then a hello round trip (a shell starting). */
const READY_TIMEOUT_MS = 20_000;
/** The agent answers PINGs at once. */
const PING_INTERVAL_MS = 5000;
/** Nothing received (not even a PONG) for this long: the connection is dead. */
const RX_TIMEOUT_MS = 20_000;
const LOCAL_ABANDON = 4000;

export type TermStatus =
  /** Not connected and not wanted (a hidden panel). */
  | { kind: 'idle' }
  | { kind: 'connecting'; attempt: number }
  | { kind: 'ready' }
  | { kind: 'retrying'; delayMs: number; reason: string }
  /** The machine is offline (or restarting for an update): waiting for it. */
  | { kind: 'offline' }
  /** Closed after a long idle time while the page was hidden: reconnects once it is visible. */
  | { kind: 'sleeping' }
  /** The shell was opened in another window (4001 / `detached`); `retry()` takes it back. */
  | { kind: 'taken' }
  /** The agent offers no terminal (4011). */
  | { kind: 'unsupported' }
  /** The shell to reattach is gone (`not_found`). */
  | { kind: 'ended' }
  /** The shell exited. */
  | { kind: 'exited'; code: number | null; signal: number | null }
  /** Stopped for a reason the user has to act on (`reload`: signed out). */
  | { kind: 'failed'; reason: string; reload: boolean };

export interface TermChannelEvents {
  status(s: TermStatus): void;
  /** The agent's hello: the channel is attached to a shell. */
  hello(h: TermHello): void;
  out(m: TermOutMsg): void;
  state(s: AgentState): void;
  /** The socket went away (whatever the reason): a new one starts with seq 1. */
  lost(): void;
}

/**
 * One terminal channel (`/ws/terminal/{machine}`, docs/terminal.md): connects while wanted, says the hello
 * `helloMsg()` builds (a new shell or an attach), pings, and reconnects with backoff when the connection drops.
 * Ends for good on `exit`, `err`, a takeover and the closes that retrying cannot fix; `retry()` starts again.
 */
export class TerminalChannel {
  private ws: WebSocket | null = null;
  private phase: 'closed' | 'opening' | 'joining' | 'hello' | 'ready' = 'closed';
  private wanted = false;
  private readonly backoff = new Backoff(TERMINAL_RECONNECT_STEPS_MS);
  private timers: number[] = [];
  private pollAbort: AbortController | null = null;
  private lastRxMs = 0;
  private lastTickMs = 0;
  private pingId = 0;
  private statusNow: TermStatus = { kind: 'idle' };

  constructor(
    private readonly machine: string,
    private readonly helloMsg: () => TermViewerMsg,
    private readonly events: TermChannelEvents,
  ) {
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && this.statusNow.kind === 'sleeping' && this.wanted) this.connect();
    });
  }

  get status(): TermStatus {
    return this.statusNow;
  }

  get ready(): boolean {
    return this.phase === 'ready' && this.ws?.readyState === WebSocket.OPEN;
  }

  /** Connect (if not connected or connecting). */
  start(): void {
    this.wanted = true;
    if (this.phase !== 'closed') return;
    switch (this.statusNow.kind) {
      case 'idle':
      case 'sleeping':
        this.backoff.reset();
        this.connect();
    }
  }

  /** Close the channel (the shell keeps running) and stay closed. An end state (exited, taken, …) stays. */
  stop(): void {
    this.wanted = false;
    if (this.statusNow.kind === 'idle' || isFinal(this.statusNow)) return;
    const live = this.phase !== 'closed';
    this.teardown(1000, 'closed');
    if (live) this.events.lost();
    this.setStatus({ kind: 'idle' });
  }

  /** Connect again now, whatever the state (Retry, Take it back, New shell). */
  retry(): void {
    this.wanted = true;
    const live = this.phase !== 'closed';
    this.teardown(1000, 'reconnect');
    if (live) this.events.lost();
    this.backoff.reset();
    this.connect();
  }

  sendJson(m: TermViewerMsg): boolean {
    if (!this.ready) return false;
    this.ws!.send(JSON.stringify(m));
    return true;
  }

  sendBinary(b: Bytes): boolean {
    if (!this.ready) return false;
    this.ws!.send(b);
    return true;
  }

  // -------------------------------------------------------------------------

  private setStatus(s: TermStatus): void {
    this.statusNow = s;
    this.events.status(s);
  }

  private connect(): void {
    this.teardown();
    this.phase = 'opening';
    this.setStatus({ kind: 'connecting', attempt: this.backoff.attempts });
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl(`/ws/terminal/${encodeURIComponent(this.machine)}`));
    } catch (e) {
      this.closed(1006, e instanceof Error ? e.message : 'connect failed', true);
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    this.after(OPEN_TIMEOUT_MS, () => {
      if (this.ws === ws && this.phase === 'opening') this.abandon('connect timeout');
    });
    this.after(READY_TIMEOUT_MS, () => {
      if (this.ws === ws && this.phase !== 'ready') this.abandon('agent did not answer');
    });
    ws.onopen = () => {
      this.phase = 'joining';
      this.lastRxMs = performance.now();
    };
    ws.onmessage = (ev: MessageEvent<string | ArrayBuffer>) => {
      if (this.ws === ws) this.receive(ev.data);
    };
    ws.onclose = (ev) => {
      if (this.ws !== ws) return;
      const neverOpened = this.phase === 'opening';
      this.teardown();
      this.events.lost();
      this.closed(ev.code, ev.reason, neverOpened);
    };
  }

  private abandon(reason: string): void {
    if (this.phase === 'closed') return;
    console.info(`[rd] terminal channel abandoned: ${reason}`);
    this.teardown(LOCAL_ABANDON, reason);
    this.events.lost();
    this.closed(LOCAL_ABANDON, reason, false);
  }

  private receive(data: string | ArrayBuffer): void {
    this.lastRxMs = performance.now();
    if (typeof data !== 'string') {
      if (this.phase !== 'ready') return;
      let m;
      try {
        m = parse(data);
      } catch (e) {
        if (e instanceof ProtoError) return;
        throw e;
      }
      if (m.type === 'term_out') this.events.out(m);
      return;
    }
    if (this.phase === 'joining') {
      // The gateway's `session` message: joined. Say hello.
      if (messageType(data) === 'session') {
        this.phase = 'hello';
        this.ws?.send(JSON.stringify(this.helloMsg()));
      }
      return;
    }
    const m = parseTermAgentMsg(data);
    if (!m) return;
    switch (m.t) {
      case 'hello':
        if (this.phase !== 'hello') return;
        this.phase = 'ready';
        this.backoff.reset();
        this.every(PING_INTERVAL_MS, () => this.tick());
        this.setStatus({ kind: 'ready' });
        this.events.hello(m);
        return;
      case 'state':
        this.events.state(m.state);
        return;
      case 'exit':
        this.finish({ kind: 'exited', code: m.code, signal: m.signal });
        return;
      case 'detached':
        this.finish(
          m.reason === DetachReason.TAKEN_OVER || m.reason === ''
            ? { kind: 'taken' }
            : { kind: 'failed', reason: `Detached by the agent (${m.reason}).`, reload: false },
        );
        return;
      case 'err':
        this.finish(
          m.code === TermErr.NOT_FOUND
            ? { kind: 'ended' }
            : { kind: 'failed', reason: describeTermError(m.code, m.msg), reload: false },
        );
    }
  }

  /** The agent ended the channel on purpose (it closes the socket right after): no reconnect. */
  private finish(s: TermStatus): void {
    this.teardown(1000, 'done');
    this.events.lost();
    this.setStatus(s);
  }

  private tick(): void {
    // A hidden or frozen page runs timers late: that is not silence on the network.
    const now = performance.now();
    if (now - this.lastTickMs > 3 * PING_INTERVAL_MS) this.lastRxMs = now;
    this.lastTickMs = now;
    if (now - this.lastRxMs > RX_TIMEOUT_MS) {
      this.abandon('no answer from the agent');
      return;
    }
    this.ws?.send(encodePing(this.pingId++ >>> 0, BigInt(Math.round(now * 1000))));
  }

  private closed(code: number, reason: string, neverOpened: boolean): void {
    this.phase = 'closed';
    if (!this.wanted) {
      this.setStatus({ kind: 'idle' });
      return;
    }
    switch (closeDecision(code, neverOpened)) {
      case 'taken':
        this.setStatus({ kind: 'taken' });
        return;
      case 'signed-out':
        this.setStatus({
          kind: 'failed',
          reason: 'Signed out or not allowed. Reload the page to sign in.',
          reload: true,
        });
        return;
      case 'protocol':
        this.setStatus({
          kind: 'failed',
          reason: `Protocol error${reason ? ` (${reason})` : ''}. Reload the page or update the agent.`,
          reload: true,
        });
        return;
      case 'unsupported':
        this.setStatus({ kind: 'unsupported' });
        return;
      case 'offline':
        this.setStatus({ kind: 'offline' });
        this.pollOnline();
        return;
      case 'idle':
        // Nobody typed or printed for an hour. Visible: someone looks at it, so reconnect at once.
        if (document.hidden) this.setStatus({ kind: 'sleeping' });
        else this.connect();
        return;
      case 'busy':
        this.retryLater('too many terminals open on this machine');
        return;
      case 'probe':
        // A refused upgrade (signed out) looks like a network failure: ask the HTTP API.
        void probeAccess(AbortSignal.timeout(5000)).then((access) => {
          if (this.phase !== 'closed' || !this.wanted) return;
          if (access === 'denied')
            this.setStatus({
              kind: 'failed',
              reason: 'Signed out or not allowed. Reload the page to sign in.',
              reload: true,
            });
          else this.retryLater(reason || 'network');
        });
        return;
      case 'retry':
        this.retryLater(describeClose(code, reason));
    }
  }

  private retryLater(reason: string): void {
    const delayMs = this.backoff.next();
    this.setStatus({ kind: 'retrying', delayMs, reason });
    this.after(delayMs, () => {
      if (this.wanted) this.connect();
    });
  }

  private pollOnline(): void {
    const poll = async () => {
      const abort = new AbortController();
      this.pollAbort = abort;
      const m = await fetchMachine(this.machine, AbortSignal.any([abort.signal, AbortSignal.timeout(5000)])).catch(
        () => null,
      );
      if (abort.signal.aborted || !this.wanted) return;
      if (m?.online && !(m.update && (m.update.phase === 'downloading' || m.update.phase === 'installing'))) {
        this.connect();
        return;
      }
      this.after(OFFLINE_POLL_MS, () => void poll());
    };
    this.after(OFFLINE_POLL_MS, () => void poll());
  }

  private teardown(code?: number, reason?: string): void {
    for (const t of this.timers) {
      clearTimeout(t);
      clearInterval(t);
    }
    this.timers = [];
    this.pollAbort?.abort();
    this.pollAbort = null;
    const ws = this.ws;
    this.ws = null;
    this.phase = 'closed';
    if (!ws) return;
    ws.onopen = ws.onmessage = ws.onclose = null;
    if (ws.readyState === WebSocket.CONNECTING || ws.readyState === WebSocket.OPEN) {
      try {
        ws.close(code ?? 1000, reason);
      } catch {
        ws.close();
      }
    }
  }

  private after(ms: number, f: () => void): void {
    this.timers.push(window.setTimeout(f, ms));
  }

  private every(ms: number, f: () => void): void {
    this.lastTickMs = performance.now();
    this.timers.push(window.setInterval(f, ms));
  }
}

/** States the channel only leaves through `retry()`. */
export function isFinal(s: TermStatus): boolean {
  switch (s.kind) {
    case 'taken':
    case 'unsupported':
    case 'ended':
    case 'exited':
    case 'failed':
      return true;
    default:
      return false;
  }
}

function describeClose(code: number, reason: string): string {
  // Browsers give 1006 no reason, so one here is the viewer's own diagnosis.
  if (reason) return reason;
  const known: Record<number, string> = {
    1000: 'closed by the agent',
    1001: 'gateway restarting',
    1012: 'gateway restarting',
    4004: 'the machine did not answer',
    1006: 'network',
  };
  return known[code] ?? `code ${code}`;
}

/** The `t` of a JSON text frame, without trusting the rest. */
function messageType(data: string): string | null {
  try {
    const v: unknown = JSON.parse(data);
    return typeof v === 'object' && v !== null && typeof (v as { t?: unknown }).t === 'string'
      ? (v as { t: string }).t
      : null;
  } catch {
    return null;
  }
}
