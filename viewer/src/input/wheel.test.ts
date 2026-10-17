import { describe, expect, it } from 'vitest';
import { ScrollPhase, WheelMode } from '../proto/bin';
import { GESTURE_END_MS, LINES_PER_NOTCH, LINES_PER_PAGE, WheelNormalizer } from './wheel';

const PIXEL = 0;
const LINE = 1;
const PAGE = 2;

describe('WheelNormalizer', () => {
  it('passes DOM line deltas through as Line mode', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 3, deltaMode: LINE }, 0);
    expect(w.flush(0)).toEqual([{ mode: WheelMode.Line, dx: 0, dy: 3, phase: ScrollPhase.None }]);
    expect(w.active).toBe(false);
  });

  it('turns whole-notch pixel deltas into lines', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 100, deltaMode: PIXEL }, 0);
    w.push({ deltaX: 0, deltaY: -240, deltaMode: PIXEL }, 1);
    expect(w.flush(2)).toEqual([
      { mode: WheelMode.Line, dx: 0, dy: (1 - 2) * LINES_PER_NOTCH, phase: ScrollPhase.None },
    ]);
  });

  it('detects notches scaled by the device pixel ratio', () => {
    const w = new WheelNormalizer(1.25);
    w.push({ deltaX: 0, deltaY: 80, deltaMode: PIXEL }, 0);
    expect(w.flush(0)[0]).toMatchObject({ mode: WheelMode.Line, dy: LINES_PER_NOTCH });
  });

  it('converts pages to lines', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 1, deltaMode: PAGE }, 0);
    expect(w.flush(0)[0]).toMatchObject({ mode: WheelMode.Line, dy: LINES_PER_PAGE });
  });

  it('keeps fractional line remainders', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 0.6, deltaMode: LINE }, 0);
    expect(w.flush(0)).toEqual([]);
    w.push({ deltaX: 0, deltaY: 0.6, deltaMode: LINE }, 1);
    expect(w.flush(1)).toEqual([{ mode: WheelMode.Line, dx: 0, dy: 1, phase: ScrollPhase.None }]);
  });

  it('sends smooth deltas as Pixel with Began/Changed/Ended phases', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0.5, deltaY: 4.5, deltaMode: PIXEL }, 0);
    expect(w.flush(0)).toEqual([{ mode: WheelMode.Pixel, dx: 0, dy: 4, phase: ScrollPhase.Began }]);
    w.push({ deltaX: 0.7, deltaY: 2.7, deltaMode: PIXEL }, 16);
    // Remainders 0.5 + 0.7 and 0.5 + 2.7 carry over.
    expect(w.flush(16)).toEqual([{ mode: WheelMode.Pixel, dx: 1, dy: 3, phase: ScrollPhase.Changed }]);
    expect(w.flush(16 + GESTURE_END_MS - 1)).toEqual([]);
    expect(w.active).toBe(true);
    expect(w.flush(16 + GESTURE_END_MS)).toEqual([{ mode: WheelMode.Pixel, dx: 0, dy: 0, phase: ScrollPhase.Ended }]);
    expect(w.active).toBe(false);
  });

  it('stays in pixel mode for a round delta during a smooth gesture', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 7, deltaMode: PIXEL }, 0);
    w.flush(0);
    w.push({ deltaX: 0, deltaY: 100, deltaMode: PIXEL }, 10);
    expect(w.flush(10)).toEqual([{ mode: WheelMode.Pixel, dx: 0, dy: 100, phase: ScrollPhase.Changed }]);
  });

  it('does not treat absurd round deltas as notches', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 12_000, deltaMode: PIXEL }, 0);
    expect(w.flush(0)[0]).toMatchObject({ mode: WheelMode.Pixel, dy: 12_000 });
  });

  it('inverts when asked', () => {
    const w = new WheelNormalizer(1, true);
    w.push({ deltaX: 2, deltaY: -3, deltaMode: LINE }, 0);
    expect(w.flush(0)).toEqual([{ mode: WheelMode.Line, dx: -2, dy: 3, phase: ScrollPhase.None }]);
  });

  it('clamps huge deltas to i16', () => {
    const w = new WheelNormalizer();
    w.push({ deltaX: 0, deltaY: 40_000.7, deltaMode: PIXEL }, 0);
    expect(w.flush(0)[0]).toMatchObject({ mode: WheelMode.Pixel, dy: 32767, phase: ScrollPhase.Began });
    // The clamped-off rest is sent on the next flush, not lost.
    expect(w.flush(1)[0]).toMatchObject({ mode: WheelMode.Pixel, dy: 40_000 - 32767, phase: ScrollPhase.Changed });
  });
});
