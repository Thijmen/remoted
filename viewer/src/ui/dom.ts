export type Child = Node | string | number | null | undefined | false;

type Props<K extends keyof HTMLElementTagNameMap> = Partial<Omit<HTMLElementTagNameMap[K], 'style'>> & {
  style?: string;
};

/**
 * Tiny element builder. Text children become text nodes (never parsed as HTML), so server
 * strings (machine names, emails) are safe to pass straight in.
 */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props<K> | null = null,
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props) {
    const { style, ...rest } = props;
    Object.assign(el, rest);
    if (style) el.setAttribute('style', style);
  }
  append(el, children);
  return el;
}

export function append(el: Node, children: Child[]): void {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.appendChild(typeof c === 'string' || typeof c === 'number' ? document.createTextNode(String(c)) : c);
  }
}

/** Replace all children. */
export function setChildren(el: Node, ...children: Child[]): void {
  while (el.firstChild) el.removeChild(el.firstChild);
  append(el, children);
}

export function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} missing`);
  return el as T;
}

export function timeAgo(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const hrs = Math.round(m / 60);
  return hrs < 48 ? `${hrs} h ago` : `${Math.round(hrs / 24)} days ago`;
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}

/** Offer `data` as a downloaded JSON file. */
export function downloadJson(filename: string, data: unknown): void {
  const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }));
  const a = h('a', { href: url, download: filename });
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}
