import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MAX_INPUT, MAX_LIST_DEPTH, parseInline, parseMarkdown, renderMarkdown, safeHref } from './markdown';

// Just enough DOM for `h`: elements, text nodes and properties. Setting innerHTML throws, so a test
// fails if the renderer ever parses text as HTML.
class FakeNode {
  readonly childNodes: FakeNode[] = [];
  appendChild(c: FakeNode): FakeNode {
    this.childNodes.push(c);
    return c;
  }
}

class FakeText extends FakeNode {
  constructor(readonly data: string) {
    super();
  }
}

class FakeElement extends FakeNode {
  readonly attrs: Record<string, string> = {};
  constructor(readonly tagName: string) {
    super();
  }
  setAttribute(k: string, v: string): void {
    this.attrs[k] = v;
  }
  set innerHTML(_: string) {
    throw new Error('innerHTML must not be used');
  }
  set outerHTML(_: string) {
    throw new Error('outerHTML must not be used');
  }
}

const escape = (s: string) =>
  s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** Serialize the fake tree; text is escaped like a browser would. */
function html(n: FakeNode): string {
  if (n instanceof FakeText) return escape(n.data);
  const el = n as FakeElement & Record<string, unknown>;
  const attrs = ['href', 'target', 'rel', 'start']
    .filter((k) => el[k] !== undefined)
    .map((k) => ` ${k}="${escape(String(el[k]))}"`)
    .join('');
  const inner = el.childNodes.map(html).join('');
  return el.tagName === 'div' ? inner : `<${el.tagName}${attrs}>${inner}</${el.tagName}>`;
}

const render = (md: string) => html(renderMarkdown(md) as unknown as FakeNode);

beforeEach(() => {
  vi.stubGlobal('document', {
    createElement: (tag: string) => new FakeElement(tag),
    createTextNode: (s: string) => new FakeText(s),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('renderMarkdown', () => {
  it('renders changelog notes: headings, lists, paragraphs and inline markup', () => {
    const notes = [
      '## 1.6.0',
      '',
      '- Cmd+` cycles the remote Mac’s windows.',
      '- **Faster** start with *less* CPU; see `rd-agent update --check`.',
      '  Continued on an indented line.',
      '',
      '## 1.5.1',
      '',
      'One paragraph',
      'over two lines, with a [release page](https://github.com/Thijmen/remoted/releases).',
    ].join('\n');
    expect(render(notes)).toBe(
      '<h3>1.6.0</h3>' +
        '<ul><li>Cmd+` cycles the remote Mac’s windows.</li>' +
        '<li><strong>Faster</strong> start with <em>less</em> CPU; see <code>rd-agent update --check</code>. Continued on an indented line.</li></ul>' +
        '<h3>1.5.1</h3>' +
        '<p>One paragraph over two lines, with a <a href="https://github.com/Thijmen/remoted/releases" target="_blank" rel="noopener noreferrer">release page</a>.</p>',
    );
  });

  it('keeps HTML as text', () => {
    const out = render('<script>alert(1)</script>\n\n- <img src=x onerror=alert(1)> **<b>x</b>**');
    expect(out).toBe(
      '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>' +
        '<ul><li>&lt;img src=x onerror=alert(1)&gt; <strong>&lt;b&gt;x&lt;/b&gt;</strong></li></ul>',
    );
  });

  it('links only http(s) URLs, in a new tab without referrer or opener', () => {
    for (const bad of [
      'javascript:alert(1)',
      'JavaScript:alert(1)',
      'data:text/html,<script>alert(1)</script>',
      'vbscript:x',
      '/relative',
      '//evil.example/x',
      'mailto:a@b.c',
    ]) {
      expect(render(`[click](${bad})`), bad).not.toContain('<a');
      expect(render(`[click](${bad})`), bad).toContain('click');
    }
    expect(render('[a](http://example.test/a_b_c "title")')).toBe(
      '<p><a href="http://example.test/a_b_c" target="_blank" rel="noopener noreferrer">a</a></p>',
    );
  });

  it('nests lists and numbers ordered ones from their start', () => {
    expect(render('3. three\n4. four\n   - sub\n     1. deeper\n\n5. five')).toBe(
      '<ol start="3"><li>three</li><li>four<ul><li>sub<ol><li>deeper</li></ol></li></ul></li><li>five</li></ol>',
    );
  });

  it('renders fenced code and rules', () => {
    expect(render('```sh\nrd-agent update\n  <b>\n```\n\n---\n\nend')).toBe(
      '<pre><code>rd-agent update\n  &lt;b&gt;</code></pre><hr></hr><p>end</p>',
    );
  });
});

describe('parseMarkdown limits', () => {
  /** Depth of nested lists in a block tree. */
  function listDepth(blocks: ReturnType<typeof parseMarkdown>): number {
    let max = 0;
    for (const b of blocks) {
      if (b.t === 'list') max = Math.max(max, 1 + Math.max(0, ...b.items.map(listDepth)));
    }
    return max;
  }

  it('nests lists at most MAX_LIST_DEPTH deep; deeper markers stay text', () => {
    const md = Array.from({ length: 12 }, (_, i) => `${'  '.repeat(i)}- level ${i + 1}`).join('\n');
    const blocks = parseMarkdown(md);
    expect(listDepth(blocks)).toBe(MAX_LIST_DEPTH);
    expect(render(md)).toContain('level 5 - level 6');
    // One line of markers nests too ("- - - x"), within the same limit.
    expect(listDepth(parseMarkdown(`${'- '.repeat(5000)}x`))).toBe(MAX_LIST_DEPTH);
  });

  it('nests inline markup at most MAX_INLINE_DEPTH deep', () => {
    expect(render('[**a *b _c __d__ c_ b* a**](https://x.test)')).toBe(
      '<p><a href="https://x.test/" target="_blank" rel="noopener noreferrer"><strong>a <em>b <em>c __d__ c</em> b</em> a</strong></a></p>',
    );
  });

  it('cuts long input', () => {
    const blocks = parseMarkdown('word '.repeat(100_000));
    expect(blocks).toHaveLength(1);
    const p = blocks[0]!;
    if (p.t !== 'paragraph' || p.children[0]?.t !== 'text') throw new Error('expected a paragraph');
    expect(p.children[0].text.length).toBeLessThanOrEqual(MAX_INPUT + 1);
    expect(p.children[0].text.endsWith('…')).toBe(true);
  });

  it('stays fast on hostile input', () => {
    const n = MAX_INPUT;
    for (const md of [
      '['.repeat(n),
      '[a]('.repeat(n / 4),
      '](x "'.repeat(n / 5),
      '*a '.repeat(n / 3),
      '_a '.repeat(n / 3),
      '**a '.repeat(n / 4),
      Array.from({ length: 360 }, (_, i) => '`'.repeat(i + 1)).join('a'),
      '- a\n'.repeat(n / 4),
      '  '.repeat(n / 4) + '- x',
      '#'.repeat(n),
    ]) {
      const start = performance.now();
      parseMarkdown(md);
      expect(performance.now() - start, md.slice(0, 20)).toBeLessThan(1000);
    }
  });
});

describe('parseInline', () => {
  it('leaves stray and intraword markers alone', () => {
    expect(parseInline('a * b, snake_case_name and 2*3*4 = 24')).toEqual([
      { t: 'text', text: 'a * b, snake_case_name and 2' },
      { t: 'em', children: [{ t: 'text', text: '3' }] },
      { t: 'text', text: '4 = 24' },
    ]);
    expect(parseInline('**unclosed and [unclosed')).toEqual([{ t: 'text', text: '**unclosed and [unclosed' }]);
  });

  it('gives code spans and escapes precedence', () => {
    expect(parseInline('`*not em*` and ``a`b`` and \\*not em\\*')).toEqual([
      { t: 'code', text: '*not em*' },
      { t: 'text', text: ' and ' },
      { t: 'code', text: 'a`b' },
      { t: 'text', text: ' and *not em*' },
    ]);
  });

  it('does not nest links', () => {
    const out = parseInline('[a [b](https://b.test) c](https://a.test)');
    expect(out).toEqual([
      { t: 'link', href: 'https://a.test/', children: [{ t: 'text', text: 'a [b](https://b.test) c' }] },
    ]);
  });
});

describe('safeHref', () => {
  it('accepts absolute http(s) only', () => {
    expect(safeHref('https://example.test/x?y#z')).toBe('https://example.test/x?y#z');
    expect(safeHref('HTTP://EXAMPLE.TEST')).toBe('http://example.test/');
    expect(safeHref('javascript:alert(1)')).toBeNull();
    expect(safeHref('example.test')).toBeNull();
  });
});
