//! What the runtime is doing, for a status display on the machine itself (the binary's menu bar
//! / notification-area icon): the gateway connection, and who is connected right now.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;

/// A snapshot of the runtime's activity; see [`super::Runtime::activity`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Activity {
    pub connection: Connection,
    /// Viewers connected to this machine, oldest first.
    pub present: Vec<Presence>,
}

/// The control connection to the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    /// Dialing or registering.
    #[default]
    Connecting,
    /// Registered: the machine shows as online in the portal.
    Online,
    /// Not connected; retrying after a delay. `reason` is short and meant for people.
    Offline { reason: String },
    /// Paused on this machine ([`super::Runtime::pause`]): not connected until resumed.
    Paused,
    /// The runtime stopped.
    Stopped,
}

/// Someone connected to this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub kind: PresenceKind,
    /// Who, as the gateway named them (an email address); may be empty.
    pub user: String,
    id: u64,
}

impl Presence {
    /// For displays and tests; the runtime lists its own.
    pub fn new(kind: PresenceKind, user: impl Into<String>) -> Self {
        Presence {
            kind,
            user: user.into(),
            id: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresenceKind {
    /// A viewer session: someone sees the screen.
    Screen,
    /// A file channel (docs/files.md).
    Files,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Listed in [`Activity::present`] until dropped.
pub(crate) struct PresenceGuard {
    activity: watch::Sender<Activity>,
    id: u64,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let id = self.id;
        self.activity.send_if_modified(|a| {
            let before = a.present.len();
            a.present.retain(|p| p.id != id);
            a.present.len() != before
        });
    }
}

pub(crate) fn present(
    activity: &watch::Sender<Activity>,
    kind: PresenceKind,
    user: &str,
) -> PresenceGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    activity.send_modify(|a| {
        a.present.push(Presence {
            kind,
            user: user.to_owned(),
            id,
        });
    });
    PresenceGuard {
        activity: activity.clone(),
        id,
    }
}

pub(crate) fn set_connection(activity: &watch::Sender<Activity>, connection: Connection) {
    activity.send_if_modified(|a| {
        let changed = a.connection != connection;
        a.connection = connection;
        changed
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_lasts_as_long_as_its_guard() {
        let activity = watch::Sender::new(Activity::default());
        let screen = present(&activity, PresenceKind::Screen, "me@x");
        let files = present(&activity, PresenceKind::Files, "me@x");
        let kinds = |a: &watch::Sender<Activity>| {
            a.borrow()
                .present
                .iter()
                .map(|p| p.kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(&activity),
            [PresenceKind::Screen, PresenceKind::Files]
        );
        drop(screen);
        assert_eq!(kinds(&activity), [PresenceKind::Files]);
        drop(files);
        assert!(activity.borrow().present.is_empty());
    }

    #[test]
    fn connection_changes_notify_once() {
        let activity = watch::Sender::new(Activity::default());
        let mut rx = activity.subscribe();
        set_connection(&activity, Connection::Online);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        set_connection(&activity, Connection::Online);
        assert!(!rx.has_changed().unwrap());
    }
}
