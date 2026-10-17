export interface JitterConfig {
  sampleRate: number;
  /** Buffer level at which playback (re)starts. */
  startMs: number;
  minMs: number;
  maxMs: number;
  /** Drop a chunk while the level exceeds target + this. */
  dropAboveMs: number;
  /** Size of one dropped chunk (one Opus packet). */
  chunkMs: number;
  /** Target increase after an underrun. */
  underrunStepMs: number;
  /** Target decrease after `stableMs` of playback without an underrun. */
  relaxStepMs: number;
  stableMs: number;
}

export const DEFAULT_JITTER: JitterConfig = {
  sampleRate: 48_000,
  startMs: 40,
  minMs: 30,
  maxMs: 120,
  dropAboveMs: 60,
  chunkMs: 10,
  underrunStepMs: 10,
  relaxStepMs: 5,
  stableMs: 10_000,
};

export interface RenderDecision {
  /** Output buffered audio (true) or silence (false). */
  play: boolean;
  /** Frames to drop from the buffer before reading (catch-up). */
  skip: number;
}

export interface JitterStats {
  targetMs: number;
  playing: boolean;
  underruns: number;
  drops: number;
}

/**
 * Adaptive jitter buffer policy, evaluated once per render quantum by the AudioWorklet:
 * prime to the target level, re-prime (and raise the target) after an underrun, drop 10 ms
 * chunks when the level runs more than 60 ms above target, and slowly lower the target while
 * playback is stable.
 */
export class JitterPolicy {
  private target: number;
  private playing = false;
  private sinceUnderrunMs = 0;
  private underruns = 0;
  private drops = 0;

  constructor(private readonly cfg: JitterConfig = DEFAULT_JITTER) {
    this.target = cfg.startMs;
  }

  next(availableFrames: number, quantumFrames: number): RenderDecision {
    const { cfg } = this;
    const msToFrames = (ms: number) => Math.round((ms * cfg.sampleRate) / 1000);
    if (!this.playing) {
      if (availableFrames < msToFrames(this.target) || availableFrames < quantumFrames) {
        return { play: false, skip: 0 };
      }
      this.playing = true;
    }
    if (availableFrames < quantumFrames) {
      this.underruns++;
      this.playing = false;
      this.sinceUnderrunMs = 0;
      this.target = Math.min(cfg.maxMs, this.target + cfg.underrunStepMs);
      return { play: false, skip: 0 };
    }
    this.sinceUnderrunMs += (quantumFrames * 1000) / cfg.sampleRate;
    if (this.sinceUnderrunMs >= cfg.stableMs) {
      this.sinceUnderrunMs = 0;
      this.target = Math.max(cfg.minMs, this.target - cfg.relaxStepMs);
    }
    const chunk = msToFrames(cfg.chunkMs);
    if (availableFrames - chunk >= quantumFrames && availableFrames > msToFrames(this.target + cfg.dropAboveMs)) {
      this.drops++;
      return { play: true, skip: chunk };
    }
    return { play: true, skip: 0 };
  }

  /** Stream restarted: prime again from the start level (keeps counters). */
  reset(): void {
    this.playing = false;
    this.sinceUnderrunMs = 0;
    this.target = this.cfg.startMs;
  }

  stats(): JitterStats {
    return { targetMs: this.target, playing: this.playing, underruns: this.underruns, drops: this.drops };
  }
}
