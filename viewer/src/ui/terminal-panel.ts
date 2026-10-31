import type { ChordAction } from '../input/chords';
import { h, setChildren } from './dom';
import { icon } from './icons';
import { TerminalView } from './terminal-view';

const HEIGHT_KEY = 'rd.term.height';
const MIN_HEIGHT = 140;
const DEFAULT_HEIGHT = 320;

export interface TerminalPanelOptions {
  machine: string;
  machineName(): string;
  /** Viewer chords typed while the terminal has the keyboard. */
  chord(a: ChordAction): void;
  /** Hide the panel (the shell keeps running). */
  hide(): void;
}

/**
 * The terminal next to the remote screen during a session (docs/terminal.md): docked at the bottom, its top
 * edge drags to resize. Hiding it only detaches; opening it again in the same page reattaches the same shell
 * (also after a reload: the shell's id is in sessionStorage).
 */
export class TerminalPanel {
  readonly el: HTMLElement;
  private readonly view: TerminalView;
  private readonly statusDot = h('span', { className: 'dot' });
  private readonly detail = h('span', { className: 'term-detail muted' });
  private readonly sizeEl = h('span', { className: 'term-size muted', title: 'Font size' });
  private readonly resizer = h('div', { className: 'term-resizer', title: 'Drag to resize' });
  private open = false;

  constructor(private readonly opts: TerminalPanelOptions) {
    this.view = new TerminalView({
      machine: opts.machine,
      machineName: opts.machineName,
      storageKey: `rd.term.panel.${opts.machine}`,
      theme: 'dark',
      chord: opts.chord,
      changed: () => this.render(),
      close: () => opts.hide(),
    });
    const tool = (label: string, name: Parameters<typeof icon>[0], run: () => void) =>
      h(
        'button',
        { type: 'button', className: 'icon-button small', title: label, ariaLabel: label, onclick: run },
        icon(name, 15),
      );
    const fontStep = (d: number) => () => {
      this.view.fontSize += d;
      this.render();
      this.view.focus();
    };
    this.el = h(
      'section',
      { className: 'term-panel', ariaLabel: 'Terminal' },
      this.resizer,
      h(
        'header',
        { className: 'term-panel-head' },
        h(
          'div',
          { className: 'term-heading' },
          icon('terminal', 16),
          h('b', null, 'Terminal'),
          this.statusDot,
          this.detail,
        ),
        h(
          'div',
          { className: 'term-tools' },
          h(
            'div',
            { className: 'term-font', role: 'group', ariaLabel: 'Font size' },
            tool('Smaller text', 'minus', fontStep(-1)),
            this.sizeEl,
            tool('Larger text', 'plus', fontStep(1)),
          ),
          tool('Hide (the shell keeps running) · Ctrl+Alt+Shift+`', 'chevronDown', () => opts.hide()),
          tool('Close the terminal…', 'close', () => this.onClose()),
        ),
      ),
      this.view.el,
    );
    this.wireResizer();
    this.applyHeight(this.savedHeight());
    window.addEventListener('resize', () => {
      if (this.open) this.applyHeight(this.savedHeight());
    });
    this.render();
  }

  get isOpen(): boolean {
    return this.open;
  }

  get hasFocus(): boolean {
    return this.view.hasFocus;
  }

  /** Call once `el` is in the document: connects (or reattaches) and takes the keyboard. */
  show(): void {
    this.open = true;
    this.applyHeight(this.savedHeight());
    this.view.start();
    this.view.focus();
  }

  /** The panel is gone: detach (the shell keeps running). */
  hidden(): void {
    this.open = false;
    this.view.resetSticky();
    this.view.detach();
  }

  /** On-screen key bar while the terminal has the keyboard: Esc, Tab, arrows, F-keys, Ctrl+C. */
  sendKey(code: string): void {
    this.view.sendKey(code);
  }

  toggleSticky(code: string): boolean {
    return this.view.toggleSticky(code);
  }

  stickyHeld(code: string): boolean {
    return this.view.stickyHeld(code);
  }

  focus(): void {
    this.view.focus();
  }

  /** The panel's box changed (files drawer opened or closed). */
  relayout(): void {
    this.view.fit();
  }

  private onClose(): void {
    if (!this.view.askToEnd({ label: 'Keep it running', run: () => this.opts.hide() })) this.opts.hide();
  }

  private render(): void {
    const s = this.view.status.kind;
    this.statusDot.className = `dot ${s === 'ready' ? 'ok' : s === 'offline' || s === 'failed' || s === 'unsupported' ? 'bad' : s === 'connecting' || s === 'retrying' ? 'warn' : ''}`;
    const v = this.view;
    setChildren(this.detail, [v.shell, v.title && v.title !== v.shell ? v.title : ''].filter(Boolean).join(' · '));
    this.detail.title = this.detail.textContent ?? '';
    this.sizeEl.textContent = String(v.fontSize);
  }

  private savedHeight(): number {
    const n = Number(localStorage.getItem(HEIGHT_KEY));
    return Number.isFinite(n) && n > 0 ? n : DEFAULT_HEIGHT;
  }

  private applyHeight(px: number): number {
    const max = Math.max(MIN_HEIGHT, Math.round(window.innerHeight * 0.8));
    const height = Math.round(Math.min(max, Math.max(MIN_HEIGHT, px)));
    document.documentElement.style.setProperty('--term-h', `${height}px`);
    return height;
  }

  /** Drag the top edge to resize. */
  private wireResizer(): void {
    const r = this.resizer;
    r.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      r.setPointerCapture(e.pointerId);
      r.classList.add('active');
    });
    r.addEventListener('pointermove', (e) => {
      if (r.hasPointerCapture(e.pointerId)) this.applyHeight(window.innerHeight - e.clientY);
    });
    const end = (e: PointerEvent) => {
      if (!r.hasPointerCapture(e.pointerId)) return;
      r.releasePointerCapture(e.pointerId);
      r.classList.remove('active');
      localStorage.setItem(HEIGHT_KEY, String(this.applyHeight(window.innerHeight - e.clientY)));
      this.view.focus();
    };
    r.addEventListener('pointerup', end);
    r.addEventListener('pointercancel', end);
  }
}
