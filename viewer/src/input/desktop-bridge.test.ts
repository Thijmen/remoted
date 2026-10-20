import { describe, expect, it, vi } from 'vitest';
import {
  BRIDGE_VERSION,
  type BridgeHost,
  DesktopBridge,
  INVOKE_TIMEOUT_MS,
  NATIVE_RECENT_MS,
  REASSERT_INTERVAL_MS,
} from './desktop-bridge';

interface Call {
  cmd: string;
  args: Record<string, unknown> | undefined;
  resolve(): void;
  reject(err: unknown): void;
}

/** A page host with a fake Tauri IPC whose answers the test controls. */
function fakeTauri() {
  const calls: Call[] = [];
  const host: BridgeHost = {
    __TAURI_INTERNALS__: {
      invoke: (cmd, args) =>
        new Promise<void>((resolve, reject) => calls.push({ cmd, args, resolve: () => resolve(), reject })),
    },
  };
  return { host, calls };
}

function setup(host: BridgeHost) {
  const keys: Array<[string, boolean, boolean]> = [];
  let releases = 0;
  const warnings: unknown[] = [];
  const clock = { now: 100_000 };
  const bridge = new DesktopBridge(
    host,
    { key: (code, down, repeat) => keys.push([code, down, repeat]), release: () => releases++ },
    (_msg, err) => warnings.push(err),
    () => clock.now,
  );
  return { bridge, keys, warnings, releases: () => releases, clock };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe('window.rdNative', () => {
  it('is always defined (also in a plain browser) and routes keys and release', () => {
    const host: BridgeHost = {};
    const { bridge, keys, releases } = setup(host);
    expect(host.rdNative?.version).toBe(BRIDGE_VERSION);
    expect(BRIDGE_VERSION).toBe(1);
    host.rdNative!.key('Tab', true, false);
    host.rdNative!.key('Tab', false, undefined as unknown as boolean);
    host.rdNative!.release();
    expect(keys).toEqual([
      ['Tab', true, false],
      ['Tab', false, false],
    ]);
    expect(releases()).toBe(1);
    // No app: the grab state is tracked, but DOM keys are never ignored.
    bridge.setActive(true);
    expect(bridge.active).toBe(true);
    expect(bridge.grabbing).toBe(false);
  });

  it('ignores malformed calls', () => {
    const host: BridgeHost = {};
    const { keys } = setup(host);
    const key = host.rdNative!.key as (...a: unknown[]) => void;
    key(17, true, false);
    key('KeyA', 'yes', false);
    key();
    expect(keys).toEqual([]);
  });
});

describe('rd_set_grab', () => {
  const sent = (calls: Call[]) => calls.map((c) => c.args?.active);

  it('sends one request at a time, numbered, and only the latest wanted state', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(false);
    expect(calls).toEqual([]);
    bridge.setActive(true);
    bridge.setActive(true);
    bridge.setActive(false);
    bridge.setActive(true);
    bridge.setActive(false);
    // One in flight; the burst behind it collapses to its final state.
    expect(sent(calls)).toEqual([true]);
    calls[0]!.resolve();
    await flush();
    expect(sent(calls)).toEqual([true, false]);
    calls[1]!.resolve();
    await flush();
    expect(sent(calls)).toEqual([true, false]);
    expect(calls.map((c) => c.cmd)).toEqual(['rd_set_grab', 'rd_set_grab']);
    const seqs = calls.map((c) => c.args?.seq as number);
    expect(seqs[1]).toBeGreaterThan(seqs[0]!);
  });

  it('confirms once the app accepted `true` and the page still wants it', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(true);
    expect(bridge.grabbing).toBe(false); // no answer yet: the hook may not be in place
    calls[0]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
    bridge.setActive(false);
    expect(bridge.grabbing).toBe(false);
    calls[1]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(false);
  });

  it('a burst that ends where it started needs no extra request', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(true);
    bridge.setActive(false);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    expect(sent(calls)).toEqual([true]);
    expect(bridge.grabbing).toBe(true);
  });

  it('release() from the app gives the keys back and the next eligible state asks again', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, releases } = setup(host);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    host.rdNative!.release();
    expect(releases()).toBe(1);
    expect(bridge.grabbing).toBe(false);
    expect(bridge.active).toBe(false);
    bridge.setActive(true);
    expect(sent(calls)).toEqual([true, true]);
    calls[1]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
  });

  it('does not repeat a refused request until a change or retry()', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, warnings } = setup(host);
    bridge.setActive(true);
    calls[0]!.reject(new Error('the rd window is not the foreground window'));
    await flush();
    expect(bridge.grabbing).toBe(false);
    expect(sent(calls)).toEqual([true]);
    bridge.retry();
    expect(sent(calls)).toEqual([true, true]);
    calls[1]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
    bridge.retry(); // nothing to retry while grabbing
    bridge.setActive(false);
    calls[2]!.reject(new Error('again'));
    await flush();
    bridge.setActive(true);
    calls[3]!.reject(new Error('again'));
    await flush();
    expect(sent(calls)).toEqual([true, true, false, true]);
    expect(warnings).toHaveLength(1);
  });

  it('treats a request without an answer as refused', async () => {
    vi.useFakeTimers();
    try {
      const { host, calls } = fakeTauri();
      const { bridge } = setup(host);
      bridge.setActive(true);
      vi.advanceTimersByTime(INVOKE_TIMEOUT_MS + 1);
      expect(bridge.grabbing).toBe(false);
      // The late answer is ignored; a click asks again.
      calls[0]!.resolve();
      await vi.runAllTimersAsync();
      expect(bridge.grabbing).toBe(false);
      bridge.retry();
      expect(sent(calls)).toEqual([true, true]);
      calls[1]!.resolve();
      await vi.runAllTimersAsync();
      expect(bridge.grabbing).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it('reassert() re-installs the hook (false, then true) when no key came through it lately, backing off', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, warnings, clock } = setup(host);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    bridge.reassert();
    expect(bridge.grabbing).toBe(false);
    expect(sent(calls)).toEqual([true, false]);
    calls[1]!.resolve();
    await flush();
    expect(sent(calls)).toEqual([true, false, true]);
    calls[2]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
    // Not again within the interval, then the interval doubles.
    clock.now += REASSERT_INTERVAL_MS - 1;
    bridge.reassert();
    expect(sent(calls)).toHaveLength(3);
    clock.now += 1;
    bridge.reassert();
    expect(sent(calls)).toHaveLength(4);
    calls[3]!.resolve();
    await flush();
    calls[4]!.resolve();
    await flush();
    clock.now += REASSERT_INTERVAL_MS;
    bridge.reassert();
    expect(sent(calls)).toHaveLength(5); // waiting for 2 × the interval
    expect(warnings).toHaveLength(1);
  });

  it('reassert() does nothing while keys come through the hook (DOM keys are ones it passes on purpose)', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, clock } = setup(host);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    host.rdNative!.key('KeyA', true, false);
    clock.now += NATIVE_RECENT_MS - 1;
    bridge.reassert();
    expect(sent(calls)).toEqual([true]);
    clock.now += 1;
    bridge.reassert();
    expect(sent(calls)).toEqual([true, false]);
  });

  it('an answer to a call sent before release() does not confirm a grab the app dropped', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(true);
    // The app installs the hook, drops it on its own (e.g. desktop switch) and its release() overtakes the answer.
    host.rdNative!.release();
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(false);
    expect(sent(calls)).toEqual([true, true]); // asked again
    calls[1]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
  });

  it('numbers calls from the clock, so a new page outranks a late call of the old one', () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(true);
    expect(calls[0]!.args?.seq as number).toBeGreaterThan((Date.now() - 60_000) * 1000);
  });

  it('survives an invoke that throws or returns a non-promise', async () => {
    const warnings: unknown[] = [];
    const throwing = new DesktopBridge(
      {
        __TAURI_INTERNALS__: {
          invoke: () => {
            throw new Error('ipc unavailable');
          },
        },
      },
      { key: () => {}, release: () => {} },
      (_m, e) => warnings.push(e),
    );
    expect(() => throwing.setActive(true)).not.toThrow();
    expect(throwing.grabbing).toBe(false);
    expect(warnings).toHaveLength(1);

    const plain = new DesktopBridge(
      { __TAURI_INTERNALS__: { invoke: () => undefined } },
      { key: () => {}, release: () => {} },
    );
    plain.setActive(true);
    await flush();
    expect(plain.grabbing).toBe(true);
  });
});

describe('refusal and other commands', () => {
  it('remembers why the app refused a grab until one is accepted, and reports each answer', async () => {
    const { host, calls } = fakeTauri();
    let changes = 0;
    const bridge = new DesktopBridge(host, { key: () => {}, release: () => {}, changed: () => changes++ }, () => {});
    expect(bridge.inApp).toBe(true);
    expect(bridge.refusal).toBeNull();
    bridge.setActive(true);
    // Tauri rejects with the command's error string.
    calls[0]!.reject('rd Desktop needs the Accessibility permission to grab the keyboard');
    await flush();
    expect(bridge.refusal).toMatch(/Accessibility permission/);
    expect(changes).toBe(1);
    // Turning the grab off keeps the reason (the Troubleshoot screen takes the focus, and so the grab).
    bridge.setActive(false);
    calls[1]!.resolve();
    await flush();
    expect(bridge.refusal).toMatch(/Accessibility permission/);
    bridge.setActive(true);
    calls[2]!.resolve();
    await flush();
    expect(bridge.refusal).toBeNull();
    expect(bridge.grabbing).toBe(true);
    expect(changes).toBe(3);
  });

  it('call() reaches the app, and rejects outside it or without an answer', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    const status = bridge.call('rd_keyboard_status');
    expect(calls.map((c) => c.cmd)).toEqual(['rd_keyboard_status']);
    calls[0]!.resolve();
    await expect(status).resolves.toBeUndefined();

    vi.useFakeTimers();
    try {
      const late = bridge.call('rd_keyboard_status');
      vi.advanceTimersByTime(INVOKE_TIMEOUT_MS);
      await expect(late).rejects.toThrow('no answer');
    } finally {
      vi.useRealTimers();
    }

    const plain = setup({}).bridge;
    expect(plain.inApp).toBe(false);
    await expect(plain.call('rd_keyboard_status')).rejects.toThrow('not in the desktop app');
  });
});
