import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AckFlags, type Bytes, VideoFlags, type VideoMsg, parse } from '../proto/bin';
import { VideoPipeline } from './pipeline';

/** A WebCodecs VideoDecoder stand-in that the test drives by hand. */
class FakeDecoder {
  static all: FakeDecoder[] = [];
  static throwOnDecode = false;
  static async isConfigSupported(config: VideoDecoderConfig) {
    return { supported: true, config };
  }
  state: CodecState = 'unconfigured';
  decodeQueueSize = 0;
  readonly timestamps: number[] = [];

  constructor(private readonly init: VideoDecoderInit) {
    FakeDecoder.all.push(this);
  }
  configure(): void {
    this.state = 'configured';
  }
  decode(chunk: { timestamp: number }): void {
    if (FakeDecoder.throwOnDecode) throw new DOMException('bad chunk', 'EncodingError');
    this.timestamps.push(chunk.timestamp);
  }
  close(): void {
    this.state = 'closed';
  }
  /** Emit the output for the n-th submitted chunk. */
  output(n: number): void {
    const timestamp = this.timestamps[n]!;
    this.init.output({ timestamp, displayWidth: 64, displayHeight: 48, close() {} } as unknown as VideoFrame);
  }
  /** An asynchronous decoder error (GPU process crash, device loss). */
  fail(): void {
    this.state = 'closed';
    this.init.error(new DOMException('decoder lost', 'OperationError'));
  }
}

class FakeChunk {
  readonly timestamp: number;
  constructor(init: { timestamp: number }) {
    this.timestamp = init.timestamp;
  }
}

const frame = (seq: number, key = false): VideoMsg => ({
  type: 'video',
  flags: key ? VideoFlags.KEY : 0,
  epoch: 1,
  seq,
  captureUs: BigInt(seq * 16_000),
  encodeUs: 0,
  payload: new Uint8Array([0, 0, 0, 1, 0x65]),
});

describe('VideoPipeline ACKs', () => {
  let acks: string[];
  let keyframeReqs: string[];
  let pipe: VideoPipeline;

  beforeEach(async () => {
    FakeDecoder.all = [];
    FakeDecoder.throwOnDecode = false;
    vi.stubGlobal('VideoDecoder', FakeDecoder);
    vi.stubGlobal('EncodedVideoChunk', FakeChunk);
    acks = [];
    keyframeReqs = [];
    const canvas = { width: 0, height: 0, getContext: () => ({ drawImage() {} }) } as unknown as HTMLCanvasElement;
    pipe = new VideoPipeline({
      canvas,
      sendAck: (b: Bytes) => {
        const m = parse(b);
        if (m.type !== 'video_ack') throw new Error(m.type);
        acks.push(`${m.flags & AckFlags.DISCARDED ? 'discarded' : 'decoded'} ${m.seq}`);
      },
      requestKeyframe: (reason) => keyframeReqs.push(reason),
      agentToViewerUs: () => null,
    });
    await pipe.configure({
      t: 'video_config',
      epoch: 1,
      codec: 'avc1.640033',
      width: 64,
      height: 48,
      description: 'AA==',
    });
  });

  afterEach(() => void vi.unstubAllGlobals());

  it('ACKs decoded frames after output', () => {
    pipe.onVideo(frame(1, true), 0);
    pipe.onVideo(frame(2), 0);
    expect(acks).toEqual([]);
    FakeDecoder.all[0]!.output(0);
    FakeDecoder.all[0]!.output(1);
    expect(acks).toEqual(['decoded 1', 'decoded 2']);
  });

  it('ACKs the frames inside a decoder that failed asynchronously, in order', () => {
    pipe.onVideo(frame(1, true), 0);
    pipe.onVideo(frame(2), 0);
    pipe.onVideo(frame(3), 0);
    FakeDecoder.all[0]!.output(0);
    FakeDecoder.all[0]!.fail();
    expect(acks).toEqual(['decoded 1', 'discarded 2', 'discarded 3']);
    expect(keyframeReqs).toEqual(['decoder_error']);
    // The replacement decoder waits for a keyframe; deltas are discarded (and ACKed) meanwhile.
    pipe.onVideo(frame(4), 0);
    expect(acks.at(-1)).toBe('discarded 4');
  });

  it('ACKs in seq order when decode() throws synchronously', () => {
    pipe.onVideo(frame(1, true), 0);
    pipe.onVideo(frame(2), 0);
    FakeDecoder.throwOnDecode = true;
    pipe.onVideo(frame(3), 0);
    expect(acks).toEqual(['discarded 1', 'discarded 2', 'discarded 3']);
    expect(keyframeReqs).toEqual(['decoder_error']);
  });

  it('never ACKs a frame twice when a late output follows the error', () => {
    pipe.onVideo(frame(1, true), 0);
    const old = FakeDecoder.all[0]!;
    old.fail();
    old.output(0); // stale decoder: ignored
    expect(acks).toEqual(['discarded 1']);
  });
});
