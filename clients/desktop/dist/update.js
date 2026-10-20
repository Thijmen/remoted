// The update window (src/update.rs). The app pushes its state as window.rdUpdate(view); this
// page shows it and has one button. Links open in the system browser: the app stops every
// navigation away from its own pages and hands the URL to the browser.
const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

let latest = 0;
let view = null;

window.rdUpdate = (next) => {
  // A push can overtake the answer to rd_update_state: keep the newest.
  if (!next || next.seq < latest) return;
  latest = next.seq;
  view = next;
  render(next);
};

function describeUpdate(v) {
  const install = { label: 'Restart & update', action: 'install' };
  const check = { label: 'Check for Updates', action: 'check' };
  switch (v.status) {
    case 'checking':
      return { title: 'Checking for updates…', status: '', button: { ...check, disabled: true } };
    case 'downloading': {
      const pct = v.progress == null ? '' : ` ${Math.round(v.progress * 100)}%`;
      return {
        title: `rd Desktop ${v.version} is available`,
        status: `Downloading…${pct}`,
        button: { ...install, disabled: true },
      };
    }
    case 'ready':
      return {
        title: `rd Desktop ${v.version} is ready`,
        status: `You have ${v.current}. Updating restarts rd Desktop.`,
        button: install,
      };
    case 'manual':
      return {
        title: `rd Desktop ${v.version} is available`,
        status: `You have ${v.current}. This copy cannot update itself: ${v.blocker}`,
        button: { label: 'Download', action: 'install' },
      };
    case 'upToDate':
      return {
        title: 'rd Desktop is up to date',
        status: `${v.current} is the newest version.`,
        button: { ...check, label: 'Check again' },
      };
    case 'failed':
      return {
        title: v.mode === 'about' ? '' : `rd Desktop ${v.current}`,
        status: '',
        button: { ...check, label: 'Try again' },
      };
    case 'installing':
      return {
        title: `Installing rd Desktop ${v.version}…`,
        status: 'rd Desktop restarts in a moment.',
        button: { ...install, disabled: true },
      };
    default: // idle
      return { title: v.mode === 'about' ? '' : `rd Desktop ${v.current}`, status: '', button: check };
  }
}

function describeNotes(v) {
  let status = v.previous ? `Updated from ${v.previous}.` : '';
  if (v.status === 'checking' && v.notes.length === 0) status = 'Loading the release notes…';
  return { title: `What's new in rd Desktop ${v.version}`, status, button: null };
}

const TITLES = { update: 'rd Desktop update', notes: "What's new in rd Desktop", about: 'About rd Desktop' };

function render(v) {
  const notesMode = v.mode === 'notes';
  const { title, status, button } = notesMode ? describeNotes(v) : describeUpdate(v);
  document.title = TITLES[v.mode] || TITLES.update;
  $('about').hidden = v.mode !== 'about';
  $('about-version').textContent = `version ${v.current}`;
  $('title').textContent = title;
  $('title').hidden = !title;
  $('status').textContent = status;
  $('status').hidden = !status;
  const progress = $('progress');
  progress.hidden = v.status !== 'downloading';
  if (v.progress == null) progress.removeAttribute('value');
  else progress.value = v.progress;
  $('error').textContent = v.error || '';
  $('error').hidden = !v.error;

  const notes = $('notes');
  notes.replaceChildren();
  for (const n of v.notes) {
    const heading = document.createElement('h2');
    heading.textContent = n.version;
    const body = document.createElement('div');
    // Rendered and sanitized by the app (src/notes.rs): no raw HTML, images or non-web links.
    body.innerHTML = n.html;
    notes.append(heading, body);
  }
  const none = notesMode && v.notes.length === 0 && v.status !== 'checking';
  if (none) {
    const p = document.createElement('p');
    p.className = 'empty';
    p.textContent = 'The notes are on the release page.';
    notes.append(p);
  }
  notes.hidden = v.notes.length === 0 && !none;

  $('release').href = v.releaseUrl;
  const action = $('action');
  action.hidden = !button;
  if (button) {
    action.textContent = button.label;
    action.disabled = !!button.disabled;
    action.dataset.action = button.action;
  }
}

$('action').addEventListener('click', async () => {
  const action = $('action');
  action.disabled = true;
  try {
    await invoke(action.dataset.action === 'install' ? 'rd_update_install' : 'rd_update_check');
  } catch (e) {
    $('error').textContent = String(e);
    $('error').hidden = false;
  }
  // "Download" changes nothing in the app: enable the button again.
  if (view && view.status === 'manual') render(view);
});

invoke('rd_update_state')
  .then(window.rdUpdate)
  .catch((e) => {
    $('error').textContent = String(e);
    $('error').hidden = false;
  });
