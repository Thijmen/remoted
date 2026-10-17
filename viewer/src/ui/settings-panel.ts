import { CHORD_LABEL } from '../input/chords';
import { h } from './dom';
import { MAX_KBPS_LIMIT, MIN_KBPS, PRESETS, type Settings } from './settings';

type Option<T> = { value: T; label: string };

function select<T extends string | number>(
  id: string,
  options: ReadonlyArray<Option<T>>,
  current: T,
  onChange: (v: T) => void,
): HTMLSelectElement {
  const el = h(
    'select',
    { id },
    ...options.map((o) => h('option', { value: String(o.value), selected: o.value === current }, o.label)),
  );
  el.addEventListener('change', () => {
    const o = options.find((x) => String(x.value) === el.value);
    if (o) onChange(o.value);
  });
  return el;
}

function checkbox(id: string, checked: boolean, onChange: (v: boolean) => void): HTMLInputElement {
  const el = h('input', { id, type: 'checkbox', checked });
  el.addEventListener('change', () => onChange(el.checked));
  return el;
}

/**
 * Settings form. Every change calls `onChange` with the complete new settings. `idPrefix` keeps
 * element ids unique when more than one panel exists on the page.
 */
export function settingsPanel(initial: Settings, onChange: (s: Settings) => void, idPrefix = 'set'): HTMLElement {
  let s = { ...initial };
  const id = (name: string) => `${idPrefix}-${name}`;
  const update = <K extends keyof Settings>(k: K, v: Settings[K]) => {
    s = { ...s, [k]: v };
    onChange(s);
  };

  const kbps = h('input', {
    id: id('kbps'),
    type: 'number',
    min: String(MIN_KBPS),
    max: String(MAX_KBPS_LIMIT),
    step: '1000',
    value: String(s.maxKbps),
  });
  kbps.addEventListener('change', () => {
    const v = Number(kbps.value);
    if (Number.isFinite(v) && v >= MIN_KBPS && v <= MAX_KBPS_LIMIT) update('maxKbps', Math.round(v));
    else kbps.value = String(s.maxKbps);
  });

  const volume = h('input', {
    id: id('volume'),
    type: 'range',
    min: '0',
    max: '1',
    step: '0.05',
    value: String(s.volume),
  });
  volume.addEventListener('input', () => update('volume', Number(volume.value)));

  const field = (forId: string, label: string, control: HTMLElement) => [
    h('label', { htmlFor: forId }, label),
    control,
  ];

  return h(
    'div',
    { className: 'settings' },
    ...field(
      id('preset'),
      'Resolution',
      select(id('preset'), PRESETS, s.preset, (v) => update('preset', v)),
    ),
    ...field(
      id('fps'),
      'Frame rate',
      select(
        id('fps'),
        [
          { value: 120, label: '120 fps (only visible on a 120 Hz screen)' },
          { value: 60, label: '60 fps' },
          { value: 30, label: '30 fps' },
        ] as const,
        s.fps,
        (v) => update('fps', v),
      ),
    ),
    ...field(id('kbps'), 'Max bitrate (kbps)', kbps),
    ...field(
      id('adaptive'),
      'Adaptive bitrate',
      checkbox(id('adaptive'), s.adaptive, (v) => update('adaptive', v)),
    ),
    ...field(
      id('audio'),
      'Audio',
      checkbox(id('audio'), s.audio, (v) => update('audio', v)),
    ),
    ...field(id('volume'), 'Volume', volume),
    ...field(
      id('render'),
      'Render mode',
      select(
        id('render'),
        [
          { value: 'immediate', label: 'Immediate (lowest latency)' },
          { value: 'raf', label: 'Latest frame per display refresh' },
        ] as const,
        s.renderMode,
        (v) => update('renderMode', v),
      ),
    ),
    ...field(
      id('invert'),
      'Invert scrolling',
      checkbox(id('invert'), s.invertScroll, (v) => update('invertScroll', v)),
    ),
    ...field(
      id('mapping'),
      'Modifier keys',
      select(
        id('mapping'),
        [
          { value: 'windows-to-mac', label: 'Windows keyboard → Mac (Ctrl→Cmd, Win→Ctrl)' },
          { value: 'passthrough', label: 'Pass through' },
        ] as const,
        s.mapping,
        (v) => update('mapping', v),
      ),
    ),
    h('span', { className: 'muted' }, 'Shortcuts'),
    h(
      'span',
      { className: 'muted' },
      `${CHORD_LABEL} + F fullscreen · L pointer lock · H HUD · T latency test · D disconnect`,
    ),
  );
}
