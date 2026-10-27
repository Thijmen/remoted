import { describe, expect, it } from 'vitest';
import { defaultSettings, displaySize, pickDisplay, resolveSize, sanitizeSettings, streamParams } from './settings';

const mac = { id: 0, w_pt: 1728, h_pt: 1117, scale: 2 };
const win = { platform: 'Win32' };

describe('resolveSize', () => {
  const vp = { width: 1536, height: 750, dpr: 1.25 };
  it('uses fixed presets as-is', () => {
    expect(resolveSize('1728x1118', mac, vp)).toEqual({ width: 1728, height: 1118 });
    expect(resolveSize('1280x828', mac, vp)).toEqual({ width: 1280, height: 828 });
  });
  it('native = points × scale, even', () => {
    expect(resolveSize('native', mac, vp)).toEqual({ width: 3456, height: 2234 });
  });
  it('fit matches the viewport in device pixels with the display aspect', () => {
    const s = resolveSize('fit', mac, vp);
    expect(s.height).toBeLessThanOrEqual(938);
    expect(s.width % 2).toBe(0);
    expect(s.height % 2).toBe(0);
    expect(s.width / s.height).toBeCloseTo(1728 / 1117, 2);
  });
  it('fit never exceeds native', () => {
    expect(resolveSize('fit', mac, { width: 5000, height: 4000, dpr: 2 })).toEqual({ width: 3456, height: 2234 });
  });
});

describe('sanitizeSettings', () => {
  const d = defaultSettings(win);
  it('defaults the modifier mapping to auto (resolved per session) and keeps explicit ones', () => {
    expect(d.mapping).toBe('auto');
    expect(defaultSettings({ platform: 'MacIntel' }).mapping).toBe('auto');
    expect(sanitizeSettings({ mapping: 'windows-to-mac' }, d).mapping).toBe('windows-to-mac');
    expect(sanitizeSettings({ mapping: 'mac-to-windows' }, d).mapping).toBe('mac-to-windows');
    expect(sanitizeSettings({ mapping: 'sideways' }, d).mapping).toBe('auto');
  });
  it('keeps valid fields and replaces invalid ones', () => {
    const s = sanitizeSettings(
      { preset: 'native', fps: 45, maxKbps: 5e9, audio: 'yes', renderMode: 'raf', volume: -1 },
      d,
    );
    expect(s).toMatchObject({
      preset: 'native',
      fps: 60,
      maxKbps: 100_000,
      audio: d.audio,
      renderMode: 'raf',
      volume: 0,
    });
  });
  it('accepts 30, 60 and 120 fps (default 60)', () => {
    expect(d.fps).toBe(60);
    for (const fps of [30, 60, 120]) expect(sanitizeSettings({ fps }, d).fps).toBe(fps);
    expect(sanitizeSettings({ fps: 90 }, d).fps).toBe(60);
  });
  it('survives garbage', () => {
    expect(sanitizeSettings('nope', d)).toEqual(d);
    expect(sanitizeSettings(null, d)).toEqual(d);
  });
});

describe('streamParams', () => {
  it('builds a start message body', () => {
    const p = streamParams({ ...defaultSettings(win), maxKbps: 5000 }, { width: 1728, height: 1118 });
    expect(p).toEqual({
      width: 1728,
      height: 1118,
      fps: 60,
      codec: 'avc',
      start_kbps: 5000,
      min_kbps: 1000,
      max_kbps: 5000,
      adaptive: true,
      audio: true,
      display: 0,
    });
  });
});

describe('displays', () => {
  const list = [
    { id: 0, w_pt: 1920, h_pt: 1080, scale: 1.5 },
    { id: 2, w_pt: 1920, h_pt: 1080, scale: 1 },
  ];
  it('keeps a valid stored display id and drops an invalid one', () => {
    const d = defaultSettings(win);
    expect(d.display).toBe(0);
    expect(sanitizeSettings({ display: 2 }, d).display).toBe(2);
    expect(sanitizeSettings({ display: -1 }, d).display).toBe(0);
    expect(sanitizeSettings({ display: 1.5 }, d).display).toBe(0);
    expect(sanitizeSettings({ display: '2' }, d).display).toBe(0);
  });
  it('picks the wanted display, or the main one when it is gone', () => {
    expect(pickDisplay(list, 2)?.id).toBe(2);
    expect(pickDisplay(list, 7)?.id).toBe(0);
    expect(pickDisplay([], 0)).toBeUndefined();
  });
  it('labels a display with its pixel size', () => {
    expect(displaySize(list[0]!)).toBe('2880×1620');
  });
});

describe('touch defaults', () => {
  it('starts a phone session at 30 fps and 10 Mbps', () => {
    const d = defaultSettings(win, true);
    expect(d.fps).toBe(30);
    expect(d.maxKbps).toBe(10_000);
    // The desktop default is unchanged.
    expect(defaultSettings(win).fps).toBe(60);
    expect(defaultSettings(win).maxKbps).toBe(20_000);
  });
});
