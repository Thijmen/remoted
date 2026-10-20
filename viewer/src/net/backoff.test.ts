import { describe, expect, it } from 'vitest';
import { Backoff, closeAction } from './backoff';

describe('Backoff', () => {
  it('follows 0.5 → 1 → 2 → 4 → 10 s and stays at 10 s', () => {
    const b = new Backoff();
    expect([b.next(), b.next(), b.next(), b.next(), b.next(), b.next(), b.next()]).toEqual([
      500, 1000, 2000, 4000, 10_000, 10_000, 10_000,
    ]);
    expect(b.attempts).toBe(7);
  });

  it('starts over after reset', () => {
    const b = new Backoff();
    b.next();
    b.next();
    b.reset();
    expect(b.next()).toBe(500);
  });
});

describe('closeAction', () => {
  it('never reconnects after a take-over', () => {
    expect(closeAction(4001)).toBe('stop');
  });
  it('polls when the agent is offline', () => {
    expect(closeAction(4003)).toBe('poll');
  });
  it('waits for an agent that installs an update', () => {
    expect(closeAction(4010)).toBe('update');
  });
  it('stops when retrying cannot help', () => {
    expect(closeAction(4002)).toBe('stop');
    expect(closeAction(4005)).toBe('stop');
    expect(closeAction(4008)).toBe('stop');
  });
  it('retries everything else', () => {
    for (const code of [1000, 1001, 1006, 1011, 4000, 4004, 4006, 4009]) expect(closeAction(code)).toBe('retry');
  });
});
