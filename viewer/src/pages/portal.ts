import '../ui/style.css';
import {
  HttpError,
  addDevice,
  fetchMachines,
  fetchMe,
  machineIdFrom,
  removeDevice,
  replaceDeviceToken,
} from '../net/api';
import type { DeviceIssued, MachineInfo } from '../proto/control';
import { byId, h, setChildren, timeAgo } from '../ui/dom';
import { icon } from '../ui/icons';

const REFRESH_MS = 5000;

const root = byId('app');
const who = h('span', { className: 'muted' });
const addButton = h('button', { className: 'primary', type: 'button', hidden: true }, icon('plus', 16), 'Add device');
const list = h('div', { className: 'machines' });
const status = h('div', { className: 'muted' });
/** Add device, and the one-time setup code after adding a device or replacing its token. */
const dialog = h('dialog');
/** The gateway can add devices. */
let canAdd = false;

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
      h('a', { className: 'button primary', href: `/m/${encodeURIComponent(m.machine)}` }, 'Connect'),
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

async function refresh(): Promise<void> {
  try {
    const machines = await fetchMachines();
    const empty = canAdd ? 'No machines yet: add one with Add device.' : 'No machines registered.';
    setChildren(
      list,
      ...(machines.length ? machines.map(machineCard) : [h('div', { className: 'card muted' }, empty)]),
    );
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
