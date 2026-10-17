//! The input thread: owns the session's [`InputInjector`] so injection (which may block in the
//! OS) never stalls the network tasks. Events are applied strictly in arrival order.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tracing::{error, warn};

use crate::platform::{InputEvent, InputInjector, Platform};

/// While input arrives, tell the OS the user is active this often (wakes/keeps the display on).
const USER_ACTIVITY_INTERVAL: Duration = Duration::from_secs(30);
/// How long session teardown waits for the thread to release everything.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

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
    ) -> std::io::Result<Self> {
        // Unbounded: the network reader must never block, and input is never dropped.
        let (tx, rx) = crossbeam_channel::unbounded();
        let (stop, stop_rx) = crossbeam_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("rd-input".into())
            .spawn(move || input_loop(injector, platform, rx, stop_rx))?;
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
    rx: Receiver<InputEvent>,
    stop: Receiver<()>,
) {
    // The session declared activity when it started.
    let mut last_activity = Instant::now();
    loop {
        crossbeam_channel::select! {
            recv(rx) -> ev => {
                let Ok(ev) = ev else { break };
                match ev {
                    InputEvent::ReleaseAll => injector.release_all(),
                    ev => injector.inject(ev),
                }
                if last_activity.elapsed() >= USER_ACTIVITY_INTERVAL {
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
