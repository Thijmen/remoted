import { describe, expect, it } from 'vitest';
import { parseMachines, parseProbeEvt, parseViewerIncoming } from './control';

describe('parseViewerIncoming', () => {
  it('decodes the gateway session message with defaults', () => {
    expect(parseViewerIncoming('{"t":"session","machine":"mbp","session":"abc"}')).toEqual({
      t: 'session',
      machine: 'mbp',
      session: 'abc',
      agent: { locked: false, perms: { screen: false, input: false }, in_session: false },
    });
  });

  it('decodes the agent hello', () => {
    const m = parseViewerIncoming(
      '{"t":"hello","proto":1,"agent":{"os":"macos","version":"0.1.0","displays":[{"id":0,"w_pt":1728,"h_pt":1117,"scale":2.0}]},"caps":["audio","refine",3]}',
    );
    expect(m).toEqual({
      t: 'hello',
      proto: 1,
      agent: { os: 'macos', version: '0.1.0', displays: [{ id: 0, w_pt: 1728, h_pt: 1117, scale: 2 }] },
      caps: ['audio', 'refine'],
    });
  });

  it('decodes video_config, state, stats and error', () => {
    expect(
      parseViewerIncoming(
        '{"t":"video_config","epoch":1,"codec":"avc1.640033","width":1728,"height":1118,"description":"AWQAM/8="}',
      ),
    ).toMatchObject({
      t: 'video_config',
      epoch: 1,
      codec: 'avc1.640033',
    });
    expect(
      parseViewerIncoming('{"t":"state","locked":true,"perms":{"screen":true,"input":false},"in_session":true}'),
    ).toEqual({
      t: 'state',
      locked: true,
      perms: { screen: true, input: false },
      in_session: true,
    });
    expect(parseViewerIncoming('{"t":"stats","target_kbps":8000,"enc_ms_p50":4.5}')).toMatchObject({
      t: 'stats',
      target_kbps: 8000,
      enc_ms_p50: 4.5,
      drop_post: 0,
    });
    expect(parseViewerIncoming('{"t":"error","code":"tcc_screen"}')).toEqual({
      t: 'error',
      code: 'tcc_screen',
      msg: '',
    });
    expect(parseViewerIncoming('{"t":"gw_error","code":"agent_offline","msg":"x"}')).toEqual({
      t: 'gw_error',
      code: 'agent_offline',
      msg: 'x',
    });
  });

  it('ignores unknown types, malformed JSON and missing required fields', () => {
    expect(parseViewerIncoming('{"t":"from_the_future"}')).toBeNull();
    expect(parseViewerIncoming('not json')).toBeNull();
    expect(parseViewerIncoming('[1,2]')).toBeNull();
    expect(parseViewerIncoming('{"t":"video_config","epoch":"1"}')).toBeNull();
    expect(parseViewerIncoming('{"t":"hello","proto":1}')).toBeNull();
  });
});

describe('parseProbeEvt', () => {
  it('decodes hello and done', () => {
    expect(parseProbeEvt('{"t":"hello","server_time_us":5,"client_ip":"1.2.3.4","user":"me"}')).toEqual({
      t: 'hello',
      server_time_us: 5,
      client_ip: '1.2.3.4',
      user: 'me',
    });
    expect(parseProbeEvt('{"t":"done","test":"down","elapsed_ms":10,"frames":2,"bytes":3}')).toEqual({
      t: 'done',
      test: 'down',
      elapsed_ms: 10,
      frames: 2,
      bytes: 3,
      window_drops: 0,
      stopped: false,
    });
    expect(parseProbeEvt('{"t":"done","test":"sideways","elapsed_ms":1,"frames":1,"bytes":1}')).toBeNull();
  });
});

describe('parseMachines', () => {
  it('decodes entries and drops unsafe recovery links', () => {
    const ms = parseMachines([
      {
        machine: 'mbp',
        name: 'MacBook Pro',
        online: true,
        since_ms: 1,
        os: 'macOS 26.3',
        agent_version: '0.1.0',
        state: { locked: true },
        viewer: { user: 'a@b', since_ms: 2 },
        recovery: [
          { label: 'VNC', url: 'https://guac.example/#/client/x' },
          { label: 'evil', url: 'javascript:alert(1)' },
        ],
      },
      { name: 'no id' },
      'junk',
    ]);
    expect(ms).toHaveLength(1);
    expect(ms[0]!.state).toEqual({ locked: true, perms: { screen: false, input: false }, in_session: false });
    expect(ms[0]!.recovery).toEqual([{ label: 'VNC', url: 'https://guac.example/#/client/x' }]);
    expect(ms[0]!.viewer).toEqual({ user: 'a@b', since_ms: 2 });
  });
  it('returns [] for non-arrays', () => {
    expect(parseMachines({})).toEqual([]);
  });
});
