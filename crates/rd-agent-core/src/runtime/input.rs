//! The input thread: owns the session's [`InputInjector`] so injection (which may block in the
//! OS) never stalls the network tasks. Events are applied strictly in arrival order.
//!
//! While the host is locked, input is dropped — unless the secure-desktop helper is active
//! (Windows), in which case the injector forwards it to the helper and the helper types it at
//! the real lock screen.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tracing::{error, info, warn};

use crate::platform::{InputEvent, InputInjector, LockState, Platform};

/// While input arrives, tell the OS the user is active this often (wakes/keeps the display on).
const USER_ACTIVITY_INTERVAL: Duration = Duration::from_secs(30);
/// How long session teardown waits for the thread to release everything.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The lock state is re-read at most this often (on the next event after it expired).
const LOCK_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// Called on the input thread the first time input is dropped in a locked period (once per
/// period), so the viewer can be told the host is locked.
pub(crate) type OnLockedDrop = Box<dyn Fn() + Send>;

pub(crate) struct InputThread {
    tx: Sender<InputEvent>,
    /// Dropping or signalling it ends the thread, whoever still holds an event sender.
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

impl InputThread {
    pub(crate) fn spawn(
        injector: Box<dyn InputInjector>,
        platform: Arc<dyn Platform>,
        on_locked_drop: OnLockedDrop,
    ) -> std::io::Result<Self> {
        // Unbounded: the network reader must never block, and input is never lost on the way
        // (only deliberately dropped while the host is locked).
        let (tx, rx) = crossbeam_channel::unbounded();
        let (stop, stop_rx) = crossbeam_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("rd-input".into())
            .spawn(move || input_loop(injector, platform, on_locked_drop, rx, stop_rx))?;
        Ok(InputThread { tx, stop, thread })
    }

    /// A sender for the network reader.
    pub(crate) fn sender(&self) -> Sender<InputEvent> {
        self.tx.clone()
    }

    /// Stops the thread, which releases every held key and button on the way out. Waits up to
    /// [`JOIN_TIMEOUT`].
    pub(crate) async fn finish(self) {
        let InputThread { tx, stop, thread } = self;
        drop(tx);
        let _ = stop.send(());
        let join = tokio::task::spawn_blocking(move || thread.join());
        match tokio::time::timeout(JOIN_TIMEOUT, join).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) | Ok(Err(_)) => error!("input thread panicked"),
            Err(_) => warn!("input thread did not stop in time"),
        }
    }
}

fn input_loop(
    mut injector: Box<dyn InputInjector>,
    platform: Arc<dyn Platform>,
    on_locked_drop: OnLockedDrop,
    rx: Receiver<InputEvent>,
    stop: Receiver<()>,
) {
    // The session declared activity when it started.
    let mut last_activity = Instant::now();
    let mut lock = LockCheck::new(platform.clone());
    loop {
        crossbeam_channel::select! {
            recv(rx) -> ev => {
                let Ok(ev) = ev else { break };
                let delivered = match ev {
                    InputEvent::ReleaseAll => {
                        injector.release_all();
                        true
                    }
                    _ if lock.locked(Instant::now()) && !platform.secure_input_ready() => {
                        if lock.dropped == 0 {
                            info!("the host is locked: dropping viewer input until it is unlocked");
                            // Keys held when the lock came up (say, the lock shortcut itself)
                            // must not stay down; releasing them types nothing.
                            injector.release_all();
                            on_locked_drop();
                        }
                        lock.dropped += 1;
                        false
                    }
                    ev => {
                        injector.inject(ev);
                        true
                    }
                };
                // Dropped input is not user activity: nothing reached the host.
                if delivered && last_activity.elapsed() >= USER_ACTIVITY_INTERVAL {
                    platform.declare_user_activity();
                    last_activity = Instant::now();
                }
            }
            // A stop signal or the session dropping us.
            recv(stop) -> _ => break,
        }
    }
    // The session is over: never leave anything held down.
    injector.release_all();
}

/// Cached lock state (the platform query may cost a system call or two).
struct LockCheck {
    platform: Arc<dyn Platform>,
    checked_at: Option<Instant>,
    locked: bool,
    /// Events dropped in the current locked period; 0 until the first one.
    dropped: u64,
}

impl LockCheck {
    fn new(platform: Arc<dyn Platform>) -> Self {
        LockCheck {
            platform,
            checked_at: None,
            locked: false,
            dropped: 0,
        }
    }

    /// Whether the host is locked, re-read when the cached value is older than
    /// [`LOCK_CHECK_INTERVAL`]. An unknown state counts as unlocked.
    fn locked(&mut self, now: Instant) -> bool {
        if self
            .checked_at
            .is_none_or(|t| now.saturating_duration_since(t) >= LOCK_CHECK_INTERVAL)
        {
            self.checked_at = Some(now);
            let locked = self.platform.lock_state() == LockState::Locked;
            if !locked && self.dropped > 0 {
                info!(
                    dropped = self.dropped,
                    "the host is unlocked: injecting viewer input again"
                );
                self.dropped = 0;
            }
            self.locked = locked;
        }
        self.locked
    }
}
