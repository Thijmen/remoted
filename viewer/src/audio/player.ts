import type { AudioMsg } from '../proto/bin';
import type { AudioConfigMsg } from '../proto/control';
import { RING_FRAMES, type ToWorklet, WORKLET_NAME, type WorkletStats } from './messages';
import { AudioRing, CHANNELS } from './ring';
import workletUrl from './worklet.ts?worker&url';

const SAMPLE_RATE = 48_000;

export interface AudioStats {
  /** 'sab' = SharedArrayBuffer ring, 'message' = PCM posted to the worklet (not isolated). */
  transport: 'sab' | 'message';
  packets: number;
  decodeErrors: number;
  /** Decoded frames that did not fit into the ring. */
  overflowFrames: number;
  worklet: WorkletStats | null;
  lastError: string;
}

/**
 * Opus → AudioDecoder → ring → AudioWorklet. The AudioContext must be created from a user
 * gesture (the Connect click), so construct this synchronously in that handler.
 */
export class AudioPlayer {
  readonly stats: AudioStats;
  private readonly ctx: AudioContext;
  private readonly gain: GainNode;
  private node: AudioWorkletNode | null = null;
  private ring: AudioRing | null = null;
  private decoder: AudioDecoder | null = null;
  private ready: Promise<void>;
  private scratch = new Float32Array(0);

  constructor(volume: number) {
    this.ctx = new AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: 'interactive' });
    this.gain = new GainNode(this.ctx, { gain: volume });
    this.gain.connect(this.ctx.destination);
    this.stats = {
      transport: crossOriginIsolated ? 'sab' : 'message',
      packets: 0,
      decodeErrors: 0,
      overflowFrames: 0,
      worklet: null,
      lastError: '',
    };
    this.ready = this.init();
  }

  private async init(): Promise<void> {
    try {
      await this.ctx.audioWorklet.addModule(workletUrl);
    } catch (e) {
      this.stats.lastError = `worklet: ${String(e)}`;
      return;
    }
    const node = new AudioWorkletNode(this.ctx, WORKLET_NAME, { numberOfInputs: 0, outputChannelCount: [CHANNELS] });
    node.port.onmessage = (e: MessageEvent<WorkletStats>) => {
      if (e.data?.type === 'stats') this.stats.worklet = e.data;
    };
    this.node = node;
    let shared: SharedArrayBuffer | null = null;
    if (this.stats.transport === 'sab') {
      shared = new SharedArrayBuffer(AudioRing.bytesFor(RING_FRAMES));
      this.ring = new AudioRing(shared, RING_FRAMES);
    }
    this.post({ type: 'init', buffer: shared, capacity: RING_FRAMES });
    node.connect(this.gain);
  }

  /** `audio_config` from the agent: (re)create the Opus decoder. */
  async configure(cfg: AudioConfigMsg): Promise<void> {
    await this.ready;
    this.closeDecoder();
    this.post({ type: 'reset' });
    if (cfg.codec !== 'opus') {
      this.stats.lastError = `unsupported audio codec ${cfg.codec}`;
      return;
    }
    const config: AudioDecoderConfig = {
      codec: 'opus',
      sampleRate: cfg.sample_rate || SAMPLE_RATE,
      numberOfChannels: cfg.channels || CHANNELS,
    };
    try {
      const support = await AudioDecoder.isConfigSupported(config);
      if (!support.supported) throw new Error('opus not supported');
    } catch (e) {
      this.stats.lastError = String(e);
      return;
    }
    const decoder = new AudioDecoder({
      output: (data) => this.onDecoded(data),
      error: (e) => {
        this.stats.decodeErrors++;
        this.stats.lastError = e.message;
        if (this.decoder === decoder) {
          this.decoder = null;
          void this.configure(cfg);
        }
      },
    });
    decoder.configure(config);
    this.decoder = decoder;
    this.stats.lastError = '';
  }

  onAudio(msg: AudioMsg): void {
    const d = this.decoder;
    if (!d || d.state !== 'configured') return;
    this.stats.packets++;
    try {
      d.decode(new EncodedAudioChunk({ type: 'key', timestamp: msg.seq * 10_000, data: msg.payload }));
    } catch (e) {
      this.stats.decodeErrors++;
      this.stats.lastError = String(e);
    }
  }

  setVolume(v: number): void {
    this.gain.gain.setTargetAtTime(v, this.ctx.currentTime, 0.02);
  }

  /** Resume playback (must be called from a user gesture if the context was suspended). */
  resume(): void {
    void this.ctx.resume();
  }

  /** Session lost: drop decoder and buffered audio. */
  reset(): void {
    this.closeDecoder();
    this.post({ type: 'reset' });
  }

  close(): void {
    this.closeDecoder();
    this.node?.disconnect();
    this.node = null;
    void this.ctx.close();
  }

  private onDecoded(data: AudioData): void {
    try {
      const frames = data.numberOfFrames;
      const pcm = this.interleave(data, frames);
      if (this.ring) {
        this.stats.overflowFrames += frames - this.ring.write(pcm);
      } else {
        // Message mode: the worklet owns the ring; transfer a copy.
        const copy = pcm.slice();
        this.node?.port.postMessage({ type: 'pcm', data: copy } satisfies ToWorklet, [copy.buffer]);
      }
    } catch (e) {
      this.stats.decodeErrors++;
      this.stats.lastError = String(e);
    } finally {
      data.close();
    }
  }

  /** Copy planar channels into an interleaved stereo scratch buffer (mono is duplicated). */
  private interleave(data: AudioData, frames: number): Float32Array {
    const need = frames * CHANNELS;
    if (this.scratch.length < need * 2) this.scratch = new Float32Array(need * 2);
    const left = this.scratch.subarray(need, need + frames);
    const right = data.numberOfChannels > 1 ? this.scratch.subarray(need + frames, need * 2) : left;
    data.copyTo(left, { planeIndex: 0, format: 'f32-planar' });
    if (right !== left) data.copyTo(right, { planeIndex: 1, format: 'f32-planar' });
    const out = this.scratch.subarray(0, need);
    for (let i = 0; i < frames; i++) {
      out[i * 2] = left[i]!;
      out[i * 2 + 1] = right[i]!;
    }
    return out;
  }

  private post(m: ToWorklet): void {
    this.node?.port.postMessage(m);
  }

  private closeDecoder(): void {
    const d = this.decoder;
    this.decoder = null;
    if (d && d.state !== 'closed') d.close();
  }
}
