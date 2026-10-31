import '../ui/style.css';
import {
  HttpError,
  addDevice,
  fetchActivity,
  fetchMachines,
  fetchMe,
  machineIdFrom,
  removeDevice,
  replaceDeviceToken,
  requestAgentUpdate,
} from '../net/api';
import type { ActivityEvent, DeviceIssued, MachineInfo } from '../proto/control';
import { activityLines } from '../ui/activity';
import { type PendingUpdate, advancePending, pendingUpdate, updateCardView } from '../ui/agent-update';
import { type Command, CommandPalette } from '../ui/command-palette';
import { type Child, byId, h, setChildren, timeAgo } from '../ui/dom';
import { type IconName, icon, osMark } from '../ui/icons';
import { renderMarkdown } from '../ui/markdown';
import {
  ACTION_LABELS,
  type Action,
  FILTERS,
  FILTER_LABELS,
  type Filter,
  type Summary,
  actionUrl,
  actions,
  filterCounts,
  matchesFilter,
  moveSelection,
  osFamily,
  osLabel,
  osShort,
  primaryAction,
  shortUser,
  statusView,
  summarize,
} from '../ui/portal-model';

const REFRESH_MS = 5000;
/** A refused update request is shown in the details panel this long. */
const UPDATE_ERROR_MS = 60_000;
/** A failed New token / Remove is shown this long. */
const ACTION_ERROR_MS = 15_000;
/** Activity lines shown before "Show more". */
const ACTIVITY_SHORT = 6;
const ACTIVITY_LONG = 40;

const IS_MAC = /Mac|iPhone|iPad/.test(navigator.userAgent);
const ACTION_ICONS: Record<Action, IconName> = { connect: 'monitor', terminal: 'terminal', files: 'folder' };

// ---------------------------------------------------------------------------
// state; the page is rebuilt from it on every refresh

/** The last listing; null until the first one arrives. */
let machines: MachineInfo[] | null = null;
/** The gateway's recent events; null when it keeps none. */
let activity: ActivityEvent[] | null = null;
let activityExpanded = false;
let canAdd = false;
let filter: Filter = 'all';
/** The machine shown in the details panel. */
let selected: string | null = null;
let updatedAt = '';
let loadError = '';
let actionError: { text: string; at: number } | null = null;
/** Updates asked for from this page, until the agent answers. */
const pendingUpdates = new Map<string, PendingUpdate>();
/** Update requests the gateway refused. */
const updateErrors = new Map<string, { text: string; at: number }>();

// ---------------------------------------------------------------------------
// layout

document.body.classList.add('portal');
const root = byId('app');
/** Add device, What's new, and the one-time setup code after adding a device or replacing its token. */
const dialog = h('dialog');
const palette = new CommandPalette(commands);

const navAll = navItem('monitor', 'Devices', 'all');
const navSessions = navItem('live', 'Sessions', 'in-use');
const navUpdates = navItem('update', 'Updates', 'update');
const meName = h('div', { className: 'console-me-name' });
const meEmail = h('div', { className: 'console-me-email' });
const meAvatar = h('span', { className: 'avatar', ariaHidden: 'true' });
const me = h(
  'div',
  { className: 'console-me', hidden: true },
  meAvatar,
  h('div', { className: 'console-me-text' }, meName, meEmail),
);

const side = h(
  'aside',
  { className: 'console-side' },
  h(
    'div',
    { className: 'console-brand' },
    h('span', { className: 'brand-mark' }, icon('monitor', 18)),
    h('span', null, 'Remote desktops'),
  ),
  h('nav', { className: 'console-nav', ariaLabel: 'Devices' }, navAll.el, navSessions.el, navUpdates.el),
  h('div', { className: 'console-tools' }, h('div', { className: 'side-label' }, 'Tools'), ...toolLinks()),
  me,
);

const searchButton = h(
  'button',
  { className: 'console-search', type: 'button', title: 'Search devices and commands' },
  icon('search', 16),
  h('span', null, 'Jump to a device or run a command…'),
  h('kbd', null, IS_MAC ? '⌘K' : 'Ctrl K'),
);
searchButton.addEventListener('click', () => palette.open());
const addButton = h(
  'button',
  { className: 'primary', type: 'button', hidden: true },
  icon('plus', 16),
  h('span', { className: 'add-label' }, 'Add device'),
);
addButton.addEventListener('click', openAddDialog);

const stats = h('div', { className: 'console-stats' });
const chips = h('div', { className: 'chips', role: 'toolbar', ariaLabel: 'Filter' });
const status = h('span', { className: 'console-status muted', role: 'status' });
const list = h('div', { className: 'device-list', role: 'list', ariaLabel: 'Devices' });
const activityBox = h('section', { className: 'console-activity', hidden: true, ariaLabel: 'Recent activity' });
const hints = h(
  'div',
  { className: 'console-hints muted' },
  hint(['↑', '↓'], 'select'),
  hint(['↵'], 'open'),
  hint(['T'], 'terminal'),
  hint(['F'], 'files'),
  hint([IS_MAC ? '⌘K' : 'Ctrl K'], 'commands'),
);

const main = h(
  'main',
  { className: 'console-main' },
  h('header', { className: 'console-top' }, h('h1', null, 'Devices'), searchButton, addButton),
  stats,
  h('div', { className: 'console-filterbar' }, chips, status),
  list,
  hints,
  activityBox,
  h('footer', { className: 'console-foot muted' }, ...toolLinks()),
);

const inspector = h('aside', { className: 'console-inspector', hidden: true, ariaLabel: 'Device details' });
const scrim = h('div', { className: 'console-scrim', hidden: true });
scrim.addEventListener('click', () => select(null));
const shell = h('div', { className: 'console' }, side, main, inspector, scrim);
root.append(shell, dialog);

function navItem(name: IconName, label: string, f: Filter): { el: HTMLButtonElement; count: HTMLElement } {
  const count = h('span', { className: 'nav-count' });
  const el = h('button', { className: 'nav-item', type: 'button' }, icon(name, 16), h('span', null, label), count);
  el.addEventListener('click', () => setFilter(f));
  return { el, count };
}

function toolLinks(): HTMLElement[] {
  return [
    h('a', { className: 'nav-item', href: '/probe' }, icon('activity', 16), 'Network probe'),
    h('a', { className: 'nav-item', href: '/latency-target' }, icon('gauge', 16), 'Latency target'),
    h('a', { className: 'nav-item', href: '/keytest' }, icon('keyboard', 16), 'Key test'),
  ];
}

function hint(keys: string[], what: string): HTMLElement {
  return h('span', null, ...keys.flatMap((k) => [h('kbd', null, k), ' ']), what);
}

// ---------------------------------------------------------------------------
// small pieces

function errorText(e: unknown): string {
  return e instanceof HttpError ? e.message : 'Could not reach the gateway.';
}

function findMachine(machine: string | null): MachineInfo | undefined {
  return machine === null ? undefined : machines?.find((m) => m.machine === machine);
}

function nameOf(machine: string): string {
  return findMachine(machine)?.name ?? machine;
}

function osBadge(m: MachineInfo, size = 16): HTMLElement {
  const family = osFamily(m.os);
  return h(
    'span',
    { className: `os-badge ${family}`, title: osLabel(m.os) || 'Unknown system' },
    osMark(family === 'other' && !m.screen ? 'linux' : family, size),
  );
}

function dot(level: string): HTMLElement {
  return h('span', { className: `dot ${level}` });
}

function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

/** When an activity happened: the time today, else the day too. */
function when(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const days = Math.floor((today.getTime() - ms) / 86_400_000) + 1;
  if (days <= 0) return clock(ms);
  if (days === 1) return `Yesterday ${clock(ms)}`;
  if (days < 7) return `${d.toLocaleDateString([], { weekday: 'short' })} ${clock(ms)}`;
  return d.toLocaleDateString([], { day: 'numeric', month: 'short' });
}

/** Keep the focus on the same control when the page is rebuilt. */
function focusKey<T extends HTMLElement>(el: T, key: string): T {
  el.dataset.focus = key;
  return el;
}

function actionLink(m: MachineInfo, a: Action, className: string, withLabel = true): HTMLAnchorElement {
  const titles: Record<Action, string> = {
    connect: `Take over the screen of ${m.name}`,
    terminal: `A shell on ${m.name}, in its own page`,
    files: `Browse, download and upload files on ${m.name}`,
  };
  const link = h(
    'a',
    { className, href: actionUrl(m, a), title: titles[a] },
    icon(ACTION_ICONS[a], withLabel ? 15 : 14),
    withLabel ? ACTION_LABELS[a] : null,
  );
  if (!withLabel) link.ariaLabel = `${ACTION_LABELS[a]}: ${m.name}`;
  return focusKey(link, `${className}:${a}:${m.machine}`);
}

function go(m: MachineInfo, a: Action): void {
  if (actions(m).includes(a)) location.assign(actionUrl(m, a));
}

// ---------------------------------------------------------------------------
// selection, filters, keys

/** `focusRow`: move the focus to the row (of the machine that was selected, when closing). */
function select(machine: string | null, focusRow = false): void {
  const row = machine ?? selected;
  selected = machine;
  render();
  if (focusRow && row) {
    const name = list.querySelector<HTMLElement>(`[data-focus="row:${CSS.escape(row)}"]`);
    name?.focus({ preventScroll: true });
    name?.closest('.device-row')?.scrollIntoView({ block: 'nearest' });
  }
}

function setFilter(f: Filter): void {
  filter = f;
  render();
}

function visibleMachines(): MachineInfo[] {
  return (machines ?? []).filter((m) => matchesFilter(m, filter));
}

document.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    if (!dialog.open) palette.open();
    return;
  }
  if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey || dialog.open || palette.isOpen) return;
  const target = e.target instanceof HTMLElement ? e.target : null;
  if (target?.closest('input, textarea, select, [contenteditable="true"]')) return;
  // On a row's name, Enter opens the machine; on other buttons and links it keeps its meaning.
  const onRow = target?.classList.contains('device-name') ?? false;
  const onControl = target?.closest('a, button') != null && !onRow;
  // A focused row (after Esc, or reached with Tab) counts as selected.
  const current = (onRow ? target?.closest<HTMLElement>('.device-row')?.dataset.machine : undefined) ?? selected;
  const m = findMachine(current);
  switch (e.key) {
    case 'ArrowDown':
    case 'ArrowUp':
    case 'j':
    case 'k': {
      const down = e.key === 'ArrowDown' || e.key === 'j';
      const ids = visibleMachines().map((x) => x.machine);
      const next = moveSelection(ids, current, down ? 1 : -1);
      if (next === null) return;
      e.preventDefault();
      select(next, true);
      return;
    }
    case 'Enter': {
      if (onControl || !m) return;
      const a = primaryAction(m);
      if (!a) return;
      e.preventDefault();
      go(m, a);
      return;
    }
    case 't':
    case 'f':
      if (m) go(m, e.key === 't' ? 'terminal' : 'files');
      return;
    case '/':
      e.preventDefault();
      palette.open();
      return;
    case 'Escape':
      if (selected === null) return;
      e.preventDefault();
      select(null, true);
      return;
  }
});

list.addEventListener('click', (e) => {
  const t = e.target instanceof Element ? e.target : null;
  if (!t || t.closest('a, button:not(.device-name)')) return;
  const row = t.closest<HTMLElement>('.device-row[data-machine]');
  if (row?.dataset.machine) select(row.dataset.machine);
});

list.addEventListener('dblclick', (e) => {
  const t = e.target instanceof Element ? e.target : null;
  if (!t || t.closest('a, button:not(.device-name)')) return;
  const m = findMachine(t.closest<HTMLElement>('.device-row[data-machine]')?.dataset.machine ?? null);
  const a = m && primaryAction(m);
  if (m && a) go(m, a);
});

// ---------------------------------------------------------------------------
// the palette's commands

function commands(): Command[] {
  const out: Command[] = [];
  const ms = machines ?? [];
  for (const m of ms) {
    const keywords = `${m.machine} ${osLabel(m.os)}`;
    for (const a of actions(m)) {
      out.push({
        group: 'Devices',
        title: m.name,
        hint: ACTION_LABELS[a],
        keywords,
        icon: ACTION_ICONS[a],
        run: () => go(m, a),
      });
    }
  }
  for (const m of ms) {
    out.push({
      group: 'Details',
      title: `${m.name}: details`,
      keywords: m.machine,
      icon: 'info',
      run: () => select(m.machine, true),
    });
  }
  for (const m of ms) {
    const a = m.update?.available;
    if (a && m.online && m.update?.can_install && !pendingUpdates.has(m.machine)) {
      out.push({
        group: 'Updates',
        title: `Update ${m.name} to ${a.version}`,
        hint: m.viewer ? 'ends the session' : '',
        keywords: m.machine,
        icon: 'update',
        run: () => startUpdate(m, a.version),
      });
    }
  }
  if (canAdd) out.push({ group: 'Portal', title: 'Add device', icon: 'plus', run: openAddDialog });
  for (const f of FILTERS) {
    if (f !== filter)
      out.push({ group: 'Portal', title: `Show: ${FILTER_LABELS[f]}`, icon: 'search', run: () => setFilter(f) });
  }
  out.push(
    { group: 'Tools', title: 'Network probe', icon: 'activity', run: () => location.assign('/probe') },
    { group: 'Tools', title: 'Latency target', icon: 'gauge', run: () => location.assign('/latency-target') },
    { group: 'Tools', title: 'Key test', icon: 'keyboard', run: () => location.assign('/keytest') },
  );
  return out;
}

// ---------------------------------------------------------------------------
// device actions (dialogs and requests)

function replaceToken(m: MachineInfo): void {
  if (!confirm(`Give ${m.name} a new token? It disconnects until you run its setup again with the new code.`)) return;
  replaceDeviceToken(m.machine).then(
    (d) => showIssued(d, true),
    (e: unknown) => showActionError(errorText(e)),
  );
}

function remove(m: MachineInfo): void {
  if (!confirm(`Remove ${m.name}? Its agent disconnects and its token stops working.`)) return;
  removeDevice(m.machine).then(
    () => {
      if (selected === m.machine) selected = null;
      void refresh();
    },
    (e: unknown) => showActionError(errorText(e)),
  );
}

function showActionError(text: string): void {
  actionError = { text, at: Date.now() };
  render();
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
  const b = focusKey(h('button', { className, type: 'button' }, 'Update now'), `update:${m.machine}`);
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

/** packaging/linux/install.sh, which installs the agent and sets it up with the code. */
const LINUX_INSTALL = 'https://raw.githubusercontent.com/Thijmen/remoted/main/packaging/linux/install.sh';

/** A command line in a box, with its own copy button. */
function commandBox(command: string): HTMLElement {
  const text = h('code', { className: 'mono', style: 'word-break:break-all;flex:1' }, command);
  const copy = h('button', { className: 'small', type: 'button' }, 'Copy');
  copy.addEventListener('click', () => {
    navigator.clipboard.writeText(command).then(
      () => (copy.textContent = 'Copied'),
      () => (copy.textContent = 'Select and copy it'),
    );
  });
  return h('div', { className: 'row', style: 'align-items:flex-start' }, text, copy);
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
        h('summary', null, 'Linux server: install and set up in one command'),
        h(
          'div',
          { className: 'stack' },
          h(
            'p',
            { className: 'muted', style: 'margin:0' },
            'Run it as the user whose shell you want in the browser (not root). It installs the agent to ~/.local/bin and starts it as a systemd user service; the server gets the terminal and files, no screen.',
          ),
          commandBox(`curl -fsSL ${LINUX_INSTALL} | sh -s -- ${d.setupCode}`),
        ),
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

// ---------------------------------------------------------------------------
// rendering

/** The version being installed, from this page's request or the agent's report; null when none. */
function updatingTo(m: MachineInfo): string | null {
  return updateCardView(m, pendingUpdates.get(m.machine) ?? null, Date.now())?.updating ?? null;
}

function renderNav(s: Summary): void {
  const counts: [typeof navAll, Filter, number, string][] = [
    [navAll, 'all', s.total, ''],
    [navSessions, 'in-use', s.inUse.length, s.inUse.length ? 'warn' : ''],
    [navUpdates, 'update', s.updates, s.updates ? 'accent' : ''],
  ];
  for (const [item, f, n, level] of counts) {
    item.el.classList.toggle('on', filter === f);
    item.el.setAttribute('aria-pressed', String(filter === f));
    item.count.className = `nav-count ${level}`;
    setChildren(
      item.count,
      String(n),
      f === 'in-use' && n ? h('span', { className: 'nav-count-unit' }, ' live') : null,
    );
  }
}

function statTile(f: Filter | null, key: Child[], value: Child, extra: Child): HTMLElement {
  const content: Child[] = [h('div', { className: 'stat-key' }, ...key), value, extra];
  if (f === null) return h('div', { className: 'stat' }, ...content);
  const b = focusKey(
    h('button', { className: `stat${filter === f ? ' on' : ''}`, type: 'button' }, ...content),
    `stat:${f}`,
  );
  b.setAttribute('aria-pressed', String(filter === f));
  b.title = `Show: ${FILTER_LABELS[f]}`;
  b.addEventListener('click', () => setFilter(filter === f ? 'all' : f));
  return b;
}

const VERSION_COLORS = ['#e3a04a', '#7aa2ff', '#c08cff', '#f2645f', '#4fc3c8'];

function renderStats(ms: MachineInfo[], s: Summary): void {
  const value = (n: number, of?: number) =>
    h('div', { className: 'stat-value' }, String(n), of !== undefined ? h('small', null, ` / ${of}`) : null);
  const detailLine = (text: string) => h('div', { className: 'stat-detail' }, text);
  const first = s.inUse[0];
  const sessions = first
    ? s.inUse.length === 1
      ? `${first.name} · ${first.viewer ? timeAgo(first.viewer.since_ms) : ''}`
      : s.inUse.map((m) => m.name).join(', ')
    : 'None right now';
  let other = 0;
  const versions = s.versions.map((v) => ({
    ...v,
    color: !v.version
      ? 'var(--muted)'
      : v.version === s.newest
        ? 'var(--ok)'
        : VERSION_COLORS[other++ % VERSION_COLORS.length]!,
  }));
  setChildren(
    stats,
    statTile(
      'online',
      ['Online'],
      value(s.online, s.total),
      h(
        'div',
        { className: 'stat-segments', ariaHidden: 'true' },
        ...ms.map((m) => h('i', { className: m.online ? '' : 'off', title: m.name })),
      ),
    ),
    statTile(
      'in-use',
      [s.inUse.length ? dot('warn') : null, 'Live sessions'],
      value(s.inUse.length),
      detailLine(sessions),
    ),
    statTile(
      'update',
      ['Agent updates'],
      value(s.updates),
      detailLine(s.newest ? `Newest version ${s.newest}` : 'No agent seen yet'),
    ),
    statTile(
      null,
      ['Agent versions'],
      h(
        'div',
        { className: 'stat-bar', ariaHidden: 'true' },
        ...versions.map((v) => h('i', { style: `flex:${v.count};background:${v.color}` })),
      ),
      h(
        'div',
        { className: 'stat-legend' },
        ...versions.map((v) => h('span', { style: `--c:${v.color}` }, `${v.version || 'not seen'} × ${v.count}`)),
      ),
    ),
  );
}

function renderChips(ms: MachineInfo[]): void {
  const counts = filterCounts(ms);
  setChildren(
    chips,
    ...FILTERS.filter((f) => f !== 'offline' || counts.offline > 0 || filter === 'offline').map((f) => {
      const b = focusKey(
        h('button', { className: 'chip', type: 'button' }, FILTER_LABELS[f], h('b', null, String(counts[f]))),
        `chip:${f}`,
      );
      b.setAttribute('aria-pressed', String(filter === f));
      b.addEventListener('click', () => setFilter(f));
      return b;
    }),
  );
}

function renderStatus(): void {
  if (actionError && Date.now() - actionError.at > ACTION_ERROR_MS) actionError = null;
  const text = actionError?.text || loadError;
  status.classList.toggle('error', text !== '');
  status.textContent = text || (updatedAt ? `Updated ${updatedAt}` : 'Loading…');
}

function agentCell(m: MachineInfo, newest: string): HTMLElement {
  const cell = h('div', { className: 'cell-agent' });
  const to = updatingTo(m);
  const avail = m.update?.available?.version;
  if (!m.agent_version) cell.append('—');
  else cell.append(m.agent_version);
  if (to !== null) cell.append(h('span', { className: 'to' }, ' → ', to || '…', h('span', { className: 'spinner' })));
  else if (avail) cell.append(h('span', { className: 'to' }, ` → ${avail}`));
  else if (m.agent_version && m.agent_version === newest) cell.append(h('span', { className: 'latest' }, ' · latest'));
  return cell;
}

function statusCell(m: MachineInfo): HTMLElement {
  const st = statusView(m);
  const to = updatingTo(m);
  let level: string = st.level;
  let label = st.label;
  let detailText = '';
  if (to !== null) {
    level = 'accent';
    label = 'Updating';
    detailText = to ? `to ${to}` : '';
  } else if (st.label === 'In use') {
    detailText = `${shortUser(st.user)} · ${st.at ? timeAgo(st.at) : ''}`;
  } else if (st.at) {
    detailText = m.online ? `since ${timeAgo(st.at)}` : `last seen ${timeAgo(st.at)}`;
  }
  return h(
    'div',
    { className: 'cell-status' },
    dot(level),
    h(
      'div',
      null,
      h('span', { className: `status-label ${level}` }, label),
      detailText ? h('small', null, detailText) : null,
    ),
  );
}

function accessCell(m: MachineInfo): HTMLElement {
  const has: Record<Action, boolean> = { connect: m.screen, terminal: m.terminal, files: m.files };
  const missing: Record<Action, string> = { connect: 'No screen', terminal: 'No terminal', files: 'No file transfer' };
  return h(
    'div',
    { className: 'cell-access' },
    ...(['connect', 'terminal', 'files'] as const).map((a) =>
      has[a] && m.online
        ? actionLink(m, a, 'access-link', false)
        : h(
            'span',
            { className: 'access-link off', title: has[a] ? 'Offline' : missing[a] },
            icon(ACTION_ICONS[a], 14),
          ),
    ),
  );
}

function deviceRow(m: MachineInfo, newest: string): HTMLElement {
  const isSel = m.machine === selected;
  const name = focusKey(
    h(
      'button',
      { className: 'device-name', type: 'button', title: 'Details' },
      h('b', null, m.name),
      h('span', null, osShort(m.os) || m.machine),
    ),
    `row:${m.machine}`,
  );
  name.setAttribute('aria-pressed', String(isSel));
  const a = primaryAction(m);
  let primary: HTMLElement;
  if (a) primary = actionLink(m, a, 'button small primary');
  else {
    primary = focusKey(h('button', { className: 'small', type: 'button' }, 'Details'), `details:${m.machine}`);
    primary.addEventListener('click', () => select(m.machine));
  }
  const row = h(
    'div',
    { className: `device-row${isSel ? ' selected' : ''}${m.online ? '' : ' offline'}`, role: 'listitem' },
    h('div', { className: 'cell-device' }, osBadge(m), name),
    statusCell(m),
    agentCell(m, newest),
    accessCell(m),
    h('div', { className: 'cell-actions' }, primary),
  );
  row.dataset.machine = m.machine;
  return row;
}

function renderList(ms: MachineInfo[], s: Summary): void {
  const head = h(
    'div',
    { className: 'device-row device-head', ariaHidden: 'true' },
    h('span', null, 'Device'),
    h('span', null, 'Status'),
    h('span', null, 'Agent'),
    h('span', null, 'Access'),
    h('span'),
  );
  if (machines === null) {
    setChildren(list, h('div', { className: 'device-empty' }, loadError || 'Loading devices…'));
    return;
  }
  if (ms.length === 0) {
    setChildren(
      list,
      h(
        'div',
        { className: 'device-empty' },
        canAdd ? 'No machines yet: add one with Add device.' : 'No machines registered.',
      ),
    );
    return;
  }
  const shown = ms.filter((m) => matchesFilter(m, filter));
  if (shown.length === 0) {
    const all = h('button', { className: 'small', type: 'button' }, 'Show all');
    all.addEventListener('click', () => setFilter('all'));
    setChildren(
      list,
      head,
      h('div', { className: 'device-empty' }, `No devices are ${FILTER_LABELS[filter].toLowerCase()}. `, all),
    );
    return;
  }
  setChildren(list, head, ...shown.map((m) => deviceRow(m, s.newest)));
}

function renderActivity(): void {
  if (activity === null) {
    activityBox.hidden = true;
    return;
  }
  activityBox.hidden = false;
  const lines = activityLines(activity, nameOf, activityExpanded ? ACTIVITY_LONG : ACTIVITY_SHORT);
  const more = activityLines(activity, nameOf, ACTIVITY_SHORT + 1).length > ACTIVITY_SHORT;
  const toggle = more
    ? focusKey(
        h('button', { className: 'small ghost', type: 'button' }, activityExpanded ? 'Show less' : 'Show more'),
        'activity:more',
      )
    : null;
  toggle?.addEventListener('click', () => {
    activityExpanded = !activityExpanded;
    render();
  });
  setChildren(
    activityBox,
    h('div', { className: 'section-title' }, 'Recent activity', h('span', { className: 'spacer' }), toggle),
    ...(lines.length
      ? lines.map((l) =>
          h(
            'div',
            { className: 'activity-item' },
            dot(l.level),
            h('span', null, ...l.parts.map((p) => (typeof p === 'string' ? p : h('b', null, p.strong)))),
            h(
              'time',
              { dateTime: new Date(l.at_ms).toISOString(), title: new Date(l.at_ms).toLocaleString() },
              when(l.at_ms),
            ),
          ),
        )
      : [h('div', { className: 'activity-empty muted' }, 'Nothing yet since the gateway started.')]),
  );
}

function panel(title: Child[], className = ''): (...body: Child[]) => HTMLElement {
  return (...body) =>
    h('section', { className: `panel ${className}` }, h('h3', { className: 'section-title' }, ...title), ...body);
}

function fact(label: string, value: Child, className = ''): Child[] {
  return [h('dt', null, label), h('dd', { className }, value)];
}

/** Agent version, updates and their outcome. */
function agentPanel(m: MachineInfo): HTMLElement {
  const now = Date.now();
  const refused = updateErrors.get(m.machine);
  if (refused && now - refused.at > UPDATE_ERROR_MS) updateErrors.delete(m.machine);
  const view = updateCardView(m, pendingUpdates.get(m.machine) ?? null, now);
  const body: Child[] = [];
  const versions = h('div', { className: 'agent-versions' }, h('span', null, m.agent_version || 'unknown'));
  body.push(versions);
  if (view?.updating != null) {
    versions.append(icon('arrowRight', 15), h('span', { className: 'to' }, view.updating || '…'));
    body.push(
      h(
        'div',
        { className: 'panel-note accent' },
        h('span', { className: 'spinner' }),
        'Updating; the agent restarts.',
      ),
    );
  } else if (view?.available) {
    const a = view.available;
    const notes = focusKey(
      h('button', { className: 'link-button', type: 'button' }, "What's new"),
      `notes:${m.machine}`,
    );
    notes.addEventListener('click', () => openNotes(m));
    versions.append(icon('arrowRight', 15), h('span', { className: 'to' }, a.version), notes);
    if (view.canInstall && m.online) {
      if (m.viewer) {
        body.push(
          h(
            'div',
            { className: 'panel-note warn' },
            icon('alert', 15),
            `Updating ends the session of ${shortUser(m.viewer.user)}. ${m.name} is back in about 15 s.`,
          ),
        );
      }
      body.push(h('div', { className: 'row' }, updateButton(m, a.version, 'small primary')));
    } else if (!view.canInstall) {
      body.push(
        h(
          'div',
          { className: 'panel-note' },
          `Update by hand: ${view.cannotInstall || 'this agent cannot update itself.'}`,
        ),
      );
    } else {
      body.push(h('div', { className: 'panel-note' }, 'It can be updated once it is back online.'));
    }
  } else if (m.update) {
    const checked = m.update.checked_ms;
    body.push(
      h(
        'div',
        { className: 'panel-note' },
        icon('check', 15),
        checked ? `Up to date (checked ${timeAgo(checked)})` : 'Up to date',
      ),
    );
  } else {
    body.push(h('div', { className: 'panel-note' }, 'This agent does not report updates (before 1.5.0).'));
  }
  const o = view?.outcome;
  if (o) {
    body.push(
      o.level === 'muted'
        ? h('div', { className: 'panel-note' }, o.at ? `${o.text} ${timeAgo(o.at)}` : o.text)
        : h('div', { className: `notice ${o.level}` }, o.text),
    );
  }
  if (view?.error) body.push(h('div', { className: 'panel-note' }, view.error));
  if (refused) body.push(h('div', { className: 'notice bad' }, refused.text));
  return panel(['Agent'])(...body);
}

function inspectorContent(m: MachineInfo): Child[] {
  const close = focusKey(
    h('button', { className: 'icon-button', type: 'button', title: 'Close (Esc)' }, icon('close', 16)),
    'insp:close',
  );
  close.ariaLabel = 'Close details';
  close.addEventListener('click', () => select(null, true));
  const acts = actions(m);
  const st = m.state;
  const facts: Child[] = [
    ...fact(
      m.online ? 'Online since' : 'Last seen',
      m.since_ms ? `${clock(m.since_ms)} · ${timeAgo(m.since_ms)}` : 'unknown',
      m.online ? '' : 'bad',
    ),
  ];
  if (!m.screen) facts.push(...fact('Screen', 'None: terminal and files only'));
  else if (m.online && st) {
    const perm = (ok: boolean) =>
      h('span', { className: ok ? 'good' : 'bad' }, icon(ok ? 'check' : 'close', 14), ok ? 'Granted' : 'Missing');
    facts.push(...fact('Screen access', perm(st.perms.screen)), ...fact('Input access', perm(st.perms.input)));
    facts.push(
      ...fact(
        'Screen',
        st.locked ? 'Locked' : st.disconnected ? 'Remote Desktop disconnected' : 'Unlocked',
        st.locked || st.disconnected ? 'warn' : '',
      ),
    );
  }
  facts.push(...fact('Machine id', h('code', null, m.machine)), ...fact('System', osLabel(m.os) || 'unknown'));

  const out: Child[] = [
    h(
      'div',
      { className: 'inspector-head' },
      osBadge(m, 20),
      h(
        'div',
        { className: 'inspector-title' },
        h('h2', null, m.name),
        h('div', { className: 'muted' }, osShort(m.os) || m.machine),
      ),
      close,
    ),
    acts.length
      ? h(
          'div',
          { className: 'inspector-actions' },
          ...acts.map((a, i) => actionLink(m, a, i === 0 ? 'button primary' : 'button')),
        )
      : h(
          'div',
          { className: 'notice bad' },
          `${m.name} is offline${m.since_ms ? `; last seen ${timeAgo(m.since_ms)}` : ''}.`,
        ),
  ];
  if (m.viewer) {
    out.push(
      panel(
        [dot('warn'), 'Live session'],
        'live',
      )(
        h(
          'div',
          { className: 'session-who' },
          h('span', { className: 'avatar small', ariaHidden: 'true' }, initial(m.viewer.user)),
          h(
            'div',
            null,
            h('div', { className: 'session-user' }, m.viewer.user),
            h('div', { className: 'muted' }, `Connected ${timeAgo(m.viewer.since_ms)}`),
          ),
        ),
      ),
    );
  }
  out.push(panel(['Device'])(h('dl', { className: 'facts' }, ...facts)), agentPanel(m));
  if (m.recovery.length) {
    out.push(
      panel(['Recovery'])(
        h(
          'div',
          { className: 'row' },
          ...m.recovery.map((r) =>
            h('a', { className: 'button small', href: r.url, target: '_blank', rel: 'noopener noreferrer' }, r.label),
          ),
        ),
      ),
    );
  }
  if (m.managed) {
    const token = focusKey(
      h('button', { className: 'small', type: 'button' }, icon('key', 14), 'New token'),
      `token:${m.machine}`,
    );
    token.addEventListener('click', () => replaceToken(m));
    const del = focusKey(
      h('button', { className: 'small danger', type: 'button' }, icon('trash', 14), 'Remove device'),
      `remove:${m.machine}`,
    );
    del.addEventListener('click', () => remove(m));
    out.push(panel(['Access token'])(h('div', { className: 'row' }, token, del)));
  }
  return out;
}

function initial(email: string): string {
  return (email.trim()[0] ?? '?').toUpperCase();
}

function renderInspector(): void {
  const m = findMachine(selected);
  if (!m) selected = null;
  inspector.hidden = !m;
  scrim.hidden = !m;
  shell.classList.toggle('with-inspector', m !== undefined);
  if (!m) return;
  const scroll = inspector.scrollTop;
  setChildren(inspector, ...inspectorContent(m));
  inspector.scrollTop = scroll;
}

/** Rebuild the page from the last listing and this page's own state. */
function render(): void {
  const focused = document.activeElement instanceof HTMLElement ? document.activeElement.dataset.focus : undefined;
  const now = Date.now();
  const ms = machines ?? [];
  for (const m of ms) {
    const p = pendingUpdates.get(m.machine);
    const next = p && advancePending(p, m, now);
    if (next) pendingUpdates.set(m.machine, next);
    else pendingUpdates.delete(m.machine);
  }
  const s = summarize(ms);
  renderNav(s);
  renderStats(ms, s);
  renderChips(ms);
  renderStatus();
  renderList(ms, s);
  renderInspector();
  renderActivity();
  if (focused)
    document.querySelector<HTMLElement>(`[data-focus="${CSS.escape(focused)}"]`)?.focus({ preventScroll: true });
}

/** The gateway answered 404 for `/api/activity` (an older gateway in development): stop asking. */
let noActivity = false;

async function refresh(): Promise<void> {
  const [listing, events] = await Promise.allSettled([
    fetchMachines(),
    noActivity ? Promise.resolve(null) : fetchActivity(),
  ]);
  if (listing.status === 'fulfilled') {
    machines = listing.value;
    loadError = '';
    updatedAt = new Date().toLocaleTimeString();
  } else {
    const e: unknown = listing.reason;
    loadError = e instanceof HttpError ? `Could not load machines (HTTP ${e.status}).` : 'Could not reach the gateway.';
  }
  // A failed activity request keeps the last list.
  if (events.status === 'fulfilled') {
    activity = events.value;
    noActivity = events.value === null;
  }
  render();
}

void fetchMe()
  .then((who) => {
    if (!who) return;
    meName.textContent = shortUser(who.email);
    meEmail.textContent = who.dev ? `${who.email} (dev mode)` : who.email;
    meAvatar.textContent = initial(who.email);
    me.title = who.email;
    me.hidden = false;
    canAdd = who.devices;
    addButton.hidden = !who.devices;
    render();
  })
  .catch(() => undefined);
render();
void refresh();
setInterval(() => {
  if (!document.hidden) void refresh();
}, REFRESH_MS);
