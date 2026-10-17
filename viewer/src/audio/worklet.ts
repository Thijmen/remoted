// AudioWorkletProcessor: plays PCM from the ring through the adaptive jitter policy.
// Loaded with audioWorklet.addModule (bundled separately via `?worker&url`).

import { DEFAULT_JITTER, JitterPolicy } from './jitter';
import { RING_FRAMES, type ToWorklet, WORKLET_NAME, type WorkletStats } from './messages';
import { AudioRing } from './ring';

// AudioWorkletGlobalScope (not part of TypeScript's DOM lib).
declare abstract class AudioWorkletProcessor {
  readonly port: MessagePort;
  abstract process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
}
declare function registerProcessor(name: string, ctor: new () => AudioWorkletProcessor): void;
declare const sampleRate: number;

/** Render quanta between stats messages (~0.25 s at 128 frames / 48 kHz). */
const STATS_EVERY = 94;

class RdAudioProcessor extends AudioWorkletProcessor {
  private ring: AudioRing | null = null;
  private readonly policy = new JitterPolicy({ ...DEFAULT_JITTER, sampleRate });
  private quanta = 0;

  constructor() {
    super();
    this.port.onmessage = (e: MessageEvent<ToWorklet>) => this.onMessage(e.data);
  }

  private onMessage(m: ToWorklet): void {
    switch (m.type) {
      case 'init':
        this.ring = m.buffer ? new AudioRing(m.buffer, m.capacity) : AudioRing.create(m.capacity || RING_FRAMES, false);
        this.policy.reset();
        return;
      case 'pcm':
        this.ring?.write(m.data);
        return;
      case 'reset':
        this.ring?.clear();
        this.policy.reset();
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0];
    const left = out?.[0];
    if (!left) return true;
    const right = out[1] ?? left;
    const ring = this.ring;
    let played = 0;
    if (ring) {
      const d = this.policy.next(ring.available(), left.length);
      if (d.skip) ring.skip(d.skip);
      if (d.play) played = ring.read(left, right);
    }
    left.fill(0, played);
    if (right !== left) right.fill(0, played);
    if (++this.quanta >= STATS_EVERY) {
      this.quanta = 0;
      const stats: WorkletStats = {
        type: 'stats',
        bufferedMs: ring ? (ring.available() * 1000) / sampleRate : 0,
        ...this.policy.stats(),
      };
      this.port.postMessage(stats);
    }
    return true;
  }
}

registerProcessor(WORKLET_NAME, RdAudioProcessor);
