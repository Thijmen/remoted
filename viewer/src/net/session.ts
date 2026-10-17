import { type AudioMsg, type Bytes, ProtoError, type VideoMsg, encodePing, encodePong, parse } from '../proto/bin';
import {
  type AgentHello,
  type AgentMsg,
  CloseCode,
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
import { type Access, fetchMachine, probeAccess, wsUrl } from './api';
import { Backoff, OFFLINE_POLL_MS, closeAction } from './backoff';
import { ClockEstimator } from './clock';

const PING_INTERVAL_MS = 1000;
/** The agent pings every 500 ms, so this much silence means the path is dead. */
const RX_TIMEOUT_MS = 5000;
/** The gateway gives the agent 10 s to join; allow for slack before giving up ourselves. */
const JOIN_TIMEOUT_MS = 15_000;
const HELLO_TIMEOUT_MS = 10_000;
/** Chromium only gives up on a hanging opening handshake after 240 s (half-up VPN, stuck proxy). */
const OPEN_TIMEOUT_MS = 10_000;
/** Bound for the HTTP requests made while reconnecting (offline poll, sign-in check). */
const FETCH_TIMEOUT_MS = 5000;
/** Sockets in a row refused before opening, while /api/me is fine, before saying so. */
const REFUSED_HINT_AFTER = 3;
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
  private lastTickMs = 0;
  private pollAbort: AbortController | null = null;
  /** Consecutive sockets that closed before opening. */
  private unopened = 0;
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
    // Page Lifecycle: a frozen page gets 'resume' before its queued tasks run.
    document.addEventListener('resume', () => {
      this.lastRxMs = performance.now();
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
    this.after(OPEN_TIMEOUT_MS, () => {
      if (this.ws === ws && this.phase === 'opening') this.abandon('connect timeout');
    });
    ws.onopen = () => {
      this.phase = 'joining';
      this.unopened = 0;
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
    const now = performance.now();
    // A late tick means this page was throttled or frozen (hidden tab, Energy Saver), not that
    // the network went quiet: queued messages may not have been processed yet. Restart the
    // grace period instead of abandoning a healthy socket.
    if (this.lastTickMs > 0 && now - this.lastTickMs > 2 * PING_INTERVAL_MS) this.lastRxMs = now;
    this.lastTickMs = now;
    if (this.phase === 'streaming' && !document.hidden && now - this.lastRxMs > RX_TIMEOUT_MS) {
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
    const neverOpened = this.phase === 'opening';
    console.info(`[rd] disconnected code=${code} reason=${reason || '-'} hidden=${document.hidden}`);
    this.teardown();
    this.on.disconnected();
    if (this.userClosed) {
      this.on.status({ kind: 'closed' });
      return;
    }
    if (neverOpened) this.unopened++;
    if (neverOpened && code === 1006) {
      // A refused upgrade (signed out, not allowed) looks exactly like a network failure: ask the HTTP API.
      this.checkAccess((access) => {
        const refused = access === 'ok' && this.unopened >= REFUSED_HINT_AFTER;
        this.react(code, refused ? 'session socket refused although signed in (proxy or Origin?)' : reason);
      });
      return;
    }
    this.react(code, reason);
  }

  private react(code: number, reason: string): void {
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

  /** Signed out or not allowed: retrying cannot help, a reload (which signs in again) can. */
  private denied(): void {
    this.on.status({ kind: 'stopped', code: CloseCode.FORBIDDEN, reason: 'signed out or not allowed' });
  }

  /** Probe /api/me, stop if access is denied, else continue with `then` (unless torn down meanwhile). */
  private checkAccess(then: (access: Access) => void): void {
    const abort = new AbortController();
    this.pollAbort = abort;
    void probeAccess(withTimeout(abort.signal)).then((access) => {
      if (abort.signal.aborted) return;
      this.pollAbort = null;
      if (access === 'denied') this.denied();
      else then(access);
    });
  }

  private pollUntilOnline(): void {
    const poll = async () => {
      const abort = new AbortController();
      this.pollAbort = abort;
      try {
        const m = await fetchMachine(this.opts.machine, withTimeout(abort.signal));
        if (abort.signal.aborted) return;
        if (m?.online) {
          this.connect();
          return;
        }
      } catch {
        if (abort.signal.aborted) return;
        // Network trouble or a timeout, but a sign-in redirect or a 403 also ends up here.
        const access = await probeAccess(withTimeout(abort.signal));
        if (abort.signal.aborted) return;
        if (access === 'denied') {
          this.denied();
          return;
        }
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

/** `signal`, also aborted after FETCH_TIMEOUT_MS (a hung request must not stall reconnecting). */
function withTimeout(signal: AbortSignal): AbortSignal {
  return AbortSignal.any([signal, AbortSignal.timeout(FETCH_TIMEOUT_MS)]);
}
