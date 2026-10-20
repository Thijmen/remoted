import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CloseCode } from '../proto/control';
import { accessFromResponse } from './api';
import { Session, type SessionStatus } from './session';

/** Just enough of a browser WebSocket for Session. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static all: FakeSocket[] = [];
  readyState = FakeSocket.CONNECTING;
  binaryType = 'blob';
  closedWith: number | null = null;
  sent: unknown[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.all.push(this);
  }
  send(d: unknown): void {
    this.sent.push(d);
  }
  close(code?: number): void {
    this.closedWith = code ?? 1000;
    this.readyState = FakeSocket.CLOSED;
  }
  // test helpers
  open(): void {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }
  /** The server side or the network ended the connection. */
  drop(code: number): void {
    this.readyState = FakeSocket.CLOSED;
    this.onclose?.({ code, reason: '' });
  }
}

const meResponse = (type: ResponseType, status: number) => ({ type, status }) as Response;

describe('accessFromResponse', () => {
  it('treats a sign-in redirect, 401 and 403 as denied', () => {
    expect(accessFromResponse({ type: 'opaqueredirect', status: 0 })).toBe('denied');
    expect(accessFromResponse({ type: 'basic', status: 401 })).toBe('denied');
    expect(accessFromResponse({ type: 'basic', status: 403 })).toBe('denied');
  });
  it('is ok on 2xx and unknown otherwise', () => {
    expect(accessFromResponse({ type: 'basic', status: 200 })).toBe('ok');
    expect(accessFromResponse({ type: 'basic', status: 502 })).toBe('unknown');
  });
});

describe('Session reconnects', () => {
  let statuses: SessionStatus[];
  let fetchMock: ReturnType<typeof vi.fn>;
  let session: Session;

  beforeEach(() => {
    vi.useFakeTimers();
    FakeSocket.all = [];
    statuses = [];
    fetchMock = vi.fn();
    vi.stubGlobal('WebSocket', FakeSocket);
    vi.stubGlobal('window', globalThis);
    vi.stubGlobal('location', { protocol: 'https:', host: 'rd.test' });
    vi.stubGlobal('document', { hidden: false, addEventListener: () => {} });
    vi.stubGlobal('fetch', fetchMock);
    vi.spyOn(console, 'info').mockImplementation(() => {});
    session = new Session(
      {
        machine: 'mbp',
        viewerInfo: () => ({ ua: 'test', dpr: 1, viewport: [800, 600], decoders: { avc: true, hevc: false } }),
        startParams: () => {
          throw new Error('not reached');
        },
      },
      {
        status: (s) => statuses.push(s),
        session: () => {},
        agentHello: () => {},
        control: () => {},
        video: () => {},
        audio: () => {},
        disconnected: () => {},
      },
    );
  });

  afterEach(() => {
    session.stop();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  const last = () => statuses[statuses.length - 1];
  const socket = (i: number) => FakeSocket.all[i]!;

  it('abandons a hanging opening handshake after 10 s and tries again', async () => {
    session.start();
    expect(FakeSocket.all).toHaveLength(1);
    expect(socket(0).url).toBe('wss://rd.test/ws/view/mbp');
    await vi.advanceTimersByTimeAsync(9_999);
    expect(socket(0).closedWith).toBeNull();
    await vi.advanceTimersByTimeAsync(1);
    expect(socket(0).closedWith).toBe(4000);
    expect(last()).toMatchObject({ kind: 'retrying', code: 4000, reason: 'connect timeout' });
    expect(fetchMock).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(500);
    expect(FakeSocket.all).toHaveLength(2);
  });

  it('does not time out a socket that opened (the join timeout takes over)', async () => {
    session.start();
    socket(0).open();
    await vi.advanceTimersByTimeAsync(12_000);
    expect(socket(0).closedWith).toBeNull();
    expect(last()).toEqual({ kind: 'joining' });
  });

  it('stops with a sign-in message when the upgrade was refused by the sign-in proxy', async () => {
    fetchMock.mockResolvedValue(meResponse('opaqueredirect', 0));
    session.start();
    socket(0).drop(1006);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/me',
      expect.objectContaining({ redirect: 'manual', cache: 'no-store' }),
    );
    expect(last()).toEqual({ kind: 'stopped', code: CloseCode.FORBIDDEN, reason: 'signed out or not allowed' });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeSocket.all).toHaveLength(1);
  });

  it('stops when the gateway answers 403', async () => {
    fetchMock.mockResolvedValue(meResponse('basic', 403));
    session.start();
    socket(0).drop(1006);
    await vi.advanceTimersByTimeAsync(0);
    expect(last()).toMatchObject({ kind: 'stopped', code: CloseCode.FORBIDDEN });
  });

  it('keeps retrying on a real network failure', async () => {
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'));
    session.start();
    socket(0).drop(1006);
    await vi.advanceTimersByTimeAsync(0);
    expect(last()).toMatchObject({ kind: 'retrying', code: 1006, delayMs: 500 });
    await vi.advanceTimersByTimeAsync(500);
    expect(FakeSocket.all).toHaveLength(2);
  });

  it('does not probe when an established socket drops', async () => {
    session.start();
    socket(0).open();
    socket(0).drop(1006);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(last()).toMatchObject({ kind: 'retrying', code: 1006 });
  });

  it('says so when the socket keeps being refused while signed in', async () => {
    fetchMock.mockResolvedValue(meResponse('basic', 200));
    session.start();
    for (let i = 0; i < 3; i++) {
      socket(i).drop(1006);
      await vi.advanceTimersByTimeAsync(0);
      const s = last();
      expect(s?.kind).toBe('retrying');
      if (s?.kind !== 'retrying') return;
      expect(s.reason.includes('refused')).toBe(i === 2);
      await vi.advanceTimersByTimeAsync(s.delayMs);
    }
  });

  it('the user can still disconnect while the sign-in check runs', async () => {
    let resolve: (r: Response) => void = () => {};
    fetchMock.mockReturnValue(new Promise<Response>((r) => (resolve = r)));
    session.start();
    socket(0).drop(1006);
    session.stop();
    resolve(meResponse('opaqueredirect', 0));
    await vi.advanceTimersByTimeAsync(0);
    expect(last()).toEqual({ kind: 'closed' });
  });

  it('stops polling an offline machine once signed out', async () => {
    fetchMock.mockImplementation((path: string) =>
      Promise.resolve(path === '/api/me' ? meResponse('opaqueredirect', 0) : Promise.reject(new TypeError('CORS'))),
    );
    session.start();
    socket(0).open();
    socket(0).drop(CloseCode.AGENT_OFFLINE);
    expect(last()).toEqual({ kind: 'offline' });
    await vi.advanceTimersByTimeAsync(3000);
    expect(last()).toMatchObject({ kind: 'stopped', code: CloseCode.FORBIDDEN });
  });

  /** `/api/machines` with one entry for mbp. */
  const listing = (m: object) =>
    ({ ok: true, status: 200, json: () => Promise.resolve([{ machine: 'mbp', ...m }]) }) as unknown as Response;

  it('waits for an updating agent, through its restart, and reconnects once it is back', async () => {
    let mbp: object = { online: true, update: { phase: 'downloading' } };
    fetchMock.mockImplementation(() => Promise.resolve(listing(mbp)));
    session.start();
    socket(0).open();
    socket(0).drop(CloseCode.AGENT_UPDATING);
    expect(last()).toEqual({ kind: 'updating' });
    expect(session.updating).toBe(true);
    await vi.advanceTimersByTimeAsync(2000);
    expect(fetchMock).toHaveBeenLastCalledWith('/api/machines', expect.anything());
    mbp = { online: false, update: { phase: 'installing' } };
    await vi.advanceTimersByTimeAsync(2000);
    expect(FakeSocket.all).toHaveLength(1);
    // Registered again; it has not reported its update status yet.
    mbp = { online: true, update: null };
    await vi.advanceTimersByTimeAsync(2000);
    expect(FakeSocket.all).toHaveLength(2);
    // Answers that belong to the restart are not errors either.
    socket(1).open();
    socket(1).drop(CloseCode.AGENT_TIMEOUT);
    expect(last()).toEqual({ kind: 'updating' });
    await vi.advanceTimersByTimeAsync(2000);
    socket(2).open();
    socket(2).drop(CloseCode.AGENT_OFFLINE);
    expect(last()).toEqual({ kind: 'updating' });
    // A take-over still stops.
    await vi.advanceTimersByTimeAsync(2000);
    socket(3).open();
    socket(3).drop(CloseCode.KICKED);
    expect(last()).toMatchObject({ kind: 'stopped', code: CloseCode.KICKED });
    expect(session.updating).toBe(false);
  });

  it('falls back to the usual handling when the agent does not come back in 3 minutes', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(listing({ online: false })));
    session.start();
    socket(0).open();
    socket(0).drop(CloseCode.AGENT_UPDATING);
    await vi.advanceTimersByTimeAsync(179_000);
    expect(FakeSocket.all).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(FakeSocket.all).toHaveLength(2);
    socket(1).open();
    socket(1).drop(CloseCode.AGENT_OFFLINE);
    expect(last()).toEqual({ kind: 'offline' });
  });
});
