// Preview of a remote image, audio or video file inside the file browser. The file is fetched
// whole over the file channel (a preview download, files/transfers.ts) and then shown by the
// browser's own <img>, <audio> or <video>; the arrows step through the folder's other media.

import { formatSize } from '../files/format';
import type { FilesController } from '../files/controller';
import { PREVIEW_ASK_BYTES, mediaType } from '../files/media';
import { joinPath } from '../files/paths';
import type { PreviewFile, PreviewRequest } from '../files/transfers';
import type { Entry } from '../proto/files';
import { type Child, h, setChildren } from './dom';
import { icon } from './icons';

export interface FilePreviewOptions {
  files: FilesController;
  /** The folder the entries are in. */
  dir: string;
  sep: string;
  /** The folder's previewable files, in the order the list shows them. */
  entries: Entry[];
  /** Another file is shown (the list selects it too). */
  shown(e: Entry): void;
  download(e: Entry): void;
  close(): void;
}

export class FilePreview {
  readonly el: HTMLElement;
  private readonly title = h('span', { className: 'preview-name' });
  private readonly counter = h('span', { className: 'preview-count' });
  private readonly body = h('div', { className: 'preview-body' });
  private readonly meta = h('div', { className: 'preview-meta' });
  private readonly prevBtn: HTMLButtonElement;
  private readonly nextBtn: HTMLButtonElement;
  private index = 0;
  private request: PreviewRequest | null = null;
  private file: PreviewFile | null = null;
  private url: string | null = null;
  private media: HTMLMediaElement | null = null;
  /** Bumped on every file shown: answers for an earlier one are dropped. */
  private seq = 0;

  constructor(private readonly opts: FilePreviewOptions) {
    const btn = (name: Parameters<typeof icon>[0], label: string, run: () => void) =>
      h(
        'button',
        { type: 'button', className: 'icon-button', title: label, ariaLabel: label, onclick: run },
        icon(name, 17),
      );
    this.prevBtn = btn('back', 'Previous (←)', () => this.step(-1));
    this.nextBtn = btn('forward', 'Next (→)', () => this.step(1));
    this.el = h(
      'div',
      { className: 'files-preview', role: 'dialog', ariaLabel: 'Preview', tabIndex: -1 },
      h(
        'header',
        { className: 'preview-head' },
        this.prevBtn,
        this.nextBtn,
        h('div', { className: 'preview-title' }, this.title, this.counter),
        btn('download', 'Download', () => this.downloadCurrent()),
        btn('close', 'Close (Esc)', () => opts.close()),
      ),
      this.body,
      this.meta,
    );
    this.el.addEventListener('keydown', (e) => this.onKey(e));
  }

  /** Show the entry at `index` of the folder's previewable files. */
  show(index: number): void {
    const e = this.opts.entries[index];
    if (!e) return;
    this.index = index;
    this.reset();
    const seq = this.seq;
    this.title.textContent = e.name;
    this.title.title = e.name;
    const n = this.opts.entries.length;
    this.counter.textContent = n > 1 ? `${index + 1} of ${n}` : '';
    this.prevBtn.disabled = index === 0;
    this.nextBtn.disabled = index === n - 1;
    this.prevBtn.hidden = this.nextBtn.hidden = n < 2;
    this.meta.textContent = formatSize(e.size);
    if (e.size > PREVIEW_ASK_BYTES) {
      this.state(
        'ask',
        icon('eye', 28),
        h('p', null, `This file is ${formatSize(e.size)}. A preview fetches all of it before it plays.`),
        h(
          'div',
          { className: 'preview-buttons' },
          h('button', { type: 'button', className: 'primary', onclick: () => this.fetch(e, seq) }, 'Preview'),
          h('button', { type: 'button', onclick: () => this.downloadCurrent() }, 'Download instead'),
        ),
      );
    } else {
      this.fetch(e, seq);
    }
    this.opts.shown(e);
  }

  focus(): void {
    this.el.focus({ preventScroll: true });
  }

  /** Stop fetching, free the file, take the pane away. */
  destroy(): void {
    this.reset();
    this.el.remove();
  }

  // -------------------------------------------------------------------------

  private fetch(e: Entry, seq: number): void {
    if (seq !== this.seq) return;
    const type = mediaType(e.name);
    if (!type) return;
    const bar = h('progress', { max: 1, value: 0 });
    const label = h('p', null, 'Fetching…');
    this.state('loading', bar, label);
    const req = this.opts.files.transfers.preview(joinPath(this.opts.dir, e.name, this.opts.sep), type.mime, (p) => {
      if (seq !== this.seq) return;
      const total = p.total ?? e.size;
      bar.value = total > 0 ? p.done / total : 0;
      label.textContent = `${formatSize(p.done)} of ${formatSize(total)}`;
    });
    this.request = req;
    req.result.then(
      (file) => {
        if (seq !== this.seq) {
          file.release();
          return;
        }
        this.request = null;
        this.file = file;
        this.url = URL.createObjectURL(file.blob);
        this.present(e, type.kind, this.url, seq);
      },
      (err: unknown) => {
        if (seq !== this.seq) return;
        this.request = null;
        this.failed(err instanceof Error ? err.message : String(err));
      },
    );
  }

  private present(e: Entry, kind: 'image' | 'audio' | 'video', url: string, seq: number): void {
    const size = formatSize(e.size);
    // Events of an element already replaced (its URL revoked) must not touch the pane.
    const current = () => seq === this.seq;
    if (kind === 'image') {
      const img = h('img', { className: 'preview-image', alt: e.name });
      img.onload = () => {
        if (!current()) return;
        this.meta.textContent = `${img.naturalWidth} × ${img.naturalHeight} · ${size}`;
      };
      img.onerror = () => current() && this.failed('This browser cannot show this image.');
      img.src = url;
      this.state('media', img);
      return;
    }
    const media = h(kind, { className: `preview-${kind}`, controls: true, autoplay: true, preload: 'auto' });
    if (media instanceof HTMLVideoElement) media.playsInline = true;
    media.onloadedmetadata = () => {
      if (!current()) return;
      const parts = [formatDuration(media.duration)];
      if (media instanceof HTMLVideoElement && media.videoWidth)
        parts.push(`${media.videoWidth} × ${media.videoHeight}`);
      parts.push(size);
      this.meta.textContent = parts.filter(Boolean).join(' · ');
    };
    media.onerror = () => current() && this.failed('This browser cannot play this file.');
    media.src = url;
    this.media = media;
    this.state(
      'media',
      kind === 'audio' ? h('div', { className: 'preview-audio-wrap' }, icon('play', 40), media) : media,
    );
  }

  private failed(message: string): void {
    this.media?.pause();
    this.media = null;
    this.state(
      'error',
      icon('alert', 28),
      h('p', null, message),
      h(
        'div',
        { className: 'preview-buttons' },
        h('button', { type: 'button', onclick: () => this.downloadCurrent() }, 'Download'),
      ),
    );
  }

  private state(kind: 'ask' | 'loading' | 'media' | 'error', ...children: Child[]): void {
    this.body.className = `preview-body preview-${kind}`;
    setChildren(this.body, ...children);
  }

  private reset(): void {
    this.seq++;
    this.request?.cancel();
    this.request = null;
    if (this.media) {
      this.media.pause();
      this.media.removeAttribute('src');
      this.media.load();
      this.media = null;
    }
    setChildren(this.body);
    if (this.url) URL.revokeObjectURL(this.url);
    this.url = null;
    this.file?.release();
    this.file = null;
  }

  private step(d: number): void {
    const i = this.index + d;
    if (i >= 0 && i < this.opts.entries.length) this.show(i);
  }

  private downloadCurrent(): void {
    const e = this.opts.entries[this.index];
    if (e) this.opts.download(e);
  }

  private onKey(e: KeyboardEvent): void {
    // The media controls use the arrows and Space themselves.
    const inControls = e.target instanceof HTMLMediaElement || e.target instanceof HTMLButtonElement;
    const handled = () => {
      e.preventDefault();
      e.stopPropagation();
    };
    if (e.key === 'Escape') return (handled(), this.opts.close());
    if (inControls || e.metaKey || e.ctrlKey || e.altKey) return;
    switch (e.key) {
      case 'ArrowLeft':
      case 'ArrowUp':
        return (handled(), this.step(-1));
      case 'ArrowRight':
      case 'ArrowDown':
        return (handled(), this.step(1));
      case ' ':
        handled();
        // Space plays and pauses; on an image it closes, like Quick Look.
        if (!this.media) return this.opts.close();
        if (this.media.paused) void this.media.play().catch(() => undefined);
        else this.media.pause();
    }
  }
}

/** `0:42`, `12:05`, `1:02:09`. */
function formatDuration(s: number): string {
  if (!Number.isFinite(s) || s <= 0) return '';
  const t = Math.round(s);
  const hh = Math.floor(t / 3600);
  const mm = Math.floor((t % 3600) / 60);
  const ss = String(t % 60).padStart(2, '0');
  return hh ? `${hh}:${String(mm).padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
}
