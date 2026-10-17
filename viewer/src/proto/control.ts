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
}

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
}

// ---------------------------------------------------------------------------
// viewer -> agent

export type ViewerMsg =
  | { t: 'hello'; proto: number; min_proto: number; viewer: ViewerInfo }
  | ({ t: 'start' } & StreamParams)
  | ({ t: 'set' } & StreamParamsPatch)
  | { t: 'keyframe_req'; reason: string }
  | { t: 'pause' }
  | { t: 'resume' };

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
}

export interface RecoveryLink {
  label: string;
  url: string;
}

export interface MachineInfo {
  machine: string;
  name: string;
  online: boolean;
  since_ms: number | null;
  os: string;
  agent_version: string;
  state: AgentState | null;
  viewer: { user: string; since_ms: number } | null;
  recovery: RecoveryLink[];
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
  };
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

function recoveryFrom(v: unknown): RecoveryLink[] {
  if (!Array.isArray(v)) return [];
  return v
    .filter(isObj)
    .map((r) => ({ label: str(r, 'label'), url: str(r, 'url') }))
    .filter((r) => r.label !== '' && /^https?:\/\//i.test(r.url));
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
      online: bool(m, 'online'),
      since_ms: typeof m.since_ms === 'number' ? m.since_ms : null,
      os: str(m, 'os'),
      agent_version: str(m, 'agent_version'),
      state: isObj(m.state) ? agentStateFrom(m.state) : null,
      viewer,
      recovery: recoveryFrom(m.recovery),
    });
  }
  return out;
}

export function parseMe(v: unknown): Me | null {
  if (!isObj(v) || typeof v.email !== 'string') return null;
  return { email: v.email, dev: bool(v, 'dev') };
}
