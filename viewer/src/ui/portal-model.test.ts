import { describe, expect, it } from 'vitest';
import type { MachineInfo, UpdateStatus } from '../proto/control';
import {
  actionUrl,
  actions,
  filterCounts,
  matchesQuery,
  moveSelection,
  osFamily,
  osShort,
  primaryAction,
  shortUser,
  statusView,
  summarize,
} from './portal-model';

const NOW = 1_800_000_000_000;

function update(version: string, over: Partial<UpdateStatus> = {}): UpdateStatus {
  return {
    available: { version, notes: '', url: '' },
    can_install: true,
    cannot_install: '',
    phase: 'idle',
    error: '',
    checked_ms: NOW,
    last: null,
    ...over,
  };
}

function machine(over: Partial<MachineInfo> = {}): MachineInfo {
  return {
    machine: 'mbp',
    name: 'MacBook',
    managed: false,
    online: true,
    since_ms: NOW - 60_000,
    os: 'macOS 26.3 (25D125)',
    agent_version: '1.8.0',
    state: null,
    viewer: null,
    recovery: [],
    update: null,
    files: true,
    terminal: true,
    screen: true,
    ...over,
  };
}

describe('summarize', () => {
  it('counts online machines, sessions and updates', () => {
    const s = summarize([
      machine({ machine: 'a', update: update('1.10.0') }),
      machine({ machine: 'b', online: false }),
      machine({ machine: 'c', viewer: { user: 'me@example.test', since_ms: NOW } }),
    ]);
    expect(s.total).toBe(3);
    expect(s.online).toBe(2);
    expect(s.inUse.map((m) => m.machine)).toEqual(['c']);
    expect(s.updates).toBe(1);
  });

  it('finds the newest version among running and offered ones, numerically', () => {
    expect(summarize([machine({ agent_version: '1.9.0', update: update('1.10.0') })]).newest).toBe('1.10.0');
    expect(summarize([machine({ agent_version: '1.11.0' }), machine({ update: update('1.9.1') })]).newest).toBe(
      '1.11.0',
    );
    expect(summarize([]).newest).toBe('');
  });

  it('groups running versions newest first, unknown last', () => {
    const s = summarize([
      machine({ agent_version: '1.8.0' }),
      machine({ agent_version: '' }),
      machine({ agent_version: '1.11.0' }),
      machine({ agent_version: '1.8.0' }),
      machine({ agent_version: '1.9.1' }),
    ]);
    expect(s.versions).toEqual([
      { version: '1.11.0', count: 1 },
      { version: '1.9.1', count: 1 },
      { version: '1.8.0', count: 2 },
      { version: '', count: 1 },
    ]);
  });
});

describe('filterCounts', () => {
  it('counts each filter', () => {
    expect(
      filterCounts([
        machine({ update: update('2.0.0') }),
        machine({ online: false }),
        machine({ viewer: { user: 'u', since_ms: NOW } }),
      ]),
    ).toEqual({ all: 3, online: 2, offline: 1, update: 1, 'in-use': 1 });
  });
});

describe('statusView', () => {
  it('prefers updating, then offline, then a session, then a lock', () => {
    expect(statusView(machine({ update: update('2.0.0', { phase: 'installing' }), online: false })).label).toBe(
      'Updating',
    );
    expect(statusView(machine({ online: false, viewer: { user: 'u', since_ms: NOW } }))).toMatchObject({
      level: 'bad',
      label: 'Offline',
    });
    expect(statusView(machine({ viewer: { user: 'u@x.test', since_ms: NOW - 5 } }))).toEqual({
      level: 'warn',
      label: 'In use',
      user: 'u@x.test',
      at: NOW - 5,
    });
    const locked = { locked: true, perms: { screen: true, input: true } } as MachineInfo['state'];
    expect(statusView(machine({ state: locked })).label).toBe('Locked');
    // A machine without a screen has no lock screen to report.
    expect(statusView(machine({ state: locked, screen: false })).label).toBe('Online');
  });
});

describe('actions', () => {
  it('offers what the machine has while it is online', () => {
    expect(actions(machine())).toEqual(['connect', 'terminal', 'files']);
    expect(actions(machine({ screen: false }))).toEqual(['terminal', 'files']);
    expect(actions(machine({ online: false }))).toEqual([]);
    expect(primaryAction(machine({ screen: false }))).toBe('terminal');
    expect(primaryAction(machine({ online: false }))).toBeNull();
  });

  it('links to the viewer, the terminal page and the file browser', () => {
    const m = machine({ machine: 'my box' });
    expect(actionUrl(m, 'connect')).toBe('/m/my%20box');
    expect(actionUrl(m, 'terminal')).toBe('/t/my%20box');
    expect(actionUrl(m, 'files')).toBe('/m/my%20box#files');
  });
});

describe('os', () => {
  it('knows the family', () => {
    expect(osFamily('macOS 26.3 (25D125)')).toBe('mac');
    expect(osFamily('macos')).toBe('mac');
    expect(osFamily('Windows 11 Pro 26H2 (26300.9711)')).toBe('windows');
    expect(osFamily('Ubuntu 24.04.5 LTS, Linux 6.8.0-134-generic')).toBe('linux');
    expect(osFamily('')).toBe('other');
  });

  it('drops build numbers and the kernel', () => {
    expect(osShort('Windows 11 Pro 26H2 (26300.9711)')).toBe('Windows 11 Pro 26H2');
    expect(osShort('macOS 26.3 (25D125)')).toBe('macOS 26.3');
    expect(osShort('Ubuntu 24.04.5 LTS, Linux 6.8.0-134-generic')).toBe('Ubuntu 24.04.5 LTS');
    expect(osShort('windows')).toBe('Windows');
  });
});

describe('moveSelection', () => {
  const ids = ['a', 'b', 'c'];
  it('moves and clamps', () => {
    expect(moveSelection(ids, 'a', 1)).toBe('b');
    expect(moveSelection(ids, 'c', 1)).toBe('c');
    expect(moveSelection(ids, 'a', -1)).toBe('a');
  });
  it('starts at an end without a selection, or when the selected row is filtered out', () => {
    expect(moveSelection(ids, null, 1)).toBe('a');
    expect(moveSelection(ids, null, -1)).toBe('c');
    expect(moveSelection(ids, 'gone', 1)).toBe('a');
    expect(moveSelection([], 'a', 1)).toBeNull();
  });
});

describe('text', () => {
  it('matches every word, in any order', () => {
    expect(matchesQuery('term mbp', 'Terminal on MBP Nieuw')).toBe(true);
    expect(matchesQuery('files mbp', 'Terminal on MBP Nieuw')).toBe(false);
    expect(matchesQuery('  ', 'anything')).toBe(true);
  });
  it('shortens an email', () => {
    expect(shortUser('thijmen@example.test')).toBe('thijmen');
    expect(shortUser('local')).toBe('local');
  });
});
