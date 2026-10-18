// Minimal Chrome DevTools Protocol client for the Edge control variant (msedge --app with
// --remote-debugging-port). Records the console and the binary WebSocket frames the page sends, and
// evaluates expressions. (rd Desktop's WebView2 has no DevTools port: see page-probe.js.)
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';

interface Target {
  id: string;
  type: string;
  url: string;
  title: string;
  webSocketDebuggerUrl?: string;
}

export interface SentFrame {
  t: number;
  /** First bytes, hex. */
  hex: string;
  type: number;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export class Cdp {
  private nextId = 1;
  private readonly pending = new Map<number, { resolve(v: unknown): void; reject(e: Error): void }>();
  /** Binary frames the page sent on its WebSockets (input types only: 0x10..0x15). */
  readonly sentInput: SentFrame[] = [];
  readonly sockets: string[] = [];

  private constructor(
    private readonly ws: WebSocket,
    private readonly out: string,
    readonly target: Target,
  ) {
    ws.onmessage = (ev) => this.onMessage(String(ev.data));
    ws.onclose = () => {
      for (const p of this.pending.values()) p.reject(new Error('CDP socket closed'));
      this.pending.clear();
    };
  }

  /** Wait for a page target whose URL matches, connect, and enable Runtime/Log/Network. */
  static async attach(port: number, match: (url: string) => boolean, out: string, timeoutMs = 90_000): Promise<Cdp> {
    const deadline = Date.now() + timeoutMs;
    let last = '';
    while (Date.now() < deadline) {
      try {
        const list = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()) as Target[];
        last = JSON.stringify(list.map((t) => [t.type, t.url]));
        const t = list.find((x) => x.type === 'page' && match(x.url) && x.webSocketDebuggerUrl);
        if (t) {
          const ws = new WebSocket(t.webSocketDebuggerUrl!);
          await new Promise<void>((resolve, reject) => {
            ws.onopen = () => resolve();
            ws.onerror = () => reject(new Error(`CDP: cannot open ${t.webSocketDebuggerUrl}`));
          });
          const cdp = new Cdp(ws, out, t);
          // Network first: it only reports frames of WebSockets created after it is enabled.
          await cdp.send('Network.enable');
          await cdp.send('Runtime.enable');
          await cdp.send('Log.enable');
          return cdp;
        }
      } catch (e) {
        last = String(e);
      }
      await sleep(500);
    }
    throw new Error(`CDP: no page target on port ${port} within ${timeoutMs} ms (last: ${last})`);
  }

  send(method: string, params: Record<string, unknown> = {}): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  /** Evaluate an expression in the page (promises awaited, result by value). */
  async eval<T = unknown>(expression: string): Promise<T> {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error(`page threw: ${d.exception?.description ?? d.text}`);
    }
    return r.result?.value as T;
  }

  /** Poll an expression until it is truthy. */
  async waitFor<T = unknown>(expression: string, timeoutMs: number, what: string): Promise<T> {
    const deadline = Date.now() + timeoutMs;
    let last: unknown;
    while (Date.now() < deadline) {
      try {
        last = await this.eval(expression);
        if (last) return last as T;
      } catch (e) {
        last = String(e);
      }
      await sleep(200);
    }
    throw new Error(`timed out waiting for ${what} (last: ${JSON.stringify(last)})`);
  }

  close(): void {
    try {
      this.ws.close();
    } catch {
      // already gone
    }
  }

  private onMessage(data: string): void {
    const m = JSON.parse(data);
    if (m.id !== undefined) {
      const p = this.pending.get(m.id);
      this.pending.delete(m.id);
      if (!p) return;
      if (m.error) p.reject(new Error(`CDP ${m.error.code}: ${m.error.message}`));
      else p.resolve(m.result);
      return;
    }
    const t = performance.now();
    const p = m.params ?? {};
    switch (m.method) {
      case 'Runtime.consoleAPICalled':
        this.log('console.jsonl', {
          t,
          type: p.type,
          args: (p.args ?? []).map((a: any) => a.value ?? a.description ?? a.type),
        });
        return;
      case 'Runtime.exceptionThrown':
        this.log('console.jsonl', { t, type: 'exception', text: p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text });
        return;
      case 'Log.entryAdded':
        this.log('console.jsonl', { t, type: `log.${p.entry?.level}`, text: p.entry?.text, url: p.entry?.url });
        return;
      case 'Network.webSocketCreated':
        this.sockets.push(p.url);
        this.log('ws-sent.jsonl', { t, created: p.url });
        return;
      case 'Network.webSocketFrameSent': {
        const r = p.response ?? {};
        if (r.opcode !== 2) return;
        const bytes = Buffer.from(String(r.payloadData ?? ''), 'base64');
        const type = bytes[0] ?? -1;
        if (type < 0x10 || type > 0x15) return; // input frames only (no PING/PONG/acks)
        const f = { t, type, hex: bytes.subarray(0, 8).toString('hex') };
        this.sentInput.push(f);
        this.log('ws-sent.jsonl', f);
        return;
      }
    }
  }

  private log(file: string, x: unknown): void {
    appendFileSync(join(this.out, file), `${JSON.stringify(x)}\n`);
  }
}

/** `d:70004` / `u:70004` for a KEY frame's hex (as recorded above), else the type. */
export function describeSent(f: SentFrame): string {
  const b = Buffer.from(f.hex, 'hex');
  if (f.type === 0x14 && b.length >= 8) return `${b[1] ? 'd' : 'u'}:${b.readUInt32LE(4).toString(16)}`;
  if (f.type === 0x15) return 'release_all';
  if (f.type === 0x12) return `btn:${b[1]}${b[2] ? 'd' : 'u'}`;
  return `0x${f.type.toString(16)}`;
}
