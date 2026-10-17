import type { AudioStats } from '../audio/player';
import type { ClockEstimator } from '../net/clock';
import type { AgentStats } from '../proto/control';
import { fmt, percentile } from '../util/stats';
import type { VideoStats } from '../video/pipeline';
import { formatBytes, h } from './dom';

export interface HudSources {
  video: VideoStats;
  clock: ClockEstimator;
  agent: AgentStats | null;
  audio: AudioStats | null;
  bufferedAmount: number;
  malformed: number;
  pointerLocked: boolean;
}

/** Per-second figures derived from the cumulative counters; also used for the JSON log. */
export interface HudSnapshot {
  t: number;
  fpsRx: number;
  fpsDecoded: number;
  fpsDrawn: number;
  kbpsRx: number;
  rttMinMs: number;
  rttP50Ms: number;
  c2pP50Ms: number;
  c2pP95Ms: number;
  decodeP50Ms: number;
}

interface Counters {
  t: number;
  rx: number;
  decoded: number;
  drawn: number;
  bytes: number;
}

/** Stats overlay (toggle: Ctrl+Alt+Shift+H). Call `tick` once per second. */
export class Hud {
  readonly el = h('div', { className: 'hud', hidden: true });
  private prev: Counters | null = null;
  last: HudSnapshot | null = null;

  get visible(): boolean {
    return !this.el.hidden;
  }

  toggle(force?: boolean): void {
    this.el.hidden = !(force ?? this.el.hidden);
  }

  tick(src: HudSources): void {
    const v = src.video;
    const now = performance.now();
    const cur: Counters = { t: now, rx: v.rxFrames, decoded: v.decoded, drawn: v.drawn, bytes: v.rxBytes };
    const p = this.prev;
    this.prev = cur;
    if (!p) return;
    const dt = (cur.t - p.t) / 1000;
    if (dt <= 0) return;
    v.decodeMs.prune(now);
    v.captureToPaintMs.prune(now);
    const c2p = v.captureToPaintMs.values();
    const snap: HudSnapshot = {
      t: Date.now(),
      fpsRx: (cur.rx - p.rx) / dt,
      fpsDecoded: (cur.decoded - p.decoded) / dt,
      fpsDrawn: (cur.drawn - p.drawn) / dt,
      kbpsRx: ((cur.bytes - p.bytes) * 8) / 1000 / dt,
      rttMinMs: (src.clock.rttMinUs ?? Number.NaN) / 1000,
      rttP50Ms: (src.clock.rttP50Us ?? Number.NaN) / 1000,
      c2pP50Ms: percentile(c2p, 50),
      c2pP95Ms: percentile(c2p, 95),
      decodeP50Ms: percentile(v.decodeMs.values(), 50),
    };
    this.last = snap;
    if (this.visible) this.el.textContent = render(snap, src);
  }
}

function render(s: HudSnapshot, src: HudSources): string {
  const v = src.video;
  const a = src.agent;
  const au = src.audio;
  const lines = [
    `fps   rx ${fmt(s.fpsRx)}  dec ${fmt(s.fpsDecoded)}  drawn ${fmt(s.fpsDrawn)}${a ? `  (cap ${fmt(a.cap_fps)} enc ${fmt(a.enc_fps)})` : ''}`,
    `kbps  rx ${fmt(s.kbpsRx)}  target ${a ? fmt(a.target_kbps) : '–'}  enc ${a ? fmt(a.enc_kbps) : '–'}`,
    `rtt   min ${fmt(s.rttMinMs, 1)}  p50 ${fmt(s.rttP50Ms, 1)} ms${a ? `  agent min ${fmt(a.rtt_min_ms, 1)}  q ${fmt(a.q_ms, 1)} ms` : ''}`,
    `c→p   p50 ${fmt(s.c2pP50Ms, 1)}  p95 ${fmt(s.c2pP95Ms, 1)} ms`,
    `dec   ${fmt(s.decodeP50Ms, 2)} ms  queue ${v.decodeQueue}${a ? `   enc p50 ${fmt(a.enc_ms_p50, 1)} p99 ${fmt(a.enc_ms_p99, 1)} ms` : ''}`,
    `drops discarded ${v.discarded}  skipped ${v.skippedDraws}${a ? `  window ${a.drop_window}/s  post ${a.drop_post}` : ''}`,
    `fly   ${a ? `${a.inflight_frames}/${a.window_frames} frames  ${formatBytes(a.inflight_bytes)}/${formatBytes(a.window_bytes)}  enc ${fmt(a.window_enc_ms, 1)} ms` : '–'}  ws buf ${formatBytes(src.bufferedAmount)}`,
  ];
  if (au) {
    const w = au.worklet;
    lines.push(
      w
        ? `audio ${fmt(w.bufferedMs)} ms (target ${fmt(w.targetMs)})  underruns ${w.underruns}  drops ${w.drops}  ${au.transport}`
        : `audio starting (${au.transport})`,
    );
  }
  lines.push(
    `video ${v.codec || '–'} ${v.width}×${v.height} ${v.accel || '–'}  epoch ${v.epoch}  kf req ${v.keyframeRequests}  dec err ${v.decoderErrors}`,
  );
  if (src.pointerLocked) lines.push('pointer locked (Esc to release)');
  const errors = [v.lastError, au?.lastError, src.malformed ? `${src.malformed} malformed frames` : ''].filter(Boolean);
  if (errors.length) lines.push(`err   ${errors.join(' · ')}`);
  return lines.join('\n');
}
