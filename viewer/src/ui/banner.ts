import { h, setChildren } from './dom';

export type BannerLevel = 'info' | 'warn' | 'error';

export interface BannerAction {
  label: string;
  /** External link (opens in a new tab) … */
  href?: string;
  /** … or an in-page action. */
  run?: () => void;
}

export interface Banner {
  level: BannerLevel;
  text: string;
  actions?: BannerAction[];
}

/** Stack of keyed banners (one per concern: connection, agent state, errors, …). */
export class Banners {
  readonly el = h('div', { className: 'banners', role: 'status' });
  private readonly items = new Map<string, Banner>();

  set(key: string, banner: Banner | null): void {
    if (banner) this.items.set(key, banner);
    else this.items.delete(key);
    this.render();
  }

  clear(): void {
    this.items.clear();
    this.render();
  }

  private render(): void {
    setChildren(
      this.el,
      ...[...this.items.values()].map((b) =>
        h(
          'div',
          { className: `banner ${b.level}` },
          h('span', { className: 'text' }, b.text),
          ...(b.actions ?? []).map((a) =>
            a.href
              ? h('a', { className: 'button', href: a.href, target: '_blank', rel: 'noopener noreferrer' }, a.label)
              : h('button', { type: 'button', onclick: () => a.run?.() }, a.label),
          ),
        ),
      ),
    );
  }
}
