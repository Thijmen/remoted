import { h } from '../ui/dom';

export interface ImeDeps {
  /** Committed text (a whole word, a composed character, an emoji). */
  text(text: string): void;
  /** A named key from the soft keyboard still goes through the HID path. */
  key(code: string): void;
  /** The soft keyboard opened or closed. */
  changed(open: boolean): void;
}

/**
 * Named keys a soft keyboard sends as key events; everything else is text for `deps.text`.
 * Backspace/Enter/arrows are sent as HID so the host sees the same keys a physical keyboard has.
 */
const NAMED = new Set([
  'Enter',
  'Backspace',
  'Tab',
  'Escape',
  'Delete',
  'ArrowLeft',
  'ArrowRight',
  'ArrowUp',
  'ArrowDown',
  'Home',
  'End',
  'PageUp',
  'PageDown',
]);

/**
 * The phone soft keyboard (docs/android.md). Android only opens one for an editable element, so
 * a hidden textarea holds the focus while it is open. Committed text — including IME
 * compositions, which `input` reports while `isComposing` — goes out as TEXT; named keys stay on
 * the HID path. Nothing is ever logged: this is what the user types.
 */
export class ImeInput {
  readonly el = h('textarea', { className: 'ime-input', tabIndex: -1, spellcheck: false });
  private composing = false;

  constructor(private readonly deps: ImeDeps) {
    this.el.setAttribute('aria-hidden', 'true');
    this.el.setAttribute('autocapitalize', 'off');
    this.el.setAttribute('autocorrect', 'off');
    this.el.setAttribute('autocomplete', 'off');
    this.el.addEventListener('compositionstart', () => {
      this.composing = true;
    });
    this.el.addEventListener('compositionend', (e) => {
      this.composing = false;
      this.commit(e.data);
    });
    this.el.addEventListener('input', (e) => {
      // While composing, the text is not final: `compositionend` commits it once.
      if (this.composing || (e as InputEvent).isComposing) return;
      this.commit(this.el.value);
    });
    this.el.addEventListener('keydown', (e) => {
      if (!NAMED.has(e.code)) return; // Letters and symbols arrive as `input`.
      e.preventDefault();
      e.stopPropagation();
      this.deps.key(e.code);
    });
    this.el.addEventListener('blur', () => this.deps.changed(false));
  }

  get open(): boolean {
    return document.activeElement === this.el;
  }

  openKeyboard(): void {
    this.el.value = '';
    this.el.focus({ preventScroll: true });
    this.deps.changed(true);
  }

  close(): void {
    if (!this.open) return;
    this.el.blur();
    this.deps.changed(false);
  }

  private commit(text: string | null | undefined): void {
    const value = text ?? '';
    this.el.value = '';
    if (value.length > 0) this.deps.text(value);
  }
}
