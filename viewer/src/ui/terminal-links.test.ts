import { describe, expect, it } from 'vitest';
import { machineFromTerminalPath, terminalUrl } from './terminal-links';

describe('terminal links', () => {
  it('builds and parses /t/{machine}', () => {
    expect(terminalUrl('mbp')).toBe('/t/mbp');
    expect(terminalUrl('my pc', true)).toBe('/t/my%20pc#new');
    expect(machineFromTerminalPath({ pathname: '/t/my%20pc', search: '' })).toBe('my pc');
    expect(machineFromTerminalPath({ pathname: '/t/mbp/', search: '' })).toBe('mbp');
    expect(machineFromTerminalPath({ pathname: '/terminal.html', search: '?machine=pc' })).toBe('pc');
    expect(machineFromTerminalPath({ pathname: '/t/%E0', search: '' })).toBeNull();
    expect(machineFromTerminalPath({ pathname: '/t/', search: '' })).toBeNull();
  });
});
