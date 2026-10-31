// The portal's command palette (Cmd+K / Ctrl+K): jump to a device's screen, terminal or files,
// or run a portal command, by typing a few letters.

import { h, setChildren } from './dom';
import { type IconName, icon } from './icons';
import { matchesQuery } from './portal-model';

export interface Command {
  /** Heading the command is listed under. */
  group: string;
  title: string;
  /** Shown at the right: the action, a shortcut. */
  hint?: string;
  /** More words that find it: machine id, OS. */
  keywords?: string;
  icon: IconName;
  run: () => void;
}

/** At most this many results are listed. */
const MAX_RESULTS = 60;

export function filterCommands(commands: readonly Command[], query: string): Command[] {
  return commands
    .filter((c) => matchesQuery(query, `${c.title} ${c.hint ?? ''} ${c.keywords ?? ''} ${c.group}`))
    .slice(0, MAX_RESULTS);
}

export class CommandPalette {
  private readonly dialog = h('dialog', { className: 'palette' });
  private readonly input = h('input', {
    type: 'text',
    placeholder: 'Jump to a device or run a command…',
    autocomplete: 'off',
    spellcheck: false,
    ariaLabel: 'Command',
  });
  private readonly list = h('div', { className: 'palette-list', id: 'palette-list', role: 'listbox' });
  private results: Command[] = [];
  private active = 0;

  constructor(private readonly commands: () => Command[]) {
    this.input.setAttribute('role', 'combobox');
    this.input.setAttribute('aria-controls', 'palette-list');
    this.input.setAttribute('aria-expanded', 'true');
    this.dialog.append(
      h(
        'div',
        { className: 'palette-box' },
        h('label', { className: 'palette-input' }, icon('search', 18), this.input, h('kbd', null, 'esc')),
        this.list,
        h(
          'div',
          { className: 'palette-foot' },
          h('span', null, h('kbd', null, '↑'), ' ', h('kbd', null, '↓'), ' choose'),
          h('span', null, h('kbd', null, '↵'), ' run'),
        ),
      ),
    );
    this.input.addEventListener('input', () => {
      this.active = 0;
      this.update();
    });
    this.input.addEventListener('keydown', (e) => this.onKey(e));
    // A click on the backdrop lands on the dialog itself.
    this.dialog.addEventListener('click', (e) => {
      if (e.target === this.dialog) this.close();
    });
    document.body.append(this.dialog);
  }

  get isOpen(): boolean {
    return this.dialog.open;
  }

  open(): void {
    if (this.dialog.open) return;
    this.input.value = '';
    this.active = 0;
    this.update();
    this.dialog.showModal();
    this.input.focus();
  }

  close(): void {
    this.dialog.close();
  }

  private onKey(e: KeyboardEvent): void {
    const n = this.results.length;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (n === 0) return;
      this.highlight((this.active + (e.key === 'ArrowDown' ? 1 : -1) + n) % n);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const c = this.results[this.active];
      if (c) this.run(c);
    }
  }

  private run(c: Command): void {
    this.close();
    c.run();
  }

  /** List the commands that match the input. */
  private update(): void {
    this.results = filterCommands(this.commands(), this.input.value);
    if (this.results.length === 0) {
      setChildren(this.list, h('div', { className: 'palette-empty' }, 'Nothing matches.'));
      this.input.removeAttribute('aria-activedescendant');
      return;
    }
    const items: HTMLElement[] = [];
    let group = '';
    this.results.forEach((c, i) => {
      if (c.group !== group) {
        group = c.group;
        items.push(h('div', { className: 'palette-group', role: 'presentation' }, group));
      }
      const item = h(
        'div',
        { className: 'palette-item', id: `palette-opt-${i}`, role: 'option' },
        icon(c.icon, 16),
        h('span', { className: 'palette-title' }, c.title),
        c.hint ? h('span', { className: 'palette-hint' }, c.hint) : null,
      );
      item.setAttribute('aria-selected', 'false');
      item.addEventListener('mousemove', () => {
        if (this.active !== i) this.highlight(i);
      });
      item.addEventListener('click', () => this.run(c));
      items.push(item);
    });
    setChildren(this.list, ...items);
    this.highlight(this.active);
  }

  private highlight(i: number): void {
    this.list.querySelector(`#palette-opt-${this.active}`)?.setAttribute('aria-selected', 'false');
    this.active = i;
    const item = this.list.querySelector(`#palette-opt-${i}`);
    item?.setAttribute('aria-selected', 'true');
    item?.scrollIntoView({ block: 'nearest' });
    this.input.setAttribute('aria-activedescendant', `palette-opt-${i}`);
  }
}
