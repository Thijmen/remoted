// What the user hands us to upload: dropped files and folders, or the file pickers.

/** A file or a folder (with its contents) to upload. */
export type UploadItem =
  { kind: 'file'; name: string; file: File } | { kind: 'dir'; name: string; children: UploadItem[] };

export interface ItemTotals {
  files: number;
  dirs: number;
  bytes: number;
}

export function totals(items: readonly UploadItem[]): ItemTotals {
  const t: ItemTotals = { files: 0, dirs: 0, bytes: 0 };
  const walk = (list: readonly UploadItem[]) => {
    for (const it of list) {
      if (it.kind === 'file') {
        t.files++;
        t.bytes += it.file.size;
      } else {
        t.dirs++;
        walk(it.children);
      }
    }
  };
  walk(items);
  return t;
}

/** Does this drag carry files (not text or a link)? */
export function dragHasFiles(dt: DataTransfer | null): boolean {
  return !!dt && Array.from(dt.types).includes('Files');
}

/**
 * The files and folders of a drop. Must be called during the drop event: the entries are taken
 * synchronously, their contents are read afterwards.
 */
export function itemsFromDrop(dt: DataTransfer): Promise<UploadItem[]> {
  const entries: FileSystemEntry[] = [];
  const loose: File[] = [];
  for (const item of Array.from(dt.items)) {
    if (item.kind !== 'file') continue;
    const entry = item.webkitGetAsEntry?.();
    if (entry) entries.push(entry);
    else {
      const f = item.getAsFile();
      if (f) loose.push(f);
    }
  }
  if (entries.length === 0 && loose.length === 0) loose.push(...Array.from(dt.files));
  return Promise.all(entries.map(fromEntry)).then((items) => [
    ...items.filter((i): i is UploadItem => i !== null),
    ...loose.map((file) => ({ kind: 'file' as const, name: file.name, file })),
  ]);
}

async function fromEntry(entry: FileSystemEntry): Promise<UploadItem | null> {
  if (entry.isFile) {
    try {
      const file = await new Promise<File>((resolve, reject) => (entry as FileSystemFileEntry).file(resolve, reject));
      return { kind: 'file', name: entry.name, file };
    } catch {
      return null; // unreadable (permissions, a file that vanished)
    }
  }
  if (entry.isDirectory) {
    const reader = (entry as FileSystemDirectoryEntry).createReader();
    const children: FileSystemEntry[] = [];
    // readEntries returns batches (about 100 in Chrome) until an empty one.
    for (;;) {
      const batch = await new Promise<FileSystemEntry[]>((resolve) => reader.readEntries(resolve, () => resolve([])));
      if (batch.length === 0) break;
      children.push(...batch);
    }
    const items = await Promise.all(children.map(fromEntry));
    return { kind: 'dir', name: entry.name, children: items.filter((i): i is UploadItem => i !== null) };
  }
  return null;
}

/**
 * Files from a file picker. With a folder picker (`webkitdirectory`) every file has a relative
 * path (`Photos/2024/a.jpg`), from which the folders are rebuilt. (Empty folders are not reported
 * by browsers.)
 */
export function itemsFromFiles(files: Iterable<File>): UploadItem[] {
  const root: UploadItem[] = [];
  for (const file of files) {
    const rel = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
    const parts = rel.split('/').filter(Boolean);
    let level = root;
    for (const dir of parts.slice(0, -1)) {
      let next = level.find((i): i is Extract<UploadItem, { kind: 'dir' }> => i.kind === 'dir' && i.name === dir);
      if (!next) {
        next = { kind: 'dir', name: dir, children: [] };
        level.push(next);
      }
      level = next.children;
    }
    level.push({ kind: 'file', name: parts[parts.length - 1] ?? file.name, file });
  }
  return root;
}
