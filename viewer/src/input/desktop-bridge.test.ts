import { describe, expect, it } from 'vitest';
import { BRIDGE_VERSION, type BridgeHost, DesktopBridge } from './desktop-bridge';

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
  const bridge = new DesktopBridge(
    host,
    { key: (code, down, repeat) => keys.push([code, down, repeat]), release: () => releases++ },
    (_msg, err) => warnings.push(err),
  );
  return { bridge, keys, warnings, releases: () => releases };
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
  it('is invoked on every transition, not on repeats', () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(false);
    expect(calls).toEqual([]);
    bridge.setActive(true);
    bridge.setActive(true);
    bridge.setActive(false);
    bridge.setActive(true);
    expect(calls.map((c) => [c.cmd, c.args])).toEqual([
      ['rd_set_grab', { active: true }],
      ['rd_set_grab', { active: false }],
      ['rd_set_grab', { active: true }],
    ]);
  });

  it('ignores DOM keys once the app accepted the grab, until it ends', async () => {
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

  it('a late answer does not confirm a grab that already ended', async () => {
    const { host, calls } = fakeTauri();
    const { bridge } = setup(host);
    bridge.setActive(true);
    bridge.setActive(false);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(false);
    calls[2]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
  });

  it('release() from the app gives the keys back to the page', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, releases } = setup(host);
    bridge.setActive(true);
    calls[0]!.resolve();
    await flush();
    host.rdNative!.release();
    expect(releases()).toBe(1);
    expect(bridge.grabbing).toBe(false);
    expect(bridge.active).toBe(true);
    // An answer still on its way when the app released does not count either.
    bridge.setActive(false);
    bridge.setActive(true);
    host.rdNative!.release();
    calls[2]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(false);
    // The next transition asks again.
    bridge.setActive(false);
    bridge.setActive(true);
    calls[4]!.resolve();
    await flush();
    expect(bridge.grabbing).toBe(true);
  });

  it('logs a failing invoke once and keeps the DOM keys', async () => {
    const { host, calls } = fakeTauri();
    const { bridge, warnings } = setup(host);
    bridge.setActive(true);
    calls[0]!.reject(new Error('command rd_set_grab not found'));
    await flush();
    expect(bridge.grabbing).toBe(false);
    bridge.setActive(false);
    bridge.setActive(true);
    calls[1]!.reject(new Error('again'));
    calls[2]!.reject(new Error('again'));
    await flush();
    expect(warnings).toHaveLength(1);
    expect(bridge.grabbing).toBe(false);
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
