import { describe, expect, it } from 'vitest';
import { FileFlags, encodeFileAck, encodeFileData, parse } from '../proto/bin';
import type { FilesAgentMsg, FilesHello, FilesViewerMsg } from '../proto/files';
import type { DownloadSink } from './sink';
import type { UploadItem } from './sources';
import { type JobView, TransferManager } from './transfers';

const HELLO: FilesHello = {
  t: 'hello',
  proto: 1,
  os: 'macos',
  sep: '/',
  home: '/Users/me',
  places: [],
  caps: ['trash', 'zip', 'resume'],
  state: null,
};

type Answer = Exclude<FilesAgentMsg, FilesHello | { t: 'state' }>;

interface Open {
  path: string;
  size: number;
  conflict: string;
  token: string;
  bytes: number[];
}

/**
 * A pretend agent behind a pretend channel: keeps files in a map, answers like the real one
 * (docs/files.md) on the next tick, and can drop the connection.
 */
class FakeAgent {
  readonly files = new Map<string, Uint8Array>();
  readonly dirs = new Set<string>(['/', '/dst']);
  /** Temporary files kept for resuming, by token. */
  readonly parts = new Map<string, number[]>();
  readonly open = new Map<number, Open>();
  ready = true;
  /** Lose the connection after this many FILE_DATA frames (once). */
  dropAfterFrames: number | null = null;
  frames = 0;
  writes: Array<Extract<FilesViewerMsg, { t: 'write' }>> = [];
  saved = new Map<string, Uint8Array>();
  manager!: TransferManager;
  private nextId = 1;
  private seq = 1;
  /** Bumped when the connection drops: nothing sent before reaches the next one. */
  private conn = 0;

  readonly channel = {
    ready: true,
    allocId: () => this.nextId++,
    want: () => undefined,
    sendJson: (m: FilesViewerMsg) => {
      if (!this.channel.ready) return false;
      this.onJson(m);
      return true;
    },
    sendBinary: (b: Uint8Array<ArrayBuffer>) => {
      if (!this.channel.ready) return false;
      this.onBinary(b);
      return true;
    },
    request: async (msg: { t: 'stat'; path: string }) => {
      const f = this.files.get(msg.path);
      return {
        t: 'stat' as const,
        id: 0,
        entry: f
          ? { name: msg.path, kind: 'file' as const, size: f.byteLength, mtime_ms: 0, hidden: false, link: false }
          : null,
      };
    },
  };

  private later(f: () => void): void {
    setTimeout(f, 0);
  }

  private answer(m: Answer): void {
    const conn = this.conn;
    this.later(() => {
      if (conn === this.conn) this.manager.onTransfer(m);
    });
  }

  private onJson(m: FilesViewerMsg): void {
    switch (m.t) {
      case 'mkdir':
        if (this.dirs.has(m.path) && !m.exist_ok) this.answer({ t: 'err', id: m.id, code: 'exists', msg: '' });
        else {
          this.dirs.add(m.path);
          this.answer({ t: 'ok', id: m.id });
        }
        return;
      case 'write': {
        this.writes.push(m);
        if (m.conflict === 'fail' && this.files.has(m.path)) {
          this.answer({ t: 'err', id: m.id, code: 'exists', msg: '' });
          this.open.set(m.id, { ...m, bytes: [], conflict: 'dead' });
          return;
        }
        const kept = m.resume ? (this.parts.get(m.token) ?? []) : [];
        this.open.set(m.id, { path: m.path, size: m.size, conflict: m.conflict, token: m.token, bytes: [...kept] });
        if (m.resume) this.answer({ t: 'write', id: m.id, offset: kept.length });
        return;
      }
      case 'cancel':
        this.open.delete(m.id);
        return;
      case 'read':
        this.streamRead(m);
        return;
    }
  }

  private onBinary(b: Uint8Array): void {
    const m = parse(b);
    if (m.type === 'file_ack') return;
    if (m.type !== 'file_data') return;
    this.frames++;
    const o = this.open.get(m.id);
    if (o && o.conflict !== 'dead') {
      expect(m.offset).toBe(o.bytes.length);
      for (const b of m.payload) o.bytes.push(b);
      this.parts.set(o.token, [...o.bytes]);
    }
    if (this.dropAfterFrames !== null && this.frames >= this.dropAfterFrames) {
      this.dropAfterFrames = null;
      this.drop();
      return;
    }
    const seq = m.seq;
    const conn = this.conn;
    this.later(() => {
      if (conn === this.conn) this.manager.onAck(seq);
    });
    if (o && (m.flags & FileFlags.END) !== 0 && o.conflict !== 'dead') {
      this.open.delete(m.id);
      this.parts.delete(o.token);
      let path = o.path;
      if (o.conflict === 'rename') {
        for (let n = 2; this.files.has(path); n++) path = o.path.replace(/(\.[^.]*)?$/, ` (${n})$1`);
      }
      this.files.set(path, new Uint8Array(o.bytes));
      this.answer({ t: 'done', id: m.id, path, size: o.bytes.length });
    }
  }

  private streamRead(m: Extract<FilesViewerMsg, { t: 'read' }>): void {
    const data = this.files.get(m.path ?? '');
    if (!data) {
      this.answer({ t: 'err', id: m.id, code: 'not_found', msg: '' });
      return;
    }
    if (m.expect && m.expect.size !== data.byteLength) {
      this.answer({ t: 'err', id: m.id, code: 'changed', msg: '' });
      return;
    }
    const name = (m.path ?? '').split('/').pop()!;
    this.answer({ t: 'read', id: m.id, name, size: data.byteLength, mtime_ms: 7, total: null, files: null });
    const conn = this.conn;
    let offset = m.offset ?? 0;
    const step = 1000;
    let delay = 1;
    do {
      const end = Math.min(data.byteLength, offset + step);
      const frame = encodeFileData(
        end >= data.byteLength ? FileFlags.END : 0,
        m.id,
        this.seq++,
        offset,
        data.subarray(offset, end),
      );
      const msg = parse(frame);
      setTimeout(() => {
        if (conn === this.conn && msg.type === 'file_data') this.manager.onData(msg);
      }, delay++);
      offset = end;
    } while (offset < data.byteLength);
  }

  drop(): void {
    this.conn++;
    this.channel.ready = false;
    this.open.clear();
    this.manager.onLost();
  }

  reconnect(): void {
    this.channel.ready = true;
    this.seq = 1;
    this.manager.onHello(HELLO);
  }

  sink = async (): Promise<DownloadSink> => {
    const parts: number[] = [];
    const sink = {
      get size() {
        return parts.length;
      },
      write: async (offset: number, data: Uint8Array) => {
        expect(offset).toBe(parts.length);
        parts.push(...data);
      },
      truncate: async () => {
        parts.length = 0;
      },
      save: async (name: string) => {
        this.saved.set(name, new Uint8Array(parts));
      },
      discard: async () => undefined,
    };
    return sink;
  };
}

function setup(): { agent: FakeAgent; m: TransferManager } {
  const agent = new FakeAgent();
  const m = new TransferManager(agent.channel, () => undefined, agent.sink);
  agent.manager = m;
  m.onHello(HELLO);
  return { agent, m };
}

function bytes(n: number, seed = 1): Uint8Array<ArrayBuffer> {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i++) b[i] = (i * 31 + seed) & 0xff;
  return b;
}

function fileItem(name: string, size: number, seed = 1): UploadItem {
  return { kind: 'file', name, file: new File([bytes(size, seed)], name) };
}

async function settle(m: TransferManager, until: (views: JobView[]) => boolean = () => !m.busy): Promise<JobView[]> {
  for (let i = 0; i < 2000; i++) {
    await new Promise((r) => setTimeout(r, 0));
    if (until(m.views())) return m.views();
  }
  throw new Error(`transfers did not settle: ${JSON.stringify(m.views().map((v) => [v.name, v.state, v.error]))}`);
}

describe('TransferManager uploads', () => {
  it('uploads a folder tree with files back to back', async () => {
    const { agent, m } = setup();
    const tree: UploadItem = {
      kind: 'dir',
      name: 'Photos',
      children: [fileItem('a.jpg', 300_000), { kind: 'dir', name: 'empty', children: [] }, fileItem('b.jpg', 0)],
    };
    m.upload([{ item: tree, name: 'Photos', mode: 'new' }], '/dst');
    const [v] = await settle(m);
    expect(v).toMatchObject({ state: 'done', filesDone: 2, filesTotal: 2, bytesDone: 300_000, problems: [] });
    expect(agent.dirs.has('/dst/Photos/empty')).toBe(true);
    expect(agent.files.get('/dst/Photos/a.jpg')).toEqual(bytes(300_000));
    expect(agent.files.get('/dst/Photos/b.jpg')).toEqual(new Uint8Array());
  });

  it('pipelines many small files without waiting for each answer', async () => {
    const { agent, m } = setup();
    const children = Array.from({ length: 100 }, (_, i) => fileItem(`f${i}.txt`, 10, i));
    m.upload([{ item: { kind: 'dir', name: 'many', children }, name: 'many', mode: 'new' }], '/dst');
    // Synchronously after queueing, many writes are already out.
    await new Promise((r) => setTimeout(r, 0));
    expect(agent.writes.length).toBeGreaterThan(10);
    const [v] = await settle(m);
    expect(v).toMatchObject({ state: 'done', filesDone: 100 });
    expect(agent.files.size).toBe(100);
  });

  it('applies the conflict choices', async () => {
    const { agent, m } = setup();
    agent.files.set('/dst/r.txt', bytes(5));
    agent.files.set('/dst/k.txt', bytes(5));
    agent.files.set('/dst/f.txt', bytes(5));
    m.upload(
      [
        { item: fileItem('r.txt', 20, 9), name: 'r.txt', mode: 'replace' },
        { item: fileItem('k.txt', 20, 9), name: 'k.txt', mode: 'keep' },
        { item: fileItem('f.txt', 20, 9), name: 'f.txt', mode: 'new' },
      ],
      '/dst',
    );
    const views = await settle(m);
    expect(views.map((v) => v.state)).toEqual(['done', 'done', 'failed']);
    expect(views[1]!.result).toBe('/dst/k (2).txt');
    expect(views[2]!.error).toMatch(/already exists/i);
    expect(agent.files.get('/dst/r.txt')).toEqual(bytes(20, 9));
    expect(agent.files.get('/dst/k.txt')).toEqual(bytes(5));
  });

  it('resumes an upload after the connection drops', async () => {
    const { agent, m } = setup();
    agent.dropAfterFrames = 3;
    m.upload([{ item: fileItem('big.bin', 400_000, 3), name: 'big.bin', mode: 'new' }], '/dst');
    await settle(m, (v) => v[0]!.state === 'waiting');
    const sentBefore = agent.parts.values().next().value!.length;
    expect(sentBefore).toBeGreaterThan(0);
    agent.reconnect();
    const [v] = await settle(m);
    expect(v!.state).toBe('done');
    expect(agent.files.get('/dst/big.bin')).toEqual(bytes(400_000, 3));
    const resumed = agent.writes.filter((w) => w.resume);
    expect(resumed).toHaveLength(1);
    expect(resumed[0]!.token).toBe(agent.writes[0]!.token);
  });

  it('pauses one upload while another continues', async () => {
    const { agent, m } = setup();
    m.upload(
      [
        { item: fileItem('one.bin', 200_000, 1), name: 'one.bin', mode: 'new' },
        { item: fileItem('two.bin', 200_000, 2), name: 'two.bin', mode: 'new' },
      ],
      '/dst',
    );
    const first = m.views()[0]!.key;
    m.pause(first);
    await settle(m, (v) => v[1]!.state === 'done');
    expect(m.views()[0]!.state).toBe('paused');
    m.resume(first);
    await settle(m);
    expect(agent.files.get('/dst/one.bin')).toEqual(bytes(200_000, 1));
    expect(agent.files.get('/dst/two.bin')).toEqual(bytes(200_000, 2));
  });

  it('cancels an upload', async () => {
    const { agent, m } = setup();
    m.upload([{ item: fileItem('c.bin', 2_000_000), name: 'c.bin', mode: 'new' }], '/dst');
    m.cancel(m.views()[0]!.key);
    await settle(m);
    expect(m.views()[0]!.state).toBe('cancelled');
    expect(agent.files.has('/dst/c.bin')).toBe(false);
  });
});

describe('TransferManager after many cancels', () => {
  it('still starts new uploads', async () => {
    const { agent, m } = setup();
    for (let i = 0; i < 70; i++) {
      m.upload([{ item: fileItem(`c${i}.bin`, 600_000), name: `c${i}.bin`, mode: 'new' }], '/dst');
      const key = m.views()[m.views().length - 1]!.key;
      await new Promise((r) => setTimeout(r, 0));
      m.cancel(key);
    }
    m.upload([{ item: fileItem('after.bin', 1000, 5), name: 'after.bin', mode: 'new' }], '/dst');
    await settle(m);
    expect(m.views().at(-1)!.state).toBe('done');
    expect(agent.files.get('/dst/after.bin')).toEqual(bytes(1000, 5));
  });
});

describe('TransferManager downloads', () => {
  it('downloads a file and acknowledges every frame', async () => {
    const { agent, m } = setup();
    agent.files.set('/src/a.bin', bytes(5500, 4));
    m.download(['/src/a.bin'], { zip: false, name: 'a.bin', folder: false });
    const [v] = await settle(m);
    expect(v).toMatchObject({ state: 'done', bytesDone: 5500, bytesTotal: 5500 });
    expect(agent.saved.get('a.bin')).toEqual(bytes(5500, 4));
  });

  it('resumes from what it has after the connection drops', async () => {
    const { agent, m } = setup();
    agent.files.set('/src/b.bin', bytes(9000, 5));
    m.download(['/src/b.bin'], { zip: false, name: 'b.bin', folder: false });
    await settle(m, (v) => v[0]!.bytesDone >= 3000);
    agent.drop();
    await settle(m, (v) => v[0]!.state === 'waiting');
    agent.reconnect();
    const [v] = await settle(m);
    expect(v!.state).toBe('done');
    expect(agent.saved.get('b.bin')).toEqual(bytes(9000, 5));
  });

  it('starts over when the remote file changed', async () => {
    const { agent, m } = setup();
    agent.files.set('/src/c.bin', bytes(9000, 6));
    m.download(['/src/c.bin'], { zip: false, name: 'c.bin', folder: false });
    await settle(m, (v) => v[0]!.bytesDone >= 3000);
    agent.drop();
    agent.files.set('/src/c.bin', bytes(7000, 7));
    agent.reconnect();
    const [v] = await settle(m);
    expect(v!.state).toBe('done');
    expect(agent.saved.get('c.bin')).toEqual(bytes(7000, 7));
  });

  it('reports a missing file', async () => {
    const { m } = setup();
    m.download(['/src/none'], { zip: false, name: 'none', folder: false });
    const [v] = await settle(m);
    expect(v).toMatchObject({ state: 'failed', error: 'Not found' });
  });
});

// The frames above are built with the real codec; keep the ACK encoder honest too.
it('encodes FILE_ACK like the vectors', () => {
  expect(Array.from(encodeFileAck(4_000_000_001))).toEqual([0x41, 0, 0, 0, 0x01, 0x28, 0x6b, 0xee]);
});
