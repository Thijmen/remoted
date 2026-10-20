import type { KeyboardGrabState } from '../input/controller';
import type { ModifierMapping } from '../input/modifiers';
import { h } from './dom';
import { icon } from './icons';

/** rd Desktop's `rd_keyboard_status` answer. */
export interface KeyboardStatus {
  /** "macos", "windows" or "other". */
  platform: string;
  version: string;
  /** The app can grab the keyboard on this platform. */
  grab: boolean;
  /** macOS: the Accessibility permission. null elsewhere. */
  accessibility: boolean | null;
  /** macOS: Secure Keyboard Entry is on. null elsewhere. */
  secureInput: boolean | null;
}

export function parseKeyboardStatus(raw: unknown): KeyboardStatus | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const o = raw as Record<string, unknown>;
  const optBool = (v: unknown) => (typeof v === 'boolean' ? v : null);
  if (typeof o.platform !== 'string' || typeof o.grab !== 'boolean') return null;
  return {
    platform: o.platform,
    version: typeof o.version === 'string' ? o.version : '',
    grab: o.grab,
    accessibility: optBool(o.accessibility),
    secureInput: optBool(o.secureInput),
  };
}

/**
 * The status as far as the page knows it: asked and answered, still being asked, or the app cannot say (an
 * rd Desktop from before `rd_keyboard_status`).
 */
export type StatusState = KeyboardStatus | 'loading' | 'unavailable';

/** rd Desktop's refusal when the Accessibility permission is missing (macOS); older apps report only this. */
const ACCESSIBILITY_REFUSAL = /Accessibility permission/;

/** The grab fails (or would) for want of the macOS Accessibility permission. A fresh status wins over the refusal. */
export function needsAccessibility(grab: KeyboardGrabState, status: StatusState | null): boolean {
  if (typeof status === 'object' && status !== null && status.accessibility !== null) return !status.accessibility;
  return grab.refusal !== null && ACCESSIBILITY_REFUSAL.test(grab.refusal);
}

export interface TroubleshootInput {
  /** The remote machine's name. */
  machine: string;
  /** This computer's OS. */
  viewer: 'mac' | 'windows' | 'other';
  grab: KeyboardGrabState;
  status: StatusState | null;
  mapping: ModifierMapping;
  /** The agent's OS ("macos" | "windows"), once known. */
  remoteOs: string | undefined;
  /** The agent may inject input (its permission); null when not known. */
  hostInput: boolean | null;
}

export type CheckLevel = 'ok' | 'warn' | 'bad' | 'info';
export type CheckAction = 'open-accessibility' | 'recheck';

export interface Check {
  id: string;
  level: CheckLevel;
  title: string;
  detail?: string;
  actions?: CheckAction[];
}

const ACCESSIBILITY_STEPS =
  'In System Settings › Privacy & Security › Accessibility, switch on rd Desktop. If it is on already, remove it ' +
  'with – and add it again: after an update the old entry may no longer match the app. Then click into the video.';

/** What the Troubleshoot screen lists, most fundamental first. */
export function troubleshootChecks(i: TroubleshootInput): Check[] {
  const out: Check[] = [];
  const keys = i.viewer === 'windows' ? 'Alt+Tab, the Windows key' : 'Cmd+Tab, Cmd+Space';
  const here = i.viewer === 'mac' ? 'this Mac' : i.viewer === 'windows' ? 'this PC' : 'this computer';
  const status = typeof i.status === 'object' ? i.status : null;

  if (!i.grab.inApp) {
    out.push({
      id: 'app',
      level: 'warn',
      title: 'Running in a browser',
      detail: `A browser tab cannot capture system shortcuts (${keys}): they stay on ${here}. Open ${i.machine} in rd Desktop to send them.`,
    });
  } else {
    out.push({ id: 'app', level: 'ok', title: status?.version ? `rd Desktop ${status.version}` : 'rd Desktop' });
    out.push(
      i.grab.refusal === null
        ? {
            id: 'grab',
            level: 'ok',
            title: `Shortcuts go to ${i.machine}`,
            detail: `${keys} and the other system shortcuts are sent while the video has focus.`,
          }
        : { id: 'grab', level: 'bad', title: `Shortcuts stay on ${here}`, detail: i.grab.refusal },
    );
    if (i.viewer === 'mac' || status?.platform === 'macos') out.push(accessibilityCheck(i, status));
    if (status?.secureInput)
      out.push({
        id: 'secure-input',
        level: 'warn',
        title: 'Secure Keyboard Entry is on',
        detail:
          'A password field or an app setting (e.g. Terminal › Secure Keyboard Entry) has it on, so macOS hides keys from rd Desktop. Close the password prompt or turn the setting off.',
        actions: ['recheck'],
      });
  }

  out.push(mappingCheck(i));

  if (i.hostInput !== null)
    out.push(
      i.hostInput
        ? { id: 'host-input', level: 'ok', title: `${i.machine} accepts input` }
        : {
            id: 'host-input',
            level: 'bad',
            title: `${i.machine} cannot inject input`,
            detail: `The input permission is missing on ${i.machine}, so no keys or clicks arrive there. Grant it on that machine (or through Recovery).`,
          },
    );
  return out;
}

function accessibilityCheck(i: TroubleshootInput, status: KeyboardStatus | null): Check {
  if (status && status.accessibility !== null)
    return status.accessibility
      ? { id: 'accessibility', level: 'ok', title: 'Accessibility permission granted' }
      : {
          id: 'accessibility',
          level: 'bad',
          title: 'Accessibility permission missing',
          detail: `rd Desktop needs it to capture shortcuts. ${ACCESSIBILITY_STEPS}`,
          actions: ['open-accessibility', 'recheck'],
        };
  if (i.status === 'loading' || i.status === null)
    return { id: 'accessibility', level: 'info', title: 'Checking the Accessibility permission…' };
  // An rd Desktop from before rd_keyboard_status: only its refusal tells.
  return needsAccessibility(i.grab, null)
    ? {
        id: 'accessibility',
        level: 'bad',
        title: 'Accessibility permission missing',
        detail: `rd Desktop needs it to capture shortcuts. ${ACCESSIBILITY_STEPS}`,
      }
    : {
        id: 'accessibility',
        level: 'info',
        title: 'Accessibility permission',
        detail: 'Update rd Desktop to check it here.',
      };
}

function mappingCheck(i: TroubleshootInput): Check {
  switch (i.mapping) {
    case 'mac-to-windows':
      return {
        id: 'mapping',
        level: 'info',
        title: 'Modifier keys: Mac keyboard → Windows',
        detail: 'Cmd acts as Ctrl (Cmd+C copies), Ctrl as the Windows key, and Cmd+Tab is sent as Alt+Tab.',
      };
    case 'windows-to-mac':
      return {
        id: 'mapping',
        level: 'info',
        title: 'Modifier keys: Windows keyboard → Mac',
        detail: 'Ctrl acts as Cmd (Ctrl+C copies), the Windows key as Ctrl, and Alt+Tab is sent as Cmd+Tab.',
      };
    case 'passthrough':
      if (i.viewer === 'mac' && i.remoteOs === 'windows')
        return {
          id: 'mapping',
          level: 'warn',
          title: 'Modifier keys: passed through',
          detail:
            'Cmd arrives as the Windows key, so Cmd+Tab opens Task View. Choose “Mac keyboard → Windows” (or Automatic) under Modifier keys to send Alt+Tab.',
        };
      if (i.viewer === 'windows' && i.remoteOs === 'macos')
        return {
          id: 'mapping',
          level: 'warn',
          title: 'Modifier keys: passed through',
          detail:
            'Alt arrives as Option, so Alt+Tab does not switch apps. Choose “Windows keyboard → Mac” (or Automatic) under Modifier keys to send Cmd+Tab.',
        };
      return {
        id: 'mapping',
        level: 'info',
        title: 'Modifier keys: passed through',
        detail: 'Keys are sent as they are.',
      };
  }
}

const ACTION_LABEL: Readonly<Record<CheckAction, string>> = {
  'open-accessibility': 'Open Accessibility settings',
  recheck: 'Check again',
};

/** The Troubleshoot view of the session menu. */
export function troubleshootPanel(
  checks: readonly Check[],
  run: (a: CheckAction) => void,
  back: () => void,
): HTMLElement {
  return h(
    'div',
    { className: 'troubleshoot stack' },
    h(
      'div',
      { className: 'row' },
      h('button', { type: 'button', onclick: back }, icon('back', 16), 'Settings'),
      h('h3', null, 'Troubleshoot'),
    ),
    h(
      'ul',
      { className: 'checks' },
      ...checks.map((c) =>
        h(
          'li',
          { className: `check ${c.level}` },
          h('span', { className: `dot ${c.level === 'info' ? '' : c.level}` }),
          h(
            'div',
            { className: 'check-body' },
            h('div', { className: 'check-title' }, c.title),
            c.detail ? h('div', { className: 'muted' }, c.detail) : null,
            c.actions?.length
              ? h(
                  'div',
                  { className: 'row' },
                  ...c.actions.map((a) => h('button', { type: 'button', onclick: () => run(a) }, ACTION_LABEL[a])),
                )
              : null,
          ),
        ),
      ),
    ),
  );
}
