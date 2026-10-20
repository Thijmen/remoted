import { type Bytes, type FileDataMsg, ProtoError, encodePing, parse } from '../proto/bin';
import { type AgentState, CloseCode } from '../proto/control';
import {
  FILES_PROTO_VERSION,
  type FilesAgentMsg,
  type FilesHello,
  type FilesViewerMsg,
  describeFileError,
  parseFilesAgentMsg,
} from '../proto/files';
import { fetchMachine, probeAccess, wsUrl } from '../net/api';
import { Backoff, OFFLINE_POLL_MS } from '../net/backoff';

/** Chromium only gives up on a hanging opening handshake after 240 s. */
const OPEN_TIMEOUT_MS = 10_000;
/** The gateway gives the agent 10 s to dial in, then a hello round trip. */
const READY_TIMEOUT_MS = 20_000;
/** The agent answers PINGs at once, also while its disk is busy. */
const PING_INTERVAL_MS = 5000;
/** Nothing received (not even a PONG) for this long: the connection is dead. */
const RX_TIMEOUT_MS = 20_000;
const LOCAL_ABANDON = 4000;

export type ChannelStatus =
  /** Not connected and not wanted. */
  | { kind: 'idle' }
  | { kind: 'connecting'; attempt: number }
  | { kind: 'ready' }
  | { kind: 'retrying'; delayMs: number; reason: string }
  /** The machine is offline (or restarting for an update): waiting for it. */
  | { kind: 'offline' }
  /** The agent has no file transfer (an older version). */
  | { kind: 'unsupported' }
  /** Not reconnecting by itself (signed out, protocol mismatch); `retry()` tries again. */
  | { kind: 'stopped'; reason: string };

export interface ChannelEvents {
  status(s: ChannelStatus): void;
  /** The agent's hello, after every (re)connect. */
  hello(h: FilesHello): void;
  state(s: AgentState): void;
  /** Agent messages for transfers (no pending request has their id). */
  transfer(m: Exclude<FilesAgentMsg, FilesHello | { t: 'state' }>): void;
  data(m: FileDataMsg): void;
  ack(seq: number): void;
  /** The socket went away: what was in flight on it is lost. */
  lost(): void;
}

/** An `err` answer, or a request that could not get one. */
export class FileError extends Error {
  override name = 'FileError';
  constructor(
    readonly code: string,
    msg: string,
  ) {
    super(describeFileError(code, msg));
  }
}

type Reply = Exclude<FilesAgentMsg, FilesHello | { t: 'state' }>;
type Request = FilesViewerMsg extends infer M ? (M extends { id: number } ? Omit<M, 'id'> : never) : never;

interface Pending {
  resolve(m: Reply): void;
  reject(e: FileError): void;
}

/**
 * One file channel (`/ws/files/{machine}`, docs/files.md): connects while `want(true)`,
 * reconnects with backoff, answers requests by id and hands transfer traffic to `events`.
 */
export class FilesChannel {
  private ws: WebSocket | null = null;
  private phase: 'closed' | 'opening' | 'joining' | 'hello' | 'ready' = 'closed';
  /** The owner wants the channel (browser open, transfers unfinished). */
  private external = false;
  /** Operations under way that need it (`hold`). */
  private holds = 0;
  /** Closed for idleness: the owner's wish alone does not reconnect, an operation does. */
  private idleClosed = false;
  private readonly backoff = new Backoff();
  private timers: number[] = [];
  private pollAbort: AbortController | null = null;
  private nextRequestId = 1;
  private readonly pending = new Map<number, Pending>();
  private lastRxMs = 0;
  private lastTickMs = 0;
  private pingId = 0;
  private status: ChannelStatus = { kind: 'idle' };
  hello: FilesHello | null = null;
  /** Resolved once ready; replaced on every reconnect. */
  private readyWaiters: Array<{ resolve(): void; reject(e: FileError): void }> = [];

  constructor(
    private readonly machine: string,
    private readonly events: ChannelEvents,
  ) {}

  get current(): ChannelStatus {
    return this.status;
  }

  get ready(): boolean {
    return this.phase === 'ready' && this.ws?.readyState === WebSocket.OPEN;
  }

  get bufferedAmount(): number {
    return this.ws?.bufferedAmount ?? 0;
  }

  /** Milliseconds since anything arrived on the current socket. */
  get quietMs(): number {
    return performance.now() - this.lastRxMs;
  }

  private get wanted(): boolean {
    return (this.external && !this.idleClosed) || this.holds > 0;
  }

  /** Keep the channel connected (true) or let it close once nothing needs it (false). */
  want(on: boolean): void {
    if (on !== this.external) {
      this.external = on;
      // Opening the browser again (or a new transfer) is a new wish: reconnect after an idle close.
      if (on) this.idleClosed = false;
    }
    this.sync();
  }

  /** Keep the channel connected until the returned function is called (an operation under way). */
  hold(): () => void {
    this.holds++;
    this.idleClosed = false;
    this.sync();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.holds--;
      // Let whoever continues after this operation take its own hold first.
      queueMicrotask(() => this.sync());
    };
  }

  /** Connect when wanted and idle; close when no longer wanted. */
  private sync(): void {
    if (this.wanted) {
      if (this.status.kind === 'idle') this.connect();
      return;
    }
    if (this.status.kind === 'idle') return;
    this.teardown(1000, 'not needed');
    this.phase = 'closed';
    this.hello = null;
    this.failPending('cancelled', 'closed');
    this.rejectWaiters('cancelled', 'closed');
    this.events.lost();
    this.setStatus({ kind: 'idle' });
  }

  /** Try again after `stopped` / `unsupported`. */
  retry(): void {
    this.teardown();
    this.backoff.reset();
    this.setStatus({ kind: 'idle' });
    this.sync();
  }

  /** Resolves once the channel is ready (connecting if needed; holds it open meanwhile). */
  whenReady(): Promise<void> {
    if (this.ready) return Promise.resolve();
    if (this.status.kind === 'unsupported') return Promise.reject(new FileError('unsupported', 'no file transfer'));
    if (this.status.kind === 'stopped') return Promise.reject(new FileError('stopped', this.status.reason));
    const release = this.hold();
    return new Promise<void>((resolve, reject) => this.readyWaiters.push({ resolve, reject })).finally(release);
  }

  /** Allocate an id for a transfer (its messages then reach `events.transfer`). */
  allocId(): number {
    const id = this.nextRequestId;
    this.nextRequestId = this.nextRequestId >= 0xffff_fff0 ? 1 : this.nextRequestId + 1;
    return id;
  }

  /** Send a request and wait for its answer (`err` rejects with a FileError). */
  async request(msg: Request): Promise<Reply> {
    const release = this.hold();
    try {
      await this.whenReady();
      const id = this.allocId();
      return await new Promise<Reply>((resolve, reject) => {
        this.pending.set(id, { resolve, reject });
        if (!this.sendJson({ ...msg, id } as FilesViewerMsg)) {
          this.pending.delete(id);
          reject(new FileError('lost', 'not connected'));
        }
      });
    } finally {
      release();
    }
  }

  /** Requests still waiting for an answer. */
  get waiting(): number {
    return this.pending.size;
  }

  sendJson(m: FilesViewerMsg): boolean {
    if (!this.ready) return false;
    this.ws!.send(JSON.stringify(m));
    return true;
  }

  sendBinary(b: Bytes): boolean {
    if (!this.ready) return false;
    this.ws!.send(b);
    return true;
  }

  /** The connection looks dead (no answer for too long): drop it and reconnect. */
  abandon(reason: string): void {
    if (this.phase === 'closed') return;
    console.info(`[rd] file channel abandoned: ${reason}`);
    this.teardown(LOCAL_ABANDON, reason);
    this.closed(LOCAL_ABANDON, reason, false);
  }

  // -------------------------------------------------------------------------

  private setStatus(s: ChannelStatus): void {
    this.status = s;
    this.events.status(s);
  }

  private connect(): void {
    this.teardown();
    this.phase = 'opening';
    this.setStatus({ kind: 'connecting', attempt: this.backoff.attempts });
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl(`/ws/files/${encodeURIComponent(this.machine)}`));
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
      this.closed(ev.code, ev.reason, neverOpened);
    };
  }

  private receive(data: string | ArrayBuffer): void {
    this.lastRxMs = performance.now();
    if (typeof data !== 'string') {
      let m;
      try {
        m = parse(data);
      } catch (e) {
        if (e instanceof ProtoError) return;
        throw e;
      }
      if (m.type === 'file_data') this.events.data(m);
      else if (m.type === 'file_ack') this.events.ack(m.seq);
      return;
    }
    if (this.phase === 'joining') {
      // The gateway's `session` message: joined. Say hello.
      if (text(data) === 'session') {
        this.phase = 'hello';
        this.ws?.send(JSON.stringify({ t: 'hello', proto: FILES_PROTO_VERSION } satisfies FilesViewerMsg));
      }
      return;
    }
    const m = parseFilesAgentMsg(data);
    if (!m) return;
    switch (m.t) {
      case 'hello':
        if (this.phase !== 'hello') return;
        this.phase = 'ready';
        this.hello = m;
        this.backoff.reset();
        this.every(PING_INTERVAL_MS, () => this.tick());
        this.setStatus({ kind: 'ready' });
        this.events.hello(m);
        for (const w of this.readyWaiters.splice(0)) w.resolve();
        return;
      case 'state':
        this.events.state(m.state);
        return;
      case 'err':
        if (m.id === 0 && m.code === 'proto_mismatch') {
          this.teardown(1000, 'protocol mismatch');
          this.stop(describeFileError(m.code, m.msg));
          return;
        }
        break;
    }
    const p = this.pending.get(m.id);
    if (p) {
      this.pending.delete(m.id);
      if (m.t === 'err') p.reject(new FileError(m.code, m.msg));
      else p.resolve(m);
      return;
    }
    this.events.transfer(m);
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
    this.hello = null;
    this.failPending('lost', reason || 'connection lost');
    this.events.lost();
    if (!this.wanted) {
      this.rejectWaiters('cancelled', 'closed');
      this.setStatus({ kind: 'idle' });
      return;
    }
    switch (code) {
      case CloseCode.UNSUPPORTED:
        this.setStatus({ kind: 'unsupported' });
        this.rejectWaiters('unsupported', 'this agent has no file transfer');
        return;
      case CloseCode.FORBIDDEN:
        this.stop('signed out or not allowed: reload the page');
        return;
      case CloseCode.PROTOCOL:
        this.stop(reason || 'protocol error');
        return;
      case CloseCode.AGENT_OFFLINE:
      case CloseCode.AGENT_UPDATING:
        this.setStatus({ kind: 'offline' });
        this.pollOnline();
        return;
      case CloseCode.IDLE_TIMEOUT:
        // Nothing happened for an hour: reconnect only when something is needed again.
        this.idleClosed = true;
        this.setStatus({ kind: 'idle' });
        this.rejectWaiters('lost', 'closed after a long idle time');
        return;
    }
    if (neverOpened && code === 1006) {
      // A refused upgrade (signed out) looks like a network failure: ask the HTTP API.
      void probeAccess(AbortSignal.timeout(5000)).then((access) => {
        if (this.phase !== 'closed' || !this.wanted) return;
        if (access === 'denied') this.stop('signed out or not allowed: reload the page');
        else this.retryLater(reason || 'network');
      });
      return;
    }
    this.retryLater(reason || `code ${code}`);
  }

  private retryLater(reason: string): void {
    const delayMs = this.backoff.next();
    this.setStatus({ kind: 'retrying', delayMs, reason });
    this.after(delayMs, () => {
      if (this.wanted) this.connect();
    });
  }

  private stop(reason: string): void {
    this.phase = 'closed';
    this.setStatus({ kind: 'stopped', reason });
    this.rejectWaiters('stopped', reason);
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

  private failPending(code: string, msg: string): void {
    const pending = [...this.pending.values()];
    this.pending.clear();
    for (const p of pending) p.reject(new FileError(code, msg));
  }

  private rejectWaiters(code: string, msg: string): void {
    for (const w of this.readyWaiters.splice(0)) w.reject(new FileError(code, msg));
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
    if (this.phase !== 'closed') this.phase = 'closed';
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

/** The `t` of a JSON text frame, without trusting the rest. */
function text(data: string): string | null {
  try {
    const v: unknown = JSON.parse(data);
    return typeof v === 'object' && v !== null && typeof (v as { t?: unknown }).t === 'string'
      ? (v as { t: string }).t
      : null;
  } catch {
    return null;
  }
}
