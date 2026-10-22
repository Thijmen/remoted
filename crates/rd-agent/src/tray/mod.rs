//! The agent's icon in the macOS menu bar / Windows notification area, with its menu: the
//! connection status and who is connected, pause / resume remote access, updates, start at
//! login, quit. Turned off with `[tray] enabled = false` or `rd-agent run --no-tray`.
//!
//! [`model`] turns what the agent knows into a [`Model`] (what the menu shows); [`control`]
//! keeps it current and carries out the clicks; `ui.rs` draws it with `tray-icon`, on the main
//! thread, which runs the platform's event loop while the agent runs on another one.

use std::sync::Arc;
use std::time::Duration;

use rd_agent_core::runtime::{Activity, Connection, PresenceKind};
use rd_proto::control::{UpdatePhase, UpdateStatus};
use tokio::sync::{Notify, mpsc, watch};
use tracing::{info, warn};

use crate::update::service::Command as UpdateCommand;

#[cfg(any(target_os = "macos", windows))]
mod icon;
#[cfg(any(target_os = "macos", windows))]
mod ui;
#[cfg(any(target_os = "macos", windows))]
pub use ui::{Ui, available, run};

/// Stand-ins where there is no tray: [`available`] is false, so the rest is never used.
#[cfg(not(any(target_os = "macos", windows)))]
mod none {
    pub struct Ui;

    pub fn available() -> bool {
        false
    }

    pub fn run<T: Send + 'static>(work: impl FnOnce(Ui) -> T + Send + 'static) -> T {
        work(Ui)
    }

    impl Ui {
        pub fn show(&self, _model: super::Model) {}

        pub async fn next_action(&mut self) -> Option<super::Action> {
            std::future::pending().await
        }
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
pub use none::{Ui, available, run};

/// A menu item that does something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pause,
    Resume,
    CheckForUpdates,
    InstallUpdate,
    ToggleStartAtLogin,
    Quit,
}

// The ids are read back only where there is a tray (macOS, Windows).
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
impl Action {
    const ALL: [Action; 6] = [
        Action::Pause,
        Action::Resume,
        Action::CheckForUpdates,
        Action::InstallUpdate,
        Action::ToggleStartAtLogin,
        Action::Quit,
    ];

    /// The menu item id.
    pub fn id(self) -> &'static str {
        match self {
            Action::Pause => "pause",
            Action::Resume => "resume",
            Action::CheckForUpdates => "check",
            Action::InstallUpdate => "install",
            Action::ToggleStartAtLogin => "login",
            Action::Quit => "quit",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.id() == id)
    }
}

/// How the icon looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Online, nobody connected.
    Idle,
    /// Someone is connected (a dot on the icon).
    Active,
    /// Not reachable from rd: paused, offline or connecting (dimmed).
    Away,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Greyed-out text.
    Label(String),
    Button {
        action: Action,
        text: String,
        enabled: bool,
    },
    Check {
        action: Action,
        text: String,
        enabled: bool,
        checked: bool,
    },
    Separator,
}

/// Everything the icon and its menu show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub icon: IconState,
    pub tooltip: String,
    pub items: Vec<Item>,
}

/// What the menu is made from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inputs {
    pub version: String,
    pub machine: String,
    pub activity: Activity,
    pub paused: bool,
    pub update: UpdateStatus,
    /// A check asked for in the menu is running.
    pub checking: bool,
    /// Whether the login service starts the agent at login; `None` when it is not installed
    /// (an agent run by hand), so there is nothing to switch.
    pub start_at_login: Option<bool>,
}

pub fn model(i: &Inputs) -> Model {
    let mut items = vec![Item::Label(format!("RD Agent {}", i.version))];
    let connection = match &i.activity.connection {
        Connection::Online => format!("Online as {}", i.machine),
        Connection::Connecting => "Connecting to the gateway…".to_owned(),
        Connection::Offline { reason } => format!("Offline: {reason}"),
        Connection::Paused => "Paused: not reachable from rd".to_owned(),
        Connection::Stopped => "Stopped".to_owned(),
    };
    items.push(Item::Label(connection));
    let mut people: Vec<String> = Vec::new();
    for p in &i.activity.present {
        let who = if p.user.is_empty() {
            "Someone"
        } else {
            &p.user
        };
        let line = match p.kind {
            PresenceKind::Screen => format!("{who} is viewing this screen"),
            PresenceKind::Files => format!("{who} is transferring files"),
        };
        if !people.contains(&line) {
            people.push(line);
        }
    }
    items.extend(people.into_iter().map(Item::Label));

    items.push(Item::Separator);
    items.push(if i.paused {
        Item::Button {
            action: Action::Resume,
            text: "Resume remote access".into(),
            enabled: true,
        }
    } else {
        Item::Button {
            action: Action::Pause,
            text: "Pause remote access".into(),
            enabled: true,
        }
    });

    items.push(Item::Separator);
    items.push(update_item(i));
    items.push(Item::Check {
        action: Action::ToggleStartAtLogin,
        text: "Start at login".into(),
        enabled: i.start_at_login.is_some(),
        checked: i.start_at_login.unwrap_or(false),
    });
    items.push(Item::Separator);
    items.push(Item::Button {
        action: Action::Quit,
        text: "Quit RD Agent".into(),
        enabled: true,
    });

    let online = i.activity.connection == Connection::Online;
    let connected = i.activity.present.len();
    let (icon, state) = match (online, connected) {
        (true, 0) => (IconState::Idle, "online".to_owned()),
        (true, 1) => (IconState::Active, "1 person connected".to_owned()),
        (true, n) => (IconState::Active, format!("{n} connections")),
        (false, _) if i.paused => (IconState::Away, "paused".to_owned()),
        (false, _) => (IconState::Away, "offline".to_owned()),
    };
    Model {
        icon,
        tooltip: format!("RD Agent: {state}"),
        items,
    }
}

fn update_item(i: &Inputs) -> Item {
    let u = &i.update;
    let button = |action, text: String, enabled| Item::Button {
        action,
        text,
        enabled,
    };
    match u.phase {
        UpdatePhase::Downloading => {
            return button(
                Action::InstallUpdate,
                "Downloading the update…".into(),
                false,
            );
        }
        UpdatePhase::Installing => {
            return button(
                Action::InstallUpdate,
                "Installing the update…".into(),
                false,
            );
        }
        _ => {}
    }
    if i.checking {
        return button(
            Action::CheckForUpdates,
            "Checking for updates…".into(),
            false,
        );
    }
    match &u.available {
        Some(a) if u.can_install => button(
            Action::InstallUpdate,
            format!("Install update {}", a.version),
            true,
        ),
        Some(a) => button(
            Action::CheckForUpdates,
            format!("Update {} available", a.version),
            false,
        ),
        None if !u.error.is_empty() => button(
            Action::CheckForUpdates,
            "Check for updates (the last try failed)".into(),
            true,
        ),
        None if u.checked_ms.is_some() => button(
            Action::CheckForUpdates,
            "Up to date: check again".into(),
            true,
        ),
        None => button(Action::CheckForUpdates, "Check for updates".into(), true),
    }
}

/// What [`control`] works with.
pub struct Hooks {
    pub version: String,
    pub machine: String,
    pub activity: watch::Receiver<Activity>,
    pub update: watch::Receiver<UpdateStatus>,
    pub paused: watch::Sender<bool>,
    pub updates: mpsc::UnboundedSender<UpdateCommand>,
    /// Notified to shut the agent down (Quit, when the service manager does not do it).
    pub quit: Arc<Notify>,
}

/// How long the menu says "Checking…" at most (the check itself has its own timeouts).
const CHECK_SHOWN: Duration = Duration::from_secs(30);

/// Keeps the menu current and carries out its clicks, until the runtime goes away.
pub async fn control(mut ui: Ui, mut hooks: Hooks) {
    let mut inputs = Inputs {
        version: hooks.version.clone(),
        machine: hooks.machine.clone(),
        start_at_login: start_at_login().await,
        ..Inputs::default()
    };
    let mut paused = hooks.paused.subscribe();
    let mut shown: Option<Model> = None;
    let mut check_until = tokio::time::Instant::now();
    loop {
        inputs.activity = hooks.activity.borrow_and_update().clone();
        inputs.update = hooks.update.borrow_and_update().clone();
        inputs.paused = *paused.borrow_and_update();
        let next = model(&inputs);
        if shown.as_ref() != Some(&next) {
            ui.show(next.clone());
            shown = Some(next);
        }
        tokio::select! {
            r = hooks.activity.changed() => if r.is_err() { return },
            // Every check reports when it is done, found something or not.
            r = hooks.update.changed() => match r {
                Err(_) => return,
                Ok(()) => inputs.checking = false,
            },
            _ = paused.changed() => {}
            () = tokio::time::sleep_until(check_until), if inputs.checking => inputs.checking = false,
            action = ui.next_action() => match action {
                None => return,
                Some(action) => {
                    act(action, &mut inputs, &hooks).await;
                    if inputs.checking {
                        check_until = tokio::time::Instant::now() + CHECK_SHOWN;
                    }
                }
            },
        }
    }
}

async fn act(action: Action, inputs: &mut Inputs, hooks: &Hooks) {
    info!(action = action.id(), "tray menu");
    match action {
        Action::Pause => {
            hooks.paused.send_replace(true);
        }
        Action::Resume => {
            hooks.paused.send_replace(false);
        }
        Action::CheckForUpdates => {
            inputs.checking = hooks.updates.send(UpdateCommand::Check).is_ok();
        }
        Action::InstallUpdate => {
            if let Some(a) = &inputs.update.available {
                let _ = hooks
                    .updates
                    .send(UpdateCommand::Install(a.version.clone()));
            }
        }
        Action::ToggleStartAtLogin => {
            if let Some(on) = inputs.start_at_login {
                let set = tokio::task::spawn_blocking(move || {
                    crate::host::service::set_start_at_login(!on)
                })
                .await;
                match set {
                    Ok(Ok(())) => info!(on = !on, "start at login changed"),
                    Ok(Err(e)) => warn!("changing start at login: {e:#}"),
                    Err(e) => warn!("changing start at login: {e}"),
                }
                inputs.start_at_login = start_at_login().await;
            }
        }
        Action::Quit => {
            info!("quit from the tray menu");
            // Under the login service, it must not start the agent again right away.
            if !crate::host::service::stop_from_within() {
                hooks.quit.notify_one();
                return;
            }
            // The service manager stops this process; in case it does not, stop anyway.
            let quit = hooks.quit.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                warn!("the login service did not stop the agent; stopping");
                quit.notify_one();
            });
        }
    }
}

async fn start_at_login() -> Option<bool> {
    match tokio::task::spawn_blocking(crate::host::service::start_at_login).await {
        Ok(Ok(on)) => on,
        Ok(Err(e)) => {
            warn!("reading start at login: {e:#}");
            None
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use rd_agent_core::runtime::Presence;
    use rd_proto::control::AvailableUpdate;

    use super::*;

    fn inputs() -> Inputs {
        Inputs {
            version: "1.8.0".into(),
            machine: "mbp".into(),
            activity: Activity {
                connection: Connection::Online,
                present: Vec::new(),
            },
            start_at_login: Some(true),
            ..Inputs::default()
        }
    }

    fn labels(m: &Model) -> Vec<&str> {
        m.items
            .iter()
            .filter_map(|i| match i {
                Item::Label(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn button(m: &Model, action: Action) -> Option<(&str, bool)> {
        m.items.iter().find_map(|i| match i {
            Item::Button {
                action: a,
                text,
                enabled,
            } if *a == action => Some((text.as_str(), *enabled)),
            _ => None,
        })
    }

    #[test]
    fn online_and_idle() {
        let m = model(&inputs());
        assert_eq!(m.icon, IconState::Idle);
        assert_eq!(m.tooltip, "RD Agent: online");
        assert_eq!(labels(&m), ["RD Agent 1.8.0", "Online as mbp"]);
        assert_eq!(
            button(&m, Action::Pause),
            Some(("Pause remote access", true))
        );
        assert_eq!(button(&m, Action::Resume), None);
        assert_eq!(
            button(&m, Action::CheckForUpdates),
            Some(("Check for updates", true))
        );
        assert!(m.items.contains(&Item::Check {
            action: Action::ToggleStartAtLogin,
            text: "Start at login".into(),
            enabled: true,
            checked: true,
        }));
        assert_eq!(button(&m, Action::Quit), Some(("Quit RD Agent", true)));
    }

    #[test]
    fn who_is_connected() {
        let mut i = inputs();
        i.activity.present = vec![
            Presence::new(PresenceKind::Screen, "me@x"),
            Presence::new(PresenceKind::Files, "me@x"),
            Presence::new(PresenceKind::Files, "me@x"),
            Presence::new(PresenceKind::Screen, ""),
        ];
        let m = model(&i);
        assert_eq!(m.icon, IconState::Active);
        assert_eq!(m.tooltip, "RD Agent: 4 connections");
        assert_eq!(
            labels(&m)[2..],
            [
                "me@x is viewing this screen",
                "me@x is transferring files",
                "Someone is viewing this screen"
            ]
        );
        i.activity.present.truncate(1);
        assert_eq!(model(&i).tooltip, "RD Agent: 1 person connected");
    }

    #[test]
    fn paused_and_offline_are_dimmed() {
        let mut i = inputs();
        i.paused = true;
        i.activity.connection = Connection::Paused;
        let m = model(&i);
        assert_eq!(m.icon, IconState::Away);
        assert_eq!(m.tooltip, "RD Agent: paused");
        assert!(labels(&m).contains(&"Paused: not reachable from rd"));
        assert_eq!(
            button(&m, Action::Resume),
            Some(("Resume remote access", true))
        );

        let mut i = inputs();
        i.activity.connection = Connection::Offline {
            reason: "cannot reach the gateway".into(),
        };
        let m = model(&i);
        assert_eq!(m.icon, IconState::Away);
        assert_eq!(m.tooltip, "RD Agent: offline");
        assert!(labels(&m).contains(&"Offline: cannot reach the gateway"));
    }

    #[test]
    fn update_item() {
        let mut i = inputs();
        i.checking = true;
        assert_eq!(
            button(&model(&i), Action::CheckForUpdates),
            Some(("Checking for updates…", false))
        );
        i.checking = false;
        i.update.checked_ms = Some(1);
        assert_eq!(
            button(&model(&i), Action::CheckForUpdates),
            Some(("Up to date: check again", true))
        );
        i.update.available = Some(AvailableUpdate {
            version: "1.9.0".into(),
            ..Default::default()
        });
        assert_eq!(
            button(&model(&i), Action::CheckForUpdates),
            Some(("Update 1.9.0 available", false))
        );
        i.update.can_install = true;
        assert_eq!(
            button(&model(&i), Action::InstallUpdate),
            Some(("Install update 1.9.0", true))
        );
        i.update.phase = UpdatePhase::Downloading;
        assert_eq!(
            button(&model(&i), Action::InstallUpdate),
            Some(("Downloading the update…", false))
        );
    }

    #[test]
    fn no_login_service() {
        let mut i = inputs();
        i.start_at_login = None;
        assert!(model(&i).items.contains(&Item::Check {
            action: Action::ToggleStartAtLogin,
            text: "Start at login".into(),
            enabled: false,
            checked: false,
        }));
    }

    #[test]
    fn action_ids_round_trip() {
        for a in Action::ALL {
            assert_eq!(Action::from_id(a.id()), Some(a));
        }
        assert_eq!(Action::from_id("nope"), None);
    }
}
