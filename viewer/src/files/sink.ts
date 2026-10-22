// Where a download is staged before the browser saves it.
//
// Most browsers (Chrome, Edge, Firefox, Safari 26+, rd Desktop) have the Origin Private File
// System with writable streams: the download goes to disk there, so its size is not limited by
// memory, and is then handed to the browser's own download as a disk-backed File. Elsewhere it
// is kept in memory, up to MEMORY_LIMIT.

const OPFS_DIR = 'rd-downloads';
/** Largest download kept in memory when there is no OPFS. */
export const MEMORY_LIMIT = 1 << 30;
/** Staged files stay this long after the browser was given them (it may still be copying). */
const KEEP_AFTER_SAVE_MS = 30 * 60_000;

export interface DownloadSink {
  /** Bytes written so far (writes are sequential). */
  readonly size: number;
  write(offset: number, data: Uint8Array): Promise<void>;
  /** Start over (a download that cannot resume). */
  truncate(): Promise<void>;
  /** Hand the finished file to the browser's download as `name`. */
  save(name: string): Promise<void>;
  /**
   * The finished file, typed `type`, to show in the page (a preview). It stays readable until
   * `discard()`, which the caller must call once done with it.
   */
  open(type: string): Promise<Blob>;
  discard(): Promise<void>;
}

export class SinkError extends Error {
  override name = 'SinkError';
}

let opfs: Promise<FileSystemDirectoryHandle | null> | null = null;

/** The staging folder, or null when this browser cannot stream to OPFS. */
function stagingDir(): Promise<FileSystemDirectoryHandle | null> {
  opfs ??= (async () => {
    try {
      if (!navigator.storage?.getDirectory || !('createWritable' in FileSystemFileHandle.prototype)) return null;
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(OPFS_DIR, { create: true });
      void cleanUp(dir);
      return dir;
    } catch {
      return null;
    }
  })();
  return opfs;
}

/** Remove staged files left over from earlier pages (older than an hour). */
async function cleanUp(dir: FileSystemDirectoryHandle): Promise<void> {
  const cutoff = Date.now() - 3600_000;
  try {
    // Downloads still running in other tabs hold a lock named after their file.
    const held = new Set(((await navigator.locks?.query())?.held ?? []).map((l) => l.name));
    for await (const [name, handle] of dir as unknown as AsyncIterable<[string, FileSystemHandle]>) {
      if (handle.kind !== 'file' || held.has(lockName(name))) continue;
      const file = await (handle as FileSystemFileHandle).getFile().catch(() => null);
      if (!file || file.lastModified < cutoff) await dir.removeEntry(name).catch(() => undefined);
    }
  } catch {
    // Iteration unsupported: leftovers stay until the browser clears site data.
  }
}

/** Whether downloads of any size are possible here (else up to MEMORY_LIMIT). */
export async function diskStaging(): Promise<boolean> {
  return (await stagingDir()) !== null;
}

/** A sink for a download of about `expected` bytes (null: unknown). */
export async function createSink(expected: number | null): Promise<DownloadSink> {
  const dir = await stagingDir();
  if (dir) {
    try {
      return await OpfsSink.open(dir);
    } catch {
      // Quota or a private window: try memory.
    }
  }
  if (expected !== null && expected > MEMORY_LIMIT) {
    throw new SinkError(
      'This browser keeps downloads in memory, and this one is over 1 GB. Use Chrome, Edge, Firefox, Safari 26 or rd Desktop.',
    );
  }
  return new MemorySink();
}

const lockName = (file: string) => `rd-download:${file}`;

/** Hold a Web Lock until the returned function is called (no-op without the API). */
function holdLock(name: string): () => void {
  let release = () => {};
  const held = new Promise<void>((resolve) => (release = resolve));
  void navigator.locks?.request(name, () => held).catch(() => undefined);
  return release;
}

class OpfsSink implements DownloadSink {
  size = 0;
  private readonly unlock: () => void;
  private constructor(
    private readonly dir: FileSystemDirectoryHandle,
    private readonly name: string,
    private readonly handle: FileSystemFileHandle,
    private writable: FileSystemWritableFileStream,
  ) {
    this.unlock = holdLock(lockName(name));
  }

  static async open(dir: FileSystemDirectoryHandle): Promise<OpfsSink> {
    const name = `dl-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    const handle = await dir.getFileHandle(name, { create: true });
    const writable = await handle.createWritable({ keepExistingData: false });
    return new OpfsSink(dir, name, handle, writable);
  }

  async write(offset: number, data: Uint8Array): Promise<void> {
    if (offset !== this.size) throw new SinkError(`expected bytes at ${this.size}, got ${offset}`);
    try {
      await this.writable.write({ type: 'write', position: offset, data: data as Uint8Array<ArrayBuffer> });
    } catch (e) {
      throw new SinkError(
        e instanceof DOMException && e.name === 'QuotaExceededError'
          ? 'Not enough free space on this computer for the download.'
          : `Cannot store the download: ${e instanceof Error ? e.message : String(e)}`,
      );
    }
    this.size += data.byteLength;
  }

  async truncate(): Promise<void> {
    await this.writable.truncate(0);
    this.size = 0;
  }

  async save(name: string): Promise<void> {
    await this.writable.close();
    const file = await this.handle.getFile();
    saveBlob(file, name);
    // The browser may still be copying it; other tabs leave it alone until this one lets go.
    setTimeout(() => {
      void this.dir.removeEntry(this.name).catch(() => undefined);
      this.unlock();
    }, KEEP_AFTER_SAVE_MS);
  }

  async open(type: string): Promise<Blob> {
    await this.writable.close();
    // A Blob made from a File points at the same bytes on disk: no copy.
    return new Blob([await this.handle.getFile()], { type });
  }

  async discard(): Promise<void> {
    // Already closed by `open`: aborting then fails, which is fine.
    await this.writable.abort().catch(() => undefined);
    await this.dir.removeEntry(this.name).catch(() => undefined);
    this.unlock();
  }
}

class MemorySink implements DownloadSink {
  size = 0;
  private parts: Uint8Array[] = [];

  write(offset: number, data: Uint8Array): Promise<void> {
    if (offset !== this.size) return Promise.reject(new SinkError(`expected bytes at ${this.size}, got ${offset}`));
    if (this.size + data.byteLength > MEMORY_LIMIT) {
      return Promise.reject(new SinkError('This browser keeps downloads in memory, and this one is over 1 GB.'));
    }
    this.parts.push(data);
    this.size += data.byteLength;
    return Promise.resolve();
  }

  truncate(): Promise<void> {
    this.parts = [];
    this.size = 0;
    return Promise.resolve();
  }

  save(name: string): Promise<void> {
    saveBlob(new Blob(this.parts as Uint8Array<ArrayBuffer>[]), name);
    this.parts = [];
    return Promise.resolve();
  }

  open(type: string): Promise<Blob> {
    const blob = new Blob(this.parts as Uint8Array<ArrayBuffer>[], { type });
    this.parts = [];
    return Promise.resolve(blob);
  }

  discard(): Promise<void> {
    this.parts = [];
    return Promise.resolve();
  }
}

/** Let the browser save `blob` as `name` (its download folder, or wherever it asks). */
export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.rel = 'noopener';
  a.style.display = 'none';
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 120_000);
}
