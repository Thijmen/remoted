/**
 * "windows-to-mac": a Windows keyboard driving a Mac — Ctrl acts as Cmd (so Ctrl+C copies) and
 * the Windows key acts as Ctrl; Alt stays Alt (= Option). "passthrough": physical keys as-is.
 */
export type ModifierMapping = 'windows-to-mac' | 'passthrough';

const WINDOWS_TO_MAC: Readonly<Record<string, string>> = {
  ControlLeft: 'MetaLeft',
  ControlRight: 'MetaRight',
  MetaLeft: 'ControlLeft',
  MetaRight: 'ControlRight',
};

/** Older Firefox versions report the Windows/Cmd key as OS{Left,Right}. */
const LEGACY: Readonly<Record<string, string>> = { OSLeft: 'MetaLeft', OSRight: 'MetaRight' };

export function mapCode(code: string, mapping: ModifierMapping): string {
  const c = LEGACY[code] ?? code;
  return mapping === 'windows-to-mac' ? (WINDOWS_TO_MAC[c] ?? c) : c;
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

/** Windows keyboards get Ctrl→Cmd by default; everything else passes through. */
export function defaultMapping(nav: PlatformHint): ModifierMapping {
  return isWindowsViewer(nav) ? 'windows-to-mac' : 'passthrough';
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
