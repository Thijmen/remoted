import '../ui/style.css';
import type { Outcome } from '../probe/analysis';
import {
  type ProbePlan,
  type ProbeReport,
  ProbeRunner,
  environment,
  fullPlan,
  planSeconds,
  postReport,
  quickPlan,
} from '../probe/runner';
import { byId, downloadJson, h, setChildren } from '../ui/dom';

const env = environment();
const quickBtn = h(
  'button',
  { type: 'button', className: 'primary' },
  `Quick check (~${Math.ceil(planSeconds(quickPlan()) / 60)} min)`,
);
const fullBtn = h('button', { type: 'button' }, 'Full suite');
const soakMinutes = h('input', { type: 'number', min: '1', max: '120', step: '1', value: '30', style: 'width:5em' });
const stopBtn = h('button', { type: 'button', disabled: true }, 'Stop');
const downloadBtn = h('button', { type: 'button', disabled: true }, 'Download JSON');
const stepLabel = h('div', { className: 'muted' }, 'Idle.');
const progress = h('progress', { max: 1, value: 0 });
const live = h('div', { className: 'mono muted' });
const rows = h('tbody');
const verdictBox = h('div', { className: 'stack' });
const reportStatus = h('span', { className: 'muted' });

let abort: AbortController | null = null;
let lastReport: ProbeReport | null = null;

function fullLabel(): void {
  const minutes = Number(soakMinutes.value) || 30;
  fullBtn.textContent = `Full suite (~${Math.ceil(planSeconds(fullPlan(minutes)) / 60)} min)`;
}
soakMinutes.addEventListener('input', fullLabel);
fullLabel();

byId('app').append(
  h(
    'main',
    { className: 'page stack' },
    h('div', { className: 'row spread' }, h('h1', null, 'Network probe'), h('a', { href: '/' }, 'Machines')),
    h(
      'p',
      { className: 'muted' },
      'Measures whether this network can carry a low-latency remote desktop over one WebSocket: RTT, ' +
        'downstream throughput and queueing, long idle connections, upstream, and an HTTP streaming fallback.',
    ),
    h(
      'section',
      { className: 'card stack' },
      h('h2', null, 'Connection'),
      h(
        'table',
        null,
        h(
          'tbody',
          null,
          ...[
            ['Origin', String(env.origin)],
            ['Page protocol', String(env.nextHopProtocol || 'unknown')],
            ['Secure context', env.secureContext ? 'yes' : 'no'],
            ['Cross-origin isolated', env.crossOriginIsolated ? 'yes' : 'no'],
          ].map(([k, v]) => h('tr', null, h('th', null, k), h('td', { className: 'mono' }, v))),
        ),
      ),
      h(
        'p',
        { className: 'muted' },
        'Certificate check: click the padlock in the address bar → Connection is secure → Certificate. ' +
          'If the issuer is your employer’s CA instead of a public CA (e.g. Let’s Encrypt), this traffic is TLS-inspected by a proxy.',
      ),
    ),
    h(
      'section',
      { className: 'card stack' },
      h('div', { className: 'row' }, quickBtn, fullBtn, h('label', null, 'soak minutes ', soakMinutes), stopBtn),
      stepLabel,
      progress,
      live,
    ),
    h(
      'section',
      { className: 'card stack' },
      h('h2', null, 'Results'),
      h(
        'table',
        null,
        h('thead', null, h('tr', null, h('th', null, 'Test'), h('th', null, ''), h('th', null, 'Result'))),
        rows,
      ),
    ),
    h(
      'section',
      { className: 'card stack' },
      h('h2', null, 'Verdict'),
      verdictBox,
      h('div', { className: 'row' }, downloadBtn, reportStatus),
    ),
  ),
);

const mark = (ok: boolean | null) =>
  ok === null
    ? h('span', { className: 'verdict-na' }, '·')
    : h('span', { className: ok ? 'verdict-pass' : 'verdict-fail' }, ok ? '✓' : '✗');

const outcomeText: Record<Outcome, string> = { pass: 'PASS', fail: 'FAIL', na: 'n/a' };

function showVerdict(r: ProbeReport): void {
  const v = r.verdict;
  setChildren(
    verdictBox,
    h(
      'div',
      { className: `verdict-${v.overall}`, style: 'font-size:18px;font-weight:600' },
      `${outcomeText[v.overall]}${r.stopped ? ' (stopped early)' : ''}`,
    ),
    h(
      'table',
      null,
      h(
        'tbody',
        null,
        ...v.criteria.map((c) =>
          h(
            'tr',
            null,
            h('td', { className: `verdict-${c.outcome}` }, outcomeText[c.outcome]),
            h('td', null, c.name),
            h('td', { className: 'muted' }, c.detail),
          ),
        ),
      ),
    ),
    r.mode === 'quick'
      ? h(
          'div',
          { className: 'muted' },
          'Quick check only: run the full suite for the 30-minute soak and long idle tests.',
        )
      : null,
  );
}

async function start(plan: ProbePlan): Promise<void> {
  abort = new AbortController();
  for (const b of [quickBtn, fullBtn, downloadBtn]) b.disabled = true;
  stopBtn.disabled = false;
  setChildren(rows);
  setChildren(verdictBox);
  reportStatus.textContent = '';
  const runner = new ProbeRunner(
    plan,
    {
      progress: (label, fraction) => {
        stepLabel.textContent = label;
        progress.value = fraction;
      },
      live: (text) => (live.textContent = text),
      row: (test, ok, summary) =>
        rows.append(
          h('tr', null, h('td', null, test), h('td', null, mark(ok)), h('td', { className: 'mono' }, summary)),
        ),
    },
    abort.signal,
  );
  try {
    lastReport = await runner.run();
    showVerdict(lastReport);
    reportStatus.textContent = 'Sending report to the gateway…';
    reportStatus.textContent = (await postReport(lastReport))
      ? 'Report logged by the gateway.'
      : 'Could not send the report (download it instead).';
  } catch (e) {
    stepLabel.textContent = `Probe failed: ${e instanceof Error ? e.message : String(e)}`;
  } finally {
    for (const b of [quickBtn, fullBtn]) b.disabled = false;
    downloadBtn.disabled = lastReport === null;
    stopBtn.disabled = true;
    abort = null;
  }
}

quickBtn.addEventListener('click', () => void start(quickPlan()));
fullBtn.addEventListener(
  'click',
  () => void start(fullPlan(Math.min(120, Math.max(1, Number(soakMinutes.value) || 30)))),
);
stopBtn.addEventListener('click', () => {
  stopBtn.disabled = true;
  abort?.abort();
});
downloadBtn.addEventListener('click', () => {
  if (lastReport) downloadJson(`rd-probe-${lastReport.startedAt.replace(/[:.]/g, '-')}.json`, lastReport);
});
window.addEventListener('beforeunload', (e) => {
  if (abort) e.preventDefault();
});
