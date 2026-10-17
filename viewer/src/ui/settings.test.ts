import { describe, expect, it } from 'vitest';
import { defaultSettings, resolveSize, sanitizeSettings, streamParams } from './settings';

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
  it('defaults the modifier mapping from the platform', () => {
    expect(d.mapping).toBe('windows-to-mac');
    expect(defaultSettings({ platform: 'MacIntel' }).mapping).toBe('passthrough');
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
