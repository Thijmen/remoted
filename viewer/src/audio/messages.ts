import type { JitterStats } from './jitter';

export const WORKLET_NAME = 'rd-audio';
/** Ring capacity in frames (power of two, ~680 ms at 48 kHz). */
export const RING_FRAMES = 32_768;

/** Main thread → worklet. */
export type ToWorklet =
  /** `buffer` is the SharedArrayBuffer ring, or null to receive PCM by message. */
  | { type: 'init'; buffer: SharedArrayBuffer | null; capacity: number }
  /** Interleaved stereo PCM (message mode only). */
  | { type: 'pcm'; data: Float32Array }
  /** New stream: flush and prime again. */
  | { type: 'reset' };

/** Worklet → main thread, a few times per second. */
export interface WorkletStats extends JitterStats {
  type: 'stats';
  bufferedMs: number;
}
