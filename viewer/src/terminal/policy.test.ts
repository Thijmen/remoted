import { describe, expect, it } from 'vitest';
import { CloseCode } from '../proto/control';
import { closeDecision } from './policy';

describe('closeDecision', () => {
  it('maps the close codes of docs/terminal.md', () => {
    expect(closeDecision(CloseCode.KICKED, false)).toBe('taken');
    expect(closeDecision(CloseCode.FORBIDDEN, false)).toBe('signed-out');
    expect(closeDecision(CloseCode.AGENT_OFFLINE, false)).toBe('offline');
    expect(closeDecision(CloseCode.AGENT_UPDATING, false)).toBe('offline');
    expect(closeDecision(CloseCode.PROTOCOL, false)).toBe('protocol');
    expect(closeDecision(CloseCode.IDLE_TIMEOUT, false)).toBe('idle');
    expect(closeDecision(CloseCode.UNSUPPORTED, false)).toBe('unsupported');
    expect(closeDecision(CloseCode.TRY_AGAIN_LATER, false)).toBe('busy');
  });

  it('retries everything else; a refused upgrade asks whether the user is still signed in', () => {
    for (const code of [1000, 1001, 1006, 1012, CloseCode.AGENT_TIMEOUT, 4000]) {
      expect(closeDecision(code, false)).toBe('retry');
    }
    expect(closeDecision(1006, true)).toBe('probe');
    expect(closeDecision(CloseCode.AGENT_OFFLINE, true)).toBe('offline');
  });
});
