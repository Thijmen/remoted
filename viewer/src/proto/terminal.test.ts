import { describe, expect, it } from 'vitest';
import { clampSize, describeExit, describeTermError, parseTermAgentMsg } from './terminal';

describe('parseTermAgentMsg', () => {
  it('decodes the hello, ignoring unknown fields', () => {
    const m = parseTermAgentMsg(
      JSON.stringify({
        t: 'hello',
        proto: 1,
        id: 'ab'.repeat(16),
        os: 'macos',
        shell: 'zsh',
        offset: 48213,
        reset: false,
        state: { locked: true, in_session: true },
        later: 1,
      }),
    );
    expect(m).toEqual({
      t: 'hello',
      proto: 1,
      id: 'ab'.repeat(16),
      os: 'macos',
      shell: 'zsh',
      offset: 48213,
      reset: false,
      end: null,
      state: {
        locked: true,
        perms: { screen: false, input: false },
        in_session: true,
        secure: 'unavailable',
        disconnected: false,
      },
    });
    expect(parseTermAgentMsg('{"t":"hello","id":"x","offset":-3,"reset":true,"end":9}')).toMatchObject({
      offset: 0,
      reset: true,
      end: 9,
      state: null,
    });
    // A hello without a shell id is useless.
    expect(parseTermAgentMsg('{"t":"hello","proto":1}')).toBeNull();
  });

  it('decodes state (flattened), exit, detached and err', () => {
    expect(parseTermAgentMsg('{"t":"state","locked":false,"in_session":true}')).toMatchObject({
      t: 'state',
      state: { locked: false, in_session: true },
    });
    expect(parseTermAgentMsg('{"t":"exit","code":0}')).toEqual({ t: 'exit', code: 0, signal: null });
    expect(parseTermAgentMsg('{"t":"exit","signal":9}')).toEqual({ t: 'exit', code: null, signal: 9 });
    expect(parseTermAgentMsg('{"t":"detached","reason":"taken_over"}')).toEqual({
      t: 'detached',
      reason: 'taken_over',
    });
    expect(parseTermAgentMsg('{"t":"err","code":"not_found"}')).toEqual({ t: 'err', code: 'not_found', msg: '' });
  });

  it('ignores unknown types and junk', () => {
    expect(parseTermAgentMsg('{"t":"later"}')).toBeNull();
    expect(parseTermAgentMsg('[1]')).toBeNull();
    expect(parseTermAgentMsg('not json')).toBeNull();
  });
});

describe('helpers', () => {
  it('clamps sizes like the agent', () => {
    expect(clampSize(0)).toBe(2);
    expect(clampSize(120.4)).toBe(120);
    expect(clampSize(5000)).toBe(1000);
    expect(clampSize(Number.NaN)).toBe(80);
  });

  it('describes errors and exits', () => {
    expect(describeTermError('spawn', 'no such file')).toBe('The shell could not be started: no such file');
    expect(describeTermError('weird', '')).toBe('The agent refused the terminal (weird).');
    expect(describeExit(0, null)).toBe('Process exited with code 0');
    expect(describeExit(null, 15)).toBe('Process ended by signal 15');
    expect(describeExit(null, null)).toBe('Process exited');
  });
});
