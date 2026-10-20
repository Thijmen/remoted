// Small modal dialogs for the file browser: confirmations and name clashes. Each is a <dialog>
// on <body>, so they work while the browser drawer is closed (a drop onto the remote screen).

import type { Clash, ClashChoice } from '../files/controller';
import { formatMtime, formatSize } from '../files/format';
import { h } from './dom';
import { type IconName, icon } from './icons';

interface Choice<T> {
  label: string;
  value: T;
  primary?: boolean;
  danger?: boolean;
}

/** Show a modal dialog and resolve with the chosen value (null: dismissed with Esc). */
function ask<T>(opts: {
  title: string;
  body: Array<Node | string>;
  choices: Array<Choice<T>>;
  iconName?: IconName;
  extra?: HTMLElement;
}): Promise<T | null> {
  return new Promise((resolve) => {
    const dialog = h('dialog', { className: 'files-dialog' });
    let result: T | null = null;
    const buttons = opts.choices.map((c) =>
      h(
        'button',
        {
          type: 'button',
          className: [c.primary ? 'primary' : '', c.danger ? 'danger' : ''].filter(Boolean).join(' '),
          onclick: () => {
            result = c.value;
            dialog.close();
          },
        },
        c.label,
      ),
    );
    dialog.append(
      h(
        'div',
        { className: 'files-dialog-head' },
        opts.iconName ? h('span', { className: 'files-dialog-icon' }, icon(opts.iconName, 22)) : null,
        h('h2', null, opts.title),
      ),
      h('div', { className: 'files-dialog-body' }, ...opts.body),
      ...(opts.extra ? [opts.extra] : []),
      h('div', { className: 'files-dialog-actions' }, ...buttons),
    );
    dialog.addEventListener('close', () => {
      dialog.remove();
      resolve(result);
    });
    // Keys typed here stay local (the session forwards only keys aimed at the remote screen).
    dialog.addEventListener('keydown', (e) => e.stopPropagation());
    document.body.append(dialog);
    dialog.showModal();
    (buttons.find((b) => b.classList.contains('primary')) ?? buttons[buttons.length - 1])?.focus();
  });
}

export function confirmDialog(opts: {
  title: string;
  text: string;
  confirm: string;
  danger?: boolean;
  iconName?: IconName;
}): Promise<boolean> {
  return ask({
    title: opts.title,
    body: [h('p', null, opts.text)],
    iconName: opts.iconName,
    choices: [
      { label: 'Cancel', value: false },
      { label: opts.confirm, value: true, primary: !opts.danger, danger: opts.danger },
    ],
  }).then((v) => v === true);
}

/** “report.pdf” already exists in Documents: replace, keep both or skip (optionally for all). */
export async function clashDialog(c: Clash, folderName: string): Promise<{ choice: ClashChoice; all: boolean } | null> {
  const isDir = c.item.kind === 'dir';
  const sameKind = isDir === (c.existing.kind === 'dir');
  const all = h('input', { type: 'checkbox', className: 'switch' });
  const extra =
    c.remaining > 0
      ? h(
          'label',
          { className: 'files-dialog-all' },
          all,
          `Do this for the ${c.remaining === 1 ? 'next clash' : `next ${c.remaining} clashes`} too`,
        )
      : undefined;
  const incoming =
    c.item.kind === 'file' ? `${formatSize(c.item.file.size)}, ${formatMtime(c.item.file.lastModified)}` : 'folder';
  const there =
    c.existing.kind === 'dir' ? 'folder' : `${formatSize(c.existing.size)}, ${formatMtime(c.existing.mtime_ms)}`;
  const choices: Array<Choice<ClashChoice>> = [
    { label: 'Skip', value: 'skip' },
    { label: 'Keep both', value: 'keep' },
  ];
  if (sameKind) {
    choices.push({ label: isDir ? 'Merge' : 'Replace', value: 'replace', primary: true });
  } else choices[1]!.primary = true;
  const choice = await ask({
    title: `“${c.item.name}” already exists`,
    iconName: 'alert',
    body: [
      h('p', null, `${folderName} already has ${c.existing.kind === 'dir' ? 'a folder' : 'a file'} with this name.`),
      h(
        'dl',
        { className: 'files-clash' },
        h('dt', null, 'Uploading'),
        h('dd', null, incoming),
        h('dt', null, 'On the remote machine'),
        h('dd', null, there),
      ),
      ...(sameKind && isDir
        ? [h('p', { className: 'muted' }, 'Merge adds the files to the folder and replaces files with the same name.')]
        : []),
    ],
    extra,
    choices,
  });
  return choice === null ? null : { choice, all: all.checked };
}

/** A one-line message with an OK button. */
export function noticeDialog(title: string, text: string): Promise<void> {
  return ask({ title, body: [h('p', null, text)], choices: [{ label: 'OK', value: true, primary: true }] }).then(
    () => undefined,
  );
}
