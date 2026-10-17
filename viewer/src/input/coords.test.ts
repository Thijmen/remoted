import { describe, expect, it } from 'vitest';
import { NORM_MAX, containRect, normalizePoint, toVideoPixels } from './coords';

describe('containRect', () => {
  it('pillarboxes a 16:10 video in a wide element', () => {
    const r = containRect({ left: 0, top: 0, width: 2000, height: 1000 }, 1600, 1000);
    expect(r).toEqual({ left: 200, top: 0, width: 1600, height: 1000 });
  });

  it('letterboxes in a tall element and honours the element offset', () => {
    const r = containRect({ left: 10, top: 20, width: 800, height: 1000 }, 1600, 1000);
    expect(r).toEqual({ left: 10, top: 270, width: 800, height: 500 });
  });

  it('returns the element itself for degenerate sizes', () => {
    const el = { left: 1, top: 2, width: 300, height: 200 };
    expect(containRect(el, 0, 0)).toBe(el);
  });
});

describe('normalizePoint', () => {
  // 1728x1118 video in a 1536x750 viewport: height-limited, pillarboxed.
  const el = { left: 0, top: 0, width: 1536, height: 750 };
  const vw = 1728;
  const vh = 1118;
  const content = containRect(el, vw, vh);

  it('maps the content corners to 0 and 65535', () => {
    expect(normalizePoint(content.left, content.top, el, vw, vh)).toEqual({ x: 0, y: 0 });
    expect(normalizePoint(content.left + content.width, content.top + content.height, el, vw, vh)).toEqual({
      x: NORM_MAX,
      y: NORM_MAX,
    });
  });

  it('maps the centre to the middle', () => {
    const p = normalizePoint(content.left + content.width / 2, content.top + content.height / 2, el, vw, vh);
    expect(p.x).toBe(Math.round(NORM_MAX / 2));
    expect(p.y).toBe(Math.round(NORM_MAX / 2));
  });

  it('clamps points over the letterbox bars to the nearest edge', () => {
    expect(normalizePoint(5, 375, el, vw, vh).x).toBe(0);
    expect(normalizePoint(1530, 375, el, vw, vh).x).toBe(NORM_MAX);
    expect(normalizePoint(content.left + 10, -50, el, vw, vh).y).toBe(0);
    expect(normalizePoint(content.left + 10, 9999, el, vw, vh).y).toBe(NORM_MAX);
  });

  it('round-trips to video pixels', () => {
    const p = normalizePoint(content.left + content.width * 0.25, content.top + content.height * 0.75, el, vw, vh);
    const px = toVideoPixels(p.x, p.y, vw, vh);
    expect(px.x).toBeCloseTo((vw - 1) * 0.25, 0);
    expect(px.y).toBeCloseTo((vh - 1) * 0.75, 0);
  });
});
