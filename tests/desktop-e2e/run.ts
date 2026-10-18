#!/usr/bin/env bun
/**
 * rd Desktop keyboard e2e on Windows: real windows, real (SendInput) clicks and scan-code keys,
 * a real dev-mode rd-gateway, a fake agent that records what reaches the "Mac".
 *
 *   bun tests/desktop-e2e/run.ts --variant grab|injected|edge|race
 *
 * Variants (docs/desktop-client.md has the bridge contract):
 *   grab      rd Desktop with RD_DESKTOP_GRAB_INJECTED=1 (debug builds: the hook grabs SendInput keys
 *             like physical ones). Types `a`, then Alt+Tab; expects KeyA once and, in the default
 *             windows-to-mac mapping, Alt+Tab as Cmd+Tab.
 *   injected  rd Desktop without the switch: SendInput keys pass the hook, like keys from KVM tools,
 *             AutoHotkey or remapping tools. Expects KeyA once (through the DOM). Then Alt+Tab, only
 *             to observe what happens locally.
 *   edge      Edge --app on the same page (no app, DOM keys): the control for the harness itself.
 *   race      As grab, after 20 rounds of 100 quick focus changes (rd_set_grab false/true pairs);
 *             also fails when a round leaves the hook off while the page wants the grab.
 *
 * The page is the normal viewer build plus page-probe.js (records DOM keys, rdNative calls and sent
 * input frames; reports over the session socket via the fake agent), because rd Desktop's WebView2
 * has no DevTools port. Edge additionally gets DevTools (console, sent frames).
 *
 * Needs (built beforehand): target/debug/rd-gateway.exe, viewer/dist,
 * clients/desktop/src-tauri/target/debug/rd-desktop.exe. Writes out/<variant>/ (logs, screenshots,
 * summary.json); exit code 1 when the variant's expectation fails.
 */
import { appendFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { Cdp, describeSent } from './cdp';
import { FakeAgent, type InputRecord, describe } from './fake-agent';

type Variant = 'grab' | 'injected' | 'edge' | 'race';
const VARIANTS: readonly Variant[] = ['grab', 'injected', 'edge', 'race'];

const ROOT = resolve(import.meta.dir, '..', '..');
const argv = process.argv.slice(2);
const variant = argv[argv.indexOf('--variant') + 1] as Variant;
if (!VARIANTS.includes(variant)) {
  console.error(`usage: bun tests/desktop-e2e/run.ts --variant ${VARIANTS.join('|')}`);
  process.exit(2);
}
const OUT = resolve(ROOT, 'out', variant);
const GATEWAY_EXE = process.env.RD_E2E_GATEWAY ?? join(ROOT, 'target', 'debug', 'rd-gateway.exe');
const APP_EXE = process.env.RD_E2E_APP ?? join(ROOT, 'clients', 'desktop', 'src-tauri', 'target', 'debug', 'rd-desktop.exe');
const EDGE_EXE = process.env.RD_E2E_EDGE ?? 'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe';
const VIEWER = 'http://127.0.0.1:18580';
const PAGE_URL = `${VIEWER}/m/mbp`;
const CDP_PORT = 9222;
/** Browser profiles: kept out of out/ (the uploaded artifact). */
const PROFILES = resolve(ROOT, 'out-profiles', variant);
const isApp = variant !== 'edge';

/** Windows set-1 scan codes. */
const SC = { A: 0x1e, ALT: 0x38, TAB: 0x0f } as const;
const EXPECT_TYPE = ['d:70004', 'u:70004'];
const RACE_ROUNDS = 20;
/** windows-to-mac: Option down, Option up, Cmd down, Tab down/up, Cmd up. */
const EXPECT_ALTTAB = ['d:700e2', 'u:700e2', 'd:700e3', 'd:7002b', 'u:7002b', 'u:700e3'];

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const t0 = performance.now();
const stamp = () => ((performance.now() - t0) / 1000).toFixed(2).padStart(7);

for (const dir of [OUT, PROFILES]) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
}

function log(...a: unknown[]): void {
  const line = `[${stamp()}] ${a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' ')}`;
  console.log(line);
  appendFileSync(join(OUT, 'run.log'), `${line}\n`);
}

async function drain(stream: ReadableStream<Uint8Array> | null | undefined, file: string): Promise<void> {
  if (!stream) return;
  for await (const chunk of stream) appendFileSync(file, chunk);
}

/** Lines from a stream, consumed one at a time. */
class Lines {
  private buf = '';
  private readonly queue: string[] = [];
  private readonly waiters: Array<(l: string | null) => void> = [];
  private done = false;

  constructor(stream: ReadableStream<Uint8Array>) {
    void this.pump(stream);
  }

  private async pump(stream: ReadableStream<Uint8Array>): Promise<void> {
    const dec = new TextDecoder();
    for await (const chunk of stream) {
      this.buf += dec.decode(chunk, { stream: true });
      let i: number;
      while ((i = this.buf.indexOf('\n')) >= 0) {
        const line = this.buf.slice(0, i).replace(/\r$/, '');
        this.buf = this.buf.slice(i + 1);
        const w = this.waiters.shift();
        if (w) w(line);
        else this.queue.push(line);
      }
    }
    this.done = true;
    for (const w of this.waiters.splice(0)) w(null);
  }

  next(timeoutMs: number): Promise<string | null> {
    const q = this.queue.shift();
    if (q !== undefined) return Promise.resolve(q);
    if (this.done) return Promise.resolve(null);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const i = this.waiters.indexOf(w);
        if (i >= 0) this.waiters.splice(i, 1);
        reject(new Error(`no answer within ${timeoutMs} ms`));
      }, timeoutMs);
      const w = (l: string | null) => {
        clearTimeout(timer);
        resolve(l);
      };
      this.waiters.push(w);
    });
  }
}

/** win-input.ps1 -Mode serve: one JSON command per line. */
class Win {
  private id = 0;
  hello: any = null;

  private constructor(
    private readonly proc: ReturnType<typeof Bun.spawn>,
    private readonly lines: Lines,
  ) {}

  static async start(): Promise<Win> {
    const proc = Bun.spawn(
      [
        'powershell.exe',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        join(import.meta.dir, 'win-input.ps1'),
        '-Mode',
        'serve',
        '-Out',
        OUT,
      ],
      { cwd: ROOT, stdin: 'pipe', stdout: 'pipe', stderr: 'pipe' },
    );
    void drain(proc.stderr as ReadableStream<Uint8Array>, join(OUT, 'win-input.err.log'));
    const win = new Win(proc, new Lines(proc.stdout as ReadableStream<Uint8Array>));
    for (;;) {
      const line = await win.lines.next(90_000);
      if (line === null) throw new Error('win-input.ps1 exited before it was ready (see win-input.err.log)');
      if (line.startsWith('{') && line.includes('"ready"')) {
        win.hello = JSON.parse(line);
        return win;
      }
      log('win-input:', line);
    }
  }

  async call<T = any>(cmd: string, args: Record<string, unknown> = {}, timeoutMs = 30_000): Promise<T> {
    const id = ++this.id;
    const stdin = this.proc.stdin as import('bun').FileSink;
    stdin.write(`${JSON.stringify({ id, cmd, ...args })}\n`);
    await stdin.flush();
    for (;;) {
      const line = await this.lines.next(timeoutMs);
      if (line === null) throw new Error(`win-input.ps1 exited during ${cmd}`);
      if (!line.startsWith('{')) {
        log('win-input:', line);
        continue;
      }
      const r = JSON.parse(line);
      if (r.id !== id) continue;
      if (!r.ok) throw new Error(`win-input ${cmd}: ${r.error}`);
      return r.result as T;
    }
  }

  async close(): Promise<void> {
    try {
      await this.call('exit', {}, 5000);
    } catch {
      // already gone
    }
    this.proc.kill();
  }
}

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

const summary: Record<string, any> = {
  variant,
  page_url: PAGE_URL,
  started_at: new Date().toISOString(),
  steps: [] as string[],
  expect: { type: EXPECT_TYPE, alttab: variant === 'grab' ? EXPECT_ALTTAB : null },
};
const cleanups: Array<() => Promise<void> | void> = [];
let helper: Win | null = null;

function step(name: string): void {
  log(`--- ${name}`);
  summary.steps.push(`${stamp().trim()}s ${name}`);
}

function keyFrames(xs: InputRecord[]): string[] {
  return xs.filter((x) => x.type !== 'mouse_btn').map(describe);
}

/** LL hook events as `<scan code hex>[e]<d|u>[*]` (e: extended, *: injected) `@<helper ms>`. */
function hookEvents(r: any): string[] {
  return (r?.keys ?? []).map(
    (k: any) => `${Number(k.sc).toString(16)}${k.ext ? 'e' : ''}${k.up ? 'u' : 'd'}${k.injected ? '*' : ''}@${k.t}`,
  );
}

function appLog(): string {
  try {
    return readFileSync(join(OUT, 'app.log'), 'utf8');
  } catch {
    return '';
  }
}

/** Grab-related app log lines (no keys are ever logged). */
function grabLines(): string[] {
  return appLog()
    .split(/\r?\n/)
    .filter((l) => /rd_set_grab|keyboard grab|gateway overridden|loading http|cannot reach the page|panicked/.test(l));
}

async function waitUntil(what: string, timeoutMs: number, f: () => boolean | Promise<boolean>): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await f()) return;
    await sleep(100);
  }
  throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
}

/** Run a command to completion; stdout+stderr as text. */
async function exec(cmd: string[]): Promise<string> {
  const p = Bun.spawn(cmd, { stdout: 'pipe', stderr: 'pipe', windowsHide: true });
  const [out, err] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text()]);
  await p.exited;
  return `${out}${err}`.trim();
}

/** Command lines of the WebView2 browser processes (not their renderers/GPU/utility children). */
async function webviewBrowsers(): Promise<unknown> {
  const out = await exec([
    'powershell.exe',
    '-NoProfile',
    '-Command',
    "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'msedgewebview2.exe' -and $_.CommandLine -notmatch '--type=' } | Select-Object ProcessId,ParentProcessId,CommandLine | ConvertTo-Json -Compress",
  ]);
  try {
    return JSON.parse(out);
  } catch {
    return out;
  }
}

async function portOpen(url: string): Promise<boolean> {
  try {
    await fetch(url, { signal: AbortSignal.timeout(1000) });
    return true;
  } catch {
    return false;
  }
}

/** viewer/dist plus page-probe.js, loaded before the viewer's module (see page-probe.js). */
function instrumentedViewer(): string {
  const dist = join(PROFILES, 'dist');
  cpSync(join(ROOT, 'viewer', 'dist'), dist, { recursive: true });
  cpSync(join(import.meta.dir, 'page-probe.js'), join(dist, 'assets', 'rd-e2e-probe.js'));
  const page = join(dist, 'viewer.html');
  const html = readFileSync(page, 'utf8');
  const patched = html.replace('<script type="module"', '<script src="/assets/rd-e2e-probe.js"></script>\n    <script type="module"');
  if (patched === html) throw new Error('viewer.html: no module script to put the probe before');
  writeFileSync(page, patched);
  return dist;
}

/** The probe's last layout report from the app log (debug builds log `rd-selftest …` titles). */
function layoutFromAppLog(): any {
  const lines = appLog().split(/\r?\n/).filter((l) => l.includes('rd-selftest {'));
  const last = lines.pop();
  if (!last) return null;
  try {
    return JSON.parse(last.slice(last.indexOf('rd-selftest {') + 'rd-selftest '.length));
  } catch {
    return null;
  }
}

async function main(): Promise<void> {
  for (const [what, path] of [
    ['gateway', GATEWAY_EXE],
    ['viewer build', join(ROOT, 'viewer', 'dist', 'viewer.html')],
    [isApp ? 'rd Desktop' : 'Edge', isApp ? APP_EXE : EDGE_EXE],
  ] as const) {
    if (!existsSync(path)) throw new Error(`${what} not found: ${path}`);
  }

  step('gateway (serving viewer/dist + page-probe.js)');
  const dist = instrumentedViewer();
  if (await portOpen(`${VIEWER}/`)) throw new Error('port 18580 is already in use');
  const gw = Bun.spawn(
    [
      GATEWAY_EXE,
      '--dev',
      '--viewer-addr',
      '127.0.0.1:18580',
      '--agent-addr',
      '127.0.0.1:18581',
      '--metrics-addr',
      '127.0.0.1:18590',
      '--assets-dir',
      dist,
    ],
    { cwd: ROOT, stdout: 'pipe', stderr: 'pipe', windowsHide: true },
  );
  cleanups.push(() => gw.kill());
  void drain(gw.stdout as ReadableStream<Uint8Array>, join(OUT, 'gateway.log'));
  void drain(gw.stderr as ReadableStream<Uint8Array>, join(OUT, 'gateway.log'));
  await waitUntil('gateway /healthz', 20_000, async () => {
    try {
      return (await fetch('http://127.0.0.1:18590/healthz')).ok;
    } catch {
      return false;
    }
  });

  step('fake agent');
  const agent = new FakeAgent(OUT);
  cleanups.push(() => agent.stop());
  await agent.start();
  await waitUntil('mbp online in /api/machines', 10_000, async () => {
    const ms = (await (await fetch(`${VIEWER}/api/machines`)).json()) as Array<{ machine: string; online: boolean }>;
    return ms.some((m) => m.machine === 'mbp' && m.online);
  });

  step('win-input helper');
  const win = await Win.start();
  helper = win;
  cleanups.push(() => win.close());
  summary.helper = win.hello;
  log('helper:', win.hello);
  summary.foreground_lock = await win.call('foregroundLock', { setZero: true });

  step(isApp ? 'launch rd Desktop' : 'launch Edge --app');
  await waitUntil(`port ${CDP_PORT} to be free (an earlier variant's browser process?)`, 15_000, async () => !(await portOpen(`http://127.0.0.1:${CDP_PORT}/json/version`)));
  let pid: number;
  if (isApp) {
    const env = ['RD_LOG=debug', `WEBVIEW2_USER_DATA_FOLDER=${join(PROFILES, 'wv2')}`];
    if (variant === 'grab' || variant === 'race') env.push('RD_DESKTOP_GRAB_INJECTED=1');
    summary.app_env = env;
    pid = await win.call<number>('launch', {
      exe: APP_EXE,
      args: `--url ${PAGE_URL}`,
      env,
      cwd: OUT,
      log: join(OUT, 'app.log'),
    });
  } else {
    const args = [
      `--app=${PAGE_URL}`,
      `--remote-debugging-port=${CDP_PORT}`,
      '--remote-allow-origins=*',
      `--user-data-dir="${join(PROFILES, 'edge')}"`,
      '--no-first-run',
      '--no-default-browser-check',
      '--window-position=0,0',
      '--window-size=1000,700',
    ].join(' ');
    summary.edge_args = args;
    pid = await win.call<number>('launch', { exe: EDGE_EXE, args, env: [], cwd: OUT, log: join(OUT, 'edge.log') });
  }
  summary.pid = pid;
  cleanups.push(async () => {
    log('kill:', await win.call('kill', { pid }));
    await waitUntil('the process to exit', 15_000, async () => !(await win.call<boolean>('alive', { pid })));
    // The WebView2/Edge browser process holds the DevTools port until it is gone.
    await waitUntil(`port ${CDP_PORT} to close`, 15_000, async () => !(await portOpen(`http://127.0.0.1:${CDP_PORT}/json/version`)));
  });
  log('pid', pid);

  // Edge only: DevTools for the console and the sent WebSocket frames (a cross-check of the probe).
  let cdp: Cdp | null = null;
  if (!isApp) {
    step('attach CDP (Edge)');
    cdp = await Cdp.attach(CDP_PORT, (u) => u.startsWith(PAGE_URL), OUT);
    cleanups.push(() => cdp?.close());
  }

  /** The probe's layout report: app log (title channel) or DevTools. */
  const layout = async (): Promise<any> => {
    if (cdp) return cdp.eval('window.__rdE2E ? window.__rdE2E.state() : null').catch(() => null);
    return layoutFromAppLog();
  };

  step('find and place the window');
  let hwnd = 0;
  await waitUntil('the top-level window', 60_000, async () => {
    const ws = await win.call<any[]>('windows', { pid });
    let w = ws.filter((x) => (isApp ? x.title === 'rd' : x.class === 'Chrome_WidgetWin_1' && x.title));
    if (!isApp && w.length === 0) {
      // Edge may hand the window to another process of the same profile.
      w = (await win.call<any[]>('windows', {})).filter((x) => x.class === 'Chrome_WidgetWin_1' && /mbp|rd/.test(x.title));
    }
    w.sort((a, b) => b.rect.w * b.rect.h - a.rect.w * a.rect.h);
    if (w[0]) {
      hwnd = w[0].hwnd;
      summary.window = w[0];
    }
    return hwnd !== 0;
  });
  log('window:', summary.window);
  await win.call('place', { hwnd, x: 0, y: 0, w: 1000, h: 700 });
  if (isApp) summary.webview2_browser = await webviewBrowsers();

  /** Where the page's (0,0) is on screen, given the page's inner size. */
  const pageOrigin = async (l: any): Promise<{ x: number; y: number; how: string } | null> => {
    const content = await win.call<{ client: Rect; widgets: Array<Rect & { visible: boolean }> }>('content', { hwnd });
    const want = { w: Math.round(l.inner[0] * l.dpr), h: Math.round(l.inner[1] * l.dpr) };
    const widget = content.widgets.find((w) => w.visible && Math.abs(w.w - want.w) <= 4 && Math.abs(w.h - want.h) <= 4);
    const c = content.client;
    summary.geometry = { inner: l.inner, dpr: l.dpr, content };
    if (widget) return { x: widget.x, y: widget.y, how: 'render widget' };
    // The page fills the client area's width and sits at its bottom (Edge draws its title bar inside).
    if (Math.abs(c.w - want.w) <= 4 && c.h >= want.h) return { x: c.x, y: c.y + (c.h - want.h), how: 'client area' };
    return null; // not laid out for the new window size yet
  };
  const screenPoint = (o: { x: number; y: number }, r: any, dpr: number) => ({
    x: Math.round(o.x + (r.x + r.w / 2) * dpr),
    y: Math.round(o.y + (r.y + r.h / 2) * dpr),
  });

  step('wait for the connect screen (probe layout)');
  let connectAt: { x: number; y: number } | null = null;
  await waitUntil('an enabled Connect button laid out for the placed window', 60_000, async () => {
    const l = await layout();
    if (!l?.connect || l.connect.disabled || l.connect.hidden) return false;
    const o = await pageOrigin(l);
    if (!o) return false;
    connectAt = screenPoint(o, l.connect, l.dpr);
    summary.connect_layout = { layout: l, origin: o, at: connectAt };
    return true;
  });
  summary.connect_click = { ...connectAt!, window_at: await win.call('at', connectAt!) };
  log('connect click at', summary.connect_click);
  await win.call('screenshot', { path: join(OUT, '1-connect-screen.png') });

  step('click Connect');
  await win.call('click', connectAt!);
  await waitUntil('the viewer to stream (fake agent got `start`)', 30_000, () => agent.started).catch(async (e) => {
    log('no session yet:', String(e), await win.call('foreground'));
    await win.call('screenshot', { path: join(OUT, '1b-after-connect-click.png') });
    throw e;
  });
  summary.viewer_ua = agent.viewerUa;

  step('click the video (OS focus into the page)');
  await sleep(500);
  let st = (await agent.command('report')).state;
  const origin = await pageOrigin(st);
  if (!origin || !st.canvas) throw new Error(`no canvas/page origin: ${JSON.stringify({ origin, canvas: st.canvas })}`);
  const canvasAt = screenPoint(origin, st.canvas, st.dpr);
  summary.canvas_click = { ...canvasAt, window_at: await win.call('at', canvasAt) };
  await win.call('click', canvasAt);
  await sleep(700);
  let fg = await isForeground(win, hwnd);
  if (!fg.ok) {
    log('not foreground after the click:', fg.fg, '— SetForegroundWindow + click again');
    summary.set_foreground = await win.call('setForeground', { hwnd });
    await win.call('click', canvasAt);
    await sleep(700);
    fg = await isForeground(win, hwnd);
  }
  summary.foreground_after_focus_click = fg.fg;
  st = (await agent.command('report')).state;
  summary.page_before = st;
  log('foreground ok:', fg.ok, 'page:', { hasFocus: st.hasFocus, active: st.active, tauri: st.tauri, rdNative: st.rdNative, platform: st.platform });
  if (!fg.ok) throw new Error(`the window under test is not the foreground window: ${JSON.stringify(fg.fg)}`);

  if (isApp) {
    step('wait for the grab');
    await waitUntil('`keyboard grab on` in the app log', 6000, () => appLog().includes('keyboard grab on')).catch((e) =>
      log(String(e), '(continuing: the variant records what happens without it)'),
    );
    await sleep(300); // the page confirms after the IPC reply
  }

  if (variant === 'race') {
    // rd_set_grab is async: Tauri runs each call as its own task, so a quick false/true pair can
    // reach the hook thread as true/false and leave the hook off while the page, whose last call
    // (true) resolved, believes it grabs. Each round ends on the video, so the page wants the grab:
    // count the rounds after which the app's last word was "off".
    step(`race: ${RACE_ROUNDS} rounds of 100 focus changes (rd_set_grab false/true pairs)`);
    const before = grabLines().length;
    const rounds: string[] = [];
    for (let k = 0; k < RACE_ROUNDS; k++) {
      // First half: all changes in one task (a page may fold them into one decision). Second
      // half: every change in its own task, so the calls really go out back to back.
      const spread = k >= RACE_ROUNDS / 2;
      const params = spread ? { rounds: 1, n: 30, pauseMs: 0, yieldMs: 0 } : { rounds: 1, n: 100, pauseMs: 0 };
      const r = await agent.command('race', params, 60_000);
      await sleep(700); // let the app finish the round's calls
      const last = grabLines().filter((l) => /keyboard grab (on|off)/.test(l)).pop() ?? '';
      const m = /keyboard grab (on|off(?: \(\w+\))?)/.exec(last);
      rounds.push(`${m ? m[1] : '?'}${spread ? ' spread' : ''}${r.activeIsCanvas ? '' : ' (video not focused)'}`);
    }
    await sleep(1000);
    const lines = grabLines().slice(before);
    summary.race = {
      rounds,
      hook_off_while_page_wants_it: rounds.filter((r) => r.startsWith('off')).length,
      set_grab_lines: lines.filter((l) => l.includes('rd_set_grab')).length,
      grab_on: lines.filter((l) => l.includes('keyboard grab on')).length,
      grab_off: lines.filter((l) => l.includes('keyboard grab off')).length,
      last_lines: lines.slice(-6),
    };
    log('race:', summary.race);
  }

  // A second LL hook, installed now (after the app's): it runs before the app's hook and sees every
  // event, while the helper's first one (installed before the app started) runs after it.
  summary.late_hook = await win.call('hookStart', { name: 'late' });
  cleanups.push(() => win.call('hookStop', { name: 'late' }).then(() => undefined));
  const frames: Record<string, any> = {};
  summary.frames = frames;

  const phase = async (name: string, seq: Array<Record<string, unknown>>, settleMs: number, screenshot: string) => {
    step(name);
    const pre = (await agent.command('report')).state;
    const fgBefore = await isForeground(win, hwnd);
    const mark = agent.inputs.length;
    const cdpMark = cdp?.sentInput.length ?? 0;
    for (const name of ['early', 'late']) await win.call('hook', { name }); // drop earlier events
    await win.call('keys', { seq, gapMs: 60 });
    await sleep(settleMs);
    const after = await isForeground(win, hwnd);
    const post = (await agent.command('report').catch((e) => ({ state: { error: String(e) } }))).state;
    const since = (a: string[] | undefined, b: string[] | undefined) => (b ?? []).slice((a ?? []).length);
    const r = {
      foreground_before: fgBefore.ok,
      agent: keyFrames(agent.inputs.slice(mark)),
      page_dom: since(pre.dom, post.dom),
      page_rdnative: since(pre.nat, post.nat),
      page_sent: since(pre.sent, post.sent).filter((x: string) => !x.startsWith('btn:')),
      page_sent_cdp: cdp ? cdp.sentInput.slice(cdpMark).map(describeSent) : undefined,
      ll_hook_first: hookEvents(await win.call('hook', { name: 'late' })),
      ll_hook_after_rd: hookEvents(await win.call('hook', { name: 'early' })),
      foreground_after: after.ok,
      foreground_window_after: after.ok ? undefined : after.fg,
      page_after: { hasFocus: post.hasFocus, active: post.active },
    };
    log(`${name}:`, r);
    await win.call('screenshot', { path: join(OUT, screenshot) });
    return r;
  };

  frames.type = await phase('type a (scan code 0x1E)', [{ sc: SC.A }, { sc: SC.A, up: true }], 800, '2-after-typing.png');
  if (variant === 'grab' || variant === 'injected') {
    frames.alttab = await phase(
      'Alt+Tab (scan codes 0x38, 0x0F)',
      [{ sc: SC.ALT }, { sc: SC.TAB }, { sc: SC.TAB, up: true }, { sc: SC.ALT, up: true }],
      1200,
      '3-after-alt-tab.png',
    );
  }

  step('collect');
  summary.page_after = await agent
    .command('report')
    .then((r) => r.state)
    .catch((e) => ({ error: String(e) }));
  summary.agent_all = agent.inputs.map(describe);
  summary.agent_other_frames = agent.otherFrames;
  if (cdp) summary.page_sockets = cdp.sockets;
}

async function isForeground(win: Win, hwnd: number): Promise<{ ok: boolean; fg: any }> {
  const fg = await win.call<any>('foreground');
  return { ok: fg.hwnd === hwnd, fg };
}

function verdict(): { pass: boolean; problems: string[] } {
  const problems: string[] = [];
  const f = summary.frames ?? {};
  const same = (a: string[] | undefined, b: string[]) => JSON.stringify(a ?? null) === JSON.stringify(b);
  if (summary.error) problems.push(`harness error: ${summary.error.split('\n')[0]}`);
  if (!f.type) problems.push('no typing phase');
  else if (!same(f.type.agent, EXPECT_TYPE))
    problems.push(`typing a: agent got [${f.type.agent.join(' ')}], expected [${EXPECT_TYPE.join(' ')}]`);
  if (variant === 'race' && summary.race?.hook_off_while_page_wants_it)
    problems.push(`race: hook off after ${summary.race.hook_off_while_page_wants_it}/${RACE_ROUNDS} rounds that ended on the video`);
  if (variant === 'race' && summary.race && summary.race.set_grab_lines === 0)
    problems.push('race: no rd_set_grab calls at all, so the rounds exercised nothing');
  if (variant === 'grab') {
    if (!f.alttab) problems.push('no Alt+Tab phase');
    else if (!same(f.alttab.agent, EXPECT_ALTTAB))
      problems.push(`Alt+Tab: agent got [${f.alttab.agent.join(' ')}], expected [${EXPECT_ALTTAB.join(' ')}]`);
    else if (!f.alttab.foreground_after) problems.push('Alt+Tab switched windows locally');
  }
  return { pass: problems.length === 0, problems };
}

function report(v: { pass: boolean; problems: string[] }): void {
  const f = summary.frames ?? {};
  const lines = [`=== desktop-e2e ${variant}: ${v.pass ? 'PASS' : 'FAIL'}`, ...v.problems.map((p) => `  ! ${p}`)];
  const pb = summary.page_before;
  if (pb) lines.push(`  page   hasFocus=${pb.hasFocus} active=${pb.active} tauri=${pb.tauri} rdNative=${pb.rdNative} platform=${pb.platform} settings=${pb.storedSettings ?? 'default'}`);
  for (const phase of ['type', 'alttab'] as const) {
    const p = f[phase];
    if (!p) continue;
    lines.push(
      `  ${phase.padEnd(6)} agent=[${p.agent.join(' ')}] page_sent=[${p.page_sent.join(' ')}] dom=[${p.page_dom.join(' ')}] rdNative=[${p.page_rdnative.join(' ')}]`,
      `         ll_hook_first=[${p.ll_hook_first.join(' ')}] ll_hook_after_rd=[${p.ll_hook_after_rd.join(' ')}]`,
      `         foreground before=${p.foreground_before} after=${p.foreground_after}${p.foreground_window_after ? ` (now: ${p.foreground_window_after.process} "${p.foreground_window_after.title}")` : ''}`,
    );
  }
  if (summary.race) lines.push(`  race   ${JSON.stringify(summary.race)}`);
  if (isApp) for (const l of summary.app_grab_lines_tail ?? []) lines.push(`  app    ${l.replace(/^\[[^\]]*\]\[[^\]]*\]/, '')}`);
  const text = lines.join('\n');
  console.log(text);
  writeFileSync(join(OUT, 'result.txt'), `${text}\n`);
  // An annotation keeps a failure visible on steps marked continue-on-error.
  if (!v.pass && process.env.GITHUB_ACTIONS) console.log(`::error title=desktop-e2e ${variant}::${v.problems.join(' | ')}`);
  const stepSummary = process.env.GITHUB_STEP_SUMMARY;
  if (stepSummary) appendFileSync(stepSummary, `\n\`\`\`\n${text}\n\`\`\`\n`);
}

let exitCode = 1;
try {
  await Promise.race([
    main(),
    sleep(240_000).then(() => {
      throw new Error('variant timed out after 240 s');
    }),
  ]);
} catch (e) {
  summary.error = e instanceof Error ? (e.stack ?? e.message) : String(e);
  log('ERROR', summary.error);
} finally {
  const grab = grabLines();
  summary.app_grab_lines_total = grab.length;
  summary.app_grab_lines_tail = grab.slice(-12);
  if (helper) {
    await helper.call('screenshot', { path: join(OUT, '9-final.png') }).catch(() => undefined);
    summary.foreground_final = await helper.call('foreground').catch((e) => String(e));
  }
  for (const c of cleanups.reverse()) {
    try {
      await c();
    } catch (e) {
      log('cleanup:', String(e));
    }
  }
  const v = verdict();
  summary.pass = v.pass;
  summary.problems = v.problems;
  summary.finished_at = new Date().toISOString();
  writeFileSync(join(OUT, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  report(v);
  exitCode = v.pass ? 0 : 1;
}
process.exit(exitCode);
