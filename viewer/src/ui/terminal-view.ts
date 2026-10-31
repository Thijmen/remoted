import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import { Unicode11Addon } from '@xterm/addon-unicode11';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { type ITheme, Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { type ChordAction, chordFor } from '../input/chords';
import { isMacViewer } from '../input/modifiers';
import { type TermOutMsg, encodeTermAck, encodeTermIn } from '../proto/bin';
import { TERMINAL_PROTO_VERSION, type TermHello, type TermViewerMsg, clampSize, describeExit } from '../proto/terminal';
import { type TermStatus, TerminalChannel, isFinal } from '../terminal/channel';
import { type TerminalMods, NO_MODS, applyMods, modForCode, terminalKey } from '../terminal/keys';
import { AckTracker, OutputPosition, ReplayFilter, binaryStringBytes, splitInput, textBytes } from '../terminal/output';
import { type Child, h, setChildren } from './dom';
import { icon } from './icons';

export const FONT_SIZE_KEY = 'rd.term.fontSize';
export const DEFAULT_FONT_SIZE = 13;
export const MIN_FONT_SIZE = 8;
export const MAX_FONT_SIZE = 32;
const RESIZE_DEBOUNCE_MS = 150;
/** An ack waits for the next animation frame, or this long in a hidden tab (no frames there). */
const ACK_FALLBACK_MS = 250;

const FONT_FAMILY =
  "ui-monospace, 'SF Mono', SFMono-Regular, Menlo, 'Cascadia Mono', 'Cascadia Code', Consolas, 'DejaVu Sans Mono', 'Liberation Mono', monospace";

const DARK: ITheme = {
  background: '#0f1117',
  foreground: '#e3e6ed',
  cursor: '#7aa2ff',
  cursorAccent: '#0f1117',
  selectionBackground: 'rgba(91, 140, 255, 0.38)',
  scrollbarSliderBackground: 'rgba(143, 152, 168, 0.25)',
  scrollbarSliderHoverBackground: 'rgba(143, 152, 168, 0.4)',
  scrollbarSliderActiveBackground: 'rgba(143, 152, 168, 0.55)',
  black: '#1b1f28',
  red: '#f2645f',
  green: '#3fcf8e',
  yellow: '#f0b544',
  blue: '#5b8cff',
  magenta: '#c58af9',
  cyan: '#4cc9d9',
  white: '#c9ced8',
  brightBlack: '#6b7385',
  brightRed: '#ff8a85',
  brightGreen: '#6fe3ad',
  brightYellow: '#ffd27a',
  brightBlue: '#8fb0ff',
  brightMagenta: '#dcb1ff',
  brightCyan: '#7fe0ec',
  brightWhite: '#ffffff',
};

const LIGHT: ITheme = {
  background: '#fbfbfd',
  foreground: '#1f2430',
  cursor: '#2f62e0',
  cursorAccent: '#fbfbfd',
  selectionBackground: 'rgba(47, 98, 224, 0.25)',
  scrollbarSliderBackground: 'rgba(60, 66, 80, 0.2)',
  scrollbarSliderHoverBackground: 'rgba(60, 66, 80, 0.35)',
  scrollbarSliderActiveBackground: 'rgba(60, 66, 80, 0.5)',
  black: '#1f2430',
  red: '#c93a33',
  green: '#1c8a5a',
  yellow: '#9a6700',
  blue: '#2f62e0',
  magenta: '#9038c4',
  cyan: '#0e7f8e',
  white: '#8a91a0',
  brightBlack: '#5d6575',
  brightRed: '#e0524b',
  brightGreen: '#23a26c',
  brightYellow: '#b47d0a',
  brightBlue: '#4a7cf0',
  brightMagenta: '#a956d8',
  brightCyan: '#1597a8',
  brightWhite: '#3c4250',
};

export interface TerminalViewOptions {
  machine: string;
  /** For messages ("mbp is offline"). */
  machineName(): string;
  /** sessionStorage key of the shell's id, so that a reload of the page reattaches. */
  storageKey: string;
  /** Start a new shell even when one is stored (a "New terminal" tab got a copy of the storage). */
  fresh?: boolean;
  /** `auto` follows the system's light / dark setting. */
  theme: 'auto' | 'dark';
  /** Viewer chords (Ctrl+Alt+Shift+…) typed while the terminal has the keyboard. */
  chord?(a: ChordAction): void;
  /** Status, shell or title changed. */
  changed?(): void;
  /** The user picked Close after the shell ended (no button when absent). */
  close?(): void;
}

/** A written chunk of shell output, until the terminal has processed it. */
interface Chunk {
  replay: boolean;
  generation: number;
  seq: number;
}

export function loadFontSize(): number {
  const n = Number(localStorage.getItem(FONT_SIZE_KEY));
  return Number.isFinite(n) && n >= MIN_FONT_SIZE && n <= MAX_FONT_SIZE ? n : DEFAULT_FONT_SIZE;
}

/**
 * A shell on the remote machine in xterm.js (docs/terminal.md): the terminal, its channel, the notices
 * (reconnecting, ended, taken over) and the find bar. Reattaches by itself after a dropped connection,
 * and after a reload of the page (the shell's id is in sessionStorage).
 */
export class TerminalView {
  /** Sticky key-bar modifiers for the next typed character (see `stickyText`). */
  private sticky: TerminalMods = NO_MODS;

  readonly el: HTMLElement;
  /** xterm's parent, the box it fits into (the fit addon measures it; padding goes on its wrapper). */
  private readonly host = h('div', { className: 'term-screen' });
  private readonly notice = h('div', { className: 'term-notice', hidden: true, role: 'status' });
  private readonly prompt = h('div', { className: 'term-notice term-prompt', hidden: true, role: 'alertdialog' });
  private readonly findBar: HTMLElement;
  private readonly findInput = h('input', {
    type: 'search',
    placeholder: 'Find',
    ariaLabel: 'Find in the terminal',
    spellcheck: false,
  });
  private readonly term: Terminal;
  private readonly fitter = new FitAddon();
  private readonly search = new SearchAddon();
  private readonly channel: TerminalChannel;
  private readonly output = new OutputPosition();
  private readonly replay = new ReplayFilter();
  private acks = new AckTracker();
  /** Bumped on every new socket: callbacks of output from an older one must not ack on the new one. */
  private generation = 0;
  private readonly chunks: Chunk[] = [];
  private ackQueued = false;
  private opened = false;
  private shellId: string | null;
  /** The hello asked to attach (else it started a new shell). */
  private attaching = false;
  /** End the shell as soon as the channel is attached (× while reconnecting). */
  private closeOnHello = false;
  private resizeTimer = 0;
  private fitQueued = false;
  private readonly mac = isMacViewer(navigator);
  private readonly darkQuery = window.matchMedia('(prefers-color-scheme: dark)');
  /** The shell's name (`zsh`, `pwsh`) and the title it set (OSC 0/2). */
  shell = '';
  title = '';
  os = '';

  constructor(private readonly opts: TerminalViewOptions) {
    this.shellId = opts.fresh ? null : sessionStorage.getItem(opts.storageKey);
    if (opts.fresh) sessionStorage.removeItem(opts.storageKey);
    this.term = new Terminal({
      allowProposedApi: true,
      fontFamily: FONT_FAMILY,
      fontSize: loadFontSize(),
      lineHeight: 1.15,
      scrollback: 10_000,
      macOptionIsMeta: true,
      macOptionClickForcesSelection: true,
      rightClickSelectsWord: false,
      cursorBlink: true,
      cursorStyle: 'block',
      cursorInactiveStyle: 'outline',
      drawBoldTextInBrightColors: false,
      theme: this.theme(),
      linkHandler: {
        activate: (e, uri) => this.openLink(e, uri),
        allowNonHttpProtocols: false,
      },
    });
    this.term.loadAddon(this.fitter);
    this.term.loadAddon(this.search);
    this.term.loadAddon(new WebLinksAddon((e, uri) => this.openLink(e, uri)));
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = '11';
    this.registerReplayGuards();
    this.term.attachCustomKeyEventHandler((e) => this.onKey(e));
    this.term.onData((d) => this.input(textBytes(this.stickyText(d))));
    this.term.onBinary((d) => this.input(binaryStringBytes(d)));
    this.term.onResize(() => this.scheduleResize());
    this.term.onTitleChange((t) => {
      this.title = t;
      this.opts.changed?.();
    });
    if (opts.theme === 'auto') {
      this.darkQuery.addEventListener('change', () => (this.term.options.theme = this.theme()));
    }

    this.findBar = this.buildFindBar();
    this.el = h(
      'div',
      { className: 'term-view' },
      h('div', { className: 'term-host' }, this.host),
      this.findBar,
      this.prompt,
      this.notice,
    );
    this.el.addEventListener('focusin', () => this.el.classList.add('focused'));
    this.el.addEventListener('focusout', () => this.el.classList.remove('focused'));
    new ResizeObserver(() => this.scheduleFit()).observe(this.host);

    this.channel = new TerminalChannel(opts.machine, () => this.helloMsg(), {
      status: (s) => this.onStatus(s),
      hello: (m) => this.onHello(m),
      out: (m) => this.onOut(m),
      state: () => {},
      lost: () => this.generation++,
    });
  }

  get status(): TermStatus {
    return this.channel.status;
  }

  /** Attached to a shell right now. */
  get attached(): boolean {
    return this.channel.ready;
  }

  get fontSize(): number {
    return this.term.options.fontSize ?? DEFAULT_FONT_SIZE;
  }

  set fontSize(n: number) {
    const size = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(n)));
    this.term.options.fontSize = size;
    localStorage.setItem(FONT_SIZE_KEY, String(size));
    this.scheduleFit();
  }

  /** Show the terminal (once `el` is in the document) and connect: a stored shell is reattached. */
  start(): void {
    if (!this.opened) {
      this.opened = true;
      this.term.open(this.host);
      this.loadRenderer();
      void document.fonts?.ready.then(() => this.scheduleFit());
    }
    this.fit();
    this.channel.start();
  }

  /** Disconnect; the shell keeps running and `start()` reattaches it. */
  detach(): void {
    this.channel.stop();
  }

  /** End the shell (the agent answers `exit`). */
  endShell(): void {
    if (this.channel.ready) {
      this.channel.sendJson({ t: 'close' });
      return;
    }
    if (!this.shellId || isFinal(this.status)) return;
    // Not attached right now (reconnecting): end it once attached.
    this.closeOnHello = true;
    this.channel.retry();
  }

  /**
   * The × button: ask before ending the shell. `keep` offers to close the terminal and leave the shell running
   * (the session panel). Returns false when there is no running shell to ask about (just close).
   */
  askToEnd(keep?: { label: string; run(): void }): boolean {
    if (!this.shellId || isFinal(this.status)) return false;
    const done = (then?: () => void) => {
      this.prompt.hidden = true;
      if (then) then();
      else this.focus();
    };
    const end = h(
      'button',
      { type: 'button', className: 'small danger', onclick: () => done(() => (this.endShell(), this.focus())) },
      'End shell',
    );
    const keepButton = keep
      ? h('button', { type: 'button', className: 'small primary', onclick: () => done(keep.run) }, keep.label)
      : null;
    setChildren(
      this.prompt,
      h(
        'span',
        { className: 'text' },
        keep
          ? 'Close the terminal? The shell can keep running; opening the terminal again shows it.'
          : 'End the shell? Programs running in it stop.',
      ),
      keepButton,
      end,
      h('button', { type: 'button', className: 'small', onclick: () => done() }, 'Cancel'),
    );
    this.prompt.onkeydown = (e) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopPropagation();
      done();
    };
    this.prompt.hidden = false;
    (keepButton ?? end).focus();
    return true;
  }

  /** Forget the shell (it ended) and start a new one. */
  newShell(): void {
    this.forgetShell();
    this.closeOnHello = false;
    this.output.start(0);
    this.term.reset();
    this.channel.retry();
    this.focus();
  }

  /** Attach a shell that was opened in another window back here. */
  takeBack(): void {
    this.channel.retry();
    this.focus();
  }

  /** Connect again now (Retry, Reconnect now). */
  reconnect(): void {
    this.channel.retry();
  }

  focus(): void {
    this.term.focus();
  }

  get hasFocus(): boolean {
    return this.el.contains(document.activeElement);
  }

  /** A key from the on-screen key bar: the phone keyboard has no Esc, Tab, arrows or F-keys. */
  sendKey(code: string): void {
    const seq = terminalKey(code);
    if (seq === null) return;
    this.term.input(seq, true);
    this.focus();
  }

  /** Toggle a sticky Ctrl/Alt/Shift for the next typed character; returns whether it is held. */
  toggleSticky(code: string): boolean {
    const mod = modForCode(code);
    if (mod === null) return false;
    this.sticky = { ...this.sticky, [mod]: !this.sticky[mod] };
    return this.sticky[mod];
  }

  stickyHeld(code: string): boolean {
    const mod = modForCode(code);
    return mod !== null && this.sticky[mod];
  }

  /** The terminal lost the keyboard (or got a new shell): no modifier is left armed. */
  resetSticky(): void {
    this.sticky = NO_MODS;
  }

  /** Ctrl+C and Alt+B: the modifiers apply to the next single character the phone types. */
  private stickyText(data: string): string {
    if (data.length !== 1 || (!this.sticky.ctrl && !this.sticky.alt)) return data;
    const out = applyMods(data, this.sticky);
    this.sticky = { ...this.sticky, ctrl: false, alt: false };
    return out;
  }

  /** Fit the terminal to its box now (after a layout change). */
  fit(): void {
    if (!this.opened || this.host.clientWidth === 0 || this.host.clientHeight === 0) return;
    try {
      this.fitter.fit();
    } catch {
      // Not measurable yet (fonts, hidden): the next resize fits it.
    }
  }

  // -------------------------------------------------------------------------
  // channel

  private helloMsg(): TermViewerMsg {
    const cols = clampSize(this.term.cols);
    const rows = clampSize(this.term.rows);
    this.attaching = this.shellId !== null;
    return this.shellId
      ? {
          t: 'hello',
          proto: TERMINAL_PROTO_VERSION,
          cols,
          rows,
          attach: this.shellId,
          offset: this.output.offset,
        }
      : { t: 'hello', proto: TERMINAL_PROTO_VERSION, cols, rows };
  }

  private onHello(m: TermHello): void {
    this.acks = new AckTracker();
    if (m.reset) {
      // After the output already queued (it belongs to what is being replaced).
      this.term.write('', () => this.term.reset());
    }
    this.output.start(m.offset);
    this.replay.begin(this.attaching, m.offset, m.end, performance.now());
    this.shellId = m.id;
    sessionStorage.setItem(this.opts.storageKey, m.id);
    this.shell = m.shell;
    this.os = m.os;
    this.term.options.windowsPty = m.os === 'windows' ? { backend: 'conpty' } : {};
    if (this.closeOnHello) {
      this.closeOnHello = false;
      this.channel.sendJson({ t: 'close' });
    }
    // The size may have changed between the hello and its answer.
    this.scheduleResize();
    this.opts.changed?.();
  }

  private onOut(m: TermOutMsg): void {
    const { bytes, skipped } = this.output.accept(m.offset, m.payload);
    if (skipped > 0) console.info(`[rd] terminal: ${skipped} bytes of output were dropped (history overtaken)`);
    const chunk: Chunk = {
      replay: this.replay.frame(m.offset, m.payload.byteLength, performance.now()),
      generation: this.generation,
      seq: m.seq,
    };
    // Before write: right after a key press xterm parses at once and calls back inside `write`.
    this.chunks.push(chunk);
    this.term.write(bytes, () => {
      this.chunks.shift();
      if (chunk.generation !== this.generation) return;
      this.acks.processed(chunk.seq);
      this.scheduleAck();
    });
  }

  private scheduleAck(): void {
    if (this.ackQueued) return;
    this.ackQueued = true;
    let raf = 0;
    let timer = 0;
    const flush = () => {
      cancelAnimationFrame(raf);
      clearTimeout(timer);
      if (!this.ackQueued) return;
      this.ackQueued = false;
      const seq = this.acks.take();
      if (seq !== null) this.channel.sendBinary(encodeTermAck(seq));
    };
    raf = requestAnimationFrame(flush);
    timer = window.setTimeout(flush, ACK_FALLBACK_MS);
  }

  private input(bytes: Uint8Array): void {
    if (!this.channel.ready || bytes.byteLength === 0) return;
    this.replay.input();
    for (const part of splitInput(bytes)) this.channel.sendBinary(encodeTermIn(part));
  }

  private scheduleResize(): void {
    clearTimeout(this.resizeTimer);
    this.resizeTimer = window.setTimeout(() => {
      this.channel.sendJson({ t: 'resize', cols: clampSize(this.term.cols), rows: clampSize(this.term.rows) });
    }, RESIZE_DEBOUNCE_MS);
  }

  private scheduleFit(): void {
    if (this.fitQueued) return;
    this.fitQueued = true;
    requestAnimationFrame(() => {
      this.fitQueued = false;
      this.fit();
    });
  }

  private forgetShell(): void {
    this.shellId = null;
    sessionStorage.removeItem(this.opts.storageKey);
  }

  /**
   * Queries in replayed history (device attributes, cursor position, colours, modes) are not answered: the
   * program that asked got its answer long ago, and a second one would arrive as typed input.
   */
  private registerReplayGuards(): void {
    const p = this.term.parser;
    const inReplay = () => this.chunks[0]?.replay === true;
    for (const id of [
      { final: 'c' },
      { prefix: '>', final: 'c' },
      { prefix: '=', final: 'c' },
      { final: 'n' },
      { prefix: '?', final: 'n' },
      { intermediates: '$', final: 'p' },
      { prefix: '?', intermediates: '$', final: 'p' },
      { prefix: '>', final: 'q' },
    ]) {
      p.registerCsiHandler(id, inReplay);
    }
    p.registerDcsHandler({ intermediates: '$', final: 'q' }, inReplay);
    for (const osc of [4, 10, 11, 12]) p.registerOscHandler(osc, (data) => inReplay() && data.includes('?'));
  }

  private onStatus(s: TermStatus): void {
    switch (s.kind) {
      case 'exited':
        this.forgetShell();
        this.term.write(`\r\n\x1b[0m\x1b[2m[${describeExit(s.code, s.signal)}]\x1b[0m\r\n`);
        break;
      case 'ended':
        this.forgetShell();
        break;
    }
    this.renderNotice(s);
    this.opts.changed?.();
  }

  // -------------------------------------------------------------------------
  // keys, links, renderer

  private onKey(e: KeyboardEvent): boolean {
    const down = e.type === 'keydown';
    if (this.opts.chord && !e.getModifierState('AltGraph')) {
      const chord = chordFor(e);
      if (chord) {
        if (down && !e.repeat) this.opts.chord(chord);
        e.preventDefault();
        return false;
      }
    }
    const key = e.key.toLowerCase();
    // Ctrl+Shift+C / V (Cmd+C / V on a Mac are the browser's own copy and paste, which xterm handles).
    const shortcut = !this.mac && e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey;
    if (shortcut && key === 'c') {
      if (down && this.term.hasSelection()) void navigator.clipboard?.writeText(this.term.getSelection());
      e.preventDefault();
      return false;
    }
    // The browser's paste (as plain text) then reaches xterm as a paste event.
    if (shortcut && key === 'v') return false;
    if ((this.mac ? e.metaKey && !e.ctrlKey : shortcut) && key === 'f') {
      if (down) this.openFind();
      e.preventDefault();
      return false;
    }
    if (down && e.key === 'Enter' && (this.status.kind === 'ended' || this.status.kind === 'exited')) {
      e.preventDefault();
      this.newShell();
      return false;
    }
    return true;
  }

  private openLink(e: MouseEvent, uri: string): void {
    if (!(this.mac ? e.metaKey : e.ctrlKey)) return;
    if (!/^https?:\/\//i.test(uri)) return;
    window.open(uri, '_blank', 'noopener,noreferrer');
  }

  /**
   * WebGL draws much faster than the DOM renderer. Its addon is a chunk of its own, loaded once the terminal
   * shows (the DOM renderer draws until then); without WebGL, or once its context is lost, the DOM one stays.
   */
  private loadRenderer(): void {
    import('@xterm/addon-webgl')
      .then(({ WebglAddon }) => {
        const webgl = new WebglAddon();
        webgl.onContextLoss(() => {
          webgl.dispose();
          this.scheduleFit();
        });
        this.term.loadAddon(webgl);
        // Its cells can measure differently from the DOM renderer's.
        this.scheduleFit();
      })
      .catch((e: unknown) => console.info('[rd] terminal: WebGL unavailable, using the DOM renderer', e));
  }

  private theme(): ITheme {
    return this.opts.theme === 'auto' && !this.darkQuery.matches ? LIGHT : DARK;
  }

  // -------------------------------------------------------------------------
  // notices, find

  private renderNotice(s: TermStatus): void {
    const name = this.opts.machineName();
    const button = (label: string, run: () => void, primary = false): HTMLButtonElement =>
      h('button', { type: 'button', className: `small${primary ? ' primary' : ''}`, onclick: run }, label);
    let level: 'info' | 'warn' | 'error' = 'info';
    let text = '';
    const actions: Child[] = [];
    switch (s.kind) {
      case 'idle':
      case 'ready':
        this.notice.hidden = true;
        return;
      case 'connecting':
        text = s.attempt > 0 || this.output.offset > 0 ? 'Reconnecting…' : `Connecting to ${name}…`;
        break;
      case 'retrying':
        level = 'warn';
        text = `Connection lost (${s.reason}). Reconnecting in ${Math.round(s.delayMs / 1000)} s…`;
        actions.push(button('Reconnect now', () => this.reconnect()));
        break;
      case 'offline':
        level = 'warn';
        text = `${name} is offline. Reconnecting when it is back…`;
        actions.push(button('Retry now', () => this.reconnect()));
        break;
      case 'sleeping':
        text = 'Disconnected after a long idle time.';
        actions.push(button('Reconnect', () => this.reconnect(), true));
        break;
      case 'taken':
        level = 'warn';
        text = 'Opened in another window.';
        actions.push(button('Take it back', () => this.takeBack(), true));
        break;
      case 'unsupported':
        level = 'error';
        text = `${name} doesn't offer a terminal (turned off in its agent config, or the agent is too old).`;
        actions.push(button('Retry', () => this.reconnect()));
        break;
      case 'ended':
        text = 'The shell has ended.';
        actions.push(button('New shell', () => this.newShell(), true));
        break;
      case 'exited':
        text = 'The shell has exited.';
        actions.push(button('New shell', () => this.newShell(), true));
        if (this.opts.close) actions.push(button('Close', () => this.opts.close?.()));
        break;
      case 'failed':
        level = 'error';
        text = s.reason;
        actions.push(s.reload ? button('Reload', () => location.reload()) : button('Retry', () => this.reconnect()));
        break;
    }
    this.notice.className = `term-notice ${level}`;
    setChildren(this.notice, h('span', { className: 'text' }, text), ...actions);
    this.notice.hidden = false;
  }

  private buildFindBar(): HTMLElement {
    const find = (back: boolean) => {
      const q = this.findInput.value;
      if (!q) return;
      if (back) this.search.findPrevious(q);
      else this.search.findNext(q);
    };
    this.findInput.addEventListener('input', () => {
      const q = this.findInput.value;
      if (q) this.search.findNext(q, { incremental: true });
      else this.term.clearSelection();
    });
    this.findInput.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        find(e.shiftKey);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        this.closeFind();
      }
    });
    const tool = (label: string, name: Parameters<typeof icon>[0], run: () => void) =>
      h(
        'button',
        { type: 'button', className: 'icon-button', title: label, ariaLabel: label, onclick: run },
        icon(name, 15),
      );
    return h(
      'div',
      { className: 'term-find', hidden: true },
      icon('search', 15),
      this.findInput,
      tool('Previous match (Shift+Enter)', 'up', () => find(true)),
      tool('Next match (Enter)', 'chevronDown', () => find(false)),
      tool('Close (Esc)', 'close', () => this.closeFind()),
    );
  }

  private openFind(): void {
    this.findBar.hidden = false;
    const sel = this.term.getSelection();
    if (sel && !sel.includes('\n')) this.findInput.value = sel;
    this.findInput.focus();
    this.findInput.select();
  }

  private closeFind(): void {
    this.findBar.hidden = true;
    this.search.clearDecorations();
    this.focus();
  }
}
