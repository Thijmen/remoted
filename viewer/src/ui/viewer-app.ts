import { AudioPlayer } from '../audio/player';
import { CHORD_LABEL, type ChordAction } from '../input/chords';
import { InputController, type InputOptions } from '../input/controller';
import { resolveMapping } from '../input/modifiers';
import { fetchMachine } from '../net/api';
import { Session, type SessionStatus } from '../net/session';
import type { AudioMsg, VideoMsg } from '../proto/bin';
import {
  type AgentHello,
  type AgentMsg,
  type AgentState,
  type AgentStats,
  CloseCode,
  type DecoderCaps,
  type GwErrorMsg,
  type MachineInfo,
  type StreamParams,
  type StreamParamsPatch,
  type ViewerInfo,
} from '../proto/control';
import { LatencyTest } from '../video/latency-test';
import { VideoPipeline, probeDecoders } from '../video/pipeline';
import { type Banner, type BannerAction, Banners } from './banner';
import { downloadJson, h, setChildren, timeAgo } from './dom';
import { Hud, type HudSnapshot } from './hud';
import { type Settings, loadSettings, resolveSize, saveSettings, streamParams } from './settings';
import { settingsPanel } from './settings-panel';
import { ScreenWakeLock } from './wakelock';

/** Keyboard Lock API (Chromium; not in TypeScript's DOM lib). */
interface KeyboardLock {
  lock(codes?: string[]): Promise<void>;
  unlock(): void;
}
const keyboardLock = (): KeyboardLock | undefined => (navigator as Navigator & { keyboard?: KeyboardLock }).keyboard;

const STATS_LOG_MAX = 7200;
const FIT_RESIZE_DEBOUNCE_MS = 400;

const AGENT_ERRORS: Readonly<Record<string, string>> = {
  tcc_screen:
    'Screen Recording permission is not granted on the host. Grant it in System Settings › Privacy & Security (use Recovery).',
  tcc_input: 'Accessibility permission is not granted on the host: input cannot be injected.',
  capture_stopped: 'Screen capture stopped on the host.',
  display_asleep: 'The host display is asleep.',
  proto_mismatch: 'Viewer and agent speak incompatible protocol versions. Reload the page or update the agent.',
};

/** Machine name from `/m/{machine}` (or `?machine=` when opened as /viewer.html in dev). */
export function machineFromLocation(loc: Location = location): string | null {
  const m = /^\/m\/([^/]+)\/?$/.exec(loc.pathname);
  if (m) {
    try {
      return decodeURIComponent(m[1]!);
    } catch {
      return null;
    }
  }
  return new URLSearchParams(loc.search).get('machine');
}

/** The viewer page: connect screen, session, video, audio, input, HUD, banners. */
export class ViewerApp {
  private settings: Settings;
  private info: MachineInfo | null = null;
  /** Decoder support for `hello`, probed once at load. */
  private readonly decoders: Promise<DecoderCaps> = probeDecoders();
  private decoderCaps: DecoderCaps = { avc: false, hevc: false };
  private readonly banners = new Banners();
  private readonly hud = new Hud();
  private readonly wake = new ScreenWakeLock();
  private readonly canvas = h('canvas', { tabIndex: 0, width: 1728, height: 1118 });
  private readonly stage = h('div', { className: 'stage', hidden: true }, this.canvas);
  private readonly connectView = h('main', { className: 'connect' });
  private readonly toolbar = h('div', { className: 'toolbar card stack', hidden: true });
  private readonly session: Session;
  private readonly video: VideoPipeline;
  private readonly input: InputController;
  private readonly latency: LatencyTest;
  private audio: AudioPlayer | null = null;
  private agentHello: AgentHello | null = null;
  private agentStats: AgentStats | null = null;
  private agentState: AgentState | null = null;
  private requested: StreamParams | null = null;
  /** The session view is shown (not the connect screen). */
  private active = false;
  /** A session is wanted (connecting, streaming, reconnecting): wake lock, audio, HUD, leave-page guard. */
  private live = false;
  /** `pause` was sent for the current session (hidden tab). */
  private paused = false;
  private hudTimer = 0;
  private resizeTimer = 0;
  private readonly statsLog: Array<HudSnapshot & { agent: AgentStats | null }> = [];

  constructor(
    root: HTMLElement,
    private readonly machine: string,
  ) {
    this.settings = loadSettings(machine);
    document.title = `${machine} · rd`;
    this.session = new Session(
      {
        machine,
        viewerInfo: () => this.viewerInfo(),
        startParams: (hello) => this.startParams(hello),
      },
      {
        status: (s) => this.onStatus(s),
        session: (m) => this.setAgentState(m.agent),
        agentHello: (m) => {
          this.agentHello = m;
          // "auto" modifier mapping depends on the agent's OS.
          this.input.setOptions(this.inputOptions(this.settings));
        },
        control: (m) => this.onControl(m),
        video: (m: VideoMsg, rxUs: number) => this.video.onVideo(m, rxUs),
        audio: (m: AudioMsg) => this.audio?.onAudio(m),
        disconnected: () => this.onDisconnected(),
      },
    );
    this.video = new VideoPipeline(
      {
        canvas: this.canvas,
        sendAck: (b) => this.session.send(b),
        requestKeyframe: (reason) => this.session.sendJson({ t: 'keyframe_req', reason }),
        agentToViewerUs: (us) => this.session.clock.toViewerUs(us),
        resized: (w, hgt) => this.input.setVideoSize(w, hgt),
        drawn: (ctx) => this.latency.onDrawn(ctx),
      },
      this.settings.renderMode,
    );
    this.input = new InputController(
      this.canvas,
      this.session,
      {
        chord: (a) => this.onChord(a),
        pointerLockChanged: (locked) =>
          this.banners.set(
            'pointer-lock',
            locked ? { level: 'info', text: 'Pointer locked — press Esc to release.' } : null,
          ),
      },
      this.inputOptions(this.settings),
    );
    this.latency = new LatencyTest({
      canvas: this.canvas,
      tapKey: (hid) => this.input.tapKey(hid),
      pointer: () => this.input.pointer,
      report: (text, done) =>
        this.banners.set('latency', {
          level: 'info',
          text,
          actions: done ? [{ label: 'Dismiss', run: () => this.banners.set('latency', null) }] : [],
        }),
    });
    const handle = h(
      'button',
      { type: 'button', className: 'toolbar-handle', title: 'Menu', onclick: () => this.toggleToolbar() },
      '▾',
    );
    this.stage.append(handle, this.toolbar, this.hud.el);
    root.append(this.connectView, this.stage, this.banners.el);
    window.addEventListener('beforeunload', (e) => {
      if (this.live) e.preventDefault();
    });
    document.addEventListener('visibilitychange', () => this.onVisibility());
    document.addEventListener('fullscreenchange', () => {
      if (!document.fullscreenElement) keyboardLock()?.unlock();
    });
    window.addEventListener('resize', () => this.onResize());
  }

  async boot(): Promise<void> {
    this.renderConnect();
    const [caps, info] = await Promise.all([this.decoders, fetchMachine(this.machine).catch(() => null)]);
    this.decoderCaps = caps;
    this.info = info;
    this.renderConnect();
  }

  // -------------------------------------------------------------------------
  // connect screen

  private renderConnect(): void {
    const info = this.info;
    const name = info?.name ?? this.machine;
    const problems: string[] = [];
    if (!window.isSecureContext)
      problems.push('This page is not a secure context (HTTPS or localhost), so WebCodecs is unavailable.');
    else if (typeof VideoDecoder === 'undefined')
      problems.push('This browser has no WebCodecs VideoDecoder. Use a current Chrome or Edge.');
    const connect = h(
      'button',
      { type: 'button', className: 'primary big', disabled: problems.length > 0, onclick: () => this.connect() },
      'Connect',
    );
    const badges: HTMLElement[] = [];
    if (info) {
      badges.push(h('span', { className: `badge ${info.online ? 'ok' : 'bad'}` }, info.online ? 'online' : 'offline'));
      if (info.state?.locked) badges.push(h('span', { className: 'badge warn' }, 'locked'));
      if (info.state && !info.state.perms.screen)
        badges.push(h('span', { className: 'badge bad' }, 'no screen permission'));
      if (info.state && !info.state.perms.input)
        badges.push(h('span', { className: 'badge warn' }, 'no input permission'));
      if (info.viewer)
        badges.push(
          h('span', { className: 'badge warn' }, `in use by ${info.viewer.user} (${timeAgo(info.viewer.since_ms)})`),
        );
    }
    setChildren(
      this.connectView,
      h(
        'div',
        { className: 'card stack' },
        h('div', { className: 'row spread' }, h('h1', null, name), h('a', { href: '/' }, 'All machines')),
        info
          ? h(
              'div',
              { className: 'muted' },
              [info.os, info.agent_version && `agent ${info.agent_version}`].filter(Boolean).join(' · '),
            )
          : h('div', { className: 'muted' }, 'Machine status unavailable.'),
        badges.length ? h('div', { className: 'row' }, ...badges) : null,
        info?.viewer ? h('div', { className: 'muted' }, 'Connecting takes over the existing session.') : null,
        ...problems.map((p) => h('div', { className: 'badge bad' }, p)),
        settingsPanel(this.settings, (s) => this.applySettings(s)),
        h(
          'div',
          { className: 'row' },
          connect,
          ...this.recoveryActions().map((a) =>
            h('a', { className: 'button', href: a.href!, target: '_blank', rel: 'noopener noreferrer' }, a.label),
          ),
        ),
        h(
          'div',
          { className: 'muted' },
          `In session: ${CHORD_LABEL}+F fullscreen, +L pointer lock, +H stats, +D disconnect.`,
        ),
      ),
    );
  }

  private connect(): void {
    this.active = true;
    this.connectView.hidden = true;
    this.stage.hidden = false;
    this.canvas.focus();
    this.goLive();
    void this.decoders.then((caps) => {
      this.decoderCaps = caps;
      if (this.active) this.session.start();
    });
  }

  /** Reconnect after `stopped` (a banner click). */
  private reconnect(): void {
    this.goLive();
    this.session.start();
  }

  /** Session resources. Must run inside a user gesture (Connect / Reconnect click): the AudioContext. */
  private goLive(): void {
    if (this.settings.audio) this.startAudio();
    this.live = true;
    void this.wake.acquire();
    clearInterval(this.hudTimer);
    this.hudTimer = window.setInterval(() => this.tickHud(), 1000);
  }

  /** No session wanted any more: let the display sleep, stop audio and the HUD, no leave-page prompt. */
  private goIdle(): void {
    this.live = false;
    clearInterval(this.hudTimer);
    this.hudTimer = 0;
    this.audio?.close();
    this.audio = null;
    this.wake.release();
  }

  /** Must run inside a user gesture (Connect click / settings change). */
  private startAudio(): void {
    if (this.audio) return;
    try {
      this.audio = new AudioPlayer(this.settings.volume);
    } catch (e) {
      this.banners.set('audio', {
        level: 'warn',
        text: `Audio unavailable: ${e instanceof Error ? e.message : String(e)}`,
        actions: [{ label: 'Dismiss', run: () => this.banners.set('audio', null) }],
      });
    }
  }

  private disconnect(): void {
    this.session.stop();
  }

  private showConnectScreen(): void {
    this.active = false;
    this.goIdle();
    this.stopLatencyTest();
    if (document.pointerLockElement) document.exitPointerLock();
    if (document.fullscreenElement) void document.exitFullscreen();
    this.toolbar.hidden = true;
    this.stage.hidden = true;
    this.connectView.hidden = false;
    this.banners.clear();
    void fetchMachine(this.machine)
      .catch(() => null)
      .then((info) => {
        this.info = info;
        this.renderConnect();
      });
  }

  // -------------------------------------------------------------------------
  // session events

  private viewerInfo(): ViewerInfo {
    return {
      ua: navigator.userAgent,
      dpr: window.devicePixelRatio,
      viewport: [window.innerWidth, window.innerHeight],
      decoders: this.decoderCaps,
    };
  }

  private inputOptions(s: Settings): InputOptions {
    return {
      mapping: resolveMapping(s.mapping, navigator, this.agentHello?.agent.os),
      invertScroll: s.invertScroll,
    };
  }

  private startParams(hello: AgentHello): StreamParams {
    const display = hello.agent.displays[0];
    const size = resolveSize(this.settings.preset, display, this.viewport());
    this.requested = streamParams(this.settings, size, display?.id ?? 0);
    return this.requested;
  }

  private viewport() {
    return { width: window.innerWidth, height: window.innerHeight, dpr: window.devicePixelRatio };
  }

  private onStatus(s: SessionStatus): void {
    this.input.setEnabled(s.kind === 'streaming');
    const name = this.info?.name ?? this.machine;
    const reconnectNow: BannerAction = { label: 'Reconnect now', run: () => this.session.start() };
    const leave: BannerAction = { label: 'Disconnect', run: () => this.disconnect() };
    let banner: Banner | null = null;
    switch (s.kind) {
      case 'connecting':
        banner = s.attempt
          ? { level: 'info', text: `Reconnecting to ${name}…`, actions: [reconnectNow, leave] }
          : { level: 'info', text: `Connecting to ${name}…` };
        break;
      case 'joining':
        banner = { level: 'info', text: `Waiting for ${name} to join…` };
        break;
      case 'handshake':
        banner = { level: 'info', text: 'Starting session…' };
        break;
      case 'streaming':
        // (Re)connected while hidden: nothing is watching, so do not stream.
        if (document.hidden) this.pause();
        break;
      case 'retrying':
        banner = {
          level: 'warn',
          text: `Connection lost (${describeClose(s.code, s.reason)}). Reconnecting in ${(s.delayMs / 1000).toFixed(1)} s…`,
          actions: [reconnectNow, leave],
        };
        break;
      case 'offline':
        banner = {
          level: 'warn',
          text: `${name} is offline. Waiting for it to come back…`,
          actions: [...this.recoveryActions(), leave],
        };
        break;
      case 'stopped': {
        this.goIdle();
        // Signed out: a reload goes through the sign-in page and comes back here.
        const retry: BannerAction =
          s.code === CloseCode.FORBIDDEN
            ? { label: 'Reload', run: () => location.reload() }
            : { label: 'Reconnect', run: () => this.reconnect() };
        banner = { level: 'error', text: stoppedText(s.code, s.reason), actions: [retry, leave] };
        break;
      }
      case 'closed':
        this.showConnectScreen();
        return;
    }
    this.banners.set('connection', banner);
  }

  private onControl(m: Exclude<AgentMsg, AgentHello> | GwErrorMsg): void {
    switch (m.t) {
      case 'video_config':
        this.input.setVideoSize(m.width, m.height);
        this.banners.set('agent-error', null);
        void this.video.configure(m);
        return;
      case 'audio_config':
        void this.audio?.configure(m);
        return;
      case 'stats':
        this.agentStats = m;
        return;
      case 'state':
        this.setAgentState(m);
        return;
      case 'error':
        this.banners.set('agent-error', {
          level: 'error',
          text: AGENT_ERRORS[m.code] ?? `Agent error: ${m.code}${m.msg ? ` — ${m.msg}` : ''}`,
          actions: [...this.recoveryActions(), { label: 'Dismiss', run: () => this.banners.set('agent-error', null) }],
        });
        return;
      case 'gw_error':
        this.banners.set('gateway-error', {
          level: 'error',
          text: `Gateway: ${m.code}${m.msg ? ` — ${m.msg}` : ''}`,
          actions: [{ label: 'Dismiss', run: () => this.banners.set('gateway-error', null) }],
        });
    }
  }

  private setAgentState(st: AgentState): void {
    const wasLocked = this.agentState?.locked === true;
    this.agentState = st;
    // The agent drops input while the Mac is locked (Caps Lock syncs included): start from a clean slate.
    if (wasLocked && !st.locked && this.session.streaming) this.input.releaseAll();
    const name = this.info?.name ?? this.machine;
    const recovery = this.recoveryActions();
    let banner: Banner | null = null;
    if (st.locked)
      banner = { level: 'warn', text: `${name} is locked — use Recovery to unlock it.`, actions: recovery };
    else if (!st.perms.screen)
      banner = { level: 'error', text: `Screen Recording permission is missing on ${name}.`, actions: recovery };
    else if (!st.perms.input)
      banner = {
        level: 'warn',
        text: `Accessibility permission is missing on ${name}: input will not work.`,
        actions: recovery,
      };
    this.banners.set('agent-state', banner);
  }

  private onDisconnected(): void {
    this.video.reset();
    this.audio?.reset();
    this.input.forget();
    this.stopLatencyTest();
    this.agentStats = null;
    this.agentHello = null;
    this.requested = null;
    this.paused = false;
  }

  private recoveryActions(): BannerAction[] {
    return (this.info?.recovery ?? []).map((r) => ({ label: r.label, href: r.url }));
  }

  // -------------------------------------------------------------------------
  // settings, visibility, resize

  private applySettings(next: Settings): void {
    const prev = this.settings;
    this.settings = next;
    saveSettings(this.machine, next);
    this.video.setRenderMode(next.renderMode);
    this.input.setOptions(this.inputOptions(next));
    if (next.audio && this.live) this.startAudio();
    if (!next.audio && this.audio) {
      this.audio.close();
      this.audio = null;
    }
    this.audio?.setVolume(next.volume);
    if (
      next.preset !== prev.preset ||
      next.fps !== prev.fps ||
      next.maxKbps !== prev.maxKbps ||
      next.adaptive !== prev.adaptive ||
      next.audio !== prev.audio
    ) {
      this.sendParams();
    }
  }

  /** Send a `set` with whatever differs from what was last requested. */
  private sendParams(): void {
    const req = this.requested;
    if (!req || !this.session.streaming) return;
    const display = this.agentHello?.agent.displays.find((d) => d.id === req.display);
    const next = streamParams(this.settings, resolveSize(this.settings.preset, display, this.viewport()), req.display);
    const patch: StreamParamsPatch = {};
    for (const k of Object.keys(next) as Array<keyof StreamParams>) {
      if (next[k] !== req[k]) (patch as Record<string, unknown>)[k] = next[k];
    }
    if (Object.keys(patch).length === 0) return;
    this.requested = next;
    this.session.sendJson({ t: 'set', ...patch });
  }

  private onResize(): void {
    if (this.settings.preset !== 'fit') return;
    clearTimeout(this.resizeTimer);
    this.resizeTimer = window.setTimeout(() => this.sendParams(), FIT_RESIZE_DEBOUNCE_MS);
  }

  private onVisibility(): void {
    if (!this.session.streaming) return;
    if (document.hidden) {
      this.pause();
    } else if (this.paused) {
      this.paused = false;
      // No keyframe_req: the agent forces a keyframe on resume, and a decoder Chrome reclaimed while
      // hidden asks for its own.
      this.session.sendJson({ t: 'resume' });
    }
  }

  private pause(): void {
    if (!this.paused && this.session.sendJson({ t: 'pause' })) this.paused = true;
  }

  // -------------------------------------------------------------------------
  // chords, toolbar, HUD

  private onChord(a: ChordAction): void {
    switch (a) {
      case 'fullscreen':
        void this.toggleFullscreen();
        return;
      case 'pointer-lock':
        void this.input.togglePointerLock();
        return;
      case 'hud':
        this.hud.toggle();
        this.tickHud();
        return;
      case 'disconnect':
        this.disconnect();
        return;
      case 'latency-test':
        if (this.latency.active) this.stopLatencyTest();
        else this.latency.start();
    }
  }

  private stopLatencyTest(): void {
    if (!this.latency.active) return;
    this.latency.cancel();
    this.banners.set('latency', null);
  }

  private async toggleFullscreen(): Promise<void> {
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
        return;
      }
      await document.documentElement.requestFullscreen({ navigationUI: 'hide' });
      // Keyboard Lock (fullscreen only) lets Esc, Win, Alt+Tab etc. reach the host. Not in the desktop app: it
      // grabs the keyboard itself, and Chromium's own hook would get in its way.
      if (!window.__TAURI_INTERNALS__) await keyboardLock()?.lock();
    } catch {
      // Fullscreen refused (no user activation) or Keyboard Lock unsupported.
    }
    this.canvas.focus();
  }

  private toggleToolbar(): void {
    this.toolbar.hidden = !this.toolbar.hidden;
    if (this.toolbar.hidden) {
      this.canvas.focus();
      return;
    }
    const button = (label: string, run: () => void) => h('button', { type: 'button', onclick: run }, label);
    setChildren(
      this.toolbar,
      h(
        'div',
        { className: 'row' },
        button('Fullscreen', () => void this.toggleFullscreen()),
        button('Pointer lock', () => {
          this.toolbar.hidden = true;
          void this.input.togglePointerLock();
        }),
        button('Stats', () => this.onChord('hud')),
        button('Latency test', () => this.onChord('latency-test')),
        button('Download stats', () => this.downloadStats()),
        h('button', { type: 'button', className: 'primary', onclick: () => this.disconnect() }, 'Disconnect'),
      ),
      settingsPanel(this.settings, (s) => this.applySettings(s), 'tb'),
      h(
        'div',
        { className: 'row' },
        button('Close', () => this.toggleToolbar()),
      ),
    );
  }

  private tickHud(): void {
    this.hud.tick({
      video: this.video.stats,
      clock: this.session.clock,
      agent: this.agentStats,
      audio: this.audio?.stats ?? null,
      bufferedAmount: this.session.bufferedAmount,
      malformed: this.session.malformed,
      pointerLocked: this.input.pointerLocked,
    });
    const snap = this.hud.last;
    if (snap && this.session.streaming) {
      this.statsLog.push({ ...snap, agent: this.agentStats });
      if (this.statsLog.length > STATS_LOG_MAX) this.statsLog.shift();
    }
  }

  private downloadStats(): void {
    downloadJson(`rd-stats-${this.machine}-${new Date().toISOString().replace(/[:.]/g, '-')}.json`, {
      machine: this.machine,
      ua: navigator.userAgent,
      settings: this.settings,
      agent: this.agentHello?.agent ?? null,
      state: this.agentState,
      video: {
        codec: this.video.stats.codec,
        width: this.video.stats.width,
        height: this.video.stats.height,
        accel: this.video.stats.accel,
      },
      latencyTest: this.latency.lastResult,
      samples: this.statsLog,
    });
  }
}

function describeClose(code: number, reason: string): string {
  // Browsers give 1006 no reason, so one here is the viewer's own diagnosis.
  if (code === 1006 && reason) return reason;
  const known: Record<number, string> = {
    [CloseCode.AGENT_TIMEOUT]: 'agent did not answer',
    [CloseCode.SESSION_ENDED]: 'session ended by the agent',
    [CloseCode.REPLACED]: 'agent reconnected',
    1006: 'network',
  };
  return [known[code] ?? `code ${code}`, reason].filter(Boolean).join(': ');
}

function stoppedText(code: number, reason: string): string {
  switch (code) {
    case CloseCode.KICKED:
      return 'Another viewer took over this machine.';
    case CloseCode.FORBIDDEN:
      return 'Signed out or not allowed — reload the page to sign in again.';
    case CloseCode.PROTOCOL:
      return `Protocol error${reason ? `: ${reason}` : ''}. Reload the page or update the agent.`;
    case CloseCode.IDLE_TIMEOUT:
      return 'Session closed after a long period without input.';
    default:
      return `Session closed (${code}${reason ? `: ${reason}` : ''}).`;
  }
}
