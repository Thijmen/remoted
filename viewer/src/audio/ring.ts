// Single-producer / single-consumer ring of interleaved stereo float32 PCM.
// Shared between the main thread (producer: decoder output) and the AudioWorklet (consumer)
// through a SharedArrayBuffer; also usable on a plain ArrayBuffer inside the worklet when the
// page is not cross-origin isolated.
//
// Layout: Int32 header [write, read] (frame counters, wrapping at 2^32) + Float32 data.

const WRITE = 0;
const READ = 1;
const HEADER_BYTES = 8;

export const CHANNELS = 2;

export class AudioRing {
  readonly capacity: number;
  private readonly header: Int32Array;
  private readonly data: Float32Array;
  private readonly mask: number;

  /** `capacityFrames` must be a power of two. */
  constructor(
    readonly buffer: ArrayBufferLike,
    capacityFrames: number,
  ) {
    if (capacityFrames <= 0 || (capacityFrames & (capacityFrames - 1)) !== 0) {
      throw new RangeError('ring capacity must be a power of two');
    }
    if (buffer.byteLength < AudioRing.bytesFor(capacityFrames)) throw new RangeError('ring buffer too small');
    this.capacity = capacityFrames;
    this.mask = capacityFrames - 1;
    this.header = new Int32Array(buffer, 0, 2);
    this.data = new Float32Array(buffer, HEADER_BYTES, capacityFrames * CHANNELS);
  }

  static bytesFor(capacityFrames: number): number {
    return HEADER_BYTES + capacityFrames * CHANNELS * 4;
  }

  static create(capacityFrames: number, shared: boolean): AudioRing {
    const bytes = AudioRing.bytesFor(capacityFrames);
    return new AudioRing(shared ? new SharedArrayBuffer(bytes) : new ArrayBuffer(bytes), capacityFrames);
  }

  /** Frames ready to be read. */
  available(): number {
    return (Atomics.load(this.header, WRITE) - Atomics.load(this.header, READ)) >>> 0;
  }

  /** Producer: append interleaved frames; returns how many fit (the rest is dropped). */
  write(interleaved: Float32Array): number {
    const w = Atomics.load(this.header, WRITE);
    const used = (w - Atomics.load(this.header, READ)) >>> 0;
    const frames = Math.min(interleaved.length / CHANNELS, this.capacity - used);
    let idx = (w >>> 0) & this.mask;
    for (let i = 0; i < frames; i++) {
      this.data[idx * 2] = interleaved[i * 2]!;
      this.data[idx * 2 + 1] = interleaved[i * 2 + 1]!;
      idx = (idx + 1) & this.mask;
    }
    Atomics.store(this.header, WRITE, (w + frames) | 0);
    return frames;
  }

  /** Consumer: de-interleave up to `left.length` frames; returns frames read. */
  read(left: Float32Array, right: Float32Array): number {
    const r = Atomics.load(this.header, READ);
    const frames = Math.min(left.length, (Atomics.load(this.header, WRITE) - r) >>> 0);
    let idx = (r >>> 0) & this.mask;
    for (let i = 0; i < frames; i++) {
      left[i] = this.data[idx * 2]!;
      right[i] = this.data[idx * 2 + 1]!;
      idx = (idx + 1) & this.mask;
    }
    Atomics.store(this.header, READ, (r + frames) | 0);
    return frames;
  }

  /** Consumer: drop up to `frames` frames; returns frames dropped. */
  skip(frames: number): number {
    const r = Atomics.load(this.header, READ);
    const n = Math.min(frames, (Atomics.load(this.header, WRITE) - r) >>> 0);
    Atomics.store(this.header, READ, (r + n) | 0);
    return n;
  }

  /** Consumer: drop everything buffered. */
  clear(): void {
    Atomics.store(this.header, READ, Atomics.load(this.header, WRITE));
  }
}
