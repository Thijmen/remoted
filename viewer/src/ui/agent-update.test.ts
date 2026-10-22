import { describe, expect, it } from 'vitest';
import type { MachineInfo, UpdateStatus } from '../proto/control';
import {
  PENDING_MAX_MS,
  PENDING_START_MS,
  advancePending,
  compareVersions,
  pendingUpdate,
  updateCardView,
} from './agent-update';

const NOW = 1_800_000_000_000;

function status(over: Partial<UpdateStatus> = {}): UpdateStatus {
  return {
    available: { version: '1.6.0', notes: '- Faster.', url: 'https://example.test/r' },
    can_install: true,
    cannot_install: '',
    phase: 'idle',
    error: '',
    checked_ms: NOW - 1000,
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
    since_ms: NOW - 3600_000,
    os: 'macOS',
    agent_version: '1.5.0',
    state: null,
    viewer: null,
    recovery: [],
    update: status(),
    files: false,
    terminal: false,
    ...over,
  };
}

describe('updateCardView', () => {
  it('shows nothing for agents that do not report updates', () => {
    expect(updateCardView(machine({ update: null }), null, NOW)).toBeNull();
  });

  it('offers an available update, or says why it must be installed by hand', () => {
    expect(updateCardView(machine(), null, NOW)).toMatchObject({
      updating: null,
      available: { version: '1.6.0' },
      canInstall: true,
      outcome: null,
      error: '',
    });
    const byHand = machine({ update: status({ can_install: false, cannot_install: 'portable copy' }) });
    expect(updateCardView(byHand, null, NOW)).toMatchObject({ canInstall: false, cannotInstall: 'portable copy' });
  });

  it('shows an install in progress instead of the buttons, also during the restart', () => {
    const installing = status({ phase: 'installing' });
    expect(updateCardView(machine({ update: installing }), null, NOW)).toMatchObject({
      updating: '1.6.0',
      available: null,
    });
    const restarting = machine({ online: false, since_ms: NOW - 20_000, update: installing });
    expect(updateCardView(restarting, null, NOW)?.updating).toBe('1.6.0');
    // Gone for longer than an update takes: just offline.
    const gone = machine({ online: false, since_ms: NOW - 10 * 60_000, update: installing });
    expect(updateCardView(gone, null, NOW)?.updating).toBeNull();
  });

  it('shows a click as updating right away', () => {
    const m = machine();
    expect(updateCardView(m, pendingUpdate(m, '1.6.0', NOW), NOW)?.updating).toBe('1.6.0');
  });

  it('reports failures until a newer version runs, and success for a day', () => {
    const failed = status({
      available: null,
      last: { from: '1.5.0', to: '1.6.0', result: 'rolled_back', error: 'did not come back', at_ms: NOW - 60_000 },
    });
    expect(updateCardView(machine({ update: failed }), null, NOW)?.outcome).toEqual({
      level: 'warn',
      text: 'Update to 1.6.0 was rolled back: did not come back; still on 1.5.0',
      at: 0,
    });
    expect(updateCardView(machine({ update: failed, agent_version: '1.6.1' }), null, NOW)?.outcome).toBeNull();

    const ok = status({
      available: null,
      last: { from: '1.5.0', to: '1.6.0', result: 'ok', error: '', at_ms: NOW - 1000 },
    });
    expect(updateCardView(machine({ update: ok, agent_version: '1.6.0' }), null, NOW)?.outcome).toEqual({
      level: 'muted',
      text: 'Updated to 1.6.0',
      at: NOW - 1000,
    });
    expect(updateCardView(machine({ update: ok }), null, NOW + 25 * 3600_000)?.outcome).toBeNull();
  });
});

describe('advancePending', () => {
  it('follows an update through download, restart and the new version', () => {
    const m = machine();
    let p = pendingUpdate(m, '1.6.0', NOW);
    p = advancePending(p, m, NOW + 1000)!;
    expect(p).toMatchObject({ started: false });
    p = advancePending(p, machine({ update: status({ phase: 'downloading' }) }), NOW + 3000)!;
    expect(p).toMatchObject({ started: true });
    p = advancePending(p, machine({ online: false, update: status({ phase: 'installing' }) }), NOW + 8000)!;
    // Registered again, not reported yet.
    p = advancePending(p, machine({ agent_version: '1.6.0', update: null }), NOW + 15_000)!;
    expect(p).not.toBeNull();
    const back = machine({
      agent_version: '1.6.0',
      update: status({ available: null, last: { from: '1.5.0', to: '1.6.0', result: 'ok', error: '', at_ms: NOW } }),
    });
    expect(advancePending(p, back, NOW + 17_000)).toBeNull();
  });

  it('ends on a refusal, a rollback, or when nothing happens', () => {
    const m = machine({ update: status({ error: 'old error' }) });
    const p = pendingUpdate(m, '1.6.0', NOW);
    expect(advancePending(p, machine({ update: status({ error: 'version mismatch' }) }), NOW + 1000)).toBeNull();
    const rolledBack = status({
      last: { from: '1.5.0', to: '1.6.0', result: 'rolled_back', error: 'x', at_ms: NOW + 1 },
    });
    expect(advancePending({ ...p, started: true }, machine({ update: rolledBack }), NOW + 150_000)).toBeNull();
    expect(advancePending(p, m, NOW + PENDING_START_MS - 1)).not.toBeNull();
    expect(advancePending(p, m, NOW + PENDING_START_MS + 1)).toBeNull();
    const stuck = machine({ update: status({ phase: 'installing' }) });
    expect(advancePending(p, stuck, NOW + PENDING_MAX_MS + 1)).toBeNull();
  });
});

describe('compareVersions', () => {
  it('compares numerically', () => {
    expect(compareVersions('1.10.0', '1.9.2')).toBe(1);
    expect(compareVersions('1.6.0', '1.6.0')).toBe(0);
    expect(compareVersions('v1.6', '1.6.1')).toBe(-1);
    expect(compareVersions('1.6.0-beta.1', '1.6.0')).toBe(0);
  });
});
