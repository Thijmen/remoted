/**
 * How the viewer's modifier keys reach the remote side.
 * - "windows-to-mac": a Windows keyboard driving a Mac — Ctrl acts as Cmd (so Ctrl+C copies) and
 *   the Windows key acts as Ctrl; Alt stays Alt (= Option).
 * - "mac-to-windows": a Mac keyboard driving Windows — Cmd acts as Ctrl (so Cmd+C copies) and Ctrl
 *   acts as the Windows key; Option stays Alt.
 * - "passthrough": physical keys as-is.
 */
export type ModifierMapping = 'windows-to-mac' | 'mac-to-windows' | 'passthrough';

/** The setting: a mapping, or "auto" to pick one per session from the viewer's and the agent's OS. */
export type MappingSetting = 'auto' | ModifierMapping;

/** Both cross-platform mappings swap Ctrl and the Windows / Cmd key. */
const SWAP_CTRL_META: Readonly<Record<string, string>> = {
  ControlLeft: 'MetaLeft',
  ControlRight: 'MetaRight',
  MetaLeft: 'ControlLeft',
  MetaRight: 'ControlRight',
};

/** Older Firefox versions report the Windows/Cmd key as OS{Left,Right}. */
const LEGACY: Readonly<Record<string, string>> = { OSLeft: 'MetaLeft', OSRight: 'MetaRight' };

export function mapCode(code: string, mapping: ModifierMapping): string {
  const c = LEGACY[code] ?? code;
  return mapping === 'passthrough' ? c : (SWAP_CTRL_META[c] ?? c);
}

export interface PlatformHint {
  userAgentData?: { platform?: string } | undefined;
  platform?: string;
}

export function isWindowsViewer(nav: PlatformHint): boolean {
  return nav.userAgentData?.platform === 'Windows' || /Win/.test(nav.platform ?? '');
}

export function isMacViewer(nav: PlatformHint): boolean {
  return nav.userAgentData?.platform === 'macOS' || /Mac/.test(nav.platform ?? '');
}

/**
 * The mapping for a session. "auto": a Windows keyboard driving a Mac gets windows-to-mac, a Mac keyboard
 * driving Windows gets mac-to-windows; the same OS on both sides (or an agent not known yet) passes through.
 * `agentOs` is the agent's `hello` value ("macos" | "windows").
 */
export function resolveMapping(
  setting: MappingSetting,
  nav: PlatformHint,
  agentOs: string | undefined,
): ModifierMapping {
  if (setting !== 'auto') return setting;
  if (agentOs === 'macos' && isWindowsViewer(nav)) return 'windows-to-mac';
  if (agentOs === 'windows' && isMacViewer(nav)) return 'mac-to-windows';
  return 'passthrough';
}

const MODIFIER_CODES = new Set([
  'ShiftLeft',
  'ShiftRight',
  'ControlLeft',
  'ControlRight',
  'AltLeft',
  'AltRight',
  'MetaLeft',
  'MetaRight',
  'CapsLock',
  'Fn',
]);

export function isModifierCode(code: string): boolean {
  return MODIFIER_CODES.has(code);
}
