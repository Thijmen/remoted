import { describe, expect, it } from 'vitest';
import { ScrollPhase } from '../proto/bin';
import { LONG_PRESS_MS, TAP_SLOP_PX, TouchGestures } from './touch';

const at = (id: number, x: number, y: number) => ({ id, x, y });

describe('TouchGestures', () => {
  it('turns a tap into a left click at the point it went down', () => {
    const g = new TouchGestures();
    expect(g.down(at(1, 100, 200), 0)).toEqual([]);
    expect(g.move(at(1, 102, 203))).toEqual([]);
    expect(g.up(at(1, 102, 203))).toEqual([{ type: 'tap', button: 'left', x: 100, y: 200 }]);
  });

  it('treats movement within the slop as a tap', () => {
    const g = new TouchGestures();
    g.down(at(1, 0, 0), 0);
    expect(g.move(at(1, TAP_SLOP_PX, 0))).toEqual([]);
    expect(g.up(at(1, TAP_SLOP_PX, 0))).toEqual([{ type: 'tap', button: 'left', x: 0, y: 0 }]);
  });

  it('holds the left button down for a drag and releases it without a click', () => {
    const g = new TouchGestures();
    g.down(at(1, 10, 10), 0);
    expect(g.move(at(1, 40, 30))).toEqual([
      { type: 'press', x: 10, y: 10 },
      { type: 'move', x: 40, y: 30 },
    ]);
    expect(g.move(at(1, 60, 40))).toEqual([{ type: 'move', x: 60, y: 40 }]);
    expect(g.up(at(1, 60, 40))).toEqual([{ type: 'release' }]);
  });

  it('turns a still finger into a right click after the long press delay', () => {
    const g = new TouchGestures();
    g.down(at(1, 30, 40), 1000);
    expect(g.tick(1000 + LONG_PRESS_MS - 1)).toEqual([]);
    expect(g.tick(1000 + LONG_PRESS_MS)).toEqual([{ type: 'tap', button: 'right', x: 30, y: 40 }]);
    expect(g.up(at(1, 30, 40))).toEqual([]);
  });

  it('does not drag after a long press', () => {
    const g = new TouchGestures();
    g.down(at(1, 0, 0), 0);
    expect(g.tick(LONG_PRESS_MS)[0]).toMatchObject({ button: 'right' });
    expect(g.move(at(1, 80, 80))).toEqual([]);
    expect(g.up(at(1, 80, 80))).toEqual([]);
  });

  it('scrolls with two fingers, content following them, with Began/Changed/Ended', () => {
    const g = new TouchGestures();
    g.down(at(1, 100, 100), 0);
    expect(g.down(at(2, 200, 100), 0)).toEqual([]);
    // Fingers move up: the remote content moves up, i.e. scrolls down.
    expect(g.move(at(1, 100, 90))).toEqual([{ type: 'scroll', dx: 0, dy: 5, phase: ScrollPhase.Began }]);
    expect(g.move(at(2, 200, 90))).toEqual([{ type: 'scroll', dx: 0, dy: 5, phase: ScrollPhase.Changed }]);
    expect(g.up(at(1, 100, 90))).toEqual([{ type: 'scroll', dx: 0, dy: 0, phase: ScrollPhase.Ended }]);
    expect(g.up(at(2, 200, 90))).toEqual([]);
  });

  it('does not click when a second finger lands on a tap in progress', () => {
    const g = new TouchGestures();
    g.down(at(1, 10, 10), 0);
    expect(g.down(at(2, 30, 10), 100)).toEqual([]);
    expect(g.up(at(1, 10, 10))).toEqual([]);
    expect(g.up(at(2, 30, 10))).toEqual([]);
  });

  it('lets go of a drag when a second finger starts a scroll', () => {
    const g = new TouchGestures();
    g.down(at(1, 0, 0), 0);
    g.move(at(1, 40, 0));
    expect(g.down(at(2, 40, 40), 100)).toEqual([{ type: 'release' }]);
    expect(g.up(at(1, 40, 0))).toEqual([]);
    expect(g.up(at(2, 40, 40))).toEqual([]);
  });

  it('ignores a third finger while scrolling', () => {
    const g = new TouchGestures();
    g.down(at(1, 100, 100), 0);
    g.down(at(2, 200, 100), 0);
    expect(g.down(at(3, 300, 300), 0)).toEqual([]);
    expect(g.move(at(3, 400, 400))).toEqual([]);
    // Still the centroid of the first two.
    expect(g.move(at(1, 100, 80))).toMatchObject([{ type: 'scroll', dy: 10 }]);
  });

  it('carries sub-pixel deltas so a slow drag still scrolls', () => {
    const g = new TouchGestures();
    g.down(at(1, 0, 0), 0);
    g.down(at(2, 10, 0), 0);
    expect(g.move(at(1, 0, -0.8))).toEqual([]);
    expect(g.move(at(1, 0, -1.6))).toEqual([]);
    expect(g.move(at(1, 0, -2.4))).toMatchObject([{ type: 'scroll', dy: 1 }]);
  });

  it('releases a held button and ends a scroll on cancel', () => {
    const dragging = new TouchGestures();
    dragging.down(at(1, 0, 0), 0);
    dragging.move(at(1, 40, 0));
    expect(dragging.cancel()).toEqual([{ type: 'release' }]);

    const scrolling = new TouchGestures();
    scrolling.down(at(1, 0, 0), 0);
    scrolling.down(at(2, 10, 0), 0);
    scrolling.move(at(1, 0, -10));
    expect(scrolling.cancel()).toEqual([{ type: 'scroll', dx: 0, dy: 0, phase: ScrollPhase.Ended }]);
  });

  it('does not long press once a drag started', () => {
    const g = new TouchGestures();
    g.down(at(1, 0, 0), 0);
    g.move(at(1, 40, 0));
    expect(g.tick(LONG_PRESS_MS * 2)).toEqual([]);
  });
});
