import type { MappingSetting, PlatformHint } from '../input/modifiers';
import type { DisplayDesc, StreamParams } from '../proto/control';
import type { RenderMode } from '../video/pipeline';

export type ResolutionPreset = 'fit' | '1728x1118' | '2592x1676' | 'native' | '1280x828';

export const PRESETS: ReadonlyArray<{ value: ResolutionPreset; label: string }> = [
  { value: 'fit', label: 'Fit to window' },
  { value: '1728x1118', label: '1728×1118 (default)' },
  { value: '2592x1676', label: '2592×1676' },
  { value: 'native', label: 'Native' },
  { value: '1280x828', label: '1280×828 (gaming)' },
];

export interface Settings {
  preset: ResolutionPreset;
  fps: 30 | 60 | 120;
  maxKbps: number;
  adaptive: boolean;
  audio: boolean;
  /** 0..1 */
  volume: number;
  renderMode: RenderMode;
  invertScroll: boolean;
  /** Resolved per session with `resolveMapping`. */
  mapping: MappingSetting;
}

export const MIN_KBPS = 1000;
export const START_KBPS = 8000;
export const MAX_KBPS_LIMIT = 100_000;

export function defaultSettings(_nav: PlatformHint): Settings {
  return {
    preset: '1728x1118',
    fps: 60,
    maxKbps: 20_000,
    adaptive: true,
    audio: true,
    volume: 1,
    renderMode: 'immediate',
    invertScroll: false,
    mapping: 'auto',
  };
}

const storageKey = (machine: string) => `rd.settings.${machine}`;

function pick<T>(v: unknown, allowed: readonly T[], def: T): T {
  return allowed.includes(v as T) ? (v as T) : def;
}

/** Merge stored values over defaults, field by field, ignoring anything invalid. */
export function sanitizeSettings(raw: unknown, defaults: Settings): Settings {
  const o = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : {};
  const bool = (k: keyof Settings) => (typeof o[k] === 'boolean' ? (o[k] as boolean) : (defaults[k] as boolean));
  const kbps = typeof o.maxKbps === 'number' && Number.isFinite(o.maxKbps) ? o.maxKbps : defaults.maxKbps;
  const volume = typeof o.volume === 'number' && Number.isFinite(o.volume) ? o.volume : defaults.volume;
  return {
    preset: pick(
      o.preset,
      PRESETS.map((p) => p.value),
      defaults.preset,
    ),
    fps: pick(o.fps, [30, 60, 120] as const, defaults.fps),
    maxKbps: Math.round(Math.min(MAX_KBPS_LIMIT, Math.max(MIN_KBPS, kbps))),
    adaptive: bool('adaptive'),
    audio: bool('audio'),
    volume: Math.min(1, Math.max(0, volume)),
    renderMode: pick(o.renderMode, ['immediate', 'raf'] as const, defaults.renderMode),
    invertScroll: bool('invertScroll'),
    mapping: pick(o.mapping, ['auto', 'windows-to-mac', 'mac-to-windows', 'passthrough'] as const, defaults.mapping),
  };
}

export function loadSettings(machine: string, nav: PlatformHint = navigator): Settings {
  const defaults = defaultSettings(nav);
  try {
    const raw = localStorage.getItem(storageKey(machine));
    return sanitizeSettings(raw ? JSON.parse(raw) : null, defaults);
  } catch {
    return defaults;
  }
}

export function saveSettings(machine: string, s: Settings): void {
  try {
    localStorage.setItem(storageKey(machine), JSON.stringify(s));
  } catch {
    // Storage unavailable (private mode / blocked): settings last for this page only.
  }
}

const even = (v: number) => Math.max(2, Math.round(v / 2) * 2);

export interface Viewport {
  width: number;
  height: number;
  dpr: number;
}

/** Output video size for a preset, given the agent's display and the viewer's window. */
export function resolveSize(
  preset: ResolutionPreset,
  display: DisplayDesc | undefined,
  viewport: Viewport,
): { width: number; height: number } {
  const nativeW = display ? display.w_pt * display.scale : 3456;
  const nativeH = display ? display.h_pt * display.scale : 2234;
  switch (preset) {
    case 'native':
      return { width: even(nativeW), height: even(nativeH) };
    case 'fit': {
      const aspect = nativeW / nativeH;
      const vw = viewport.width * viewport.dpr;
      const vh = viewport.height * viewport.dpr;
      const width = Math.min(vw, vh * aspect, nativeW);
      return { width: even(Math.max(320, width)), height: even(Math.max(240, width / aspect)) };
    }
    default: {
      const [w, h] = preset.split('x').map(Number);
      return { width: w!, height: h! };
    }
  }
}

export function streamParams(s: Settings, size: { width: number; height: number }, display = 0): StreamParams {
  return {
    width: size.width,
    height: size.height,
    fps: s.fps,
    codec: 'avc',
    start_kbps: Math.min(START_KBPS, s.maxKbps),
    min_kbps: Math.min(MIN_KBPS, s.maxKbps),
    max_kbps: s.maxKbps,
    adaptive: s.adaptive,
    audio: s.audio,
    display,
  };
}
