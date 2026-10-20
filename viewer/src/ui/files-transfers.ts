import { formatDuration, formatRate, formatSize } from '../files/format';
import type { JobView, TransferManager } from '../files/transfers';
import { h, setChildren } from './dom';
import { icon } from './icons';

/** One line about a transfer: progress, speed and time left, or how it ended. */
export function describeJob(v: JobView): string {
  const files = v.folder && v.filesTotal > 0 ? `${v.filesDone} of ${v.filesTotal} files · ` : '';
  const amount =
    v.bytesTotal !== null ? `${formatSize(v.bytesDone)} of ${formatSize(v.bytesTotal)}` : formatSize(v.bytesDone);
  switch (v.state) {
    case 'queued':
      return v.bytesTotal !== null ? `Waiting · ${formatSize(v.bytesTotal)}` : 'Waiting';
    case 'running': {
      const speed = v.rate > 0 ? ` · ${formatRate(v.rate)}` : '';
      const eta = v.etaS !== null && v.etaS >= 1 ? ` · ${formatDuration(v.etaS)} left` : '';
      return `${files}${amount}${speed}${eta}`;
    }
    case 'paused':
      return `Paused · ${files}${amount}`;
    case 'waiting':
      return `Connection lost, continuing when it is back · ${amount}`;
    case 'done': {
      const where = v.dir === 'down' ? 'Saved to your downloads' : `Uploaded to ${v.where}`;
      const problems = v.problems.length ? ` · ${v.problems.length} not transferred` : '';
      return `${where}${problems}`;
    }
    case 'failed':
      return v.error ?? 'Failed';
    case 'cancelled':
      return 'Cancelled';
  }
}

export function percent(v: { bytesDone: number; bytesTotal: number | null }): number | null {
  if (v.bytesTotal === null) return null;
  if (v.bytesTotal === 0) return 100;
  return Math.min(100, (v.bytesDone / v.bytesTotal) * 100);
}

/** The transfer list at the bottom of the file browser. */
export class TransfersView {
  readonly el = h('section', { className: 'files-transfers', ariaLabel: 'Transfers' });
  private readonly list = h('ul', { className: 'transfer-list' });
  private readonly title = h('h3', null, 'Transfers');
  private readonly clear = h('button', { type: 'button', className: 'small ghost' }, 'Clear finished');
  private expanded = new Set<number>();

  constructor(
    private readonly transfers: TransferManager,
    private readonly show: (folder: string) => void,
  ) {
    this.clear.onclick = () => this.transfers.clearFinished();
    this.el.append(h('header', { className: 'transfers-head' }, this.title, this.clear), this.list);
  }

  render(): void {
    const views = this.transfers.views();
    this.el.hidden = views.length === 0;
    const open = views.filter((v) => v.state !== 'done' && v.state !== 'cancelled' && v.state !== 'failed').length;
    this.title.textContent = open ? `Transfers · ${open} in progress` : 'Transfers';
    this.clear.hidden = views.every((v) => v.state !== 'done' && v.state !== 'cancelled' && v.state !== 'failed');
    // Newest first.
    setChildren(this.list, ...views.reverse().map((v) => this.row(v)));
  }

  private row(v: JobView): HTMLElement {
    const pct = percent(v);
    const act = (label: string, name: Parameters<typeof icon>[0], run: () => void) =>
      h(
        'button',
        { type: 'button', className: 'icon-button', title: label, ariaLabel: label, onclick: run },
        icon(name, 16),
      );
    const actions = h('div', { className: 'transfer-actions' });
    if (v.canPause) actions.append(act('Pause', 'pause', () => this.transfers.pause(v.key)));
    if (v.canResume) actions.append(act('Resume', 'play', () => this.transfers.resume(v.key)));
    if (v.canRetry) actions.append(act('Try again', 'retry', () => this.transfers.retry(v.key)));
    if (v.state === 'done' && v.dir === 'up') {
      actions.append(act('Show in the file browser', 'folder', () => this.show(v.where)));
    }
    if (v.state !== 'done' && v.state !== 'cancelled' && v.state !== 'failed') {
      actions.append(act('Cancel', 'close', () => this.transfers.cancel(v.key)));
    }
    const bar =
      v.state === 'running' || v.state === 'paused' || v.state === 'waiting' || v.state === 'queued'
        ? h(
            'div',
            { className: `transfer-bar${pct === null ? ' indeterminate' : ''}` },
            h('span', { style: `width:${pct ?? 30}%` }),
          )
        : null;
    const problems =
      v.problems.length > 0
        ? h(
            'details',
            {
              className: 'transfer-problems',
              open: this.expanded.has(v.key),
              ontoggle: (e: Event) => {
                if ((e.target as HTMLDetailsElement).open) this.expanded.add(v.key);
                else this.expanded.delete(v.key);
              },
            },
            h('summary', null, `${v.problems.length} not transferred`),
            h('ul', null, ...v.problems.slice(0, 200).map((p) => h('li', null, p))),
          )
        : null;
    return h(
      'li',
      { className: `transfer ${v.state}` },
      h(
        'span',
        { className: 'transfer-icon', title: v.dir === 'up' ? 'Upload' : 'Download' },
        icon(
          v.state === 'done' ? 'check' : v.state === 'failed' ? 'alert' : v.dir === 'up' ? 'upload' : 'download',
          16,
        ),
      ),
      h(
        'div',
        { className: 'transfer-main' },
        h(
          'div',
          { className: 'transfer-name', title: v.name },
          icon(v.folder ? 'folder' : 'file', 14),
          h('span', null, v.name),
        ),
        bar,
        h('div', { className: 'transfer-detail' }, describeJob(v)),
        problems,
      ),
      actions,
    );
  }
}

/** The compact indicator on the session screen while the browser is closed. */
export class TransferIndicator {
  readonly el = h('button', { type: 'button', className: 'transfer-indicator', hidden: true });

  constructor(
    private readonly transfers: TransferManager,
    open: () => void,
  ) {
    this.el.onclick = open;
  }

  render(visible: boolean): void {
    const s = this.transfers.summary();
    this.el.hidden = !visible || s.count === 0;
    if (this.el.hidden) return;
    const pct = s.bytesTotal > 0 ? Math.floor((s.bytesDone / s.bytesTotal) * 100) : 0;
    const label = `${s.count === 1 ? '1 transfer' : `${s.count} transfers`} · ${s.paused ? 'paused' : `${pct}%`}${
      s.rate > 0 ? ` · ${formatRate(s.rate)}` : ''
    }`;
    this.el.title = 'Show transfers';
    setChildren(
      this.el,
      icon('transfer', 16),
      h('span', null, label),
      h('span', { className: 'transfer-indicator-bar' }, h('span', { style: `width:${pct}%` })),
    );
  }
}
