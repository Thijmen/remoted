/** Screen Wake Lock held while a session is active; re-acquired when the tab becomes visible. */
export class ScreenWakeLock {
  private sentinel: WakeLockSentinel | null = null;
  private wanted = false;

  constructor() {
    document.addEventListener('visibilitychange', () => {
      if (this.wanted && document.visibilityState === 'visible' && !this.sentinel) void this.acquire();
    });
  }

  async acquire(): Promise<void> {
    this.wanted = true;
    if (this.sentinel || !('wakeLock' in navigator)) return;
    try {
      const s = await navigator.wakeLock.request('screen');
      s.addEventListener('release', () => {
        if (this.sentinel === s) this.sentinel = null;
      });
      if (this.wanted) this.sentinel = s;
      else void s.release();
    } catch {
      // Not allowed (hidden tab, battery saver, policy): try again on the next visibility change.
    }
  }

  release(): void {
    this.wanted = false;
    const s = this.sentinel;
    this.sentinel = null;
    void s?.release();
  }
}
