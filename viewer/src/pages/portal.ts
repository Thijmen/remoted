import '../ui/style.css';
import { HttpError, fetchMachines, fetchMe } from '../net/api';
import type { MachineInfo } from '../proto/control';
import { byId, h, setChildren, timeAgo } from '../ui/dom';

const REFRESH_MS = 5000;

const root = byId('app');
const who = h('span', { className: 'muted' });
const list = h('div', { className: 'stack' });
const status = h('div', { className: 'muted' });

root.append(
  h(
    'main',
    { className: 'page stack' },
    h('div', { className: 'row spread' }, h('h1', null, 'Remote desktops'), who),
    list,
    status,
    h(
      'div',
      { className: 'row muted' },
      h('a', { href: '/probe' }, 'Network probe'),
      '·',
      h('a', { href: '/latency-target' }, 'Latency target'),
      '·',
      h('a', { href: '/keytest' }, 'Key test'),
    ),
  ),
);

function stateBadges(m: MachineInfo): HTMLElement[] {
  const out = [h('span', { className: `badge ${m.online ? 'ok' : 'bad'}` }, m.online ? 'online' : 'offline')];
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

function machineCard(m: MachineInfo): HTMLElement {
  const details = [
    m.machine !== m.name ? m.machine : '',
    m.os,
    m.agent_version && `agent ${m.agent_version}`,
    m.since_ms !== null && (m.online ? `online since ${timeAgo(m.since_ms)}` : `last seen ${timeAgo(m.since_ms)}`),
  ].filter(Boolean);
  return h(
    'section',
    { className: 'card stack' },
    h(
      'div',
      { className: 'row spread' },
      h(
        'div',
        { className: 'row' },
        h('span', { className: `dot ${m.online ? 'ok' : 'bad'}` }),
        h('h2', { style: 'margin:0' }, m.name),
      ),
      h('a', { className: 'button primary', href: `/m/${encodeURIComponent(m.machine)}` }, 'Connect'),
    ),
    h('div', { className: 'muted' }, details.join(' · ')),
    h('div', { className: 'row' }, ...stateBadges(m)),
    m.recovery.length
      ? h(
          'div',
          { className: 'row' },
          h('span', { className: 'muted' }, 'Recovery:'),
          ...m.recovery.map((r) =>
            h('a', { className: 'button', href: r.url, target: '_blank', rel: 'noopener noreferrer' }, r.label),
          ),
        )
      : null,
  );
}

async function refresh(): Promise<void> {
  try {
    const machines = await fetchMachines();
    setChildren(
      list,
      ...(machines.length
        ? machines.map(machineCard)
        : [h('div', { className: 'card muted' }, 'No machines registered.')]),
    );
    status.textContent = `Updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    status.textContent =
      e instanceof HttpError ? `Could not load machines (HTTP ${e.status}).` : 'Could not reach the gateway.';
  }
}

void fetchMe()
  .then((me) => {
    if (me) who.textContent = me.dev ? `${me.email} (dev mode)` : me.email;
  })
  .catch(() => undefined);
void refresh();
setInterval(() => {
  if (!document.hidden) void refresh();
}, REFRESH_MS);
