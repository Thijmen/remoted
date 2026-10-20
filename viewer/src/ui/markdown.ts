// A Markdown subset for release notes (an agent's update notes come from the changelog): headings,
// paragraphs, bullet and numbered lists (nested), fenced code, rules, **bold**, *em*, `code` and
// [links](https://…).
//
// The text is untrusted. It is parsed into a small tree and built with `h`, so it only ever becomes
// text nodes inside a fixed set of elements; nothing is parsed as HTML. Links must be http(s) and open
// in a new tab without a referrer or an opener. Limits keep hostile input cheap: the input is cut at
// MAX_INPUT, lists nest at most MAX_LIST_DEPTH deep and inline markup MAX_INLINE_DEPTH deep (deeper
// markup stays plain text), and every scan is linear or bounded.

import { type Child, h } from './dom';

/** Longer input is cut (the agent sends at most 16 KiB of notes). */
export const MAX_INPUT = 64 * 1024;
export const MAX_LIST_DEPTH = 4;
export const MAX_INLINE_DEPTH = 4;
/** Longest link destination (with its title) looked at. */
const MAX_DESTINATION = 2048;

export type Inline =
  | { t: 'text'; text: string }
  | { t: 'code'; text: string }
  | { t: 'strong'; children: Inline[] }
  | { t: 'em'; children: Inline[] }
  | { t: 'link'; href: string; children: Inline[] };

export type Block =
  | { t: 'heading'; level: number; children: Inline[] }
  | { t: 'paragraph'; children: Inline[] }
  /** Each item is a list of blocks. */
  | { t: 'list'; ordered: boolean; start: number; items: Block[][] }
  | { t: 'code'; text: string }
  | { t: 'rule' };

// ---------------------------------------------------------------------------
// blocks

export function parseMarkdown(src: string): Block[] {
  let text = src;
  if (text.length > MAX_INPUT) {
    text = text.slice(0, MAX_INPUT);
    // Do not leave half a surrogate pair behind.
    if (/[\uD800-\uDBFF]$/.test(text)) text = text.slice(0, -1);
    text += '…';
  }
  const lines = text.replace(/\r\n?/g, '\n').replace(/\t/g, '    ').split('\n');
  return parseBlocks(lines, 0);
}

function isBlank(line: string): boolean {
  return line.trim() === '';
}

function indentOf(line: string): number {
  let i = 0;
  while (line[i] === ' ') i++;
  return i;
}

function isDigit(c: string | undefined): boolean {
  return c !== undefined && c >= '0' && c <= '9';
}

interface Fence {
  char: string;
  len: number;
  indent: number;
}

/** An opening code fence: ``` or ~~~ (at least three). */
function fenceOf(line: string): Fence | null {
  const indent = indentOf(line);
  const char = line[indent];
  if (indent > 3 || (char !== '`' && char !== '~')) return null;
  let len = 0;
  while (line[indent + len] === char) len++;
  if (len < 3) return null;
  // A backtick fence's info string cannot contain backticks (it would be inline code).
  if (char === '`' && line.indexOf('`', indent + len) >= 0) return null;
  return { char, len, indent };
}

function closesFence(line: string, fence: Fence): boolean {
  const indent = indentOf(line);
  if (indent > 3) return false;
  let len = 0;
  while (line[indent + len] === fence.char) len++;
  return len >= fence.len && isBlank(line.slice(indent + len));
}

function headingOf(line: string): Block | null {
  const indent = indentOf(line);
  if (indent > 3) return null;
  let level = 0;
  while (line[indent + level] === '#') level++;
  const after = line[indent + level];
  if (level < 1 || level > 6 || (after !== undefined && after !== ' ')) return null;
  let content = line.slice(indent + level).trim();
  // An optional closing run of #s, separated by a space: "## 1.6.0 ##".
  let end = content.length;
  while (end > 0 && content[end - 1] === '#') end--;
  if (end === 0) content = '';
  else if (end < content.length && content[end - 1] === ' ') content = content.slice(0, end).trimEnd();
  return { t: 'heading', level, children: parseInline(content) };
}

function isRule(line: string): boolean {
  if (indentOf(line) > 3) return false;
  const chars = line.replace(/ /g, '');
  return chars.length >= 3 && (/^-+$/.test(chars) || /^\*+$/.test(chars) || /^_+$/.test(chars));
}

interface ListMarker {
  ordered: boolean;
  start: number;
  /** Where the item's content starts; continuation lines are indented at least this far. */
  offset: number;
  /** The first line's content. */
  rest: string;
}

/** `- x`, `* x`, `+ x`, `1. x` or `1) x`. */
function listMarkerOf(line: string): ListMarker | null {
  const indent = indentOf(line);
  if (indent > 3) return null;
  let p = indent;
  let ordered = false;
  let start = 1;
  const c = line[p];
  if (c === '-' || c === '*' || c === '+') {
    p++;
  } else {
    while (isDigit(line[p]) && p - indent < 9) p++;
    if (p === indent || (line[p] !== '.' && line[p] !== ')')) return null;
    ordered = true;
    start = Number(line.slice(indent, p));
    p++;
  }
  if (p < line.length && line[p] !== ' ') return null;
  let spaces = 0;
  while (line[p + spaces] === ' ') spaces++;
  // No content, or a wide gap (content that is itself indented): the content starts one space in.
  if (p + spaces >= line.length || spaces > 4) spaces = 1;
  return { ordered, start, offset: p + spaces, rest: line.slice(p + spaces) };
}

/** Whether `line` starts a block other than a paragraph (so it cannot continue one). */
function startsBlock(line: string, depth: number): boolean {
  return (
    fenceOf(line) !== null ||
    headingOf(line) !== null ||
    isRule(line) ||
    (depth < MAX_LIST_DEPTH && listMarkerOf(line) !== null)
  );
}

function parseBlocks(lines: string[], depth: number): Block[] {
  const out: Block[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length) out.push({ t: 'paragraph', children: parseInline(para.join(' ')) });
    para = [];
  };
  let i = 0;
  while (i < lines.length) {
    const line = lines[i]!;
    if (isBlank(line)) {
      flush();
      i++;
      continue;
    }
    const fence = fenceOf(line);
    if (fence) {
      flush();
      const body: string[] = [];
      for (i++; i < lines.length && !closesFence(lines[i]!, fence); i++) {
        const l = lines[i]!;
        body.push(l.slice(Math.min(indentOf(l), fence.indent)));
      }
      i++; // the closing fence (or the end)
      out.push({ t: 'code', text: body.join('\n') });
      continue;
    }
    const heading = headingOf(line);
    if (heading) {
      flush();
      out.push(heading);
      i++;
      continue;
    }
    // Before lists: "- - -" is a rule.
    if (isRule(line)) {
      flush();
      out.push({ t: 'rule' });
      i++;
      continue;
    }
    if (depth < MAX_LIST_DEPTH && listMarkerOf(line)) {
      flush();
      i = parseList(lines, i, depth, out);
      continue;
    }
    para.push(line.trim());
    i++;
  }
  flush();
  return out;
}

/** The list starting at `lines[i]`; returns the index after it. */
function parseList(lines: string[], i: number, depth: number, out: Block[]): number {
  const first = listMarkerOf(lines[i]!)!;
  const list: Block & { t: 'list' } = { t: 'list', ordered: first.ordered, start: first.start, items: [] };
  out.push(list);
  while (i < lines.length) {
    const marker = listMarkerOf(lines[i]!);
    if (!marker || marker.ordered !== list.ordered) break;
    const body = [marker.rest];
    for (i++; i < lines.length; i++) {
      const line = lines[i]!;
      if (isBlank(line)) {
        // Blank lines belong to the item when it goes on after them (indented).
        let j = i + 1;
        while (j < lines.length && isBlank(lines[j]!)) j++;
        if (j >= lines.length || indentOf(lines[j]!) < marker.offset) break;
        for (; i < j; i++) body.push('');
        i--;
        continue;
      }
      if (indentOf(line) >= marker.offset) {
        body.push(line.slice(marker.offset));
        continue;
      }
      // A "lazy" line continues the item's text, unless it starts something else.
      if (body[body.length - 1] !== '' && !startsBlock(line, depth)) {
        body.push(line.trim());
        continue;
      }
      break;
    }
    list.items.push(parseBlocks(body, depth + 1));
    // Blank lines between items keep the list going.
    let j = i;
    while (j < lines.length && isBlank(lines[j]!)) j++;
    const next = j < lines.length ? listMarkerOf(lines[j]!) : null;
    if (!next || next.ordered !== list.ordered) break;
    i = j;
  }
  return i;
}

// ---------------------------------------------------------------------------
// inline

type Tok =
  | { k: 'text'; s: string }
  | { k: 'code'; s: string }
  | { k: 'delim'; c: '*' | '_'; n: number; open: boolean; close: boolean }
  | { k: 'open' }
  /** `]`, with the raw text it stands for and the destination of a following `(…)`. */
  | { k: 'close'; raw: string; href: string | null };

const PUNCT = '!"#$%&\'()*+,-./:;<=>?@[\\]^_`{|}~';

function isSpace(c: string | undefined): boolean {
  return c === undefined || /\s/.test(c);
}

function isAlnum(c: string | undefined): boolean {
  return c !== undefined && /[\p{L}\p{N}]/u.test(c);
}

/** Only absolute http(s) URLs become links. */
export function safeHref(raw: string): string | null {
  try {
    const url = new URL(raw);
    return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : null;
  } catch {
    return null;
  }
}

/** `(dest)` or `(dest "title")` starting at `s[p] === '('`: the destination and the index after `)`. */
function destinationAt(s: string, p: number): { dest: string; end: number } | null {
  const limit = Math.min(s.length, p + MAX_DESTINATION);
  let i = p + 1;
  while (i < limit && !isSpace(s[i]) && s[i] !== ')' && s[i] !== '(') i++;
  const dest = s.slice(p + 1, i);
  while (i < limit && s[i] === ' ') i++;
  if (s[i] === '"' && dest !== '') {
    const close = s.indexOf('"', i + 1);
    if (close < 0 || close >= limit) return null;
    i = close + 1;
    while (i < limit && s[i] === ' ') i++;
  }
  return i < limit && s[i] === ')' ? { dest, end: i + 1 } : null;
}

function tokenize(s: string): Tok[] {
  const toks: Tok[] = [];
  let text = '';
  const push = (t: Tok) => {
    if (text) toks.push({ k: 'text', s: text });
    text = '';
    toks.push(t);
  };
  // Backtick run lengths known to have no closing run further on: a later opener of the same
  // length cannot find one either, so each length is searched in vain at most once.
  const noCloser = new Set<number>();
  let i = 0;
  while (i < s.length) {
    const c = s[i]!;
    if (c === '\\' && i + 1 < s.length && PUNCT.includes(s[i + 1]!)) {
      text += s[i + 1];
      i += 2;
      continue;
    }
    if (c === '`') {
      let n = 1;
      while (s[i + n] === '`') n++;
      const close = noCloser.has(n) ? -1 : backtickCloser(s, i + n, n);
      if (close < 0) {
        noCloser.add(n);
        text += '`'.repeat(n);
        i += n;
        continue;
      }
      let code = s.slice(i + n, close);
      if (code.length > 2 && code.startsWith(' ') && code.endsWith(' ') && code.trim() !== '') {
        code = code.slice(1, -1);
      }
      push({ k: 'code', s: code });
      i = close + n;
      continue;
    }
    if (c === '*' || c === '_') {
      let n = 1;
      while (s[i + n] === c) n++;
      const before = s[i - 1];
      const after = s[i + n];
      // Simplified flanking rules; `_` never opens or closes inside a word (snake_case).
      const open = !isSpace(after) && (c === '*' || !isAlnum(before));
      const close = !isSpace(before) && (c === '*' || !isAlnum(after));
      push({ k: 'delim', c, n, open, close });
      i += n;
      continue;
    }
    if (c === '[') {
      push({ k: 'open' });
      i++;
      continue;
    }
    if (c === ']') {
      const d = s[i + 1] === '(' ? destinationAt(s, i + 1) : null;
      push(d ? { k: 'close', raw: s.slice(i, d.end), href: d.dest } : { k: 'close', raw: ']', href: null });
      i = d ? d.end : i + 1;
      continue;
    }
    text += c;
    i++;
  }
  if (text) toks.push({ k: 'text', s: text });
  return toks;
}

/** Start of the next run of exactly `n` backticks at or after `from`, or -1. */
function backtickCloser(s: string, from: number, n: number): number {
  let i = s.indexOf('`', from);
  while (i >= 0) {
    let len = 1;
    while (s[i + len] === '`') len++;
    if (len === n) return i;
    i = s.indexOf('`', i + len);
  }
  return -1;
}

export function parseInline(s: string): Inline[] {
  const toks = tokenize(s);
  return build(toks, 0, toks.length, 0, false);
}

function tokText(t: Tok): string {
  switch (t.k) {
    case 'text':
      return t.s;
    case 'code':
      return t.s;
    case 'delim':
      return t.c.repeat(t.n);
    case 'open':
      return '[';
    case 'close':
      return t.raw;
  }
}

class InlineOut {
  readonly nodes: Inline[] = [];
  text(s: string): void {
    const last = this.nodes[this.nodes.length - 1];
    if (last?.t === 'text') last.text += s;
    else if (s) this.nodes.push({ t: 'text', text: s });
  }
  push(n: Inline): void {
    this.nodes.push(n);
  }
}

/**
 * Inline nodes for `toks[from, to)`. Links are found first and are atomic at this level (emphasis
 * cannot close inside one); emphasis pairs an opener with the next closer of the same kind.
 */
function build(toks: Tok[], from: number, to: number, depth: number, inLink: boolean): Inline[] {
  const out = new InlineOut();
  if (depth >= MAX_INLINE_DEPTH) {
    for (let i = from; i < to; i++) {
      const t = toks[i]!;
      if (t.k === 'code') out.push({ t: 'code', text: t.s });
      else out.text(tokText(t));
    }
    return out.nodes;
  }

  // Units: single tokens, or a whole link [start, end] (an `[` and its `]` with a destination).
  const match = new Map<number, number>();
  if (!inLink) {
    const stack: number[] = [];
    for (let i = from; i < to; i++) {
      const t = toks[i]!;
      if (t.k === 'open') stack.push(i);
      else if (t.k === 'close' && stack.length) match.set(stack.pop()!, i);
    }
  }
  const units: Array<{ start: number; end: number }> = [];
  for (let i = from; i < to; i++) {
    const m = match.get(i);
    const close = m === undefined ? undefined : toks[m];
    if (close?.k === 'close' && close.href !== null) {
      units.push({ start: i, end: m! });
      i = m!;
    } else {
      units.push({ start: i, end: i });
    }
  }

  // Openers whose kind found no closer from this unit on: later ones cannot either.
  const failedFrom = new Map<string, number>();
  const closerOf = (u: number, t: Tok & { k: 'delim' }): number => {
    const key = `${t.c}${t.n}`;
    const failed = failedFrom.get(key);
    if (failed !== undefined && u >= failed) return -1;
    for (let v = u + 1; v < units.length; v++) {
      const unit = units[v]!;
      const c = toks[unit.start]!;
      if (unit.start === unit.end && c.k === 'delim' && c.c === t.c && c.n === t.n && c.close) return v;
    }
    failedFrom.set(key, u);
    return -1;
  };

  for (let u = 0; u < units.length; u++) {
    const unit = units[u]!;
    const t = toks[unit.start]!;
    if (unit.end > unit.start) {
      const close = toks[unit.end] as Tok & { k: 'close' };
      const children = build(toks, unit.start + 1, unit.end, depth + 1, true);
      const href = safeHref(close.href!);
      if (href) out.push({ t: 'link', href, children });
      else
        for (const c of children)
          if (c.t === 'text') out.text(c.text);
          else out.push(c);
      continue;
    }
    if (t.k === 'code') {
      out.push({ t: 'code', text: t.s });
      continue;
    }
    if (t.k === 'delim' && t.open && t.n <= 2) {
      const c = closerOf(u, t);
      if (c >= 0) {
        const children = build(toks, unit.start + 1, units[c]!.start, depth + 1, inLink);
        out.push(t.n === 2 ? { t: 'strong', children } : { t: 'em', children });
        u = c;
        continue;
      }
    }
    out.text(tokText(t));
  }
  return out.nodes;
}

// ---------------------------------------------------------------------------
// DOM

/** Rendered Markdown in a `div.markdown`. Headings start at h3: they sit under a dialog's h2. */
export function renderMarkdown(src: string): HTMLDivElement {
  return h('div', { className: 'markdown' }, ...parseMarkdown(src).map(blockEl));
}

function headingTag(level: number): 'h3' | 'h4' | 'h5' {
  return level <= 2 ? 'h3' : level === 3 ? 'h4' : 'h5';
}

function blockEl(b: Block): HTMLElement {
  switch (b.t) {
    case 'heading':
      return h(headingTag(b.level), null, ...b.children.map(inlineEl));
    case 'paragraph':
      return h('p', null, ...b.children.map(inlineEl));
    case 'list': {
      const items = b.items.map(itemEl);
      return b.ordered ? h('ol', b.start === 1 ? null : { start: b.start }, ...items) : h('ul', null, ...items);
    }
    case 'code':
      return h('pre', null, h('code', null, b.text));
    case 'rule':
      return h('hr');
  }
}

/** A list item's leading paragraph goes straight into the `<li>` (no gap above it). */
function itemEl(blocks: Block[]): HTMLLIElement {
  const [first, ...rest] = blocks;
  if (first?.t === 'paragraph') return h('li', null, ...first.children.map(inlineEl), ...rest.map(blockEl));
  return h('li', null, ...blocks.map(blockEl));
}

function inlineEl(n: Inline): Child {
  switch (n.t) {
    case 'text':
      return n.text;
    case 'code':
      return h('code', null, n.text);
    case 'strong':
      return h('strong', null, ...n.children.map(inlineEl));
    case 'em':
      return h('em', null, ...n.children.map(inlineEl));
    case 'link':
      return h('a', { href: n.href, target: '_blank', rel: 'noopener noreferrer' }, ...n.children.map(inlineEl));
  }
}
