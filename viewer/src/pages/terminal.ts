import '../ui/style.css';
import { fetchMachine } from '../net/api';
import type { MachineInfo } from '../proto/control';
import { byId, h, setChildren } from '../ui/dom';
import { icon } from '../ui/icons';
import { TerminalView } from '../ui/terminal-view';
import { NEW_TERMINAL_HASH, inDesktopApp, machineFromTerminalPath, terminalUrl } from '../ui/terminal-links';

/** The terminal page (`/t/{machine}`, docs/terminal.md): one shell, full window, without a screen session. */
class TerminalPage {
  private info: MachineInfo | null = null;
  private readonly view: TerminalView;
  private readonly statusEl = h('span', { className: 'term-status' });
  private readonly nameEl = h('b', { className: 'term-name' });
  private readonly detailEl = h('span', { className: 'term-detail muted' });
  private readonly sizeEl = h('span', { className: 'term-size muted', title: 'Font size' });

  constructor(
    root: HTMLElement,
    private readonly machine: string,
  ) {
    const fresh = location.hash === NEW_TERMINAL_HASH;
    if (fresh) history.replaceState(null, '', location.pathname + location.search);
    this.view = new TerminalView({
      machine,
      machineName: () => this.name,
      storageKey: `rd.term.page.${machine}`,
      fresh,
      theme: 'auto',
      changed: () => this.render(),
      close: () => this.closePage(),
    });
    const tool = (label: string, name: Parameters<typeof icon>[0], run: () => void, text?: string) =>
      h(
        'button',
        { type: 'button', className: 'icon-button', title: label, ariaLabel: label, onclick: run },
        icon(name, 16),
        text ?? null,
      );
    const fontStep = (d: number) => () => {
      this.view.fontSize += d;
      this.render();
      this.view.focus();
    };
    const header = h(
      'header',
      { className: 'term-head' },
      h(
        'a',
        { className: 'button icon-button term-back', href: '/', title: 'All machines', ariaLabel: 'All machines' },
        icon('back', 16),
      ),
      h('div', { className: 'term-heading' }, this.nameEl, this.statusEl, this.detailEl),
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
        h(
          'button',
          {
            type: 'button',
            className: 'small',
            title: inDesktopApp() ? 'Another shell, in your browser' : 'Another shell, in a new tab',
            onclick: () => window.open(terminalUrl(machine, true), '_blank', 'noopener'),
          },
          icon('plus', 15),
          'New terminal',
        ),
        tool('Close the terminal (ends the shell)', 'close', () => this.onClose()),
      ),
    );
    root.append(h('div', { className: 'term-layout' }, header, h('main', { className: 'term-main' }, this.view.el)));
    this.render();
  }

  private get name(): string {
    return this.info?.name ?? this.machine;
  }

  async boot(): Promise<void> {
    this.view.start();
    this.view.focus();
    this.info = await fetchMachine(this.machine).catch(() => null);
    this.render();
  }

  private onClose(): void {
    if (!this.view.askToEnd()) this.closePage();
  }

  /** Close the tab when a script opened it (New terminal); else back to the machines. */
  private closePage(): void {
    window.close();
    setTimeout(() => location.assign('/'), 150);
  }

  private render(): void {
    document.title = `${this.name} — Terminal`;
    this.nameEl.textContent = this.name;
    const v = this.view;
    this.detailEl.textContent = [v.shell, v.title && v.title !== v.shell ? v.title : ''].filter(Boolean).join(' · ');
    this.detailEl.title = this.detailEl.textContent;
    this.sizeEl.textContent = String(v.fontSize);
    const [cls, text] = statusLabel(v.status.kind);
    setChildren(this.statusEl, h('span', { className: `dot ${cls}` }), text);
    this.statusEl.title = text;
  }
}

/** Dot class and words for the header. */
function statusLabel(kind: TerminalView['status']['kind']): [string, string] {
  switch (kind) {
    case 'ready':
      return ['ok', 'Connected'];
    case 'idle':
    case 'connecting':
      return ['warn', 'Connecting…'];
    case 'retrying':
      return ['warn', 'Reconnecting…'];
    case 'offline':
      return ['bad', 'Offline'];
    case 'sleeping':
    case 'taken':
      return ['', 'Disconnected'];
    case 'ended':
    case 'exited':
      return ['', 'Ended'];
    case 'unsupported':
    case 'failed':
      return ['bad', 'Unavailable'];
  }
}

const root = byId('app');
const machine = machineFromTerminalPath();
if (machine) {
  void new TerminalPage(root, machine).boot();
} else {
  document.body.classList.remove('term-page');
  root.append(
    h(
      'main',
      { className: 'page' },
      h('h1', null, 'No machine selected'),
      h('p', null, h('a', { href: '/' }, 'Choose a machine')),
    ),
  );
}
