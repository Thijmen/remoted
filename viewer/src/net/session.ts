import { type AudioMsg, type Bytes, ProtoError, type VideoMsg, encodePing, encodePong, parse } from '../proto/bin';
import {
  type AgentHello,
  type AgentMsg,
  type GwErrorMsg,
  MIN_PROTO_VERSION,
  PROTO_VERSION,
  type SessionMsg,
  type StreamParams,
  type ViewerInfo,
  type ViewerMsg,
  parseViewerIncoming,
} from '../proto/control';
import { nowUs } from '../util/stats';
import { fetchMachine, wsUrl } from './api';
import { Backoff, OFFLINE_POLL_MS, closeAction } from './backoff';
import { ClockEstimator } from './clock';

const PING_INTERVAL_MS = 1000;
/** The agent pings every 500 ms, so this much silence means the path is dead. */
const RX_TIMEOUT_MS = 5000;
/** The gateway gives the agent 10 s to join; allow for slack before giving up ourselves. */
const JOIN_TIMEOUT_MS = 15_000;
const HELLO_TIMEOUT_MS = 10_000;
/** Close code we use when abandoning a socket locally (private-use range). */
const LOCAL_ABANDON = 4000;

export type SessionStatus =
  | { kind: 'connecting'; attempt: number }
  /** Socket open, gateway is waiting for the agent to join. */
  | { kind: 'joining' }
  /** Joined; hello sent, waiting for the agent's hello. */
  | { kind: 'handshake' }
  | { kind: 'streaming' }
  | { kind: 'retrying'; code: number; reason: string; delayMs: number }
  /** Agent offline (4003): polling /api/machines. */
  | { kind: 'offline' }
  /** Closed and not reconnecting automatically (e.g. taken over). */
  | { kind: 'stopped'; code: number; reason: string }
  /** Closed by the user. */
  | { kind: 'closed' };

export interface SessionHandlers {
  status(s: SessionStatus): void;
  session(m: SessionMsg): void;
  agentHello(m: AgentHello): void;
  /** Agent messages other than hello, plus gateway errors. */
  control(m: Exclude<AgentMsg, AgentHello> | GwErrorMsg): void;
  video(m: VideoMsg, rxUs: number): void;
  audio(m: AudioMsg, rxUs: number): void;
  /** The socket went away (any reason); media/input state must be reset. */
  disconnected(): void;
}

export interface SessionOptions {
  machine: string;
  viewerInfo(): ViewerInfo;
  /** Parameters for `start`, chosen once the agent's displays are known. */
  startParams(hello: AgentHello): StreamParams;
}

type Phase = 'opening' | 'joining' | 'handshake' | 'streaming';

/**
 * The viewer session WebSocket (`/ws/view/{machine}`): connect → wait for the gateway's
 * `session` → `hello` → agent `hello` → `start`, PING/PONG, close-code handling and reconnects.
 */
export class Session {
  readonly clock = new ClockEstimator();
  private ws: WebSocket | null = null;
  private phase: Phase = 'opening';
  private readonly backoff = new Backoff();
  private userClosed = true;
  private timers: number[] = [];
  private pingId = 0;
  private openedUs = 0;
  private lastRxMs = 0;
  private pollAbort: AbortController | null = null;
  /** Binary frames that failed to parse (for the HUD). */
  malformed = 0;

  constructor(
    private readonly opts: SessionOptions,
    private readonly on: SessionHandlers,
  ) {
    // Hidden tabs may have timers throttled heavily; give the stream a fresh grace period.
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden) this.lastRxMs = performance.now();
    });
  }

  /** Connect now (also used for a manual reconnect after `stopped`). */
  start(): void {
    this.userClosed = false;
    this.backoff.reset();
    this.teardown();
    this.connect();
  }

  /** User-initiated disconnect; no reconnect. */
  stop(): void {
    this.userClosed = true;
    const hadSocket = this.ws !== null;
    this.teardown(1000, 'viewer disconnect');
    if (hadSocket) this.on.disconnected();
    this.on.status({ kind: 'closed' });
  }

  get streaming(): boolean {
    return this.phase === 'streaming' && this.ws?.readyState === WebSocket.OPEN;
  }

  get bufferedAmount(): number {
    return this.ws?.bufferedAmount ?? 0;
  }

  /** Send a binary frame; false if not connected. */
  send(data: Bytes): boolean {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN || this.phase === 'opening' || this.phase === 'joining') {
      return false;
    }
    ws.send(data);
    return true;
  }

  sendJson(m: ViewerMsg): boolean {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(m));
    return true;
  }

  // -------------------------------------------------------------------------

  private connect(): void {
    this.phase = 'opening';
    this.clock.reset();
    this.on.status({ kind: 'connecting', attempt: this.backoff.attempts });
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl(`/ws/view/${encodeURIComponent(this.opts.machine)}`));
    } catch (e) {
      this.closed(1006, e instanceof Error ? e.message : 'connect failed');
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onopen = () => {
      this.phase = 'joining';
      this.openedUs = nowUs();
      this.lastRxMs = performance.now();
      this.on.status({ kind: 'joining' });
      this.after(JOIN_TIMEOUT_MS, () => {
        if (this.phase === 'joining') this.abandon('agent did not join');
      });
    };
    ws.onmessage = (ev: MessageEvent<string | ArrayBuffer>) => this.receive(ev.data);
    ws.onclose = (ev) => {
      if (this.ws === ws) this.closed(ev.code, ev.reason);
    };
  }

  private receive(data: string | ArrayBuffer): void {
    const rxUs = nowUs();
    this.lastRxMs = rxUs / 1000;
    if (typeof data !== 'string') {
      this.receiveBinary(data, rxUs);
      return;
    }
    const m = parseViewerIncoming(data);
    if (!m) return;
    switch (m.t) {
      case 'session':
        if (this.phase !== 'joining') return;
        this.phase = 'handshake';
        this.on.status({ kind: 'handshake' });
        this.on.session(m);
        this.sendJson({
          t: 'hello',
          proto: PROTO_VERSION,
          min_proto: MIN_PROTO_VERSION,
          viewer: this.opts.viewerInfo(),
        });
        this.every(PING_INTERVAL_MS, () => this.tick());
        this.after(HELLO_TIMEOUT_MS, () => {
          if (this.phase === 'handshake') this.abandon('no hello from agent');
        });
        return;
      case 'hello':
        if (this.phase !== 'handshake') return;
        if (m.proto < MIN_PROTO_VERSION || m.proto > PROTO_VERSION) {
          this.teardown(1000, 'protocol mismatch');
          this.on.disconnected();
          this.on.status({ kind: 'stopped', code: 4005, reason: `agent speaks protocol ${m.proto}` });
          return;
        }
        this.phase = 'streaming';
        this.backoff.reset();
        this.on.agentHello(m);
        this.sendJson({ t: 'start', ...this.opts.startParams(m) });
        this.on.status({ kind: 'streaming' });
        return;
      default:
        this.on.control(m);
    }
  }

  private receiveBinary(data: ArrayBuffer, rxUs: number): void {
    let m;
    try {
      m = parse(data);
    } catch (e) {
      if (e instanceof ProtoError) {
        this.malformed++;
        return;
      }
      throw e;
    }
    switch (m.type) {
      case 'video':
        this.on.video(m, rxUs);
        return;
      case 'audio':
        this.on.audio(m, rxUs);
        return;
      case 'ping':
        this.ws?.send(encodePong(m.id, m.tOriginUs, BigInt(nowUs())));
        return;
      case 'pong': {
        const origin = Number(m.tOriginUs);
        // Ignore pongs to pings from an earlier connection.
        if (origin >= this.openedUs) this.clock.addSample(origin, Number(m.tReplyUs), rxUs);
        return;
      }
      default:
      // Unknown and viewer->agent types are ignored (forward compatibility).
    }
  }

  private tick(): void {
    if (this.phase === 'streaming' && !document.hidden && performance.now() - this.lastRxMs > RX_TIMEOUT_MS) {
      this.abandon('no data from agent');
      return;
    }
    this.ws?.send(encodePing(this.pingId++ >>> 0, BigInt(nowUs())));
  }

  /** Drop a socket we consider dead without waiting for the close handshake. */
  private abandon(reason: string): void {
    this.teardown(LOCAL_ABANDON, reason);
    this.closed(LOCAL_ABANDON, reason);
  }

  private closed(code: number, reason: string): void {
    this.teardown();
    this.on.disconnected();
    if (this.userClosed) {
      this.on.status({ kind: 'closed' });
      return;
    }
    switch (closeAction(code)) {
      case 'stop':
        this.on.status({ kind: 'stopped', code, reason });
        return;
      case 'poll':
        this.on.status({ kind: 'offline' });
        this.pollUntilOnline();
        return;
      case 'retry': {
        const delayMs = this.backoff.next();
        this.on.status({ kind: 'retrying', code, reason, delayMs });
        this.after(delayMs, () => this.connect());
      }
    }
  }

  private pollUntilOnline(): void {
    const poll = async () => {
      const abort = new AbortController();
      this.pollAbort = abort;
      try {
        const m = await fetchMachine(this.opts.machine, abort.signal);
        if (abort.signal.aborted) return;
        if (m?.online) {
          this.connect();
          return;
        }
      } catch {
        if (abort.signal.aborted) return;
        // Transient (network, auth redirect): keep polling.
      }
      this.after(OFFLINE_POLL_MS, () => void poll());
    };
    this.after(OFFLINE_POLL_MS, () => void poll());
  }

  /** Close the socket (if any) and cancel timers and polling. */
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
    this.phase = 'opening';
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
    this.timers.push(window.setInterval(f, ms));
  }
}
