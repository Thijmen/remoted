import { describe, expect, it } from 'vitest';
import type { ActivityEvent } from '../proto/control';
import { GROUP_MS, activityLines, formatDuration, lineText } from './activity';

const NAMES: Record<string, string> = { mbp: 'MBP Nieuw', pc: 'Monster PC', vm: 'Download VM', box: 'PROPPFREXX' };
const nameOf = (m: string) => NAMES[m] ?? m;

function ev(event: string, machine: string, at_ms: number, over: Partial<ActivityEvent> = {}): ActivityEvent {
  return { at_ms, event, machine, user: 'thijmen@example.test', from: '', to: '', duration_ms: null, ...over };
}

const texts = (events: ActivityEvent[], max = 10) => activityLines(events, nameOf, max).map(lineText);

describe('activityLines', () => {
  it('says what happened, with short user names', () => {
    expect(
      texts([
        ev('session_start', 'mbp', 10),
        ev('session_end', 'pc', 9, { duration_ms: 3_720_000 }),
        ev('terminal_open', 'vm', 8),
        ev('files_open', 'vm', 7),
        ev('agent_update', 'pc', 6, { from: '1.8.0', to: '1.10.0' }),
        ev('agent_updated', 'pc', 5, { user: '', from: '1.8.0', to: '1.10.0' }),
        ev('device_add', 'vm', 4),
        ev('device_token', 'box', 3),
        ev('device_remove', 'gone-box', 2, { user: '' }),
      ]),
    ).toEqual([
      'thijmen connected to MBP Nieuw',
      'thijmen left Monster PC after 1 h 2 min',
      'thijmen opened a terminal on Download VM',
      'thijmen opened the files of Download VM',
      'thijmen started updating Monster PC to 1.10.0',
      'Monster PC was updated from 1.8.0 to 1.10.0',
      'thijmen added Download VM',
      'thijmen gave PROPPFREXX a new token',
      'Someone removed gone-box',
    ]);
  });

  it('puts machines that came online together on one line', () => {
    const t = 1_000_000;
    expect(
      texts([
        ev('agent_online', 'pc', t),
        ev('agent_online', 'box', t - 2000),
        ev('agent_online', 'mbp', t - 3000),
        ev('agent_online', 'vm', t - 4000),
        ev('agent_offline', 'vm', t - 5000),
        ev('agent_offline', 'pc', t - 6000),
        ev('agent_online', 'mbp', t - GROUP_MS - 10_000),
      ]),
    ).toEqual([
      'Monster PC, PROPPFREXX and 2 more came online',
      'Download VM and Monster PC went offline',
      'MBP Nieuw came online',
    ]);
  });

  it('skips unknown events and stops at max', () => {
    expect(texts([ev('something_new', 'pc', 3), ev('device_add', 'pc', 2), ev('device_add', 'vm', 1)], 1)).toEqual([
      'thijmen added Monster PC',
    ]);
  });
});

describe('formatDuration', () => {
  it('rounds to seconds, minutes and hours', () => {
    expect(formatDuration(42_400)).toBe('42 s');
    expect(formatDuration(5 * 60_000 + 20_000)).toBe('5 min');
    expect(formatDuration(2 * 3_600_000)).toBe('2 h');
  });
});
