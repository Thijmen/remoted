import type { AgentState } from '../proto/control';
import type { Entry, FilesHello } from '../proto/files';
import { type ChannelStatus, FileError, FilesChannel } from './channel';
import { caseSensitive, keepBothName } from './paths';
import type { UploadItem } from './sources';
import { TransferManager, type UploadPlan } from './transfers';

/** What to do with an upload whose name exists in the target folder. */
export type ClashChoice = 'replace' | 'keep' | 'skip';

export interface Clash {
  item: UploadItem;
  existing: Entry;
  /** How many more clashes follow this one. */
  remaining: number;
  folder: string;
}

/** Asks the user about one clash; `all` applies the choice to the rest of this batch. */
export type ClashResolver = (c: Clash) => Promise<{ choice: ClashChoice; all: boolean } | null>;

/**
 * The file channel of one machine and its transfers, shared by the file browser, the session view
 * (drops onto the screen, the transfer indicator) and the page (leave-page guard). Connected while
 * the browser is open or a transfer is unfinished.
 */
export class FilesController {
  readonly channel: FilesChannel;
  readonly transfers: TransferManager;
  hello: FilesHello | null = null;
  state: AgentState | null = null;
  private readonly listeners = new Set<() => void>();
  private browserOpen = false;
  private notifyQueued = false;

  constructor(readonly machine: string) {
    this.channel = new FilesChannel(machine, {
      status: () => this.update(),
      hello: (h) => {
        this.hello = h;
        this.state = h.state;
        this.transfers.onHello(h);
        this.update();
      },
      state: (s) => {
        this.state = s;
        this.transfers.onState(s);
      },
      transfer: (m) => this.transfers.onTransfer(m),
      data: (m) => this.transfers.onData(m),
      ack: (seq) => this.transfers.onAck(seq),
      lost: () => this.transfers.onLost(),
    });
    this.transfers = new TransferManager(this.channel, () => this.update());
  }

  get status(): ChannelStatus {
    return this.channel.current;
  }

  /** Called on every change (connection, transfers); returns the unsubscribe function. */
  subscribe(f: () => void): () => void {
    this.listeners.add(f);
    return () => this.listeners.delete(f);
  }

  setBrowserOpen(open: boolean): void {
    this.browserOpen = open;
    this.update();
  }

  /** Where a drop onto the remote screen goes when the browser has not been opened yet. */
  async defaultDropFolder(): Promise<string> {
    await this.channel.whenReady();
    const h = this.hello!;
    return h.places.find((p) => p.kind === 'desktop')?.path ?? h.home;
  }

  /**
   * Upload `items` into the remote folder `dest`, asking `resolve` about names that exist there.
   * Returns how many items were queued (0 when everything was skipped or the user cancelled).
   */
  async upload(items: UploadItem[], dest: string, resolve: ClashResolver): Promise<number> {
    if (items.length === 0) return 0;
    // Keep the channel while the user decides about clashes (the browser may be closed).
    const release = this.channel.hold();
    try {
      return await this.planUpload(items, dest, resolve);
    } finally {
      release();
    }
  }

  private async planUpload(items: UploadItem[], dest: string, resolve: ClashResolver): Promise<number> {
    const reply = await this.channel.request({ t: 'list', path: dest });
    if (reply.t !== 'list') throw new FileError('io', 'unexpected answer');
    const sensitive = caseSensitive(this.hello?.os ?? '');
    const norm = (s: string) => (sensitive ? s : s.toLowerCase());
    const existing = new Map(reply.entries.map((e) => [norm(e.name), e]));
    const taken = new Set(reply.entries.map((e) => e.name));
    const clashes = items.filter((it) => existing.has(norm(it.name)));
    const plans: UploadPlan[] = [];
    let always: ClashChoice | null = null;
    let asked = 0;
    for (const item of items) {
      const there = existing.get(norm(item.name));
      if (!there) {
        plans.push({ item, name: item.name, mode: 'new' });
        taken.add(item.name);
        continue;
      }
      let choice: ClashChoice | null = always;
      if (!choice) {
        asked++;
        const answer = await resolve({ item, existing: there, remaining: clashes.length - asked, folder: dest });
        if (!answer) return 0; // the user cancelled the whole upload
        choice = answer.choice;
        if (answer.all) always = choice;
      }
      const sameKind = (item.kind === 'dir') === (there.kind === 'dir');
      if (choice === 'skip') continue;
      if (choice === 'replace' && sameKind) {
        plans.push({ item, name: item.name, mode: item.kind === 'dir' ? 'merge' : 'replace' });
      } else if (item.kind === 'dir' || !sameKind) {
        // Folders (and a file meeting a folder) get their new name here; files get it from the agent.
        const name = keepBothName(item.name, taken, item.kind === 'dir', sensitive);
        taken.add(name);
        plans.push({ item, name, mode: 'new' });
      } else {
        plans.push({ item, name: item.name, mode: 'keep' });
      }
    }
    this.transfers.upload(plans, dest);
    return plans.length;
  }

  private update(): void {
    this.channel.want(this.browserOpen || this.transfers.busy);
    if (this.notifyQueued) return;
    this.notifyQueued = true;
    queueMicrotask(() => {
      this.notifyQueued = false;
      for (const f of this.listeners) f();
    });
  }
}
