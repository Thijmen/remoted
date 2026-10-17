// Key test, opened ON the host: live log of keyboard and mouse events, to verify injected input.
import '../ui/style.css';
import { byId, h, setChildren } from '../ui/dom';

const MAX_ROWS = 500;

const body = h('tbody');
const capture = h('input', { type: 'checkbox', checked: true, id: 'capture' });
const logMoves = h('input', { type: 'checkbox', checked: false, id: 'moves' });
const held = h('span', { className: 'mono' });
const pressed = new Set<string>();

byId('app').append(
  h(
    'main',
    { className: 'page stack' },
    h('h1', null, 'Key test'),
    h(
      'div',
      { className: 'row' },
      capture,
      h('label', { htmlFor: 'capture' }, 'Prevent browser shortcuts'),
      logMoves,
      h('label', { htmlFor: 'moves' }, 'Log mouse moves'),
      h('button', { type: 'button', onclick: () => setChildren(body) }, 'Clear'),
    ),
    h('div', null, h('span', { className: 'muted' }, 'Held: '), held),
    h(
      'div',
      { className: 'card log' },
      h(
        'table',
        null,
        h(
          'thead',
          null,
          h(
            'tr',
            null,
            ...['time', 'event', 'code', 'key', 'loc', 'rep', 'mods', 'detail'].map((t) => h('th', null, t)),
          ),
        ),
        body,
      ),
    ),
  ),
);

function mods(e: KeyboardEvent | MouseEvent): string {
  return [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Meta'].filter(Boolean).join('+');
}

function log(
  type: string,
  cells: { code?: string; key?: string; loc?: string; rep?: string; mods?: string; detail?: string },
): void {
  const row = h(
    'tr',
    null,
    ...[performance.now().toFixed(1), type, cells.code, cells.key, cells.loc, cells.rep, cells.mods, cells.detail].map(
      (c) => h('td', null, c ?? ''),
    ),
  );
  body.prepend(row);
  while (body.childElementCount > MAX_ROWS) body.lastElementChild?.remove();
}

function onKey(e: KeyboardEvent): void {
  if (capture.checked && e.target !== capture && e.target !== logMoves) e.preventDefault();
  if (e.type === 'keydown') pressed.add(e.code);
  else pressed.delete(e.code);
  held.textContent = [...pressed].join(' ');
  log(e.type, {
    code: e.code,
    key: e.key === ' ' ? 'Space' : e.key,
    loc: String(e.location),
    rep: e.repeat ? 'yes' : '',
    mods: mods(e),
    detail: e.isComposing ? 'composing' : '',
  });
}

window.addEventListener('keydown', onKey);
window.addEventListener('keyup', onKey);
window.addEventListener('blur', () => {
  pressed.clear();
  held.textContent = '';
  log('blur', {});
});

for (const type of ['mousedown', 'mouseup', 'click', 'dblclick', 'auxclick', 'contextmenu'] as const) {
  window.addEventListener(type, (e) => {
    if (type === 'contextmenu' && capture.checked) e.preventDefault();
    log(type, {
      mods: mods(e),
      detail: `button ${e.button} buttons ${e.buttons} detail ${e.detail} @ ${e.clientX},${e.clientY}`,
    });
  });
}

window.addEventListener('mousemove', (e) => {
  if (logMoves.checked)
    log('mousemove', {
      mods: mods(e),
      detail: `${e.clientX},${e.clientY} Δ ${e.movementX},${e.movementY} buttons ${e.buttons}`,
    });
});

window.addEventListener(
  'wheel',
  (e) => {
    log('wheel', { mods: mods(e), detail: `Δ ${e.deltaX.toFixed(2)},${e.deltaY.toFixed(2)} mode ${e.deltaMode}` });
  },
  { passive: true },
);
