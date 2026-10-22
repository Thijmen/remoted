// JSON control messages — mirror of crates/rd-proto/src/control.rs (the source of truth).
// Text frames tagged by "t". Receivers ignore unknown "t" values and unknown fields; missing
// optional fields get the same defaults as serde on the Rust side.

export const PROTO_VERSION = 1;
export const MIN_PROTO_VERSION = 1;

/** WebSocket close codes used by the gateway and agent. */
export const CloseCode = {
  KICKED: 4001,
  FORBIDDEN: 4002,
  AGENT_OFFLINE: 4003,
  AGENT_TIMEOUT: 4004,
  PROTOCOL: 4005,
  SESSION_ENDED: 4006,
  IDLE_TIMEOUT: 4008,
  REPLACED: 4009,
  /** The agent is installing an update; it comes back by itself, usually within a minute. */
  AGENT_UPDATING: 4010,
  /** The agent does not support it (a file channel to an agent without file transfer). */
  UNSUPPORTED: 4011,
  /** Standard 1013: too many file channels to this machine, try again later. */
  TRY_AGAIN_LATER: 1013,
} as const;

// ---------------------------------------------------------------------------
// shared types

export interface Perms {
  screen: boolean;
  input: boolean;
}

export interface AgentState {
  locked: boolean;
  perms: Perms;
  in_session: boolean;
  /** Windows secure-desktop helper; agents before this feature omit it. */
  secure: SecureState;
  /** Windows: the agent's Remote Desktop session is disconnected (nothing to show). */
  disconnected: boolean;
}

/** Whether the Windows lock screen / UAC prompt can be shown in the viewer. */
export type SecureState = 'unavailable' | 'starting' | 'ready' | 'failed';

export interface DisplayDesc {
  id: number;
  w_pt: number;
  h_pt: number;
  scale: number;
}

export type VideoCodec = 'avc' | 'hevc';

export interface DecoderCaps {
  avc: boolean;
  hevc: boolean;
}

export interface ViewerInfo {
  ua: string;
  dpr: number;
  /** CSS pixels [w, h]. */
  viewport: [number, number];
  decoders: DecoderCaps;
}

export interface AgentInfo {
  /** "macos" | "windows" */
  os: string;
  version: string;
  displays: DisplayDesc[];
}

export interface StreamParams {
  width: number;
  height: number;
  fps: number;
  codec: VideoCodec;
  start_kbps: number;
  min_kbps: number;
  max_kbps: number;
  adaptive: boolean;
  audio: boolean;
  display: number;
}

export type StreamParamsPatch = Partial<StreamParams>;

export interface AgentStats {
  target_kbps: number;
  enc_kbps: number;
  cap_fps: number;
  enc_fps: number;
  drop_window: number;
  drop_post: number;
  enc_ms_p50: number;
  enc_ms_p99: number;
  rtt_min_ms: number;
  q_ms: number;
  inflight_frames: number;
  inflight_bytes: number;
  window_frames: number;
  window_bytes: number;
  /** Encode time counted in the window horizon (0 from agents that predate it). */
  window_enc_ms: number;
}

// ---------------------------------------------------------------------------
// viewer -> agent

export type ViewerMsg =
  | { t: 'hello'; proto: number; min_proto: number; viewer: ViewerInfo }
  | ({ t: 'start' } & StreamParams)
  | ({ t: 'set' } & StreamParamsPatch)
  | { t: 'keyframe_req'; reason: string }
  | { t: 'pause' }
  | { t: 'resume' }
  | { t: 'sas' };

// ---------------------------------------------------------------------------
// agent -> viewer, gateway -> viewer (same socket)

export interface AgentHello {
  t: 'hello';
  proto: number;
  agent: AgentInfo;
  caps: string[];
}

export interface VideoConfigMsg {
  t: 'video_config';
  epoch: number;
  /** WebCodecs codec string, e.g. "avc1.640033". */
  codec: string;
  width: number;
  height: number;
  /** base64 avcC record. */
  description: string;
}

export interface AudioConfigMsg {
  t: 'audio_config';
  codec: string;
  sample_rate: number;
  channels: number;
}

export type AgentStatsMsg = { t: 'stats' } & AgentStats;
export type AgentStateMsg = { t: 'state' } & AgentState;

export interface AgentErrorMsg {
  t: 'error';
  /** tcc_screen | tcc_input | capture_stopped | display_asleep | proto_mismatch | bad_request | internal */
  code: string;
  msg: string;
}

export type AgentMsg = AgentHello | VideoConfigMsg | AudioConfigMsg | AgentStatsMsg | AgentStateMsg | AgentErrorMsg;

export interface SessionMsg {
  t: 'session';
  machine: string;
  session: string;
  agent: AgentState;
}

export interface GwErrorMsg {
  t: 'gw_error';
  code: string;
  msg: string;
}

export type GatewayToViewer = SessionMsg | GwErrorMsg;

/** Everything that can arrive as text on the viewer session socket. */
export type ViewerIncoming = AgentMsg | GatewayToViewer;

// ---------------------------------------------------------------------------
// probe socket

export type ProbeTest = 'down' | 'up' | 'idle';

export type ProbeCtl =
  { t: 'run'; test: ProbeTest; kbps: number; fps: number; seconds: number; windowed: boolean } | { t: 'stop' };

export interface ProbeHello {
  t: 'hello';
  server_time_us: number;
  client_ip: string;
  user: string;
}

export interface ProbeDone {
  t: 'done';
  test: ProbeTest;
  elapsed_ms: number;
  frames: number;
  bytes: number;
  window_drops: number;
  stopped: boolean;
}

export interface ProbeErrorMsg {
  t: 'error';
  code: string;
  msg: string;
}

export type ProbeEvt = ProbeHello | ProbeDone | ProbeErrorMsg;

// ---------------------------------------------------------------------------
// HTTP API (/api/me, /api/machines)

export interface Me {
  email: string;
  dev: boolean;
  /** The gateway can add devices (portal "Add device"). */
  devices: boolean;
}

export interface RecoveryLink {
  label: string;
  url: string;
}

export type UpdatePhase = 'idle' | 'checking' | 'downloading' | 'installing' | 'unknown';
export type UpdateResult = 'ok' | 'rolled_back' | 'failed' | 'unknown';

/** A newer agent version. */
export interface AvailableUpdate {
  version: string;
  /** Release notes of every version newer than the running one, newest first, as Markdown. */
  notes: string;
  /** Release page (http(s) only, else empty). */
  url: string;
}

/** How an update attempt ended. */
export interface UpdateOutcome {
  from: string;
  to: string;
  result: UpdateResult;
  error: string;
  /** Unix ms (agent clock); 0 when unknown. */
  at_ms: number;
}

/** An agent's `update_status` (docs/protocol.md), as listed by `/api/machines`. */
export interface UpdateStatus {
  available: AvailableUpdate | null;
  can_install: boolean;
  /** Why not, when `can_install` is false. */
  cannot_install: string;
  phase: UpdatePhase;
  /** Why the last check or update request failed. */
  error: string;
  checked_ms: number | null;
  last: UpdateOutcome | null;
}

/** The agent is downloading or installing an update (it ends sessions and restarts meanwhile). */
export function updateInProgress(u: UpdateStatus | null): boolean {
  return u !== null && (u.phase === 'downloading' || u.phase === 'installing');
}

export interface MachineInfo {
  machine: string;
  name: string;
  /** Added in the portal: its token can be replaced and the device removed there. */
  managed: boolean;
  online: boolean;
  since_ms: number | null;
  os: string;
  agent_version: string;
  state: AgentState | null;
  viewer: { user: string; since_ms: number } | null;
  recovery: RecoveryLink[];
  /** Null for agents that do not report updates (before 1.5.0), and on older gateways. */
  update: UpdateStatus | null;
  /** The agent supports file transfer (docs/files.md); false on older agents and gateways. */
  files: boolean;
  /** The agent offers a terminal (docs/terminal.md); false on older agents and gateways. */
  terminal: boolean;
}

// ---------------------------------------------------------------------------
// defensive decoding of untrusted JSON

type Obj = Record<string, unknown>;

function isObj(v: unknown): v is Obj {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function num(o: Obj, k: string, def = 0): number {
  const v = o[k];
  return typeof v === 'number' && Number.isFinite(v) ? v : def;
}

function str(o: Obj, k: string, def = ''): string {
  const v = o[k];
  return typeof v === 'string' ? v : def;
}

function bool(o: Obj, k: string, def = false): boolean {
  const v = o[k];
  return typeof v === 'boolean' ? v : def;
}

/** Required fields: absent or wrongly typed => the whole message is rejected. */
class Invalid extends Error {}

function reqNum(o: Obj, k: string): number {
  const v = o[k];
  if (typeof v !== 'number' || !Number.isFinite(v)) throw new Invalid(k);
  return v;
}

function reqStr(o: Obj, k: string): string {
  const v = o[k];
  if (typeof v !== 'string') throw new Invalid(k);
  return v;
}

export function agentStateFrom(v: unknown): AgentState {
  const o = isObj(v) ? v : {};
  const p = isObj(o.perms) ? o.perms : {};
  return {
    locked: bool(o, 'locked'),
    perms: { screen: bool(p, 'screen'), input: bool(p, 'input') },
    in_session: bool(o, 'in_session'),
    secure: secureFrom(o.secure),
    disconnected: bool(o, 'disconnected'),
  };
}

function secureFrom(v: unknown): SecureState {
  return v === 'starting' || v === 'ready' || v === 'failed' ? v : 'unavailable';
}

function displayFrom(v: unknown): DisplayDesc | null {
  if (!isObj(v)) return null;
  return { id: num(v, 'id'), w_pt: num(v, 'w_pt'), h_pt: num(v, 'h_pt'), scale: num(v, 'scale', 1) };
}

function statsFrom(o: Obj): AgentStats {
  return {
    target_kbps: num(o, 'target_kbps'),
    enc_kbps: num(o, 'enc_kbps'),
    cap_fps: num(o, 'cap_fps'),
    enc_fps: num(o, 'enc_fps'),
    drop_window: num(o, 'drop_window'),
    drop_post: num(o, 'drop_post'),
    enc_ms_p50: num(o, 'enc_ms_p50'),
    enc_ms_p99: num(o, 'enc_ms_p99'),
    rtt_min_ms: num(o, 'rtt_min_ms'),
    q_ms: num(o, 'q_ms'),
    inflight_frames: num(o, 'inflight_frames'),
    inflight_bytes: num(o, 'inflight_bytes'),
    window_frames: num(o, 'window_frames'),
    window_bytes: num(o, 'window_bytes'),
    window_enc_ms: num(o, 'window_enc_ms'),
  };
}

function parseJsonObject(text: string): Obj | null {
  try {
    const v: unknown = JSON.parse(text);
    return isObj(v) ? v : null;
  } catch {
    return null;
  }
}

/**
 * Decode a text frame from the viewer session socket. Returns null for malformed JSON, unknown
 * message types or messages missing required fields (all of which are ignored by the caller).
 */
export function parseViewerIncoming(text: string): ViewerIncoming | null {
  const o = parseJsonObject(text);
  if (!o) return null;
  try {
    switch (o.t) {
      case 'session':
        return {
          t: 'session',
          machine: reqStr(o, 'machine'),
          session: reqStr(o, 'session'),
          agent: agentStateFrom(o.agent),
        };
      case 'gw_error':
        return { t: 'gw_error', code: reqStr(o, 'code'), msg: str(o, 'msg') };
      case 'hello': {
        const a = isObj(o.agent) ? o.agent : null;
        if (!a) throw new Invalid('agent');
        const displays = Array.isArray(a.displays) ? a.displays.map(displayFrom).filter((d) => d !== null) : [];
        const caps = Array.isArray(o.caps) ? o.caps.filter((c): c is string => typeof c === 'string') : [];
        return {
          t: 'hello',
          proto: reqNum(o, 'proto'),
          agent: { os: str(a, 'os'), version: str(a, 'version'), displays },
          caps,
        };
      }
      case 'video_config':
        return {
          t: 'video_config',
          epoch: reqNum(o, 'epoch'),
          codec: reqStr(o, 'codec'),
          width: reqNum(o, 'width'),
          height: reqNum(o, 'height'),
          description: reqStr(o, 'description'),
        };
      case 'audio_config':
        return {
          t: 'audio_config',
          codec: reqStr(o, 'codec'),
          sample_rate: reqNum(o, 'sample_rate'),
          channels: reqNum(o, 'channels'),
        };
      case 'stats':
        return { t: 'stats', ...statsFrom(o) };
      case 'state':
        return { t: 'state', ...agentStateFrom(o) };
      case 'error':
        return { t: 'error', code: reqStr(o, 'code'), msg: str(o, 'msg') };
      default:
        return null;
    }
  } catch (e) {
    if (e instanceof Invalid) return null;
    throw e;
  }
}

function isProbeTest(v: unknown): v is ProbeTest {
  return v === 'down' || v === 'up' || v === 'idle';
}

/** Decode a text frame from the probe socket (null = ignore). */
export function parseProbeEvt(text: string): ProbeEvt | null {
  const o = parseJsonObject(text);
  if (!o) return null;
  try {
    switch (o.t) {
      case 'hello':
        return {
          t: 'hello',
          server_time_us: num(o, 'server_time_us'),
          client_ip: str(o, 'client_ip'),
          user: str(o, 'user'),
        };
      case 'done': {
        if (!isProbeTest(o.test)) throw new Invalid('test');
        return {
          t: 'done',
          test: o.test,
          elapsed_ms: reqNum(o, 'elapsed_ms'),
          frames: reqNum(o, 'frames'),
          bytes: reqNum(o, 'bytes'),
          window_drops: num(o, 'window_drops'),
          stopped: bool(o, 'stopped'),
        };
      }
      case 'error':
        return { t: 'error', code: reqStr(o, 'code'), msg: str(o, 'msg') };
      default:
        return null;
    }
  } catch (e) {
    if (e instanceof Invalid) return null;
    throw e;
  }
}

function isHttpUrl(url: string): boolean {
  return /^https?:\/\//i.test(url);
}

function recoveryFrom(v: unknown): RecoveryLink[] {
  if (!Array.isArray(v)) return [];
  return v
    .filter(isObj)
    .map((r) => ({ label: str(r, 'label'), url: str(r, 'url') }))
    .filter((r) => r.label !== '' && isHttpUrl(r.url));
}

function oneOf<T extends string>(v: unknown, values: readonly T[], other: T): T {
  return typeof v === 'string' && (values as readonly string[]).includes(v) ? (v as T) : other;
}

const PHASES = ['idle', 'checking', 'downloading', 'installing'] as const;
const RESULTS = ['ok', 'rolled_back', 'failed'] as const;

/** Lenient like serde on the Rust side: missing fields get defaults, unknown values become `unknown`. */
export function updateStatusFrom(v: unknown): UpdateStatus | null {
  if (!isObj(v)) return null;
  const a = isObj(v.available) ? v.available : null;
  const l = isObj(v.last) ? v.last : null;
  const url = a ? str(a, 'url') : '';
  return {
    available:
      a && str(a, 'version')
        ? { version: str(a, 'version'), notes: str(a, 'notes'), url: isHttpUrl(url) ? url : '' }
        : null,
    can_install: bool(v, 'can_install'),
    cannot_install: str(v, 'cannot_install'),
    phase: v.phase === undefined ? 'idle' : oneOf(v.phase, PHASES, 'unknown'),
    error: str(v, 'error'),
    checked_ms: typeof v.checked_ms === 'number' && Number.isFinite(v.checked_ms) ? v.checked_ms : null,
    last: l
      ? {
          from: str(l, 'from'),
          to: str(l, 'to'),
          result: oneOf(l.result, RESULTS, 'unknown'),
          error: str(l, 'error'),
          at_ms: num(l, 'at_ms'),
        }
      : null,
  };
}

/** Decode the `/api/machines` response; malformed entries are skipped. */
export function parseMachines(v: unknown): MachineInfo[] {
  if (!Array.isArray(v)) return [];
  const out: MachineInfo[] = [];
  for (const m of v) {
    if (!isObj(m) || typeof m.machine !== 'string' || m.machine === '') continue;
    const viewer = isObj(m.viewer) ? { user: str(m.viewer, 'user'), since_ms: num(m.viewer, 'since_ms') } : null;
    out.push({
      machine: m.machine,
      name: str(m, 'name', m.machine) || m.machine,
      managed: bool(m, 'managed'),
      online: bool(m, 'online'),
      since_ms: typeof m.since_ms === 'number' ? m.since_ms : null,
      os: str(m, 'os'),
      agent_version: str(m, 'agent_version'),
      state: isObj(m.state) ? agentStateFrom(m.state) : null,
      viewer,
      recovery: recoveryFrom(m.recovery),
      update: updateStatusFrom(m.update),
      files: bool(m, 'files'),
      terminal: bool(m, 'terminal'),
    });
  }
  return out;
}

export function parseMe(v: unknown): Me | null {
  if (!isObj(v) || typeof v.email !== 'string') return null;
  return { email: v.email, dev: bool(v, 'dev'), devices: bool(v, 'devices') };
}

/** A device's credentials, returned once when it is added or its token is replaced. */
export interface DeviceIssued {
  machine: string;
  name: string | null;
  gateway: string;
  token: string;
  setupCode: string;
}

export function parseDeviceIssued(v: unknown): DeviceIssued | null {
  if (!isObj(v)) return null;
  const machine = str(v, 'machine');
  const setupCode = str(v, 'setup_code');
  if (!machine || !setupCode) return null;
  return {
    machine,
    name: typeof v.name === 'string' && v.name ? v.name : null,
    gateway: str(v, 'gateway'),
    token: str(v, 'token'),
    setupCode,
  };
}
