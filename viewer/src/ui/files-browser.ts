// The file browser (docs/files.md): browse the remote machine's folders, download, upload by drop
// or picker, new folder, rename, move to the Trash. Used as a drawer next to the remote screen and
// as a page of its own on the connect screen.

import { FileError } from '../files/channel';
import type { FilesController } from '../files/controller';
import { formatMtime, formatSize } from '../files/format';
import { baseName, caseSensitive, invalidName, joinPath } from '../files/paths';
import { type UploadItem, dragHasFiles, itemsFromDrop, itemsFromFiles, totals } from '../files/sources';
import { type Entry, FileCaps, type Place, describeFileError } from '../proto/files';
import { type Child, h, setChildren } from './dom';
import { clashDialog, confirmDialog, noticeDialog } from './files-dialogs';
import { TransfersView } from './files-transfers';
import { type IconName, icon } from './icons';

const ROW_HEIGHT = 32;
const OVERSCAN = 8;
const TYPEAHEAD_MS = 700;

type SortKey = 'name' | 'size' | 'mtime';

interface Listing {
  path: string;
  parent: string | null;
  entries: Entry[];
  truncated: boolean;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

const PLACE_ICONS: Record<Place['kind'], IconName> = {
  home: 'home',
  desktop: 'monitor',
  documents: 'documents',
  downloads: 'download',
  drive: 'drive',
  volume: 'drive',
  other: 'folder',
};

function prefKey(machine: string, what: string): string {
  return `rd.files.${what}.${machine}`;
}

export type FilesLayout = 'drawer' | 'page';

export interface FilesBrowserOptions {
  machineName(): string;
  /** `drawer`: next to the remote screen; `page`: the whole window. */
  layout: FilesLayout;
  close(): void;
}

export class FilesBrowser {
  readonly el: HTMLElement;
  private readonly status = h('div', { className: 'files-status', role: 'status' });
  private readonly titleName = h('span', { className: 'files-title-name' });
  private readonly places = h('nav', { className: 'files-places', ariaLabel: 'Places' });
  private readonly placesMenu = h('select', { className: 'files-places-select', ariaLabel: 'Places' });
  private readonly crumbs = h('div', { className: 'files-crumbs' });
  private readonly filterInput = h('input', {
    type: 'search',
    className: 'files-filter',
    placeholder: 'Filter',
    ariaLabel: 'Filter this folder',
  });
  private readonly backBtn: HTMLButtonElement;
  private readonly fwdBtn: HTMLButtonElement;
  private readonly upBtn: HTMLButtonElement;
  private readonly actions = h('div', { className: 'files-actions' });
  private readonly head = h('div', { className: 'files-list-head', role: 'row' });
  private readonly scroller = h('div', { className: 'files-scroll', tabIndex: 0, role: 'grid' });
  private readonly spacer = h('div', { className: 'files-spacer' });
  private readonly rowsEl = h('div', { className: 'files-rows' });
  private readonly empty = h('div', { className: 'files-empty' });
  private readonly foot = h('div', { className: 'files-foot' });
  private readonly dropHint = h('div', { className: 'files-drop', hidden: true });
  private readonly transfersView: TransfersView;
  private readonly fileInput = h('input', { type: 'file', multiple: true, hidden: true });
  private readonly folderInput = h('input', { type: 'file', multiple: true, hidden: true });
  private readonly menu = h('div', { className: 'files-menu', role: 'menu', hidden: true });

  private listing: Listing | null = null;
  private loading = false;
  private error: string | null = null;
  private dir: string | null = null;
  private history: string[] = [];
  private historyAt = -1;
  private sort: { key: SortKey; asc: boolean } = { key: 'name', asc: true };
  private filter = '';
  private showHidden: boolean;
  private view: Entry[] = [];
  private readonly selected = new Set<string>();
  private cursor = -1;
  private anchor = -1;
  private renaming: string | null = null;
  private creating = false;
  private typeahead = '';
  private typeaheadAt = 0;
  private loadSeq = 0;
  private dragDepth = 0;
  private dropTarget: string | null = null;
  private opened = false;
  private unsubscribe: (() => void) | null = null;
  private renderQueued = false;
  private lastClick: { name: string; at: number } | null = null;

  constructor(
    private readonly files: FilesController,
    private readonly opts: FilesBrowserOptions,
  ) {
    this.showHidden = localStorage.getItem(prefKey(files.machine, 'hidden')) === '1';
    const nav = (name: IconName, label: string, run: () => void) =>
      h(
        'button',
        { type: 'button', className: 'icon-button', title: label, ariaLabel: label, onclick: run },
        icon(name, 17),
      );
    this.backBtn = nav('back', 'Back (Alt+←)', () => this.goHistory(-1));
    this.fwdBtn = nav('forward', 'Forward (Alt+→)', () => this.goHistory(1));
    this.upBtn = nav('up', 'Enclosing folder (Alt+↑)', () => this.goUp());
    const refresh = nav('refresh', 'Refresh (F5)', () => void this.reload());
    this.transfersView = new TransfersView(files.transfers, (folder) => void this.navigate(folder));
    this.scroller.append(this.spacer, this.rowsEl, this.empty);
    this.fileInput.onchange = () => this.uploadPicked(this.fileInput);
    this.folderInput.onchange = () => this.uploadPicked(this.folderInput);
    this.folderInput.setAttribute('webkitdirectory', '');
    this.placesMenu.onchange = () => {
      if (this.placesMenu.value) void this.navigate(this.placesMenu.value);
    };
    this.filterInput.oninput = () => {
      this.filter = this.filterInput.value.trim().toLowerCase();
      this.refreshView(true);
    };
    this.filterInput.onkeydown = (e) => {
      if (e.key === 'ArrowDown' || e.key === 'Enter') {
        e.preventDefault();
        if (this.view.length) this.setCursor(0, 'select');
        this.scroller.focus();
      } else if (e.key === 'Escape' && this.filterInput.value) {
        e.preventDefault();
        e.stopPropagation();
        this.filterInput.value = '';
        this.filter = '';
        this.refreshView(true);
      }
    };
    this.crumbs.onclick = (e) => {
      if (e.target === this.crumbs) this.editPath();
    };

    this.el = h(
      'section',
      { className: `files files-${opts.layout}`, ariaLabel: 'Files', tabIndex: -1 },
      h(
        'header',
        { className: 'files-head' },
        h('div', { className: 'files-title' }, icon('folder', 18), h('span', null, 'Files'), this.titleName),
        this.status,
        h(
          'button',
          {
            type: 'button',
            className: 'icon-button',
            title: opts.layout === 'drawer' ? 'Close (Esc)' : 'Back',
            ariaLabel: 'Close',
            onclick: () => opts.close(),
          },
          icon('close', 18),
        ),
      ),
      h(
        'div',
        { className: 'files-nav' },
        this.backBtn,
        this.fwdBtn,
        this.upBtn,
        refresh,
        this.crumbs,
        this.placesMenu,
        this.filterInput,
      ),
      h(
        'div',
        { className: 'files-body' },
        this.places,
        h('div', { className: 'files-main' }, this.actions, this.head, this.scroller, this.foot),
      ),
      this.transfersView.el,
      this.dropHint,
      this.fileInput,
      this.folderInput,
      this.menu,
    );
    this.wireList();
    this.wireDrop();
    this.el.addEventListener('keydown', (e) => this.onPanelKey(e));
    document.addEventListener('pointerdown', (e) => {
      if (!this.menu.hidden && !this.menu.contains(e.target as Node)) this.closeMenu();
    });
    this.renderAll();
  }

  /** The folder shown, once one is. */
  get currentDir(): string | null {
    return this.dir;
  }

  get isOpen(): boolean {
    return this.opened;
  }

  setLayout(layout: FilesLayout): void {
    this.opts.layout = layout;
    this.el.classList.toggle('files-drawer', layout === 'drawer');
    this.el.classList.toggle('files-page', layout === 'page');
  }

  open(): void {
    if (this.opened) return;
    this.opened = true;
    this.files.setBrowserOpen(true);
    this.unsubscribe = this.files.subscribe(() => this.scheduleRender());
    if (this.dir === null) void this.start();
    else void this.reload();
    this.scheduleRender();
    queueMicrotask(() => this.scroller.focus({ preventScroll: true }));
  }

  close(): void {
    if (!this.opened) return;
    this.opened = false;
    this.closeMenu();
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.files.setBrowserOpen(false);
  }

  focus(): void {
    this.scroller.focus({ preventScroll: true });
  }

  /** Upload dropped or picked items into `dest` (default: the folder shown). */
  async uploadInto(items: UploadItem[], dest: string | null = this.dir): Promise<void> {
    if (!items.length) return;
    try {
      const folder = dest ?? (await this.files.defaultDropFolder());
      const name = this.folderName(folder);
      await this.files.upload(items, folder, (c) => clashDialog(c, name));
      if (folder === this.dir) void this.reloadSoon();
    } catch (e) {
      await noticeDialog('Upload not started', errorText(e));
    }
  }

  // -------------------------------------------------------------------------
  // navigation

  private async start(): Promise<void> {
    try {
      await this.files.channel.whenReady();
    } catch {
      this.scheduleRender();
      return;
    }
    const h = this.files.hello!;
    const saved = localStorage.getItem(prefKey(this.files.machine, 'dir'));
    const first = saved || h.home;
    if (!(await this.navigate(first)) && first !== h.home) await this.navigate(h.home);
  }

  /** Show `path`; false when it could not be listed. */
  async navigate(path: string, fromHistory = false): Promise<boolean> {
    if (!fromHistory) {
      this.history = this.history.slice(0, this.historyAt + 1);
      if (this.history[this.history.length - 1] !== path) this.history.push(path);
      this.historyAt = this.history.length - 1;
    }
    const ok = await this.load(path, true);
    if (ok && this.listing) localStorage.setItem(prefKey(this.files.machine, 'dir'), this.listing.path);
    return ok;
  }

  private goHistory(step: number): void {
    const i = this.historyAt + step;
    const path = this.history[i];
    if (path === undefined) return;
    this.historyAt = i;
    void this.navigate(path, true);
  }

  private goUp(): void {
    const parent = this.listing?.parent;
    if (!parent) return;
    const child = this.listing ? baseName(this.listing.path, this.sep) : null;
    void this.navigate(parent).then(() => {
      if (child) this.selectName(child);
    });
  }

  private async reload(): Promise<void> {
    if (this.dir) await this.load(this.dir, false);
    else void this.start();
  }

  private reloadTimer = 0;
  /** Refresh shortly (after uploads land); coalesced. */
  private reloadSoon(): Promise<void> {
    clearTimeout(this.reloadTimer);
    return new Promise((resolve) => {
      this.reloadTimer = window.setTimeout(() => void this.reload().then(resolve), 400);
    });
  }

  private async load(path: string, fresh: boolean): Promise<boolean> {
    const seq = ++this.loadSeq;
    this.loading = true;
    this.dir = path;
    this.scheduleRender();
    try {
      const reply = await this.files.channel.request({ t: 'list', path });
      if (seq !== this.loadSeq) return false;
      if (reply.t !== 'list') throw new FileError('io', 'unexpected answer');
      const keep = fresh ? null : new Set(this.selected);
      this.listing = { path: reply.path, parent: reply.parent, entries: reply.entries, truncated: reply.truncated };
      this.dir = reply.path;
      this.error = null;
      this.selected.clear();
      if (keep) for (const n of keep) if (reply.entries.some((e) => e.name === n)) this.selected.add(n);
      if (fresh) {
        this.cursor = -1;
        this.anchor = -1;
        this.scroller.scrollTop = 0;
        this.filter = '';
        this.filterInput.value = '';
      }
      this.renaming = null;
      this.creating = false;
      return true;
    } catch (e) {
      if (seq !== this.loadSeq) return false;
      this.error = errorText(e);
      if (fresh) this.listing = null;
      return false;
    } finally {
      if (seq === this.loadSeq) {
        this.loading = false;
        this.refreshView(false);
        this.renderAll();
      }
    }
  }

  private editPath(): void {
    const input = h('input', { type: 'text', className: 'files-path-input', value: this.dir ?? '', ariaLabel: 'Path' });
    const done = (go: boolean) => {
      const path = input.value.trim();
      this.renderCrumbs();
      if (go && path && path !== this.dir) void this.navigate(path);
    };
    input.onkeydown = (e) => {
      e.stopPropagation();
      if (e.key === 'Enter') done(true);
      if (e.key === 'Escape') {
        e.preventDefault();
        done(false);
        this.focus();
      }
    };
    input.onblur = () => done(false);
    setChildren(this.crumbs, input);
    input.focus();
    input.select();
  }

  // -------------------------------------------------------------------------
  // the list

  private get sep(): string {
    return this.files.hello?.sep ?? '/';
  }

  private get os(): string {
    return this.files.hello?.os ?? '';
  }

  private refreshView(keepCursorName: boolean): void {
    const cursorName = keepCursorName ? this.view[this.cursor]?.name : undefined;
    const entries = this.listing?.entries ?? [];
    const { key, asc } = this.sort;
    const dir = asc ? 1 : -1;
    this.view = entries
      .filter((e) => (this.showHidden || !e.hidden) && (!this.filter || e.name.toLowerCase().includes(this.filter)))
      .sort((a, b) => {
        const ad = a.kind === 'dir' ? 0 : 1;
        const bd = b.kind === 'dir' ? 0 : 1;
        if (ad !== bd) return ad - bd;
        let c = 0;
        if (key === 'size') c = a.size - b.size;
        else if (key === 'mtime') c = a.mtime_ms - b.mtime_ms;
        return (c || collator.compare(a.name, b.name)) * dir;
      });
    for (const n of [...this.selected]) if (!this.view.some((e) => e.name === n)) this.selected.delete(n);
    this.cursor =
      cursorName !== undefined
        ? this.view.findIndex((e) => e.name === cursorName)
        : Math.min(this.cursor, this.view.length - 1);
    this.renderList();
  }

  private wireList(): void {
    this.scroller.addEventListener('scroll', () => this.renderRows());
    new ResizeObserver(() => this.renderRows()).observe(this.scroller);
    this.scroller.addEventListener('keydown', (e) => this.onListKey(e));
    this.scroller.addEventListener('contextmenu', (e) => {
      if ((e.target as HTMLElement).closest('.frow')) return;
      e.preventDefault();
      this.selected.clear();
      this.renderRows();
      this.openMenu(e.clientX, e.clientY, null);
    });
    this.scroller.addEventListener('pointerdown', (e) => {
      if (e.target === this.scroller || e.target === this.spacer || e.target === this.empty) {
        this.selected.clear();
        this.cursor = -1;
        this.renderRows();
        this.renderFoot();
      }
    });
  }

  private renderList(): void {
    this.spacer.style.height = `${(this.view.length + (this.creating ? 1 : 0)) * ROW_HEIGHT}px`;
    this.renderHead();
    this.renderRows();
    this.renderEmpty();
    this.renderFoot();
    this.renderActions();
  }

  private renderHead(): void {
    const col = (key: SortKey, label: string) => {
      const on = this.sort.key === key;
      return h(
        'button',
        {
          type: 'button',
          className: `files-col col-${key}${on ? ' on' : ''}`,
          ariaSort: on ? (this.sort.asc ? 'ascending' : 'descending') : 'none',
          onclick: () => {
            this.sort = { key, asc: on ? !this.sort.asc : key === 'name' };
            this.refreshView(true);
          },
        },
        label,
        on ? h('span', { className: 'sort-arrow' }, this.sort.asc ? '▲' : '▼') : null,
      );
    };
    setChildren(this.head, col('name', 'Name'), col('size', 'Size'), col('mtime', 'Modified'));
  }

  private renderRows(): void {
    const total = this.view.length + (this.creating ? 1 : 0);
    const top = this.scroller.scrollTop;
    const height = this.scroller.clientHeight || 400;
    const first = Math.max(0, Math.floor(top / ROW_HEIGHT) - OVERSCAN);
    const last = Math.min(total, Math.ceil((top + height) / ROW_HEIGHT) + OVERSCAN);
    const rows: HTMLElement[] = [];
    for (let i = first; i < last; i++) {
      if (this.creating && i === 0) {
        rows.push(this.newFolderRow());
        continue;
      }
      const index = this.creating ? i - 1 : i;
      const e = this.view[index];
      if (e) rows.push(this.row(e, index, i));
    }
    setChildren(this.rowsEl, ...rows);
    const editing = this.rowsEl.querySelector('input');
    if (editing && document.activeElement !== editing) {
      editing.focus();
      const dot = editing.value.lastIndexOf('.');
      editing.setSelectionRange(0, this.renaming && dot > 0 ? dot : editing.value.length);
    }
  }

  private row(e: Entry, index: number, slot: number): HTMLElement {
    const selected = this.selected.has(e.name);
    const isDir = e.kind === 'dir';
    const nameCell =
      this.renaming === e.name
        ? this.nameEditor(e.name, (name) => void this.commitRename(e, name))
        : h('span', { className: 'fname-text' }, e.name);
    const row = h(
      'div',
      {
        className: [
          'frow',
          selected ? 'selected' : '',
          index === this.cursor ? 'cursor' : '',
          e.hidden ? 'hidden-entry' : '',
          isDir && this.dropTarget === e.name ? 'drop-target' : '',
        ]
          .filter(Boolean)
          .join(' '),
        role: 'row',
        ariaSelected: String(selected),
        style: `top:${slot * ROW_HEIGHT}px`,
        title: e.link ? `${e.name} (link)` : e.name,
      },
      h(
        'span',
        { className: 'fname', role: 'gridcell' },
        h(
          'span',
          { className: `ficon ${isDir ? 'dir' : 'file'}` },
          icon(isDir ? 'folder' : e.kind === 'file' ? 'file' : 'alert', 17),
        ),
        e.link ? h('span', { className: 'flink', title: 'Symbolic link' }, icon('link', 12)) : null,
        nameCell,
      ),
      h('span', { className: 'fsize', role: 'gridcell' }, isDir ? '—' : formatSize(e.size)),
      h('span', { className: 'fdate', role: 'gridcell' }, formatMtime(e.mtime_ms)),
    );
    row.addEventListener('pointerdown', (ev) => {
      if (ev.button !== 0 || this.renaming === e.name) return;
      // Rows are redrawn on every selection change, so the browser's dblclick (same element twice) never
      // fires: count the clicks here.
      const now = performance.now();
      const double =
        !ev.shiftKey && !ev.metaKey && !ev.ctrlKey && this.lastClick?.name === e.name && now - this.lastClick.at < 450;
      this.lastClick = double ? null : { name: e.name, at: now };
      if (double) {
        ev.preventDefault();
        this.openEntry(e);
        return;
      }
      this.clickRow(index, ev);
    });
    row.addEventListener('contextmenu', (ev) => {
      ev.preventDefault();
      if (!this.selected.has(e.name)) {
        this.selected.clear();
        this.selected.add(e.name);
        this.cursor = index;
        this.anchor = index;
        this.renderRows();
        this.renderFoot();
        this.renderActions();
      }
      this.openMenu(ev.clientX, ev.clientY, e);
    });
    return row;
  }

  private nameEditor(initial: string, commit: (name: string) => void): HTMLElement {
    const input = h('input', {
      type: 'text',
      className: 'fname-input',
      value: initial,
      ariaLabel: 'Name',
      spellcheck: false,
    });
    let finished = false;
    const finish = (save: boolean) => {
      if (finished) return;
      finished = true;
      if (save && input.value !== initial) commit(input.value);
      else {
        this.renaming = null;
        this.creating = false;
        this.renderList();
        this.focus();
      }
    };
    input.onkeydown = (e) => {
      e.stopPropagation();
      if (e.key === 'Enter') {
        e.preventDefault();
        finish(true);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        finish(false);
      }
    };
    input.onblur = () => finish(true);
    input.onpointerdown = (e) => e.stopPropagation();
    return input;
  }

  private newFolderRow(): HTMLElement {
    const base = this.os === 'windows' ? 'New folder' : 'untitled folder';
    const names = (this.listing?.entries ?? []).map((e) => e.name.toLowerCase());
    let name = base;
    for (let n = 2; names.includes(name.toLowerCase()); n++) name = `${base} ${n}`;
    const editor = this.nameEditor(name, (v) => void this.commitNewFolder(v));
    // An unchanged name still creates the folder.
    (editor as HTMLInputElement).onblur = null;
    editor.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && (editor as HTMLInputElement).value === name) void this.commitNewFolder(name);
    });
    editor.addEventListener('blur', () => {
      if (this.creating) void this.commitNewFolder((editor as HTMLInputElement).value);
    });
    return h(
      'div',
      { className: 'frow selected', role: 'row', style: 'top:0px' },
      h('span', { className: 'fname' }, h('span', { className: 'ficon dir' }, icon('folder', 17)), editor),
      h('span', { className: 'fsize' }, '—'),
      h('span', { className: 'fdate' }, ''),
    );
  }

  private renderEmpty(): void {
    let content: Child[] = [];
    if (this.error && !this.listing) {
      content = [
        h('div', { className: 'files-empty-icon bad' }, icon('alert', 26)),
        h('div', { className: 'files-empty-title' }, 'This folder cannot be shown'),
        h('div', { className: 'muted' }, this.error),
        h(
          'div',
          { className: 'row' },
          h('button', { type: 'button', className: 'small', onclick: () => void this.reload() }, 'Try again'),
          this.files.hello
            ? h(
                'button',
                { type: 'button', className: 'small', onclick: () => void this.navigate(this.files.hello!.home) },
                'Go home',
              )
            : null,
        ),
      ];
    } else if (this.loading && !this.listing) {
      content = [h('div', { className: 'muted' }, h('span', { className: 'spinner' }), 'Loading…')];
    } else if (this.listing && this.view.length === 0 && !this.creating) {
      content = this.filter
        ? [h('div', { className: 'muted' }, `Nothing here matches “${this.filterInput.value}”.`)]
        : [
            h('div', { className: 'files-empty-icon' }, icon('folder', 26)),
            h('div', { className: 'files-empty-title' }, 'This folder is empty'),
            h('div', { className: 'muted' }, 'Drop files here to upload them.'),
          ];
    }
    this.empty.hidden = content.length === 0;
    setChildren(this.empty, ...content);
  }

  private renderFoot(): void {
    if (!this.listing) {
      setChildren(this.foot);
      return;
    }
    const n = this.view.length;
    const hidden = this.listing.entries.length - n;
    const parts = [`${n} ${n === 1 ? 'item' : 'items'}`];
    if (this.selected.size) {
      const sel = this.selectedEntries();
      const bytes = sel.reduce((s, e) => s + (e.kind === 'file' ? e.size : 0), 0);
      parts.push(`${sel.length} selected${bytes ? ` (${formatSize(bytes)})` : ''}`);
    }
    if (hidden > 0 && !this.filter) parts.push(`${hidden} hidden`);
    if (this.listing.truncated) parts.push('only the first 20 000 are shown');
    const toggle = h(
      'button',
      {
        type: 'button',
        className: `icon-button small${this.showHidden ? ' on' : ''}`,
        title: this.showHidden ? 'Hide hidden files' : 'Show hidden files',
        ariaPressed: String(this.showHidden),
        onclick: () => {
          this.showHidden = !this.showHidden;
          localStorage.setItem(prefKey(this.files.machine, 'hidden'), this.showHidden ? '1' : '0');
          this.refreshView(true);
        },
      },
      icon(this.showHidden ? 'eye' : 'eyeOff', 15),
    );
    setChildren(this.foot, h('span', null, parts.join(' · ')), toggle);
  }

  private renderActions(): void {
    const sel = this.selectedEntries();
    const ready = this.files.channel.ready && this.listing !== null;
    const btn = (iconName: IconName, label: string, run: () => void, enabled: boolean, title?: string) =>
      h(
        'button',
        { type: 'button', className: 'small', disabled: !enabled, title: title ?? label, onclick: run },
        icon(iconName, 15),
        h('span', { className: 'label' }, label),
      );
    const uploadMenu = h(
      'button',
      {
        type: 'button',
        className: 'small primary',
        disabled: !ready,
        title: 'Upload files or a folder (or drop them here)',
        onclick: (e: MouseEvent) => {
          const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
          this.openUploadMenu(r.left, r.bottom + 4);
        },
      },
      icon('upload', 15),
      h('span', { className: 'label' }, 'Upload'),
    );
    setChildren(
      this.actions,
      uploadMenu,
      btn('download', 'Download', () => this.downloadSelected(), ready && sel.length > 0),
      btn('folderPlus', 'New folder', () => this.startNewFolder(), ready),
      h('span', { className: 'spacer' }),
      btn('pencil', 'Rename', () => this.startRename(), ready && sel.length === 1, 'Rename (F2)'),
      btn(
        'trash',
        this.canTrash ? this.trashLabel : 'Delete',
        () => void this.deleteSelected(false),
        ready && sel.length > 0,
      ),
    );
  }

  // -------------------------------------------------------------------------
  // selection and keys

  private selectedEntries(): Entry[] {
    return this.view.filter((e) => this.selected.has(e.name));
  }

  private clickRow(index: number, ev: PointerEvent): void {
    const e = this.view[index];
    if (!e) return;
    const toggle = ev.metaKey || ev.ctrlKey;
    if (ev.shiftKey && this.anchor >= 0) {
      this.selectRange(this.anchor, index, toggle);
    } else if (toggle) {
      if (this.selected.has(e.name)) this.selected.delete(e.name);
      else this.selected.add(e.name);
      this.anchor = index;
    } else {
      this.selected.clear();
      this.selected.add(e.name);
      this.anchor = index;
    }
    this.cursor = index;
    this.afterSelection();
    this.scroller.focus({ preventScroll: true });
  }

  private selectRange(a: number, b: number, add: boolean): void {
    if (!add) this.selected.clear();
    const [lo, hi] = a < b ? [a, b] : [b, a];
    for (let i = lo; i <= hi; i++) this.selected.add(this.view[i]!.name);
  }

  private setCursor(index: number, mode: 'select' | 'extend' | 'move'): void {
    if (this.view.length === 0) return;
    const i = Math.max(0, Math.min(this.view.length - 1, index));
    if (mode === 'extend' && this.anchor >= 0) this.selectRange(this.anchor, i, false);
    else if (mode === 'select') {
      this.selected.clear();
      this.selected.add(this.view[i]!.name);
      this.anchor = i;
    }
    this.cursor = i;
    this.scrollIntoView(i);
    this.afterSelection();
  }

  private selectName(name: string): void {
    const i = this.view.findIndex((e) => e.name === name);
    if (i >= 0) this.setCursor(i, 'select');
  }

  private afterSelection(): void {
    this.renderRows();
    this.renderFoot();
    this.renderActions();
  }

  private scrollIntoView(i: number): void {
    const slot = i + (this.creating ? 1 : 0);
    const top = slot * ROW_HEIGHT;
    if (top < this.scroller.scrollTop) this.scroller.scrollTop = top;
    else if (top + ROW_HEIGHT > this.scroller.scrollTop + this.scroller.clientHeight) {
      this.scroller.scrollTop = top + ROW_HEIGHT - this.scroller.clientHeight;
    }
  }

  private onListKey(e: KeyboardEvent): void {
    if (this.renaming !== null || this.creating) return;
    const mod = e.metaKey || e.ctrlKey;
    const page = Math.max(1, Math.floor(this.scroller.clientHeight / ROW_HEIGHT) - 1);
    const mode = e.shiftKey ? 'extend' : 'select';
    const handled = () => {
      e.preventDefault();
      e.stopPropagation();
    };
    if (e.altKey && e.key === 'ArrowUp') return (handled(), this.goUp());
    if (e.altKey && e.key === 'ArrowLeft') return (handled(), this.goHistory(-1));
    if (e.altKey && e.key === 'ArrowRight') return (handled(), this.goHistory(1));
    if (e.metaKey && e.key === 'ArrowUp') return (handled(), this.goUp());
    if (e.metaKey && e.key === 'ArrowDown') {
      handled();
      const cur = this.view[this.cursor];
      if (cur) this.openEntry(cur);
      return;
    }
    switch (e.key) {
      case 'ArrowDown':
        return (handled(), this.setCursor(this.cursor + 1, mode));
      case 'ArrowUp':
        return (handled(), this.setCursor(this.cursor < 0 ? 0 : this.cursor - 1, mode));
      case 'PageDown':
        return (handled(), this.setCursor(this.cursor + page, mode));
      case 'PageUp':
        return (handled(), this.setCursor(this.cursor - page, mode));
      case 'Home':
        return (handled(), this.setCursor(0, mode));
      case 'End':
        return (handled(), this.setCursor(this.view.length - 1, mode));
      case 'Enter': {
        handled();
        const sel = this.selectedEntries();
        if (sel.length === 1 && sel[0]!.kind === 'dir') this.openEntry(sel[0]!);
        else if (sel.length) this.downloadSelected();
        return;
      }
      case 'Backspace':
        if (mod) return (handled(), void this.deleteSelected(false));
        return (handled(), this.goUp());
      case 'Delete':
        return (handled(), void this.deleteSelected(e.shiftKey));
      case 'F2':
        return (handled(), this.startRename());
      case 'F5':
        return (handled(), void this.reload());
    }
    if (mod && e.key.toLowerCase() === 'a') {
      handled();
      for (const x of this.view) this.selected.add(x.name);
      return this.afterSelection();
    }
    if (mod && e.shiftKey && e.key.toLowerCase() === 'n') return (handled(), this.startNewFolder());
    if (mod && e.key.toLowerCase() === 'f') return (handled(), this.filterInput.focus());
    // Type to jump to a name.
    if (!mod && !e.altKey && e.key.length === 1 && e.key !== ' ') {
      handled();
      const now = performance.now();
      this.typeahead =
        now - this.typeaheadAt > TYPEAHEAD_MS ? e.key.toLowerCase() : this.typeahead + e.key.toLowerCase();
      this.typeaheadAt = now;
      const i = this.view.findIndex((x) => x.name.toLowerCase().startsWith(this.typeahead));
      if (i >= 0) this.setCursor(i, 'select');
    }
  }

  private onPanelKey(e: KeyboardEvent): void {
    if (e.key === 'Escape' && !e.defaultPrevented && this.opts.layout === 'drawer') {
      if (!this.menu.hidden) {
        this.closeMenu();
      } else if (this.selected.size && e.target === this.scroller) {
        this.selected.clear();
        this.afterSelection();
      } else {
        this.opts.close();
      }
      e.preventDefault();
    }
  }

  // -------------------------------------------------------------------------
  // actions

  private openEntry(e: Entry): void {
    if (!this.listing) return;
    if (e.kind === 'dir') void this.navigate(joinPath(this.listing.path, e.name, this.sep));
    else if (e.kind === 'file') this.download([e]);
  }

  private downloadSelected(): void {
    this.download(this.selectedEntries());
  }

  private download(entries: Entry[]): void {
    const listing = this.listing;
    if (!listing || entries.length === 0) return;
    const paths = entries.map((e) => joinPath(listing.path, e.name, this.sep));
    const only = entries.length === 1 ? entries[0]! : null;
    if (only && only.kind === 'file') {
      this.files.transfers.download(paths, { zip: false, name: only.name, folder: false });
      return;
    }
    if (!this.files.hello?.caps.includes(FileCaps.ZIP)) {
      void noticeDialog('Folders cannot be downloaded', 'This agent can only download single files.');
      return;
    }
    const base = only ? only.name : baseName(listing.path, this.sep).replace(/[\\/:]+/g, '') || this.opts.machineName();
    this.files.transfers.download(paths, { zip: true, name: `${base}.zip`, folder: true });
  }

  private openUploadMenu(x: number, y: number): void {
    this.showMenu(x, y, [
      { label: 'Upload files…', iconName: 'file', run: () => this.fileInput.click() },
      { label: 'Upload a folder…', iconName: 'folder', run: () => this.folderInput.click() },
    ]);
  }

  private uploadPicked(input: HTMLInputElement): void {
    const items = itemsFromFiles(Array.from(input.files ?? []));
    input.value = '';
    void this.uploadInto(items);
  }

  private startNewFolder(): void {
    if (!this.listing || this.creating) return;
    this.creating = true;
    this.renaming = null;
    this.scroller.scrollTop = 0;
    this.renderList();
  }

  private async commitNewFolder(name: string): Promise<void> {
    if (!this.creating || !this.listing) return;
    this.creating = false;
    const problem = invalidName(name, this.os);
    if (problem) {
      this.renderList();
      await noticeDialog('The folder was not created', problem);
      return;
    }
    try {
      await this.files.channel.request({ t: 'mkdir', path: joinPath(this.listing.path, name, this.sep) });
      await this.reload();
      this.selectName(name);
    } catch (e) {
      this.renderList();
      await noticeDialog('The folder was not created', errorText(e));
    }
    this.focus();
  }

  private startRename(): void {
    const sel = this.selectedEntries();
    if (sel.length !== 1) return;
    this.renaming = sel[0]!.name;
    this.scrollIntoView(this.view.indexOf(sel[0]!));
    this.renderRows();
  }

  private async commitRename(e: Entry, name: string): Promise<void> {
    const listing = this.listing;
    this.renaming = null;
    if (!listing) return;
    const problem = invalidName(name, this.os);
    const clash =
      !problem &&
      listing.entries.some(
        (x) => x !== e && (caseSensitive(this.os) ? x.name === name : x.name.toLowerCase() === name.toLowerCase()),
      );
    if (problem || clash) {
      this.renderRows();
      await noticeDialog(`“${e.name}” was not renamed`, problem ?? `There already is an item called “${name}”.`);
      this.focus();
      return;
    }
    try {
      await this.files.channel.request({
        t: 'rename',
        from: joinPath(listing.path, e.name, this.sep),
        to: joinPath(listing.path, name, this.sep),
      });
      await this.reload();
      this.selectName(name);
    } catch (err) {
      this.renderRows();
      await noticeDialog(`“${e.name}” was not renamed`, errorText(err));
    }
    this.focus();
  }

  private get canTrash(): boolean {
    return this.files.hello?.caps.includes(FileCaps.TRASH) ?? false;
  }

  private get trashLabel(): string {
    return this.os === 'windows' ? 'Recycle' : 'Trash';
  }

  private async deleteSelected(permanently: boolean): Promise<void> {
    const listing = this.listing;
    const sel = this.selectedEntries();
    if (!listing || sel.length === 0) return;
    const trash = this.canTrash && !permanently;
    const what = sel.length === 1 ? `“${sel[0]!.name}”` : `${sel.length} items`;
    const bin = this.os === 'windows' ? 'the Recycle Bin' : 'the Trash';
    const ok = await confirmDialog(
      trash
        ? {
            title: `Move ${what} to ${bin}?`,
            text: `They can be restored from ${bin} on ${this.opts.machineName()}.`,
            confirm: `Move to ${bin}`,
            iconName: 'trash',
          }
        : {
            title: `Delete ${what} permanently?`,
            text: `This cannot be undone.${sel.some((e) => e.kind === 'dir') ? ' Folders are deleted with everything in them.' : ''}`,
            confirm: 'Delete',
            danger: true,
            iconName: 'trash',
          },
    );
    if (!ok) {
      this.focus();
      return;
    }
    const index = Math.max(0, this.view.indexOf(sel[0]!));
    try {
      await this.files.channel.request({
        t: 'delete',
        paths: sel.map((e) => joinPath(listing.path, e.name, this.sep)),
        trash,
      });
    } catch (e) {
      await noticeDialog('Not everything was deleted', errorText(e));
    }
    await this.reload();
    this.setCursor(Math.min(index, this.view.length - 1), 'select');
    this.focus();
  }

  private copyPath(e: Entry | null): void {
    if (!this.listing) return;
    const path = e ? joinPath(this.listing.path, e.name, this.sep) : this.listing.path;
    void navigator.clipboard?.writeText(path).catch(() => undefined);
  }

  // -------------------------------------------------------------------------
  // context menu

  private openMenu(x: number, y: number, e: Entry | null): void {
    const sel = this.selectedEntries();
    type Item = { label: string; iconName: IconName; run: () => void; danger?: boolean } | null;
    const items: Item[] = e
      ? [
          e.kind === 'dir' && sel.length === 1
            ? { label: 'Open', iconName: 'folder', run: () => this.openEntry(e) }
            : null,
          {
            label: sel.length > 1 || e.kind === 'dir' ? 'Download as zip' : 'Download',
            iconName: 'download',
            run: () => this.downloadSelected(),
          },
          e.kind === 'dir' && sel.length === 1
            ? { label: 'Upload into this folder…', iconName: 'upload', run: () => this.pickInto(e) }
            : null,
          sel.length === 1 ? { label: 'Rename', iconName: 'pencil', run: () => this.startRename() } : null,
          { label: 'Copy path', iconName: 'copy', run: () => this.copyPath(sel.length === 1 ? e : null) },
          {
            label: this.canTrash ? `Move to ${this.os === 'windows' ? 'Recycle Bin' : 'Trash'}` : 'Delete',
            iconName: 'trash',
            run: () => void this.deleteSelected(false),
            danger: true,
          },
          this.canTrash
            ? {
                label: 'Delete permanently…',
                iconName: 'trash',
                run: () => void this.deleteSelected(true),
                danger: true,
              }
            : null,
        ]
      : [
          { label: 'Upload files…', iconName: 'file', run: () => this.fileInput.click() },
          { label: 'Upload a folder…', iconName: 'folder', run: () => this.folderInput.click() },
          { label: 'New folder', iconName: 'folderPlus', run: () => this.startNewFolder() },
          { label: 'Copy path', iconName: 'copy', run: () => this.copyPath(null) },
          { label: 'Refresh', iconName: 'refresh', run: () => void this.reload() },
        ];
    this.showMenu(
      x,
      y,
      items.filter((i): i is NonNullable<Item> => i !== null),
    );
  }

  private pickInto(e: Entry): void {
    const listing = this.listing;
    if (!listing) return;
    const dest = joinPath(listing.path, e.name, this.sep);
    const input = h('input', { type: 'file', multiple: true, hidden: true });
    input.onchange = () => {
      void this.uploadInto(itemsFromFiles(Array.from(input.files ?? [])), dest);
      input.remove();
    };
    this.el.append(input);
    input.click();
  }

  private showMenu(
    x: number,
    y: number,
    items: Array<{ label: string; iconName: IconName; run: () => void; danger?: boolean }>,
  ): void {
    setChildren(
      this.menu,
      ...items.map((it) =>
        h(
          'button',
          {
            type: 'button',
            role: 'menuitem',
            className: it.danger ? 'danger' : '',
            onclick: () => {
              this.closeMenu();
              it.run();
            },
          },
          icon(it.iconName, 15),
          it.label,
        ),
      ),
    );
    this.menu.hidden = false;
    const box = this.el.getBoundingClientRect();
    const mw = this.menu.offsetWidth;
    const mh = this.menu.offsetHeight;
    this.menu.style.left = `${Math.max(4, Math.min(x - box.left, box.width - mw - 4))}px`;
    this.menu.style.top = `${Math.max(4, Math.min(y - box.top, box.height - mh - 4))}px`;
    (this.menu.firstElementChild as HTMLElement | null)?.focus();
    this.menu.onkeydown = (e) => {
      const buttons = Array.from(this.menu.querySelectorAll('button'));
      const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
      if (e.key === 'ArrowDown') buttons[(i + 1) % buttons.length]?.focus();
      else if (e.key === 'ArrowUp') buttons[(i - 1 + buttons.length) % buttons.length]?.focus();
      else if (e.key === 'Escape') this.closeMenu();
      else return;
      e.preventDefault();
      e.stopPropagation();
    };
  }

  private closeMenu(): void {
    if (this.menu.hidden) return;
    this.menu.hidden = true;
    this.focus();
  }

  // -------------------------------------------------------------------------
  // drag and drop

  private wireDrop(): void {
    const target = this.el;
    target.addEventListener('dragenter', (e) => {
      if (!dragHasFiles(e.dataTransfer)) return;
      e.preventDefault();
      this.dragDepth++;
      this.updateDropTarget(e);
    });
    target.addEventListener('dragover', (e) => {
      if (!dragHasFiles(e.dataTransfer)) return;
      e.preventDefault();
      e.dataTransfer!.dropEffect = 'copy';
      this.updateDropTarget(e);
    });
    target.addEventListener('dragleave', () => {
      this.dragDepth = Math.max(0, this.dragDepth - 1);
      if (this.dragDepth === 0) this.endDrag();
    });
    target.addEventListener('drop', (e) => {
      if (!dragHasFiles(e.dataTransfer)) return;
      e.preventDefault();
      const dest = this.dropDestination();
      const items = itemsFromDrop(e.dataTransfer!);
      this.endDrag();
      void items.then((list) => this.uploadInto(list, dest));
    });
  }

  private updateDropTarget(e: DragEvent): void {
    const row = (e.target as HTMLElement).closest?.('.frow');
    let name: string | null = null;
    if (row && this.rowsEl.contains(row)) {
      const index = Array.from(this.rowsEl.children).indexOf(row);
      const slotTop = parseInt((row as HTMLElement).style.top, 10) / ROW_HEIGHT;
      const entry = this.view[slotTop - (this.creating ? 1 : 0)];
      if (index >= 0 && entry?.kind === 'dir') name = entry.name;
    }
    if (name !== this.dropTarget) {
      this.dropTarget = name;
      this.renderRows();
    }
    const folder = this.dropDestination();
    this.dropHint.hidden = false;
    setChildren(
      this.dropHint,
      h(
        'div',
        { className: 'files-drop-card' },
        icon('upload', 28),
        h('div', null, 'Drop to upload to ', h('b', null, folder ? this.folderName(folder) : 'this folder')),
      ),
    );
  }

  private dropDestination(): string | null {
    if (!this.listing) return this.dir;
    return this.dropTarget ? joinPath(this.listing.path, this.dropTarget, this.sep) : this.listing.path;
  }

  private endDrag(): void {
    this.dragDepth = 0;
    this.dropHint.hidden = true;
    if (this.dropTarget !== null) {
      this.dropTarget = null;
      this.renderRows();
    }
  }

  /** A folder's name as people know it: the place name for known places. */
  folderName(path: string): string {
    const place = this.files.hello?.places.find((p) => p.path === path);
    return place?.name ?? baseName(path, this.sep);
  }

  // -------------------------------------------------------------------------
  // rendering of everything else

  private scheduleRender(): void {
    if (this.renderQueued) return;
    this.renderQueued = true;
    requestAnimationFrame(() => {
      this.renderQueued = false;
      this.renderAll();
    });
  }

  private renderAll(): void {
    this.titleName.textContent = this.opts.machineName();
    this.renderStatus();
    this.renderPlaces();
    this.renderCrumbs();
    this.backBtn.disabled = this.historyAt <= 0;
    this.fwdBtn.disabled = this.historyAt >= this.history.length - 1;
    this.upBtn.disabled = !this.listing?.parent;
    this.renderActions();
    this.renderEmpty();
    this.renderFoot();
    this.transfersView.render();
    this.el.classList.toggle('loading', this.loading);
  }

  private renderStatus(): void {
    const s = this.files.status;
    let content: Child[] = [];
    let cls = '';
    switch (s.kind) {
      case 'connecting':
        content = [h('span', { className: 'spinner' }), 'Connecting…'];
        break;
      case 'retrying':
        content = [`Connection lost, retrying in ${Math.ceil(s.delayMs / 1000)} s`];
        cls = 'warn';
        break;
      case 'offline':
        content = [`${this.opts.machineName()} is offline; waiting for it`];
        cls = 'warn';
        break;
      case 'unsupported':
        content = ['This agent has no file transfer yet: update it from the portal.'];
        cls = 'bad';
        break;
      case 'stopped':
        content = [s.reason];
        cls = 'bad';
        break;
    }
    if (s.kind === 'unsupported' || s.kind === 'stopped') {
      content.push(
        h(
          'button',
          { type: 'button', className: 'small ghost', onclick: () => this.files.channel.retry() },
          'Try again',
        ),
      );
    }
    this.status.className = `files-status ${cls}`;
    setChildren(this.status, ...content);
  }

  private renderPlaces(): void {
    const hello = this.files.hello;
    const places = hello?.places ?? [];
    const current = this.listing?.path;
    const group = (title: string, list: Place[]) =>
      list.length
        ? [
            h('div', { className: 'files-places-title' }, title),
            ...list.map((p) =>
              h(
                'button',
                {
                  type: 'button',
                  className: `files-place${p.path === current ? ' on' : ''}`,
                  title: p.path,
                  onclick: () => void this.navigate(p.path),
                },
                icon(PLACE_ICONS[p.kind], 16),
                h('span', null, p.name),
              ),
            ),
          ]
        : [];
    const folders = places.filter((p) => p.kind !== 'drive' && p.kind !== 'volume');
    const drives = places.filter((p) => p.kind === 'drive' || p.kind === 'volume');
    setChildren(
      this.places,
      ...group('Places', folders),
      ...group(this.os === 'windows' ? 'Drives' : 'Locations', drives),
    );
    setChildren(
      this.placesMenu,
      h('option', { value: '', disabled: true, selected: !places.some((p) => p.path === current) }, 'Places'),
      ...places.map((p) => h('option', { value: p.path, selected: p.path === current }, p.name)),
    );
  }

  private renderCrumbs(): void {
    if (this.crumbs.querySelector('input')) return;
    const path = this.listing?.path ?? this.dir;
    if (!path) {
      setChildren(this.crumbs);
      return;
    }
    const sep = this.sep;
    const parts: Array<{ label: string; path: string }> = [];
    if (sep === '\\') {
      const m = /^([A-Za-z]:\\|\\\\[^\\]+\\[^\\]+\\?)(.*)$/.exec(path);
      const root = m ? m[1]! : path;
      parts.push({ label: root.replace(/\\$/, ''), path: root.endsWith('\\') ? root : `${root}\\` });
      let acc = root.endsWith('\\') ? root : `${root}\\`;
      for (const seg of (m?.[2] ?? '').split('\\').filter(Boolean)) {
        acc = joinPath(acc, seg, sep);
        parts.push({ label: seg, path: acc });
      }
    } else {
      const rootName = this.files.hello?.places.find((p) => p.path === '/' && p.kind === 'drive')?.name ?? '/';
      parts.push({ label: rootName, path: '/' });
      let acc = '/';
      for (const seg of path.split('/').filter(Boolean)) {
        acc = joinPath(acc, seg, sep);
        parts.push({ label: seg, path: acc });
      }
    }
    const nodes: HTMLElement[] = [];
    parts.forEach((p, i) => {
      if (i > 0) nodes.push(h('span', { className: 'crumb-sep', ariaHidden: 'true' }, '›'));
      nodes.push(
        h(
          'button',
          {
            type: 'button',
            className: `crumb${i === parts.length - 1 ? ' current' : ''}`,
            title: p.path,
            onclick: () => void this.navigate(p.path),
          },
          p.label,
        ),
      );
    });
    setChildren(this.crumbs, ...nodes);
    this.crumbs.title = 'Click an empty spot to type a path';
    // Keep the end of a long path visible.
    this.crumbs.scrollLeft = this.crumbs.scrollWidth;
  }
}

function errorText(e: unknown): string {
  if (e instanceof FileError) return e.message;
  if (e instanceof Error) return e.message;
  return describeFileError('io', String(e));
}

/** For the session's drop overlay: what a drop of these items amounts to. */
export function describeItems(items: UploadItem[]): string {
  const t = totals(items);
  const parts = [];
  if (t.files) parts.push(`${t.files} ${t.files === 1 ? 'file' : 'files'}`);
  if (items.some((i) => i.kind === 'dir')) parts.push(`${t.dirs} ${t.dirs === 1 ? 'folder' : 'folders'}`);
  return `${parts.join(', ')} (${formatSize(t.bytes)})`;
}
