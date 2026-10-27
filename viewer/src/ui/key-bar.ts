import { h } from './dom';

export interface KeyBarDeps {
  /** Tap a key: down and up immediately (its physical code, e.g. `ArrowLeft`). */
  tap(code: string): void;
  /** Toggle a sticky modifier; returns whether it is held on the remote side now. */
  toggle(code: string): boolean;
  /** Is the modifier held on the remote side? */
  held(code: string): boolean;
  /** Toggle the phone's soft keyboard; returns whether it is open now. */
  keyboard(): boolean;
}

interface Key {
  code: string;
  label: string;
  /** Spoken name; the label is a symbol or an abbreviation. */
  name: string;
  modifier?: boolean;
}

const CTRL: Key = { code: 'ControlLeft', label: 'Ctrl', name: 'Control', modifier: true };
const ALT: Key = { code: 'AltLeft', label: 'Alt', name: 'Alt', modifier: true };
const SHIFT: Key = { code: 'ShiftLeft', label: 'Shift', name: 'Shift', modifier: true };
/** Its label follows the agent's OS (Command on macOS, Windows elsewhere). */
const META: Key = { code: 'MetaLeft', label: 'Cmd', name: 'Command', modifier: true };

/** Keys on the bar itself; modifiers are sticky, the rest tap. */
const MAIN: Key[] = [
  { code: 'Escape', label: 'esc', name: 'Escape' },
  { code: 'Tab', label: 'tab', name: 'Tab' },
  { code: 'Backspace', label: '⌫', name: 'Backspace' },
  { code: 'Enter', label: '⏎', name: 'Enter' },
  { code: 'ArrowLeft', label: '←', name: 'Arrow left' },
  { code: 'ArrowUp', label: '↑', name: 'Arrow up' },
  { code: 'ArrowDown', label: '↓', name: 'Arrow down' },
  { code: 'ArrowRight', label: '→', name: 'Arrow right' },
];

/** In the "Fn" row: function keys and the keys a phone has no other way to press. */
const FN: Key[] = [
  ...Array.from({ length: 12 }, (_, i): Key => ({ code: `F${i + 1}`, label: `F${i + 1}`, name: `F${i + 1}` })),
  { code: 'Home', label: 'Home', name: 'Home' },
  { code: 'End', label: 'End', name: 'End' },
  { code: 'PageUp', label: 'PgUp', name: 'Page up' },
  { code: 'PageDown', label: 'PgDn', name: 'Page down' },
  { code: 'Delete', label: 'Del', name: 'Delete' },
];

/**
 * The on-screen keys for a touch screen (docs/android.md): sticky Ctrl/Alt/Shift/Cmd for
 * combinations, the navigation keys, and an "Fn" row for F1–F12, Home/End/Page/Delete.
 * Shown on coarse pointers only; the buttons never take focus, so the session keeps the keyboard.
 */
export class KeyBar {
  readonly el = h('div', { className: 'key-bar', hidden: true });
  private readonly fnRow = h('div', { className: 'key-bar-row key-bar-fn', hidden: true });
  private readonly buttons = new Map<string, HTMLButtonElement>();
  private readonly modifiers = new Set<string>();
  private readonly metaButton: HTMLButtonElement;
  /** The soft-keyboard toggle; hidden until the agent advertises `text`. */
  private readonly keyboardButton: HTMLButtonElement;

  constructor(private readonly deps: KeyBarDeps) {
    const row = h('div', { className: 'key-bar-row' });
    this.keyboardButton = h(
      'button',
      { type: 'button', className: 'key-bar-key key-bar-keyboard', tabIndex: -1, title: 'Keyboard', hidden: true },
      '⌨',
    );
    this.keyboardButton.setAttribute('aria-label', 'Keyboard');
    this.keyboardButton.setAttribute('aria-pressed', 'false');
    this.keyboardButton.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      this.setKeyboardOpen(this.deps.keyboard());
    });
    this.keyboardButton.addEventListener('click', (e) => e.preventDefault());
    row.append(this.keyboardButton);
    for (const key of [CTRL, ALT, SHIFT, META]) row.append(this.button(key));
    for (const key of MAIN) row.append(this.button(key));
    this.metaButton = this.buttons.get(META.code)!;
    const fn = h(
      'button',
      { type: 'button', className: 'key-bar-key key-bar-fn-toggle', tabIndex: -1, title: 'Function keys' },
      'Fn',
    );
    fn.setAttribute('aria-label', 'Function keys');
    fn.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      this.toggleFn();
    });
    fn.addEventListener('click', (e) => e.preventDefault());
    row.append(fn);
    for (const key of FN) this.fnRow.append(this.button(key));
    this.el.append(row, this.fnRow);
  }

  /** The agent's OS decides what the meta key is called. */
  setMeta(os: string | undefined): void {
    const mac = os === 'macos';
    this.metaButton.textContent = mac ? 'Cmd' : 'Win';
    this.metaButton.setAttribute('aria-label', mac ? 'Command' : 'Windows');
  }

  /** Only an agent that advertises `text` can type committed text; hide the button otherwise. */
  setTextAvailable(on: boolean): void {
    this.keyboardButton.hidden = !on;
    if (!on) this.setKeyboardOpen(false);
  }

  setKeyboardOpen(on: boolean): void {
    this.keyboardButton.classList.toggle('on', on);
    this.keyboardButton.setAttribute('aria-pressed', String(on));
  }

  show(on: boolean): void {
    this.el.hidden = !on;
    if (on) this.sync();
  }

  /** A new session (or a disconnect): nothing is held, and the Fn row is closed. */
  reset(): void {
    this.sync();
    this.setKeyboardOpen(false);
    this.fnRow.hidden = true;
    this.el.querySelector('.key-bar-fn-toggle')?.classList.remove('on');
  }

  private sync(): void {
    for (const code of this.modifiers) {
      const b = this.buttons.get(code);
      if (!b) continue;
      const on = this.deps.held(code);
      b.classList.toggle('on', on);
      b.setAttribute('aria-pressed', String(on));
    }
  }

  private toggleFn(): void {
    this.fnRow.hidden = !this.fnRow.hidden;
    this.el.querySelector('.key-bar-fn-toggle')?.classList.toggle('on', !this.fnRow.hidden);
  }

  private button(key: Key): HTMLButtonElement {
    const b = h('button', { type: 'button', className: 'key-bar-key', tabIndex: -1, title: key.name }, key.label);
    b.setAttribute('aria-label', key.name);
    if (key.modifier) {
      this.modifiers.add(key.code);
      b.setAttribute('aria-pressed', 'false');
    }
    // pointerdown, not click: no 300 ms delay, and preventDefault keeps focus on the canvas.
    b.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      if (key.modifier) {
        const on = this.deps.toggle(key.code);
        b.classList.toggle('on', on);
        b.setAttribute('aria-pressed', String(on));
      } else {
        this.deps.tap(key.code);
      }
    });
    b.addEventListener('click', (e) => e.preventDefault());
    this.buttons.set(key.code, b);
    return b;
  }
}
