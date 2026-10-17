import { wsUrl } from '../net/api';
import { ProtoError, encodePing, encodePong, encodeProbeUp, encodeVideoAck, parse } from '../proto/bin';
import { type ProbeCtl, type ProbeDone, type ProbeHello, type ProbeTest, parseProbeEvt } from '../proto/control';
import { nowUs } from '../util/stats';

export interface DownArrival {
  seq: number;
  /** Gateway clock when the frame was sent, ms. */
  serverMs: number;
  /** performance.now() at receipt. */
  rxMs: number;
  bytes: number;
}

export interface CloseInfo {
  code: number;
  reason: string;
  /** Not initiated by us. */
  unexpected: boolean;
  atMs: number;
}

export class ProbeSocketError extends Error {}

const OPEN_TIMEOUT_MS = 10_000;

/** One `/ws/probe` connection: hello, run/done, binary PING/PONG, VIDEO ACKs and PROBE_UP. */
export class ProbeSocket {
  onFrame: ((f: DownArrival) => void) | null = null;
  onRtt: ((rttMs: number) => void) | null = null;
  closeInfo: CloseInfo | null = null;
  private pingTimer = 0;
  private pingId = 0;
  private localClose = false;
  private pendingRun: { resolve(d: ProbeDone): void; reject(e: Error): void; test: ProbeTest } | null = null;
  private readonly closeWaiters: Array<(c: CloseInfo) => void> = [];

  private constructor(
    private readonly ws: WebSocket,
    readonly hello: ProbeHello,
    readonly connectMs: number,
  ) {
    ws.onmessage = (ev: MessageEvent<string | ArrayBuffer>) => this.receive(ev.data);
    ws.onclose = (ev) => this.closed(ev.code, ev.reason);
  }

  /** Open a socket and wait for the gateway's hello. */
  static open(): Promise<ProbeSocket> {
    return new Promise((resolve, reject) => {
      const t0 = performance.now();
      let ws: WebSocket;
      try {
        ws = new WebSocket(wsUrl('/ws/probe'));
      } catch (e) {
        reject(new ProbeSocketError(e instanceof Error ? e.message : 'cannot open WebSocket'));
        return;
      }
      ws.binaryType = 'arraybuffer';
      const timer = window.setTimeout(() => {
        ws.close();
        reject(new ProbeSocketError('no hello within 10 s'));
      }, OPEN_TIMEOUT_MS);
      ws.onmessage = (ev: MessageEvent<string | ArrayBuffer>) => {
        if (typeof ev.data !== 'string') return;
        const m = parseProbeEvt(ev.data);
        if (m?.t !== 'hello') return;
        clearTimeout(timer);
        resolve(new ProbeSocket(ws, m, performance.now() - t0));
      };
      ws.onclose = (ev) => {
        clearTimeout(timer);
        reject(new ProbeSocketError(`closed before hello (code ${ev.code}${ev.reason ? `: ${ev.reason}` : ''})`));
      };
    });
  }

  get open(): boolean {
    return this.ws.readyState === WebSocket.OPEN;
  }

  get bufferedAmount(): number {
    return this.ws.bufferedAmount;
  }

  /** Start a test; resolves with the gateway's `done`. Rejects if the socket closes first. */
  run(
    test: ProbeTest,
    params: { kbps?: number; fps?: number; seconds: number; windowed?: boolean },
  ): Promise<ProbeDone> {
    if (this.pendingRun) return Promise.reject(new ProbeSocketError('a run is already active'));
    if (!this.open) return Promise.reject(new ProbeSocketError('socket closed'));
    const msg: ProbeCtl = {
      t: 'run',
      test,
      kbps: params.kbps ?? 0,
      fps: params.fps ?? 60,
      seconds: params.seconds,
      windowed: params.windowed ?? false,
    };
    return new Promise((resolve, reject) => {
      this.pendingRun = { resolve, reject, test };
      this.ws.send(JSON.stringify(msg));
    });
  }

  stopRun(): void {
    if (this.open) this.ws.send(JSON.stringify({ t: 'stop' } satisfies ProbeCtl));
  }

  startPings(intervalMs: number): void {
    this.stopPings();
    this.pingTimer = window.setInterval(() => {
      if (this.open) this.ws.send(encodePing(this.pingId++ >>> 0, BigInt(nowUs())));
    }, intervalMs);
  }

  stopPings(): void {
    clearInterval(this.pingTimer);
    this.pingTimer = 0;
  }

  /** Send one PROBE_UP frame with `payloadLen` random bytes. */
  sendUp(payloadLen: number): void {
    const b = encodeProbeUp(payloadLen);
    // getRandomValues is limited to 64 KiB per call; random data defeats any compression.
    for (let o = 2; o < b.length; o += 65_536) crypto.getRandomValues(b.subarray(o, Math.min(b.length, o + 65_536)));
    this.ws.send(b);
  }

  /** Resolves when the socket closes (immediately if it already has). */
  whenClosed(): Promise<CloseInfo> {
    if (this.closeInfo) return Promise.resolve(this.closeInfo);
    return new Promise((resolve) => this.closeWaiters.push(resolve));
  }

  close(): void {
    this.localClose = true;
    this.stopPings();
    if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)
      this.ws.close(1000, 'probe done');
  }

  private receive(data: string | ArrayBuffer): void {
    const rxMs = performance.now();
    if (typeof data === 'string') {
      const m = parseProbeEvt(data);
      if (!m) return;
      if (m.t === 'done' && this.pendingRun) {
        const p = this.pendingRun;
        this.pendingRun = null;
        p.resolve(m);
      } else if (m.t === 'error' && this.pendingRun) {
        const p = this.pendingRun;
        this.pendingRun = null;
        p.reject(new ProbeSocketError(`gateway error ${m.code}${m.msg ? `: ${m.msg}` : ''}`));
      }
      return;
    }
    let m;
    try {
      m = parse(data);
    } catch (e) {
      if (e instanceof ProtoError) return;
      throw e;
    }
    switch (m.type) {
      case 'video':
        // ACK on receipt: windowed runs are gated by these, exactly like the agent.
        this.ws.send(encodeVideoAck(0, m.epoch, m.seq, 0, 0));
        this.onFrame?.({ seq: m.seq, serverMs: Number(m.captureUs) / 1000, rxMs, bytes: data.byteLength });
        return;
      case 'pong':
        this.onRtt?.(rxMs - Number(m.tOriginUs) / 1000);
        return;
      case 'ping':
        this.ws.send(encodePong(m.id, m.tOriginUs, BigInt(nowUs())));
        return;
      default:
    }
  }

  private closed(code: number, reason: string): void {
    this.stopPings();
    const info: CloseInfo = { code, reason, unexpected: !this.localClose, atMs: performance.now() };
    this.closeInfo = info;
    if (this.pendingRun) {
      const p = this.pendingRun;
      this.pendingRun = null;
      p.reject(
        new ProbeSocketError(`socket closed during ${p.test} test (code ${code}${reason ? `: ${reason}` : ''})`),
      );
    }
    for (const w of this.closeWaiters.splice(0)) w(info);
  }
}
