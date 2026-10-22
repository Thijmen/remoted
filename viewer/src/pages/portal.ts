import '../ui/style.css';
import {
  HttpError,
  addDevice,
  fetchMachines,
  fetchMe,
  machineIdFrom,
  removeDevice,
  replaceDeviceToken,
  requestAgentUpdate,
} from '../net/api';
import type { DeviceIssued, MachineInfo } from '../proto/control';
import { type PendingUpdate, advancePending, pendingUpdate, updateCardView } from '../ui/agent-update';
import { type Child, byId, h, setChildren, timeAgo } from '../ui/dom';
import { icon } from '../ui/icons';
import { renderMarkdown } from '../ui/markdown';
import { terminalUrl } from '../ui/terminal-links';

const REFRESH_MS = 5000;
/** A refused update request is shown on its card this long. */
const UPDATE_ERROR_MS = 60_000;

const root = byId('app');
const who = h('span', { className: 'muted' });
const addButton = h('button', { className: 'primary', type: 'button', hidden: true }, icon('plus', 16), 'Add device');
const list = h('div', { className: 'machines' });
const status = h('div', { className: 'muted' });
/** Add device, and the one-time setup code after adding a device or replacing its token. */
const dialog = h('dialog');
/** The gateway can add devices. */
let canAdd = false;
/** The last listing, re-rendered when local state changes between refreshes. */
let machines: MachineInfo[] = [];
// The list is rebuilt on every refresh, so what the cards need to remember lives here, by machine.
/** Updates asked for from this page, until the agent answers. */
const pendingUpdates = new Map<string, PendingUpdate>();
/** Update requests the gateway refused. */
const updateErrors = new Map<string, { text: string; at: number }>();

root.append(
  h(
    'main',
    { className: 'page' },
    h(
      'header',
      { className: 'portal-head' },
      h(
        'div',
        { className: 'brand' },
        h('span', { className: 'brand-mark' }, icon('monitor', 22)),
        h('h1', null, 'Remote desktops'),
      ),
      h('div', { className: 'row' }, who, addButton),
    ),
    list,
    h(
      'div',
      { className: 'row spread muted portal-foot' },
      status,
      h(
        'div',
        { className: 'row' },
        h('a', { href: '/probe' }, 'Network probe'),
        '·',
        h('a', { href: '/latency-target' }, 'Latency target'),
        '·',
        h('a', { href: '/keytest' }, 'Key test'),
      ),
    ),
  ),
  dialog,
);
addButton.addEventListener('click', openAddDialog);

function errorText(e: unknown): string {
  return e instanceof HttpError ? e.message : 'Could not reach the gateway.';
}

function stateBadges(m: MachineInfo): HTMLElement[] {
  const out: HTMLElement[] = [];
  const st = m.state;
  if (m.online && st) {
    if (st.locked) out.push(h('span', { className: 'badge warn' }, 'locked'));
    if (!st.perms.screen) out.push(h('span', { className: 'badge bad' }, 'no screen permission'));
    if (!st.perms.input) out.push(h('span', { className: 'badge warn' }, 'no input permission'));
  }
  if (m.viewer)
    out.push(h('span', { className: 'badge warn' }, `in session: ${m.viewer.user}, ${timeAgo(m.viewer.since_ms)}`));
  return out;
}

/** New token / Remove, for devices added in the portal. */
function deviceActions(m: MachineInfo): HTMLElement {
  const newToken = h('button', { className: 'small', type: 'button' }, 'New token');
  newToken.addEventListener('click', () => {
    if (!confirm(`Give ${m.name} a new token? It disconnects until you run its setup again with the new code.`)) {
      return;
    }
    replaceDeviceToken(m.machine).then(
      (d) => showIssued(d, true),
      (e: unknown) => (status.textContent = errorText(e)),
    );
  });
  const remove = h('button', { className: 'small danger', type: 'button' }, 'Remove');
  remove.addEventListener('click', () => {
    if (!confirm(`Remove ${m.name}? Its agent disconnects and its token stops working.`)) return;
    removeDevice(m.machine).then(
      () => void refresh(),
      (e: unknown) => (status.textContent = errorText(e)),
    );
  });
  return h('div', { className: 'row' }, newToken, remove);
}

/** Ask the agent to install `version`; with a session open, only after confirming. */
function startUpdate(m: MachineInfo, version: string): void {
  if (m.viewer && !confirm(`Updating ends the session of ${m.viewer.user}. ${m.name} is back in about 15 s.`)) {
    return;
  }
  updateErrors.delete(m.machine);
  pendingUpdates.set(m.machine, pendingUpdate(m, version, Date.now()));
  render();
  requestAgentUpdate(m.machine, version).then(
    () => void refresh(),
    (e: unknown) => {
      pendingUpdates.delete(m.machine);
      updateErrors.set(m.machine, { text: `The update did not start: ${errorText(e)}`, at: Date.now() });
      render();
    },
  );
}

function updateButton(m: MachineInfo, version: string, className: string, onClick?: () => void): HTMLButtonElement {
  const b = h('button', { className, type: 'button' }, 'Update');
  b.addEventListener('click', () => {
    onClick?.();
    startUpdate(m, version);
  });
  return b;
}

/** The release notes of every version newer than the agent's, from its update check. */
function openNotes(m: MachineInfo): void {
  const a = m.update?.available;
  if (!a) return;
  const canUpdate = m.online && m.update?.can_install === true;
  const close = closeButton('Close');
  setChildren(
    dialog,
    h(
      'div',
      { className: 'stack' },
      h('h2', null, `rd agent ${a.version}`),
      h('p', { className: 'muted', style: 'margin:0' }, `${m.name} runs ${m.agent_version || 'an older version'}.`),
      a.notes.trim()
        ? h('div', { className: 'release-notes' }, renderMarkdown(a.notes))
        : h('p', { className: 'muted' }, 'No release notes.'),
      h(
        'div',
        { className: 'row spread' },
        a.url ? h('a', { href: a.url, target: '_blank', rel: 'noopener noreferrer' }, 'Release page') : h('span'),
        h(
          'div',
          { className: 'row' },
          close,
          canUpdate ? updateButton(m, a.version, 'primary', () => dialog.close()) : null,
        ),
      ),
    ),
  );
  dialog.showModal();
  // Not the first link in the notes.
  close.focus();
}

/** Agent updates on a card: available, in progress, the last outcome. Nothing for older agents. */
function updateSection(m: MachineInfo): HTMLElement | null {
  const now = Date.now();
  const refused = updateErrors.get(m.machine);
  if (refused && now - refused.at > UPDATE_ERROR_MS) updateErrors.delete(m.machine);
  const view = updateCardView(m, pendingUpdates.get(m.machine) ?? null, now);
  if (!view) return null;
  const rows: Child[] = [];
  if (view.updating !== null) {
    rows.push(
      h(
        'div',
        { className: 'row' },
        h(
          'span',
          { className: 'badge accent' },
          h('span', { className: 'spinner' }),
          view.updating ? `Updating to ${view.updating}…` : 'Updating…',
        ),
      ),
    );
  } else if (view.available) {
    const a = view.available;
    const checked = m.update?.checked_ms;
    const notes = h('button', { className: 'small', type: 'button' }, "What's new");
    notes.addEventListener('click', () => openNotes(m));
    rows.push(
      h(
        'div',
        { className: 'row' },
        h(
          'span',
          { className: 'badge accent', title: checked ? `Checked ${timeAgo(checked)}` : '' },
          `Update available: ${a.version}`,
        ),
        // The buttons wrap together on a narrow card.
        h(
          'div',
          { className: 'row' },
          notes,
          view.canInstall && m.online ? updateButton(m, a.version, 'small primary') : null,
        ),
      ),
    );
    if (!view.canInstall) {
      rows.push(
        h(
          'div',
          { className: 'muted note' },
          `Update by hand: ${view.cannotInstall || 'this agent cannot update itself.'}`,
        ),
      );
    }
  }
  const o = view.outcome;
  if (o) {
    rows.push(
      o.level === 'muted'
        ? h('div', { className: 'muted note' }, o.at ? `${o.text} ${timeAgo(o.at)}` : o.text)
        : h('div', { className: `notice ${o.level} note` }, o.text),
    );
  }
  if (view.error) rows.push(h('div', { className: 'muted note' }, view.error));
  if (refused) rows.push(h('div', { className: 'error note' }, refused.text));
  return rows.length ? h('div', { className: 'machine-update' }, ...rows) : null;
}

function osLabel(os: string): string {
  return ({ macos: 'macOS', windows: 'Windows' } as Record<string, string>)[os] ?? os;
}

function machineCard(m: MachineInfo): HTMLElement {
  const details = [
    m.machine !== m.name ? m.machine : '',
    osLabel(m.os),
    m.agent_version && `agent ${m.agent_version}`,
  ].filter(Boolean);
  const seen =
    m.since_ms !== null ? (m.online ? `online since ${timeAgo(m.since_ms)}` : `last seen ${timeAgo(m.since_ms)}`) : '';
  const badges = stateBadges(m);
  return h(
    'section',
    { className: `card machine-card${m.online ? '' : ' offline'}` },
    h(
      'div',
      { className: 'machine-head' },
      h('div', { className: 'machine-avatar' }, icon('monitor', 24)),
      h(
        'div',
        { className: 'machine-title' },
        h('h2', null, m.name),
        h('div', { className: 'muted' }, details.join(' · ')),
      ),
    ),
    h(
      'div',
      { className: 'row' },
      h(
        'span',
        { className: `pill ${m.online ? 'ok' : 'bad'}` },
        h('span', { className: `dot ${m.online ? 'ok' : 'bad'}` }),
        m.online ? 'Online' : 'Offline',
      ),
      seen ? h('span', { className: 'muted' }, seen) : null,
    ),
    badges.length ? h('div', { className: 'row' }, ...badges) : null,
    updateSection(m),
    m.recovery.length
      ? h(
          'div',
          { className: 'row' },
          h('span', { className: 'muted' }, 'Recovery:'),
          ...m.recovery.map((r) =>
            h('a', { className: 'button small', href: r.url, target: '_blank', rel: 'noopener noreferrer' }, r.label),
          ),
        )
      : null,
    h(
      'div',
      { className: 'row spread actions' },
      m.managed ? deviceActions(m) : h('span'),
      h(
        'div',
        { className: 'row' },
        m.files && m.online
          ? h(
              'a',
              {
                className: 'button',
                href: `/m/${encodeURIComponent(m.machine)}#files`,
                title: 'Browse, download and upload files without taking over the screen',
              },
              'Files',
            )
          : null,
        m.terminal && m.online
          ? h(
              'a',
              {
                className: 'button',
                href: terminalUrl(m.machine),
                title: 'A shell on this machine, in its own page, without taking over the screen',
              },
              'Terminal',
            )
          : null,
        h('a', { className: 'button primary', href: `/m/${encodeURIComponent(m.machine)}` }, 'Connect'),
      ),
    ),
  );
}

function closeButton(label: string): HTMLButtonElement {
  const b = h('button', { type: 'button' }, label);
  b.addEventListener('click', () => dialog.close());
  return b;
}

function openAddDialog(): void {
  const name = h('input', { type: 'text', maxLength: 64, placeholder: 'Windows PC', autocomplete: 'off' });
  const machine = h('input', {
    type: 'text',
    required: true,
    maxLength: 64,
    pattern: '[A-Za-z0-9_\\-]{1,64}',
    placeholder: 'windows-pc',
    autocomplete: 'off',
    spellcheck: false,
  });
  // The id follows the name until it is edited by hand.
  let machineEdited = false;
  name.addEventListener('input', () => {
    if (!machineEdited) machine.value = machineIdFrom(name.value);
  });
  machine.addEventListener('input', () => (machineEdited = machine.value !== ''));
  const error = h('div', { className: 'error', hidden: true });
  const submit = h('button', { className: 'primary', type: 'submit' }, 'Add');
  const form = h(
    'form',
    { className: 'stack' },
    h('h2', null, 'Add device'),
    h(
      'p',
      { className: 'muted', style: 'margin:0' },
      'A Mac or Windows PC that runs rd-agent. You get a setup code to paste into its setup.',
    ),
    h('label', null, 'Name', name),
    h(
      'label',
      null,
      'Machine id',
      machine,
      h(
        'span',
        { className: 'muted' },
        'Letters, digits, - and _. It is part of the device address and cannot change.',
      ),
    ),
    error,
    h('div', { className: 'row', style: 'justify-content:flex-end' }, closeButton('Cancel'), submit),
  );
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    if (!form.reportValidity()) return;
    submit.disabled = true;
    error.hidden = true;
    addDevice(machine.value.trim(), name.value.trim()).then(
      (d) => showIssued(d, false),
      (e: unknown) => {
        error.textContent = errorText(e);
        error.hidden = false;
        submit.disabled = false;
      },
    );
  });
  setChildren(dialog, form);
  dialog.showModal();
  name.focus();
}

function detail(label: string, value: string): HTMLElement {
  return h('div', null, `${label}: `, h('code', { style: 'word-break:break-all' }, value));
}

/** The setup code, shown once. */
function showIssued(d: DeviceIssued, replaced: boolean): void {
  const title = d.name ?? d.machine;
  const code = h('textarea', { className: 'setup-code', readOnly: true, rows: 4, spellcheck: false });
  code.value = d.setupCode;
  code.addEventListener('focus', () => code.select());
  const copy = h('button', { className: 'primary', type: 'button' }, 'Copy setup code');
  copy.addEventListener('click', () => {
    navigator.clipboard.writeText(d.setupCode).then(
      () => (copy.textContent = 'Copied'),
      () => {
        code.focus();
        copy.textContent = 'Copy with Ctrl+C / Cmd+C';
      },
    );
  });
  setChildren(
    dialog,
    h(
      'div',
      { className: 'stack' },
      h('h2', null, replaced ? `New token for ${title}` : `${title} added`),
      h(
        'p',
        { style: 'margin:0' },
        'On the device, run ',
        h('code', null, 'rd-agent setup'),
        ' (on Windows: the last page of the installer, or the “rd agent setup” shortcut) and paste this setup code:',
      ),
      code,
      h(
        'p',
        { className: 'muted', style: 'margin:0' },
        'It is shown only once and works like a password: anyone who has it can connect as this device.',
        replaced ? ' The old token no longer works.' : '',
      ),
      h(
        'details',
        null,
        h('summary', null, 'Set up without the code'),
        h(
          'div',
          { className: 'stack muted' },
          detail('Gateway URL', d.gateway),
          detail('Machine id', d.machine),
          detail('Token', d.token),
        ),
      ),
      h('div', { className: 'row', style: 'justify-content:flex-end' }, copy, closeButton('Done')),
    ),
  );
  if (!dialog.open) dialog.showModal();
  void refresh();
}

/** Rebuild the list from the last listing and this page's own state. */
function render(): void {
  const now = Date.now();
  for (const m of machines) {
    const p = pendingUpdates.get(m.machine);
    const next = p && advancePending(p, m, now);
    if (next) pendingUpdates.set(m.machine, next);
    else pendingUpdates.delete(m.machine);
  }
  const empty = canAdd ? 'No machines yet: add one with Add device.' : 'No machines registered.';
  setChildren(list, ...(machines.length ? machines.map(machineCard) : [h('div', { className: 'card muted' }, empty)]));
}

async function refresh(): Promise<void> {
  try {
    machines = await fetchMachines();
    render();
    status.textContent = `Updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    status.textContent =
      e instanceof HttpError ? `Could not load machines (HTTP ${e.status}).` : 'Could not reach the gateway.';
  }
}

void fetchMe()
  .then((me) => {
    if (!me) return;
    who.textContent = me.dev ? `${me.email} (dev mode)` : me.email;
    canAdd = me.devices;
    addButton.hidden = !me.devices;
    void refresh();
  })
  .catch(() => undefined);
void refresh();
setInterval(() => {
  if (!document.hidden) void refresh();
}, REFRESH_MS);
