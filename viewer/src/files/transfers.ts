// Uploads and downloads over a file channel (docs/files.md).
//
// Uploads stream back to back: the `write` of the next file follows the last bytes of the one
// before without waiting for its `done`, all through one BulkWindow. Downloads run one at a time;
// their bytes are staged on disk (sink.ts) and acknowledged once written. Both pick up where they
// stopped when the channel reconnects.

import { type FileDataMsg, FileFlags, encodeFileAck, encodeFileData } from '../proto/bin';
import type { AgentState } from '../proto/control';
import { type Conflict, type FilesAgentMsg, type FilesHello, describeFileError } from '../proto/files';
import { BulkWindow, TARGET_FULL_US, TARGET_GENTLE_US } from './bulk';
import { FileError, type FilesChannel } from './channel';
import { RateMeter } from './format';
import { baseName, joinPath } from './paths';
import { type DownloadSink, SinkError, createSink } from './sink';
import type { UploadItem } from './sources';

/** Files whose `write` went out but whose `done` has not come back yet, at most. */
const MAX_UNANSWERED = 64;

export type JobState = 'queued' | 'running' | 'paused' | 'waiting' | 'done' | 'failed' | 'cancelled';

/** What the transfer list shows. */
export interface JobView {
  key: number;
  dir: 'up' | 'down';
  name: string;
  /** Uploads: the remote folder. Downloads: the remote paths' folder. */
  where: string;
  folder: boolean;
  state: JobState;
  bytesDone: number;
  bytesTotal: number | null;
  filesDone: number;
  filesTotal: number;
  /** Bytes per second while running. */
  rate: number;
  etaS: number | null;
  /** Why it failed. */
  error: string | null;
  /** Files that failed or were skipped while the rest went through. */
  problems: string[];
  canPause: boolean;
  canResume: boolean;
  canRetry: boolean;
  /** The final remote path of a finished single-file upload. */
  result: string | null;
}

/** How to upload one dropped item whose name may already exist in the target folder. */
export interface UploadPlan {
  item: UploadItem;
  /** The name it gets in the target folder. */
  name: string;
  /**
   * `new`: nothing there (a name clash is an error); `replace`: replace a file of that name;
   * `merge`: add to the existing folder, replacing files; `keep`: keep both (a file gets a free
   * `name (2)` from the agent; a folder must already carry its new name).
   */
  mode: 'new' | 'replace' | 'merge' | 'keep';
}

type MkdirOp = {
  kind: 'mkdir';
  path: string;
  existOk: boolean;
  id: number | null;
  answered: boolean;
  /** Sent again after a reconnect: `exists` is then fine. */
  resent: boolean;
  error: string | null;
};

type FileOp = {
  kind: 'file';
  file: File;
  path: string;
  conflict: Conflict;
  token: string;
  id: number | null;
  started: boolean;
  /** Next offset to send. */
  sent: number;
  /** Bytes the agent confirmed (FILE_ACK or the resume offset). */
  acked: number;
  allSent: boolean;
  answered: boolean;
  /** Waiting for the agent's offset after a reconnect. */
  resuming: boolean;
  /** The connection was lost: ask to resume when started again. */
  needResume: boolean;
  /** Every byte had been sent before the connection was lost. */
  sentBeforeLoss: boolean;
  error: string | null;
  result: string | null;
};

type UpOp = MkdirOp | FileOp;

interface JobBase {
  key: number;
  name: string;
  where: string;
  folder: boolean;
  state: JobState;
  error: string | null;
  problems: string[];
  meter: RateMeter;
}

interface UploadJob extends JobBase {
  dir: 'up';
  ops: UpOp[];
  /** Index of the next op that has not been started. */
  next: number;
  /** Every op before this index is answered. */
  low: number;
  bytesDone: number;
  bytesTotal: number;
  filesTotal: number;
}

interface DownloadJob extends JobBase {
  dir: 'down';
  paths: string[];
  zip: boolean;
  id: number | null;
  sink: DownloadSink | null;
  /** From the agent's `read`: the file's size and time (resume checks), or the zip's source total. */
  size: number | null;
  mtime: number | null;
  total: number | null;
  files: number;
  /** Zip: source bytes archived so far. */
  progress: number;
  /** A resume found the file changed: start over without `expect`. */
  restart: boolean;
}

type Job = UploadJob | DownloadJob;

interface InFlightChunk {
  seq: number;
  op: FileOp;
  job: UploadJob;
  bytes: number;
}

function token(): string {
  const b = crypto.getRandomValues(new Uint8Array(8));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

const nowUs = () => Math.round(performance.now() * 1000);

export class TransferManager {
  private readonly jobs: Job[] = [];
  private nextKey = 1;
  private readonly window = new BulkWindow();
  private readonly upInFlight: InFlightChunk[] = [];
  private pumping = false;
  /** Bumped on every connection loss: work started before it must not touch the new socket. */
  private gen = 0;
  private sep = '/';
  private activeDown: DownloadJob | null = null;
  /** Download frames are handled strictly in order (writes, then the ACK). */
  private downChain: Promise<void> = Promise.resolve();
  private changeQueued = false;

  constructor(
    private readonly channel: Pick<FilesChannel, 'ready' | 'allocId' | 'sendJson' | 'sendBinary' | 'request' | 'want'>,
    private readonly onChange: () => void,
    /** Where downloads are staged (tests replace it). */
    private readonly newSink: (expected: number | null) => Promise<DownloadSink> = createSink,
  ) {}

  // -------------------------------------------------------------------------
  // channel events (wired by the owner of the channel)

  onHello(h: FilesHello): void {
    this.sep = h.sep;
    if (h.state) this.onState(h.state);
    // Picked up again by the pumps, which mark them running.
    for (const j of this.jobs) if (j.state === 'waiting') j.state = 'queued';
    this.changed();
    void this.pumpUploads();
    void this.startDownload();
  }

  onState(s: AgentState): void {
    this.window.setTarget(s.in_session ? TARGET_GENTLE_US : TARGET_FULL_US);
  }

  onLost(): void {
    this.gen++;
    this.window.reset();
    this.upInFlight.length = 0;
    for (const job of this.jobs) {
      if (job.dir === 'up') {
        for (const op of job.ops) {
          if (op.answered) continue;
          if (op.kind === 'mkdir') {
            if (op.id !== null) {
              op.id = null;
              op.resent = true;
            }
          } else if (op.started) {
            op.id = null;
            op.started = false;
            op.needResume = true;
            op.sentBeforeLoss = op.allSent;
            op.allSent = false;
            op.resuming = false;
          }
        }
        // Ops that lost their id are started again, from the first one.
        job.low = firstUnanswered(job);
        job.next = Math.min(job.next, job.low);
      } else if (job === this.activeDown) {
        job.id = null;
      }
      if (job.state === 'running') job.state = 'waiting';
    }
    this.activeDown = null;
    this.changed();
  }

  onTransfer(m: Exclude<FilesAgentMsg, FilesHello | { t: 'state' }>): void {
    if (!('id' in m)) return;
    const down = this.activeDown;
    if (down && down.id === m.id) {
      const gen = this.gen;
      this.downChain = this.downChain.then(() => this.onDownloadMsg(down, m, gen)).catch(() => undefined);
      return;
    }
    const found = this.findUpOp(m.id);
    if (found) this.onUploadAnswer(found.job, found.op, m);
  }

  onData(m: FileDataMsg): void {
    const gen = this.gen;
    const down = this.activeDown;
    this.downChain = this.downChain
      .then(async () => {
        if (down && down.id === m.id && gen === this.gen && down.state === 'running') await this.writeChunk(down, m);
      })
      .catch((e: unknown) => {
        if (down) this.failDownload(down, e instanceof SinkError ? e.message : `Saving failed: ${String(e)}`);
      })
      .then(() => {
        // Every frame is acknowledged, in order, also the ones dropped (docs/files.md).
        if (gen === this.gen) this.channel.sendBinary(encodeFileAck(m.seq));
      });
  }

  onAck(seq: number): void {
    if (this.window.onAck(seq, nowUs()) === 0) return;
    const i = this.upInFlight.findIndex((c) => c.seq === seq);
    if (i < 0) return;
    for (const c of this.upInFlight.splice(0, i + 1)) {
      // Done, failed or cancelled: its totals are settled.
      if (c.op.answered) continue;
      c.op.acked += c.bytes;
      c.job.bytesDone += c.bytes;
      c.job.meter.add(c.job.bytesDone);
    }
    this.changed();
    void this.pumpUploads();
  }

  // -------------------------------------------------------------------------
  // what the UI asks for

  /** Queue uploads into the remote folder `dest`, one job per item. */
  upload(plans: UploadPlan[], dest: string): void {
    for (const plan of plans) {
      const ops: UpOp[] = [];
      let bytes = 0;
      let files = 0;
      const addFile = (file: File, path: string, conflict: Conflict) => {
        ops.push({
          kind: 'file',
          file,
          path,
          conflict,
          token: token(),
          id: null,
          started: false,
          sent: 0,
          acked: 0,
          allSent: false,
          answered: false,
          resuming: false,
          needResume: false,
          sentBeforeLoss: false,
          error: null,
          result: null,
        });
        bytes += file.size;
        files++;
      };
      const addDir = (items: readonly UploadItem[], dir: string, merge: boolean) => {
        for (const it of items) {
          const path = joinPath(dir, it.name, this.sep);
          if (it.kind === 'file') addFile(it.file, path, merge ? 'replace' : 'fail');
          else {
            ops.push(mkdir(path, merge));
            addDir(it.children, path, merge);
          }
        }
      };
      const target = joinPath(dest, plan.name, this.sep);
      if (plan.item.kind === 'file') {
        const conflict: Conflict = plan.mode === 'replace' ? 'replace' : plan.mode === 'keep' ? 'rename' : 'fail';
        addFile(plan.item.file, target, conflict);
      } else {
        const merge = plan.mode === 'merge' || plan.mode === 'replace';
        ops.push(mkdir(target, merge));
        addDir(plan.item.children, target, merge);
      }
      this.jobs.push({
        key: this.nextKey++,
        dir: 'up',
        name: plan.name,
        where: dest,
        folder: plan.item.kind === 'dir',
        state: 'queued',
        error: null,
        problems: [],
        meter: new RateMeter(),
        ops,
        next: 0,
        low: 0,
        bytesDone: 0,
        bytesTotal: bytes,
        filesTotal: files,
      });
    }
    this.channel.want(true);
    this.changed();
    void this.pumpUploads();
  }

  /** Queue a download: one file as itself, or several paths / a folder as a zip named `name`. */
  download(paths: string[], opts: { zip: boolean; name: string; folder: boolean }): void {
    const first = paths[0];
    if (!first) return;
    this.jobs.push({
      key: this.nextKey++,
      dir: 'down',
      name: opts.name,
      where: parentOf(first, this.sep),
      folder: opts.folder,
      state: 'queued',
      error: null,
      problems: [],
      meter: new RateMeter(),
      paths,
      zip: opts.zip,
      id: null,
      sink: null,
      size: null,
      mtime: null,
      total: null,
      files: 0,
      progress: 0,
      restart: false,
    });
    this.channel.want(true);
    this.changed();
    void this.startDownload();
  }

  pause(key: number): void {
    const job = this.job(key);
    if (!job || !canPause(job)) return;
    if (job.dir === 'down' && job === this.activeDown) {
      if (job.id !== null) this.channel.sendJson({ t: 'cancel', id: job.id });
      job.id = null;
      this.activeDown = null;
    }
    job.state = 'paused';
    this.changed();
    void this.startDownload();
    void this.pumpUploads();
  }

  resume(key: number): void {
    const job = this.job(key);
    if (!job || job.state !== 'paused') return;
    job.state = this.channel.ready ? 'queued' : 'waiting';
    this.channel.want(true);
    this.changed();
    void this.pumpUploads();
    void this.startDownload();
  }

  cancel(key: number): void {
    const job = this.job(key);
    if (!job || isFinished(job.state)) return;
    if (job.dir === 'up') {
      for (const op of job.ops) {
        if (op.kind === 'file' && op.started && !op.answered && op.id !== null) {
          this.channel.sendJson({ t: 'cancel', id: op.id });
        }
        // The agent does not answer a cancel: nothing of this job is outstanding any more.
        op.answered = true;
      }
    } else {
      if (job === this.activeDown) {
        if (job.id !== null) this.channel.sendJson({ t: 'cancel', id: job.id });
        this.activeDown = null;
      }
      const sink = job.sink;
      job.sink = null;
      void sink?.discard();
    }
    job.state = 'cancelled';
    this.changed();
    void this.pumpUploads();
    void this.startDownload();
  }

  /** Run what failed again (a failed download, or the files of an upload that failed). */
  retry(key: number): void {
    const job = this.job(key);
    if (!job || (job.state !== 'failed' && !(job.state === 'done' && job.problems.length))) return;
    job.error = null;
    job.problems = [];
    if (job.dir === 'up') {
      for (const op of job.ops) {
        if (op.error === null) continue;
        op.error = null;
        op.answered = false;
        op.id = null;
        if (op.kind === 'file') {
          job.bytesTotal += op.file.size;
          Object.assign(op, { started: false, sent: 0, acked: 0, allSent: false, needResume: false, token: token() });
        }
      }
      job.low = firstUnanswered(job);
      job.next = job.low;
    } else {
      job.restart = true;
    }
    job.state = 'queued';
    this.channel.want(true);
    this.changed();
    void this.pumpUploads();
    void this.startDownload();
  }

  /** Forget finished transfers. */
  clearFinished(): void {
    for (let i = this.jobs.length - 1; i >= 0; i--) if (isFinished(this.jobs[i]!.state)) this.jobs.splice(i, 1);
    this.changed();
  }

  /** Anything not finished (queued, running, paused, waiting for the connection)? */
  get busy(): boolean {
    return this.jobs.some((j) => !isFinished(j.state));
  }

  /** Anything moving or about to (paused transfers do not count). */
  get working(): boolean {
    return this.jobs.some((j) => j.state === 'queued' || j.state === 'running' || j.state === 'waiting');
  }

  views(): JobView[] {
    const now = performance.now();
    return this.jobs.map((j) => this.view(j, now));
  }

  /** Totals over unfinished transfers, for the compact indicator. */
  summary(): { count: number; bytesDone: number; bytesTotal: number; rate: number; paused: boolean } {
    let count = 0;
    let done = 0;
    let total = 0;
    let rate = 0;
    let paused = true;
    const now = performance.now();
    for (const j of this.jobs) {
      if (isFinished(j.state)) continue;
      const v = this.view(j, now);
      count++;
      done += v.bytesDone;
      total += v.bytesTotal ?? v.bytesDone;
      rate += v.rate;
      if (j.state !== 'paused') paused = false;
    }
    return { count, bytesDone: done, bytesTotal: total, rate, paused: count > 0 && paused };
  }

  // -------------------------------------------------------------------------
  // uploads

  private async pumpUploads(): Promise<void> {
    if (this.pumping) return;
    this.pumping = true;
    try {
      while (this.channel.ready) {
        const pick = this.nextUpWork();
        if (!pick) break;
        const { job, op } = pick;
        if (job.state !== 'running') {
          job.state = 'running';
          this.changed();
        }
        if (op.kind === 'mkdir') {
          op.id = this.channel.allocId();
          this.channel.sendJson({ t: 'mkdir', id: op.id, path: op.path, exist_ok: op.existOk || op.resent });
          job.next++;
          continue;
        }
        if (!op.started) {
          op.id = this.channel.allocId();
          op.started = true;
          job.next++;
          this.channel.sendJson({
            t: 'write',
            id: op.id,
            path: op.path,
            size: op.file.size,
            ...(op.file.lastModified ? { mtime_ms: op.file.lastModified } : {}),
            conflict: op.conflict,
            token: op.token,
            ...(op.needResume ? { resume: true } : {}),
          });
          if (op.needResume) {
            op.resuming = true;
            continue;
          }
        }
        const size = op.file.size;
        const len = Math.min(this.window.chunkSize(), size - op.sent);
        if (!this.window.canSend(len)) break;
        const gen = this.gen;
        const offset = op.sent;
        let data: Uint8Array<ArrayBuffer>;
        try {
          data = new Uint8Array(await op.file.slice(offset, offset + len).arrayBuffer());
        } catch (e) {
          // The local file cannot be read any more (moved, deleted, permissions).
          if (op.id !== null && gen === this.gen) this.channel.sendJson({ t: 'cancel', id: op.id });
          this.opFailed(
            job,
            op,
            `${op.file.name}: cannot read it here (${e instanceof Error ? e.message : String(e)})`,
          );
          continue;
        }
        // The world may have moved on while reading (pause, cancel, reconnect).
        if (gen !== this.gen || op.sent !== offset || op.answered || job.state !== 'running' || !this.channel.ready) {
          continue;
        }
        const end = offset + len >= size;
        const seq = this.window.onSent(data.byteLength, nowUs());
        this.channel.sendBinary(encodeFileData(end ? FileFlags.END : 0, op.id!, seq, offset, data));
        this.upInFlight.push({ seq, op, job, bytes: data.byteLength });
        op.sent += data.byteLength;
        if (end) op.allSent = true;
      }
    } finally {
      this.pumping = false;
    }
  }

  /** The next upload step: data of a started file first (oldest first), else the next op to start. */
  private nextUpWork(): { job: UploadJob; op: UpOp } | null {
    let unanswered = 0;
    let data: { job: UploadJob; op: UpOp } | null = null;
    for (const job of this.jobs) {
      if (job.dir !== 'up') continue;
      while (job.low < job.ops.length && job.ops[job.low]!.answered) job.low++;
      const sendable = job.state === 'running' || job.state === 'queued';
      for (let i = job.low; i < job.next; i++) {
        const op = job.ops[i]!;
        if (op.kind !== 'file' || !op.started || op.answered) continue;
        unanswered++;
        if (!data && sendable && !op.allSent && !op.resuming) data = { job, op };
      }
    }
    if (data) return data;
    if (unanswered >= MAX_UNANSWERED) return null;
    for (const job of this.jobs) {
      if (job.dir !== 'up' || !(job.state === 'running' || job.state === 'queued')) continue;
      while (job.next < job.ops.length) {
        const op = job.ops[job.next]!;
        if (op.answered || (op.kind === 'file' && op.started)) {
          job.next++;
          continue;
        }
        return { job, op };
      }
    }
    return null;
  }

  private findUpOp(id: number): { job: UploadJob; op: UpOp } | null {
    for (const job of this.jobs) {
      if (job.dir !== 'up') continue;
      for (const op of job.ops) if (op.id === id && !op.answered) return { job, op };
    }
    return null;
  }

  private onUploadAnswer(job: UploadJob, op: UpOp, m: FilesAgentMsg): void {
    if (op.kind === 'mkdir') {
      if (m.t === 'ok' || (m.t === 'err' && m.code === 'exists' && op.resent)) {
        op.answered = true;
        this.checkUploadDone(job);
      } else if (m.t === 'err') {
        this.opFailed(job, op, `${baseName(op.path, this.sep)}: ${describeFileError(m.code, m.msg)}`);
      }
      return;
    }
    switch (m.t) {
      case 'write':
        // The offset to resume from.
        if (!op.resuming) return;
        op.resuming = false;
        op.needResume = false;
        job.bytesDone += m.offset - op.acked;
        op.acked = m.offset;
        op.sent = m.offset;
        this.changed();
        void this.pumpUploads();
        return;
      case 'done':
        op.answered = true;
        op.result = m.path;
        job.bytesDone += op.file.size - op.acked;
        op.acked = op.file.size;
        job.meter.add(job.bytesDone);
        this.checkUploadDone(job);
        void this.pumpUploads();
        return;
      case 'err':
        if (m.code === 'exists' && op.sentBeforeLoss) {
          // Finished just before the connection dropped (its `done` was lost)? Then it is there, whole.
          const id = op.id;
          void this.channel
            .request({ t: 'stat', path: op.path })
            .then((r) => r.t === 'stat' && r.entry?.kind === 'file' && r.entry.size === op.file.size)
            .catch(() => false)
            .then((there) => {
              if (op.id !== id || op.answered) return;
              if (there) this.onUploadAnswer(job, op, { t: 'done', id: id!, path: op.path, size: op.file.size });
              else this.opFailed(job, op, `${op.file.name}: ${describeFileError(m.code, m.msg)}`);
            });
          return;
        }
        this.opFailed(job, op, `${op.file.name}: ${describeFileError(m.code, m.msg)}`);
        void this.pumpUploads();
    }
  }

  private opFailed(job: UploadJob, op: UpOp, message: string): void {
    op.answered = true;
    op.error = message;
    if (op.kind === 'file') {
      job.bytesDone -= op.acked;
      job.bytesTotal -= op.file.size;
      op.acked = 0;
    }
    job.problems.push(message);
    this.checkUploadDone(job);
  }

  private checkUploadDone(job: UploadJob): void {
    if (job.state === 'cancelled') return;
    if (job.ops.every((op) => op.answered)) {
      const failed = job.ops.filter((op) => op.error !== null).length;
      if (failed === job.ops.length || (!job.folder && failed > 0)) {
        job.state = 'failed';
        job.error = job.folder ? 'Nothing was uploaded.' : (job.problems[0] ?? 'Upload failed');
      } else job.state = 'done';
    }
    this.changed();
  }

  // -------------------------------------------------------------------------
  // downloads

  private async startDownload(): Promise<void> {
    if (this.activeDown || !this.channel.ready) return;
    const job = this.jobs.find(
      (j): j is DownloadJob => j.dir === 'down' && (j.state === 'queued' || j.state === 'waiting'),
    );
    if (!job) return;
    this.activeDown = job;
    job.state = 'running';
    this.changed();
    // Writes of the previous connection must land first: the offset to resume from is the sink's size.
    await this.downChain;
    if (this.activeDown !== job || job.state !== 'running' || !this.channel.ready) return;
    if (job.restart || job.zip) {
      await job.sink?.truncate();
      job.restart = false;
      job.progress = 0;
    }
    const offset = job.sink?.size ?? 0;
    job.id = this.channel.allocId();
    if (job.zip) {
      this.channel.sendJson({ t: 'read', id: job.id, paths: job.paths, zip: true, name: job.name });
    } else {
      const resume = offset > 0 && job.size !== null && job.mtime !== null;
      this.channel.sendJson({
        t: 'read',
        id: job.id,
        path: job.paths[0]!,
        ...(resume ? { offset, expect: { size: job.size!, mtime_ms: job.mtime! } } : {}),
      });
      if (!resume && job.sink && job.sink.size > 0) await job.sink.truncate();
    }
  }

  private async onDownloadMsg(job: DownloadJob, m: FilesAgentMsg, gen: number): Promise<void> {
    if (gen !== this.gen || job.state !== 'running') return;
    switch (m.t) {
      case 'read':
        if (!job.zip) {
          job.size = m.size;
          job.mtime = m.mtime_ms;
        } else {
          job.total = m.total;
          job.files = m.files ?? 0;
        }
        if (!job.sink) {
          try {
            job.sink = await this.newSink(job.zip ? m.total : m.size);
          } catch (e) {
            if (job.id !== null) this.channel.sendJson({ t: 'cancel', id: job.id });
            this.failDownload(job, e instanceof Error ? e.message : String(e));
            return;
          }
        }
        this.changed();
        return;
      case 'progress':
        job.progress = m.done;
        job.meter.add(m.done);
        this.changed();
        return;
      case 'skipped':
        job.problems.push(...m.paths.map((p) => `${p}: could not be read`));
        this.changed();
        return;
      case 'err':
        if (m.code === 'changed') {
          // The file changed since the part we have: start over.
          job.restart = true;
          job.id = null;
          this.activeDown = null;
          job.state = 'queued';
          void this.startDownload();
          return;
        }
        this.failDownload(job, describeFileError(m.code, m.msg));
    }
  }

  private async writeChunk(job: DownloadJob, m: FileDataMsg): Promise<void> {
    const sink = job.sink;
    if (!sink) throw new SinkError('bytes arrived before the download started');
    await sink.write(m.offset, m.payload);
    if (!job.zip) job.meter.add(sink.size);
    if ((m.flags & FileFlags.END) === 0) {
      this.changed();
      return;
    }
    await sink.save(job.name);
    job.sink = null;
    job.state = 'done';
    job.id = null;
    if (this.activeDown === job) this.activeDown = null;
    this.changed();
    void this.startDownload();
  }

  private failDownload(job: DownloadJob, message: string): void {
    if (isFinished(job.state)) return;
    job.state = 'failed';
    job.error = message;
    job.id = null;
    const sink = job.sink;
    job.sink = null;
    void sink?.discard();
    if (this.activeDown === job) this.activeDown = null;
    this.changed();
    void this.startDownload();
  }

  // -------------------------------------------------------------------------

  private job(key: number): Job | undefined {
    return this.jobs.find((j) => j.key === key);
  }

  private view(j: Job, now: number): JobView {
    const running = j.state === 'running';
    const rate = running ? j.meter.rate(now) : 0;
    let bytesDone: number;
    let bytesTotal: number | null;
    let filesDone = 0;
    let filesTotal = 0;
    let result: string | null = null;
    if (j.dir === 'up') {
      bytesDone = j.bytesDone;
      bytesTotal = j.bytesTotal;
      filesTotal = j.filesTotal;
      for (const op of j.ops) if (op.kind === 'file' && op.answered && op.error === null) filesDone++;
      const only = j.ops.length === 1 ? j.ops[0] : undefined;
      if (only?.kind === 'file') result = only.result;
    } else if (j.zip) {
      // Progress is in source bytes; the agent does not say how many files are done.
      bytesDone = j.progress;
      bytesTotal = j.total;
    } else {
      bytesDone = j.sink?.size ?? (j.state === 'done' ? (j.size ?? 0) : 0);
      bytesTotal = j.size;
    }
    const left = bytesTotal !== null ? Math.max(0, bytesTotal - bytesDone) : null;
    return {
      key: j.key,
      dir: j.dir,
      name: j.name,
      where: j.where,
      folder: j.folder,
      state: j.state,
      bytesDone,
      bytesTotal,
      filesDone,
      filesTotal,
      rate,
      etaS: running && rate > 0 && left !== null ? left / rate : null,
      error: j.error,
      problems: j.problems,
      canPause: canPause(j),
      canResume: j.state === 'paused',
      canRetry: j.state === 'failed' || (j.state === 'done' && j.problems.length > 0 && j.dir === 'up'),
      result,
    };
  }

  /** Coalesce change notifications into one per task. */
  private changed(): void {
    if (this.changeQueued) return;
    this.changeQueued = true;
    queueMicrotask(() => {
      this.changeQueued = false;
      this.onChange();
    });
  }
}

function mkdir(path: string, existOk: boolean): MkdirOp {
  return { kind: 'mkdir', path, existOk, id: null, answered: false, resent: false, error: null };
}

function firstUnanswered(job: UploadJob): number {
  const i = job.ops.findIndex((op) => !op.answered);
  return i < 0 ? job.ops.length : i;
}

function isFinished(s: JobState): boolean {
  return s === 'done' || s === 'failed' || s === 'cancelled';
}

/** Zip downloads cannot resume, so they cannot pause either. */
function canPause(j: Job): boolean {
  return (j.state === 'running' || j.state === 'queued' || j.state === 'waiting') && !(j.dir === 'down' && j.zip);
}

function parentOf(path: string, sep: string): string {
  const name = baseName(path, sep);
  if (name === path) return path;
  const parent = path.slice(0, path.length - name.length).replace(/[\\/]$/, '');
  return parent === '' || /^[A-Za-z]:$/.test(parent) ? parent + sep : parent;
}

export { FileError };
