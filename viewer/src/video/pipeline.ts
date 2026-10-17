import { AckFlags, type Bytes, VideoFlags, type VideoMsg, encodeVideoAck, satU16 } from '../proto/bin';
import type { VideoConfigMsg } from '../proto/control';
import { base64ToBytes } from '../util/base64';
import { TimeWindow, nowUs } from '../util/stats';

export type RenderMode = 'immediate' | 'raf';

/** Counters read (and diffed) by the HUD. */
export class VideoStats {
  rxFrames = 0;
  rxBytes = 0;
  decoded = 0;
  drawn = 0;
  /** Frames discarded without decoding (wrong epoch / waiting for keyframe / no decoder). */
  discarded = 0;
  /** Decoded frames replaced by a newer one before the next animation frame (rAF mode). */
  skippedDraws = 0;
  decoderErrors = 0;
  keyframeRequests = 0;
  /** Decode time (submit → output), ms, over the last 2 s. */
  readonly decodeMs = new TimeWindow(2000);
  /** capture → paint, ms, over the last 5 s. Needs a clock offset estimate. */
  readonly captureToPaintMs = new TimeWindow(5000);
  codec = '';
  width = 0;
  height = 0;
  epoch = -1;
  /** 'HW' (prefer-hardware accepted), 'SW' (no-preference fallback) or '' before configure. */
  accel: '' | 'HW' | 'SW' = '';
  decodeQueue = 0;
  lastError = '';
}

export interface VideoPipelineDeps {
  canvas: HTMLCanvasElement;
  sendAck(ack: Bytes): void;
  requestKeyframe(reason: string): void;
  /** Agent clock (µs) → viewer clock (µs); null while the offset is unknown. */
  agentToViewerUs(agentUs: number): number | null;
  /** The video size changed (canvas resized). */
  resized?(width: number, height: number): void;
  /** A frame was just painted (used by the latency test). */
  drawn?(ctx: CanvasRenderingContext2D, paintUs: number): void;
}

interface InFlight {
  epoch: number;
  seq: number;
  rxUs: number;
  submitUs: number;
  captureUs: number;
}

interface Queued {
  msg: VideoMsg;
  rxUs: number;
}

const KEYFRAME_REQ_MIN_INTERVAL_MS = 500;
/** Hardware decode errors without a successful frame in between before falling back to software. */
const HW_FAILURES_BEFORE_SW = 3;
/** Safety bound for the "decode submitted, no output yet" table. */
const MAX_IN_FLIGHT = 120;
/** Frames buffered while `isConfigSupported` resolves (a fraction of a second in practice). */
const MAX_QUEUED = 240;

/**
 * WebCodecs decode pipeline: `video_config` → (re)configure; VIDEO → decode or discard; every
 * frame is ACKed exactly once (after decoder output, before drawing) so the agent's flow window
 * accounts for decoder backlog.
 */
export class VideoPipeline {
  readonly stats = new VideoStats();
  private readonly ctx: CanvasRenderingContext2D;
  private decoder: VideoDecoder | null = null;
  private config: VideoDecoderConfig | null = null;
  /** Epoch of the latest `video_config`; frames of other epochs are discarded. */
  private epoch = -1;
  private configSeq = 0;
  private configuring = false;
  private queued: Queued[] = [];
  private waitingForKey = true;
  private readonly inFlight = new Map<number, InFlight>();
  private lastTs = -1;
  private latest: { frame: VideoFrame; info: InFlight | undefined } | null = null;
  private rafPending = false;
  private lastKeyReqMs = Number.NEGATIVE_INFINITY;
  private hwFailures = 0;
  private softwareOnly = false;

  constructor(
    private readonly deps: VideoPipelineDeps,
    private renderMode: RenderMode = 'immediate',
  ) {
    const ctx = deps.canvas.getContext('2d', { desynchronized: true, alpha: false });
    if (!ctx) throw new Error('2D canvas unavailable');
    this.ctx = ctx;
  }

  setRenderMode(mode: RenderMode): void {
    this.renderMode = mode;
    if (mode === 'immediate') this.drawLatest();
  }

  /** Apply a `video_config` (new epoch). Frames of this epoch arriving meanwhile are queued. */
  async configure(cfg: VideoConfigMsg): Promise<void> {
    this.discardQueued();
    this.epoch = cfg.epoch;
    this.waitingForKey = true;
    this.stats.epoch = cfg.epoch;
    this.stats.codec = cfg.codec;
    const token = ++this.configSeq;
    this.configuring = true;
    let description: Uint8Array<ArrayBuffer>;
    try {
      description = base64ToBytes(cfg.description);
    } catch {
      this.configFailed('invalid avcC description');
      return;
    }
    const chosen = await chooseConfig(
      {
        codec: cfg.codec,
        codedWidth: cfg.width,
        codedHeight: cfg.height,
        description,
        optimizeForLatency: true,
      },
      !this.softwareOnly,
    );
    if (token !== this.configSeq) return;
    if (!chosen) {
      this.configFailed(`codec ${cfg.codec} ${cfg.width}x${cfg.height} not supported`);
      return;
    }
    this.config = chosen.config;
    this.stats.accel = chosen.hw ? 'HW' : 'SW';
    this.stats.lastError = '';
    try {
      this.ensureDecoder().configure(chosen.config);
    } catch (e) {
      this.configFailed(`configure failed: ${errorText(e)}`);
      return;
    }
    this.configuring = false;
    const queued = this.queued;
    this.queued = [];
    for (const q of queued) this.process(q.msg, q.rxUs);
  }

  onVideo(msg: VideoMsg, rxUs: number): void {
    this.stats.rxFrames++;
    this.stats.rxBytes += msg.payload.byteLength;
    if (msg.epoch !== this.epoch) {
      this.discard(msg);
      return;
    }
    if (this.configuring) {
      if (this.queued.length >= MAX_QUEUED) this.discard(this.queued.shift()!.msg);
      this.queued.push({ msg, rxUs });
      return;
    }
    this.process(msg, rxUs);
  }

  /** Socket lost: drop decoder state (the canvas keeps showing the last frame). */
  reset(): void {
    this.configSeq++;
    this.configuring = false;
    this.queued = [];
    this.inFlight.clear();
    this.closeDecoder();
    this.closeLatest();
    this.epoch = -1;
    this.waitingForKey = true;
    this.lastKeyReqMs = Number.NEGATIVE_INFINITY;
  }

  // -------------------------------------------------------------------------

  private process(msg: VideoMsg, rxUs: number): void {
    const decoder = this.decoder;
    const key = (msg.flags & VideoFlags.KEY) !== 0;
    if (!decoder || decoder.state !== 'configured') {
      this.discard(msg);
      return;
    }
    if (this.waitingForKey && !key) {
      this.discard(msg);
      this.requestKeyframe('waiting_for_key');
      return;
    }
    this.waitingForKey = false;
    // Unique, increasing timestamps (refinement frames may repeat a capture time).
    const captureUs = Number(msg.captureUs);
    const ts = Math.max(this.lastTs + 1, captureUs);
    this.lastTs = ts;
    const submitUs = nowUs();
    this.inFlight.set(ts, { epoch: msg.epoch, seq: msg.seq, rxUs, submitUs, captureUs });
    this.trimInFlight();
    try {
      decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data: msg.payload }));
    } catch (e) {
      this.inFlight.delete(ts);
      this.discard(msg);
      this.decoderFailed(e);
      return;
    }
    this.stats.decodeQueue = decoder.decodeQueueSize;
  }

  private onOutput(frame: VideoFrame): void {
    const now = nowUs();
    const info = this.inFlight.get(frame.timestamp);
    if (info) {
      this.inFlight.delete(frame.timestamp);
      // ACK first: the agent's window should open as early as possible.
      this.deps.sendAck(
        encodeVideoAck(0, info.epoch, info.seq, satU16(now - info.submitUs), satU16(info.submitUs - info.rxUs)),
      );
      this.stats.decodeMs.push(now / 1000, (now - info.submitUs) / 1000);
    }
    this.stats.decoded++;
    this.hwFailures = 0;
    this.stats.decodeQueue = this.decoder?.decodeQueueSize ?? 0;
    if (this.renderMode === 'immediate') {
      this.draw(frame, info);
      return;
    }
    if (this.latest) {
      this.latest.frame.close();
      this.stats.skippedDraws++;
    }
    this.latest = { frame, info };
    if (!this.rafPending) {
      this.rafPending = true;
      requestAnimationFrame(() => {
        this.rafPending = false;
        this.drawLatest();
      });
    }
  }

  private drawLatest(): void {
    const l = this.latest;
    this.latest = null;
    if (l) this.draw(l.frame, l.info);
  }

  private draw(frame: VideoFrame, info: InFlight | undefined): void {
    const { canvas } = this.deps;
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    try {
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
        this.stats.width = w;
        this.stats.height = h;
        this.deps.resized?.(w, h);
      }
      this.ctx.drawImage(frame, 0, 0, w, h);
    } finally {
      frame.close();
    }
    const paintUs = nowUs();
    this.stats.drawn++;
    if (info) {
      const captureViewerUs = this.deps.agentToViewerUs(info.captureUs);
      if (captureViewerUs !== null)
        this.stats.captureToPaintMs.push(paintUs / 1000, (paintUs - captureViewerUs) / 1000);
    }
    this.deps.drawn?.(this.ctx, paintUs);
  }

  private discard(msg: VideoMsg): void {
    this.stats.discarded++;
    this.deps.sendAck(encodeVideoAck(AckFlags.DISCARDED, msg.epoch, msg.seq, 0, 0));
  }

  private discardQueued(): void {
    for (const q of this.queued) this.discard(q.msg);
    this.queued = [];
  }

  private configFailed(reason: string): void {
    this.configuring = false;
    this.stats.lastError = reason;
    this.discardQueued();
    this.closeDecoder();
  }

  private ensureDecoder(): VideoDecoder {
    if (this.decoder && this.decoder.state !== 'closed') return this.decoder;
    const decoder = new VideoDecoder({
      output: (f) => {
        if (this.decoder === decoder) this.onOutput(f);
        else f.close();
      },
      error: (e) => {
        if (this.decoder === decoder) this.decoderFailed(e);
      },
    });
    this.decoder = decoder;
    return decoder;
  }

  /** Decoder error: recreate it with the last config and ask for a keyframe. */
  private decoderFailed(e: unknown): void {
    this.stats.decoderErrors++;
    this.stats.lastError = `decoder: ${errorText(e)}`;
    this.inFlight.clear();
    this.closeDecoder();
    this.waitingForKey = true;
    const config = this.config;
    if (!config) return;
    if (config.hardwareAcceleration === 'prefer-hardware' && ++this.hwFailures >= HW_FAILURES_BEFORE_SW) {
      this.softwareOnly = true;
      this.config = { ...config, hardwareAcceleration: 'no-preference' };
      this.stats.accel = 'SW';
    }
    try {
      this.ensureDecoder().configure(this.config!);
    } catch (err) {
      this.configFailed(`reconfigure failed: ${errorText(err)}`);
      return;
    }
    this.requestKeyframe('decoder_error', true);
  }

  private requestKeyframe(reason: string, force = false): void {
    const now = performance.now();
    if (!force && now - this.lastKeyReqMs < KEYFRAME_REQ_MIN_INTERVAL_MS) return;
    this.lastKeyReqMs = now;
    this.stats.keyframeRequests++;
    this.deps.requestKeyframe(reason);
  }

  /** Outputs never arrive for chunks a decoder silently dropped; keep the table bounded. */
  private trimInFlight(): void {
    while (this.inFlight.size > MAX_IN_FLIGHT) {
      const oldest = this.inFlight.keys().next();
      if (oldest.done) return;
      this.inFlight.delete(oldest.value);
    }
  }

  private closeDecoder(): void {
    const d = this.decoder;
    this.decoder = null;
    if (d && d.state !== 'closed') {
      try {
        d.close();
      } catch {
        // already closed by an error
      }
    }
  }

  private closeLatest(): void {
    this.latest?.frame.close();
    this.latest = null;
  }
}

/** Prefer hardware decode; fall back to no-preference ("SW"). */
async function chooseConfig(
  base: VideoDecoderConfig,
  allowHardware: boolean,
): Promise<{ config: VideoDecoderConfig; hw: boolean } | null> {
  const prefs: HardwareAcceleration[] = allowHardware ? ['prefer-hardware', 'no-preference'] : ['no-preference'];
  for (const hardwareAcceleration of prefs) {
    const config: VideoDecoderConfig = { ...base, hardwareAcceleration };
    try {
      const support = await VideoDecoder.isConfigSupported(config);
      if (support.supported) return { config, hw: hardwareAcceleration === 'prefer-hardware' };
    } catch {
      // TypeError for malformed configs: try the next preference, then give up.
    }
  }
  return null;
}

/** Which decoders this browser supports, for the `hello` message. */
export async function probeDecoders(): Promise<{ avc: boolean; hevc: boolean }> {
  if (typeof VideoDecoder === 'undefined') return { avc: false, hevc: false };
  const check = async (codec: string) => {
    try {
      return (await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })).supported === true;
    } catch {
      return false;
    }
  };
  const [avc, hevc] = await Promise.all([check('avc1.640033'), check('hvc1.1.6.L153.B0')]);
  return { avc, hevc };
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message || e.name : String(e);
}
