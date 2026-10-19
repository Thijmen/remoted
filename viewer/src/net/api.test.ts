import { describe, expect, it } from 'vitest';
import { machineIdFrom } from './api';

describe('machineIdFrom', () => {
  it('turns a display name into a machine id', () => {
    expect(machineIdFrom('Windows PC')).toBe('windows-pc');
    expect(machineIdFrom('  Mac Studio (office) ')).toBe('mac-studio-office');
    expect(machineIdFrom('desktop_thijmen-1')).toBe('desktop_thijmen-1');
    expect(machineIdFrom('Ünïcode!')).toBe('n-code');
    expect(machineIdFrom('')).toBe('');
    expect(machineIdFrom('x'.repeat(80))).toHaveLength(64);
  });
});
