import {
  type Criterion,
  DownCollector,
  type DownSummary,
  type Outcome,
  type Summary,
  summarize,
  verdict,
} from './analysis';
import { type CloseInfo, ProbeSocket } from './socket';

export interface ProbePlan {
  mode: 'quick' | 'full';
  idleRttSeconds: number;
  ladderMbps: number[];
  ladderSeconds: number;
  soakMinutes: number;
  soakKbps: number;
  /** Idle-survival sockets run in parallel with everything else. */
  idleSurvivalMinutes: number[];
  upKbps: number;
  upSeconds: number;
  fetchKbps: number;
  fetchSeconds: number;
}

export function quickPlan(): ProbePlan {
  return {
    mode: 'quick',
    idleRttSeconds: 15,
    ladderMbps: [2, 5, 10, 20],
    ladderSeconds: 8,
    soakMinutes: 0.5,
    soakKbps: 15_000,
    idleSurvivalMinutes: [1],
    upKbps: 1000,
    upSeconds: 10,
    fetchKbps: 10_000,
    fetchSeconds: 10,
  };
}

export function fullPlan(soakMinutes = 30): ProbePlan {
  return {
    mode: 'full',
    idleRttSeconds: 60,
    ladderMbps: [2, 5, 10, 20, 40],
    ladderSeconds: 20,
    soakMinutes,
    soakKbps: 15_000,
    idleSurvivalMinutes: [2, 5, 10, 20],
    upKbps: 1000,
    upSeconds: 30,
    fetchKbps: 10_000,
    fetchSeconds: 10,
  };
}

/** Sequential test time; idle survival overlaps unless it is the longest part. */
export function planSeconds(p: ProbePlan): number {
  const seq =
    p.idleRttSeconds + p.ladderMbps.length * p.ladderSeconds + p.soakMinutes * 60 + p.upSeconds + p.fetchSeconds;
  return Math.max(seq, Math.max(0, ...p.idleSurvivalMinutes) * 60);
}

export interface LadderStep {
  targetKbps: number;
  ok: boolean;
  down: DownSummary;
  rtt: Summary;
  gatewayFrames: number;
  error?: string;
}

export interface ProbeReport {
  version: 1;
  mode: ProbePlan['mode'];
  plan: ProbePlan;
  startedAt: string;
  finishedAt: string;
  stopped: boolean;
  env: Record<string, unknown>;
  hello: { client_ip: string; user: string; connectMs: number } | null;
  idleRtt: Summary | null;
  ladder: LadderStep[];
  soak: (Omit<LadderStep, 'targetKbps'> & { kbpsMax: number; minutes: number; windowDrops: number }) | null;
  up: {
    kbps: number;
    targetKbps: number;
    bytes: number;
    elapsedMs: number;
    blockedTicks: number;
    rtt: Summary;
    error?: string;
  } | null;
  fetch: { kbps: number; ttfbMs: number; down: DownSummary; protocol: string; error?: string } | null;
  idleSurvival: Array<{ minutes: number; ok: boolean; detail: string }>;
  closes: Array<CloseInfo & { socket: string }>;
  errors: string[];
  verdict: { overall: Outcome; criteria: Criterion[] };
}

export interface ProbeEvents {
  /** Current step and overall progress 0..1. */
  progress(label: string, fraction: number): void;
  /** Live numbers for the running step. */
  live(text: string): void;
  /** A finished test row. */
  row(test: string, ok: boolean | null, summary: string): void;
}

const LADDER_PING_MS = 250;
const SOAK_PING_MS = 1000;
const UP_TICK_MS = 20;
const UP_MAX_BUFFERED = 256 * 1024;
const FETCH_CHUNK_MS = 20;

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const mbps = (kbps: number) => `${(kbps / 1000).toFixed(1)} Mbps`;
const ms = (v: number) => (Number.isFinite(v) ? `${v.toFixed(0)} ms` : '–');

function sleep(msDelay: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const t = setTimeout(resolve, msDelay);
    signal.addEventListener(
      'abort',
      () => {
        clearTimeout(t);
        resolve();
      },
      { once: true },
    );
  });
}

export function environment(): Record<string, unknown> {
  const nav = performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming | undefined;
  const conn = (navigator as Navigator & { connection?: { effectiveType?: string; downlink?: number; rtt?: number } })
    .connection;
  return {
    origin: location.origin,
    protocol: location.protocol,
    secureContext: window.isSecureContext,
    crossOriginIsolated: window.crossOriginIsolated,
    nextHopProtocol: nav?.nextHopProtocol ?? '',
    userAgent: navigator.userAgent,
    hardwareConcurrency: navigator.hardwareConcurrency,
    connection: conn ? { effectiveType: conn.effectiveType, downlinkMbps: conn.downlink, rttMs: conn.rtt } : null,
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
  };
}

/** Runs the M0 network probe suite. */
export class ProbeRunner {
  private main: ProbeSocket | null = null;
  private readonly idleSockets: ProbeSocket[] = [];
  private doneSeconds = 0;
  private readonly total: number;
  private readonly report: ProbeReport;

  constructor(
    private readonly plan: ProbePlan,
    private readonly ev: ProbeEvents,
    private readonly signal: AbortSignal,
  ) {
    this.total = planSeconds(plan);
    this.report = {
      version: 1,
      mode: plan.mode,
      plan,
      startedAt: new Date().toISOString(),
      finishedAt: '',
      stopped: false,
      env: environment(),
      hello: null,
      idleRtt: null,
      ladder: [],
      soak: null,
      up: null,
      fetch: null,
      idleSurvival: [],
      closes: [],
      errors: [],
      verdict: { overall: 'na', criteria: [] },
    };
    signal.addEventListener('abort', () => this.abortSockets(), { once: true });
  }

  /** Ask the gateway to end running tests; force-close if it does not answer promptly. */
  private abortSockets(): void {
    const sockets = [this.main, ...this.idleSockets];
    for (const s of sockets) s?.stopRun();
    setTimeout(() => {
      for (const s of sockets) s?.close();
    }, 2000);
  }

  private watchClose(s: ProbeSocket, name: string): void {
    void s.whenClosed().then((c) => {
      if (c.unexpected) this.report.closes.push({ ...c, socket: name });
    });
  }

  async run(): Promise<ProbeReport> {
    const r = this.report;
    const env = r.env;
    this.ev.row(
      'Connection',
      null,
      `${env.protocol === 'https:' ? 'HTTPS' : 'HTTP'} · page via ${env.nextHopProtocol || 'unknown protocol'}`,
    );
    const wsOk = (await this.ensureMain()) !== null;
    const idle = wsOk ? this.startIdleSurvival() : Promise.resolve();
    if (wsOk) {
      await this.idleRtt();
      await this.ladder();
      await this.soak();
      await this.upstream();
    }
    await this.fetchStream();
    if (!this.signal.aborted && this.plan.idleSurvivalMinutes.length)
      this.ev.progress('Waiting for idle-survival sockets…', this.fraction());
    await idle;
    this.main?.close();
    r.stopped = this.signal.aborted;
    r.finishedAt = new Date().toISOString();
    r.verdict = verdict({
      secure: env.protocol === 'https:',
      wsOk,
      idleRtt: r.idleRtt,
      ladder: r.ladder.map((s) => ({ targetKbps: s.targetKbps, kbps: s.down.kbps, rtt: s.rtt })),
      soak: r.soak && { kbps: r.soak.down.kbps, rtt: r.soak.rtt, minutes: r.soak.minutes },
      unexpectedDisconnects: r.closes.filter((c) => c.unexpected).length,
      idleSurvival: r.idleSurvival,
    });
    this.ev.progress(r.stopped ? 'Stopped' : 'Done', 1);
    this.ev.live('');
    return r;
  }

  private fraction(): number {
    return this.total > 0 ? Math.min(1, this.doneSeconds / this.total) : 1;
  }

  /** The main socket, reopened if a previous test lost it. */
  private async ensureMain(): Promise<ProbeSocket | null> {
    if (this.main?.open) return this.main;
    try {
      const s = await ProbeSocket.open();
      this.main = s;
      this.report.hello ??= { client_ip: s.hello.client_ip, user: s.hello.user, connectMs: Math.round(s.connectMs) };
      this.watchClose(s, 'main');
      if (this.report.ladder.length === 0 && !this.report.idleRtt) {
        this.ev.row(
          'WebSocket',
          true,
          `connected in ${ms(s.connectMs)} · client ${s.hello.client_ip || '?'} · ${s.hello.user || 'no user'}`,
        );
      }
      return s;
    } catch (e) {
      this.report.errors.push(`probe socket: ${errText(e)}`);
      this.ev.row('WebSocket', false, errText(e));
      return null;
    }
  }

  private startIdleSurvival(): Promise<void> {
    const runs = this.plan.idleSurvivalMinutes.map(async (minutes) => {
      let detail = '';
      let ok = false;
      try {
        const s = await ProbeSocket.open();
        this.idleSockets.push(s);
        this.watchClose(s, `idle-${minutes}m`);
        const done = await s.run('idle', { seconds: Math.round(minutes * 60) });
        ok = !done.stopped;
        detail = done.stopped ? 'stopped' : `survived ${minutes} min idle`;
        s.close();
      } catch (e) {
        detail = errText(e);
      }
      if (this.signal.aborted && !ok) return;
      this.report.idleSurvival.push({ minutes, ok, detail });
      this.ev.row(`Idle survival ${minutes} min`, ok, detail);
    });
    return Promise.all(runs).then(() => undefined);
  }

  private async idleRtt(): Promise<void> {
    const s = await this.ensureMain();
    if (!s || this.signal.aborted) return;
    this.ev.progress(`Idle RTT (${this.plan.idleRttSeconds} s)`, this.fraction());
    const rtts: number[] = [];
    s.onRtt = (r) => {
      rtts.push(r);
      this.ev.live(`RTT ${r.toFixed(1)} ms · ${rtts.length} samples`);
    };
    s.startPings(LADDER_PING_MS);
    await sleep(this.plan.idleRttSeconds * 1000, this.signal);
    s.stopPings();
    s.onRtt = null;
    this.doneSeconds += this.plan.idleRttSeconds;
    const sum = summarize(rtts);
    this.report.idleRtt = sum;
    this.ev.row(
      'Idle RTT',
      sum.n > 0,
      `min ${ms(sum.min)} · p50 ${ms(sum.p50)} · p95 ${ms(sum.p95)} · max ${ms(sum.max)} (${sum.n} pings)`,
    );
  }

  /** One downstream run with RTT pings through the same socket. */
  private async down(
    s: ProbeSocket,
    kbps: number,
    seconds: number,
    windowed: boolean,
    pingMs: number,
  ): Promise<{ down: DownSummary; rtt: Summary; frames: number; windowDrops: number; error?: string }> {
    const start = performance.now();
    const col = new DownCollector(start);
    const rtts: number[] = [];
    let lastRtt = Number.NaN;
    s.onFrame = (f) => col.add(f.rxMs, f.bytes, f.serverMs);
    s.onRtt = (r) => {
      rtts.push(r);
      lastRtt = r;
    };
    s.startPings(pingMs);
    const live = setInterval(
      () => this.ev.live(`${mbps(col.currentKbps(performance.now()))} · RTT ${ms(lastRtt)} · ${col.count} frames`),
      1000,
    );
    let frames = 0;
    let windowDrops = 0;
    let error: string | undefined;
    try {
      const done = await s.run('down', { kbps, fps: 60, seconds, windowed });
      frames = done.frames;
      windowDrops = done.window_drops;
    } catch (e) {
      error = errText(e);
      this.report.errors.push(error);
    } finally {
      clearInterval(live);
      s.stopPings();
      s.onFrame = null;
      s.onRtt = null;
    }
    return {
      down: col.summary(performance.now()),
      rtt: summarize(rtts),
      frames,
      windowDrops,
      ...(error ? { error } : {}),
    };
  }

  private async ladder(): Promise<void> {
    for (const target of this.plan.ladderMbps) {
      const s = await this.ensureMain();
      if (!s || this.signal.aborted) return;
      const targetKbps = target * 1000;
      this.ev.progress(`Downstream ${target} Mbps (${this.plan.ladderSeconds} s)`, this.fraction());
      const res = await this.down(s, targetKbps, this.plan.ladderSeconds, false, LADDER_PING_MS);
      this.doneSeconds += this.plan.ladderSeconds;
      const ok = !res.error && res.down.kbps >= targetKbps * 0.9;
      this.report.ladder.push({
        targetKbps,
        ok,
        down: res.down,
        rtt: res.rtt,
        gatewayFrames: res.frames,
        ...(res.error ? { error: res.error } : {}),
      });
      this.ev.row(
        `Down ${target} Mbps`,
        ok,
        res.error ??
          `${mbps(res.down.kbps)} · RTT p50 ${ms(res.rtt.p50)} p95 ${ms(res.rtt.p95)} · queue p95 ${ms(res.down.queueP95Ms)} · max gap ${ms(res.down.maxGapMs)} · bursts ${res.down.bursts}`,
      );
      // Far below target: the path is saturated, higher steps only add queueing.
      if (res.down.kbps < targetKbps * 0.5) {
        const skipped = this.plan.ladderMbps.filter((m) => m > target);
        this.doneSeconds += skipped.length * this.plan.ladderSeconds;
        if (skipped.length)
          this.ev.row('Down (higher steps)', null, `skipped ${skipped.join('/')} Mbps: path saturated`);
        return;
      }
    }
  }

  private async soak(): Promise<void> {
    const s = await this.ensureMain();
    const { soakMinutes, soakKbps } = this.plan;
    if (!s || this.signal.aborted || soakMinutes <= 0) return;
    const seconds = Math.round(soakMinutes * 60);
    this.ev.progress(`Windowed soak at up to ${mbps(soakKbps)} (${soakMinutes} min)`, this.fraction());
    const res = await this.down(s, soakKbps, seconds, true, SOAK_PING_MS);
    this.doneSeconds += seconds;
    const ok = !res.error && res.down.kbps >= 10_000;
    this.report.soak = {
      ok,
      down: res.down,
      rtt: res.rtt,
      gatewayFrames: res.frames,
      windowDrops: res.windowDrops,
      kbpsMax: soakKbps,
      minutes: soakMinutes,
      ...(res.error ? { error: res.error } : {}),
    };
    this.ev.row(
      `Soak ${soakMinutes} min (windowed)`,
      ok,
      res.error ??
        `${mbps(res.down.kbps)} avg · worst second ${mbps(res.down.minSecondKbps)} · RTT p95 ${ms(res.rtt.p95)} · window drops ${res.windowDrops} · stalls ${res.down.stalls}`,
    );
  }

  private async upstream(): Promise<void> {
    const s = await this.ensureMain();
    const { upKbps, upSeconds } = this.plan;
    if (!s || this.signal.aborted || upSeconds <= 0) return;
    this.ev.progress(`Upstream ${mbps(upKbps)} (${upSeconds} s)`, this.fraction());
    const payload = Math.max(1, Math.round((upKbps * 1000) / 8 / (1000 / UP_TICK_MS)) - 2);
    let blockedTicks = 0;
    const rtts: number[] = [];
    s.onRtt = (r) => rtts.push(r);
    s.startPings(LADDER_PING_MS);
    const tick = setInterval(() => {
      if (!s.open) return;
      if (s.bufferedAmount > UP_MAX_BUFFERED) blockedTicks++;
      else s.sendUp(payload);
    }, UP_TICK_MS);
    const live = setInterval(
      () => this.ev.live(`buffered ${(s.bufferedAmount / 1024).toFixed(0)} KiB · blocked ticks ${blockedTicks}`),
      1000,
    );
    try {
      const done = await s.run('up', { kbps: upKbps, seconds: upSeconds });
      const kbps = done.elapsed_ms > 0 ? (done.bytes * 8) / done.elapsed_ms : 0;
      this.report.up = {
        kbps,
        targetKbps: upKbps,
        bytes: done.bytes,
        elapsedMs: done.elapsed_ms,
        blockedTicks,
        rtt: summarize(rtts),
      };
      this.ev.row(
        'Upstream',
        kbps >= upKbps * 0.9,
        `${mbps(kbps)} received by gateway · RTT p95 ${ms(summarize(rtts).p95)} · blocked ticks ${blockedTicks}`,
      );
    } catch (e) {
      const error = errText(e);
      this.report.errors.push(error);
      this.report.up = {
        kbps: 0,
        targetKbps: upKbps,
        bytes: 0,
        elapsedMs: 0,
        blockedTicks,
        rtt: summarize(rtts),
        error,
      };
      this.ev.row('Upstream', false, error);
    } finally {
      clearInterval(tick);
      clearInterval(live);
      s.stopPings();
      s.onRtt = null;
    }
    this.doneSeconds += upSeconds;
  }

  /** HTTP streaming fallback: one long fetch whose chunks carry the server send time. */
  private async fetchStream(): Promise<void> {
    const { fetchKbps, fetchSeconds } = this.plan;
    if (this.signal.aborted || fetchSeconds <= 0) return;
    this.ev.progress(`HTTP fetch stream ${mbps(fetchKbps)} (${fetchSeconds} s)`, this.fraction());
    const chunkBytes = Math.max(12, Math.round((fetchKbps * FETCH_CHUNK_MS) / 8));
    const url = `/api/probe/down?kbps=${fetchKbps}&seconds=${fetchSeconds}&chunk_ms=${FETCH_CHUNK_MS}`;
    const start = performance.now();
    const col = new DownCollector(start);
    let ttfbMs = Number.NaN;
    const live = setInterval(
      () => this.ev.live(`${mbps(col.currentKbps(performance.now()))} · ${col.count} chunks`),
      1000,
    );
    try {
      const res = await fetch(url, { cache: 'no-store', signal: this.signal });
      ttfbMs = performance.now() - start;
      if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
      const reader = res.body.getReader();
      const header = new Uint8Array(12);
      const hv = new DataView(header.buffer);
      let inChunk = 0;
      let serverMs = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        const now = performance.now();
        let off = 0;
        while (off < value.length) {
          if (inChunk < header.length) {
            const n = Math.min(header.length - inChunk, value.length - off);
            header.set(value.subarray(off, off + n), inChunk);
            if (inChunk + n === header.length) serverMs = Number(hv.getBigUint64(0, true)) / 1000;
          }
          const take = Math.min(chunkBytes - inChunk, value.length - off);
          inChunk += take;
          off += take;
          if (inChunk === chunkBytes) {
            col.add(now, chunkBytes, serverMs);
            inChunk = 0;
          }
        }
      }
      const down = col.summary(performance.now());
      const entry = performance.getEntriesByName(new URL(url, location.href).href).at(-1) as
        PerformanceResourceTiming | undefined;
      this.report.fetch = { kbps: down.kbps, ttfbMs, down, protocol: entry?.nextHopProtocol ?? '' };
      this.ev.row(
        'HTTP fetch stream',
        down.kbps >= fetchKbps * 0.9 && down.bursts === 0,
        `${mbps(down.kbps)} · first byte ${ms(ttfbMs)} · queue p95 ${ms(down.queueP95Ms)} · max gap ${ms(down.maxGapMs)} · bursts ${down.bursts}${entry?.nextHopProtocol ? ` · ${entry.nextHopProtocol}` : ''}`,
      );
    } catch (e) {
      if (this.signal.aborted) return;
      const error = errText(e);
      this.report.errors.push(`fetch stream: ${error}`);
      this.report.fetch = { kbps: 0, ttfbMs, down: col.summary(performance.now()), protocol: '', error };
      this.ev.row('HTTP fetch stream', false, error);
    } finally {
      clearInterval(live);
      this.doneSeconds += fetchSeconds;
    }
  }
}

/** Send the report to the gateway log. */
export async function postReport(report: ProbeReport): Promise<boolean> {
  try {
    const res = await fetch('/api/probe/report', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(report),
    });
    return res.ok;
  } catch {
    return false;
  }
}
