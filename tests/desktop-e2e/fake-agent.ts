// A stand-in rd agent for the desktop e2e test: registers machine `mbp` on a dev-mode rd-gateway,
// answers session requests and the viewer's hello, keeps the session alive with PINGs, and records
// the input frames the viewer sends (docs/protocol.md, crates/rd-proto/src/bin.rs). No video: the
// viewer streams (and forwards input) as soon as the agent's hello arrives.
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';

export interface InputRecord {
  /** performance.now() in this process. */
  t: number;
  type: 'key' | 'release_all' | 'mouse_btn';
  down?: boolean;
  repeat?: boolean;
  /** USB HID usage, (page << 16) | usage. */
  hid?: number;
  button?: number;
}

const MSG = { MOUSE_BTN: 0x12, KEY: 0x14, RELEASE_ALL: 0x15, PING: 0x30, PONG: 0x31 } as const;
// Bun's WebSocket takes request headers (the dev gateway accepts any bearer token; the machine
// name comes from it).
const AUTH = { headers: { Authorization: 'Bearer rd1.mbp.dev' } };

const nowUs = () => BigInt(Math.round(performance.now() * 1000));

export class FakeAgent {
  readonly inputs: InputRecord[] = [];
  /** Other viewer→agent binary frame types, by type byte. */
  readonly otherFrames: Record<string, number> = {};
  registered = false;
  /** The viewer sent `start` (it is streaming and forwards input). */
  started = false;
  sessions = 0;
  viewerUa = '';
  private control: WebSocket | null = null;
  /** The current session socket (page probe commands go over it). */
  private session: WebSocket | null = null;
  private commandId = 0;
  private readonly waiting = new Map<number, (r: any) => void>();
  private readonly sockets: WebSocket[] = [];
  private readonly timers: ReturnType<typeof setInterval>[] = [];

  constructor(
    private readonly out: string,
    private readonly gw = 'ws://127.0.0.1:18581',
  ) {}

  private log(msg: string): void {
    appendFileSync(join(this.out, 'agent.log'), `${new Date().toISOString()} ${msg}\n`);
  }

  /** Connect the control socket; resolves on `registered`. */
  start(timeoutMs = 10_000): Promise<void> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('fake agent: no `registered` from the gateway')), timeoutMs);
      const c = new WebSocket(`${this.gw}/ws/agent`, AUTH as never);
      this.control = c;
      c.onopen = () =>
        c.send(
          JSON.stringify({
            t: 'register',
            machine: 'mbp',
            agent_version: 'e2e',
            os: 'macOS (fake agent)',
            gw_proto: 1,
            state: { locked: false, perms: { screen: true, input: true }, in_session: false },
          }),
        );
      c.onmessage = (ev) => {
        const m = JSON.parse(String(ev.data));
        this.log(`control <- ${JSON.stringify(m)}`);
        if (m.t === 'registered') {
          this.registered = true;
          clearTimeout(timer);
          resolve();
        } else if (m.t === 'session_request') {
          this.dial(String(m.session));
        }
      };
      c.onerror = () => this.log('control error');
      c.onclose = (ev) => this.log(`control closed ${ev.code} ${ev.reason}`);
    });
  }

  private dial(session: string): void {
    this.sessions++;
    const w = new WebSocket(`${this.gw}/ws/agent/session/${session}`, AUTH as never);
    w.binaryType = 'arraybuffer';
    this.sockets.push(w);
    this.session = w;
    let pingId = 0;
    w.onopen = () => {
      this.log(`session ${session} open`);
      // The viewer drops a session that is silent for 5 s.
      this.timers.push(setInterval(() => w.readyState === WebSocket.OPEN && w.send(ping(pingId++)), 500));
    };
    w.onmessage = (ev) => {
      if (typeof ev.data === 'string') {
        const m = JSON.parse(ev.data);
        if (m.t === 'rd_e2e_report') {
          // From tests/desktop-e2e/page-probe.js, relayed by the gateway like any text frame.
          this.log(`probe report ${m.id} (${m.cmd})`);
          this.waiting.get(m.id)?.(m);
          this.waiting.delete(m.id);
          return;
        }
        this.log(`session <- ${ev.data.slice(0, 400)}`);
        if (m.t === 'hello') {
          this.viewerUa = String(m.viewer?.ua ?? '');
          w.send(
            JSON.stringify({
              t: 'hello',
              proto: 1,
              agent: { os: 'macos', version: 'e2e', displays: [{ id: 0, w_pt: 1728, h_pt: 1117, scale: 2 }] },
              caps: [],
            }),
          );
        } else if (m.t === 'start') {
          this.started = true;
        }
        return;
      }
      const b = new Uint8Array(ev.data as ArrayBuffer);
      const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
      switch (b[0]) {
        case MSG.KEY:
          this.record({ t: 0, type: 'key', down: b[1] !== 0, repeat: (b[2]! & 1) !== 0, hid: v.getUint32(4, true) });
          return;
        case MSG.RELEASE_ALL:
          this.record({ t: 0, type: 'release_all' });
          return;
        case MSG.MOUSE_BTN:
          this.record({ t: 0, type: 'mouse_btn', button: b[1], down: b[2] !== 0 });
          return;
        case MSG.PING:
          w.send(pong(v));
          return;
        case MSG.PONG:
          return;
        default: {
          const k = `0x${(b[0] ?? 0).toString(16)}`;
          this.otherFrames[k] = (this.otherFrames[k] ?? 0) + 1;
        }
      }
    };
    w.onclose = (ev) => this.log(`session ${session} closed ${ev.code} ${ev.reason}`);
  }

  /** Ask the page probe (page-probe.js) for its state, or to run the focus race first. */
  command(cmd: 'report' | 'race', args: Record<string, unknown> = {}, timeoutMs = 15_000): Promise<any> {
    const w = this.session;
    if (!w || w.readyState !== WebSocket.OPEN) return Promise.reject(new Error('fake agent: no open session'));
    const id = ++this.commandId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiting.delete(id);
        reject(new Error(`page probe: no answer to ${cmd} within ${timeoutMs} ms`));
      }, timeoutMs);
      this.waiting.set(id, (r) => {
        clearTimeout(timer);
        resolve(r);
      });
      w.send(JSON.stringify({ t: 'rd_e2e', cmd, id, ...args }));
    });
  }

  private record(x: InputRecord): void {
    x.t = performance.now();
    this.inputs.push(x);
    appendFileSync(join(this.out, 'agent-input.jsonl'), `${JSON.stringify(x)}\n`);
  }

  stop(): void {
    for (const t of this.timers) clearInterval(t);
    for (const s of this.sockets) s.close();
    this.control?.close();
  }
}

/** PING: [0x30][flags][2 reserved][id u32][t_origin_us u64][0 u64], 24 bytes. */
function ping(id: number): Uint8Array {
  const b = new Uint8Array(24);
  const v = new DataView(b.buffer);
  b[0] = MSG.PING;
  v.setUint32(4, id >>> 0, true);
  v.setBigUint64(8, nowUs(), true);
  return b;
}

/** PONG: the PING's id and t_origin, plus t_reply (our clock). */
function pong(p: DataView): Uint8Array {
  const b = new Uint8Array(24);
  const v = new DataView(b.buffer);
  b[0] = MSG.PONG;
  v.setUint32(4, p.getUint32(4, true), true);
  v.setBigUint64(8, p.getBigUint64(8, true), true);
  v.setBigUint64(16, nowUs(), true);
  return b;
}

/** `d:70004` / `u:70004` for KEY frames, `release_all`, `btn:0d` for MOUSE_BTN. */
export function describe(x: InputRecord): string {
  if (x.type === 'key') return `${x.down ? 'd' : 'u'}:${(x.hid ?? 0).toString(16)}${x.repeat ? '(r)' : ''}`;
  if (x.type === 'mouse_btn') return `btn:${x.button}${x.down ? 'd' : 'u'}`;
  return 'release_all';
}
