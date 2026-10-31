import { describe, expect, it } from 'vitest';
import {
  type Check,
  type KeyboardStatus,
  type TroubleshootInput,
  needsAccessibility,
  parseKeyboardStatus,
  troubleshootChecks,
} from './troubleshoot';

const REFUSAL =
  'rd Desktop needs the Accessibility permission to grab the keyboard (System Settings > Privacy & Security > Accessibility)';

const mac = (accessibility: boolean, secureInput = false): KeyboardStatus => ({
  platform: 'macos',
  version: '1.6.0',
  grab: true,
  accessibility,
  secureInput,
});

function input(over: Partial<TroubleshootInput> = {}): TroubleshootInput {
  return {
    machine: 'pc',
    viewer: 'mac',
    grab: { inApp: true, grabbing: false, refusal: null },
    status: mac(true),
    mapping: 'mac-to-windows',
    remoteOs: 'windows',
    hostInput: true,
    ...over,
  };
}

const byId = (checks: Check[], id: string) => checks.find((c) => c.id === id);

describe('parseKeyboardStatus', () => {
  it('reads the app answer and rejects anything else', () => {
    expect(
      parseKeyboardStatus({
        platform: 'macos',
        version: '1.6.0',
        grab: true,
        accessibility: false,
        secureInput: false,
      }),
    ).toEqual(mac(false));
    expect(parseKeyboardStatus({ platform: 'windows', version: '1.6.0', grab: true })).toEqual({
      platform: 'windows',
      version: '1.6.0',
      grab: true,
      accessibility: null,
      secureInput: null,
    });
    expect(parseKeyboardStatus(null)).toBeNull();
    expect(parseKeyboardStatus('macos')).toBeNull();
    expect(parseKeyboardStatus({ platform: 'macos' })).toBeNull();
  });
});

describe('needsAccessibility', () => {
  const refused = { inApp: true, grabbing: false, refusal: REFUSAL };
  it('trusts a fresh status over the last refusal (granted meanwhile)', () => {
    expect(needsAccessibility(refused, mac(true))).toBe(false);
    expect(needsAccessibility({ ...refused, refusal: null }, mac(false))).toBe(true);
  });
  it('falls back to the refusal text for an app without rd_keyboard_status', () => {
    expect(needsAccessibility(refused, 'unavailable')).toBe(true);
    expect(needsAccessibility(refused, null)).toBe(true);
    expect(needsAccessibility({ ...refused, refusal: 'the rd window is not the active window' }, 'unavailable')).toBe(
      false,
    );
  });
});

describe('troubleshootChecks', () => {
  it('all well: the app, the grab, the permission, the mapping and the host', () => {
    const checks = troubleshootChecks(input());
    expect(checks.map((c) => [c.id, c.level])).toEqual([
      ['app', 'ok'],
      ['grab', 'ok'],
      ['accessibility', 'ok'],
      ['mapping', 'info'],
      ['host-input', 'ok'],
    ]);
    expect(byId(checks, 'app')?.title).toBe('rd Desktop 1.6.0');
    expect(byId(checks, 'mapping')?.detail).toMatch(/Cmd\+Tab is sent as Alt\+Tab/);
  });

  it('missing Accessibility: the refusal, the steps and the buttons', () => {
    const checks = troubleshootChecks(
      input({ grab: { inApp: true, grabbing: false, refusal: REFUSAL }, status: mac(false) }),
    );
    expect(byId(checks, 'grab')).toMatchObject({ level: 'bad', title: 'Shortcuts stay on this Mac', detail: REFUSAL });
    const acc = byId(checks, 'accessibility')!;
    expect(acc.level).toBe('bad');
    expect(acc.detail).toMatch(/remove it/);
    expect(acc.actions).toEqual(['open-accessibility', 'recheck']);
  });

  it('an older app: only the refusal tells, and nothing it cannot do is offered', () => {
    const checks = troubleshootChecks(
      input({ grab: { inApp: true, grabbing: false, refusal: REFUSAL }, status: 'unavailable' }),
    );
    expect(byId(checks, 'app')?.title).toBe('rd Desktop');
    expect(byId(checks, 'accessibility')).toMatchObject({ level: 'bad' });
    expect(byId(checks, 'accessibility')?.actions).toBeUndefined();
    expect(byId(checks, 'accessibility')?.detail).toMatch(/System Settings/);
    const fine = troubleshootChecks(input({ status: 'unavailable' }));
    expect(byId(fine, 'accessibility')).toMatchObject({ level: 'info', detail: 'Update rd Desktop to check it here.' });
    expect(byId(troubleshootChecks(input({ status: 'loading' })), 'accessibility')?.level).toBe('info');
  });

  it('Secure Keyboard Entry', () => {
    expect(byId(troubleshootChecks(input({ status: mac(true, true) })), 'secure-input')?.level).toBe('warn');
    expect(byId(troubleshootChecks(input()), 'secure-input')).toBeUndefined();
  });

  it('in a browser: no app checks, a pointer to rd Desktop', () => {
    const checks = troubleshootChecks(
      input({
        grab: { inApp: false, grabbing: false, refusal: null },
        status: null,
        viewer: 'windows',
        mapping: 'passthrough',
        remoteOs: 'windows',
      }),
    );
    expect(checks.map((c) => c.id)).toEqual(['app', 'mapping', 'host-input']);
    expect(byId(checks, 'app')).toMatchObject({ level: 'warn' });
    expect(byId(checks, 'app')?.detail).toMatch(/Alt\+Tab/);
  });

  it('Windows app: no Accessibility check', () => {
    const win: KeyboardStatus = {
      platform: 'windows',
      version: '1.6.0',
      grab: true,
      accessibility: null,
      secureInput: null,
    };
    const checks = troubleshootChecks(
      input({ viewer: 'windows', status: win, mapping: 'windows-to-mac', remoteOs: 'macos' }),
    );
    expect(byId(checks, 'accessibility')).toBeUndefined();
    expect(byId(checks, 'grab')?.detail).toMatch(/Alt\+Tab/);
    expect(byId(checks, 'mapping')?.detail).toMatch(/Alt\+Tab is sent as Cmd\+Tab/);
  });

  it('pass-through between a Mac and Windows warns that the app switcher will not map', () => {
    expect(byId(troubleshootChecks(input({ mapping: 'passthrough' })), 'mapping')).toMatchObject({ level: 'warn' });
    expect(byId(troubleshootChecks(input({ mapping: 'passthrough', remoteOs: 'macos' })), 'mapping')).toMatchObject({
      level: 'info',
    });
  });

  it('host input permission', () => {
    expect(byId(troubleshootChecks(input({ hostInput: false })), 'host-input')?.level).toBe('bad');
    expect(byId(troubleshootChecks(input({ hostInput: null })), 'host-input')).toBeUndefined();
  });

  it('explains the phone app and offers to change the gateway', () => {
    const android: KeyboardStatus = {
      platform: 'android',
      version: '0.1.0',
      grab: false,
      accessibility: null,
      secureInput: null,
    };
    const checks = troubleshootChecks(
      input({
        viewer: 'other',
        status: android,
        grab: { inApp: true, grabbing: false, refusal: 'keyboard grab not supported on this platform' },
      }),
    );
    expect(byId(checks, 'app')).toMatchObject({ level: 'ok', actions: ['change-gateway'] });
    expect(byId(checks, 'grab')).toMatchObject({ level: 'info', title: 'Shortcuts stay on this phone' });
    expect(byId(checks, 'accessibility')).toBeUndefined();
  });
});
