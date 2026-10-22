//! `rd-agent status`: what is configured and whether it works. Read-only: never prompts, never
//! registers with the gateway.

use std::path::Path;

use rd_agent_core::config::AgentConfig;

use crate::config::TokenSource;
use crate::{config, host};

pub fn run(explicit: Option<&Path>) -> anyhow::Result<()> {
    let path = config::path(explicit)?;
    let cfg = if path.is_file() {
        match AgentConfig::load(&path) {
            Ok(cfg) => {
                println!("config:       {}", path.display());
                println!("  gateway:    {}", cfg.gateway_url);
                println!("  machine:    {}", cfg.machine);
                Some(cfg)
            }
            Err(e) => {
                println!("config:       {} (unreadable: {e:#})", path.display());
                None
            }
        }
    } else {
        println!(
            "config:       {} (missing; run `rd-agent setup`)",
            path.display()
        );
        None
    };

    // Resolved like `run` does, so this shows the token the agent will use. The Keychain is
    // only read when nothing else has a token. The token itself is never printed.
    let lookup = cfg.clone().unwrap_or_else(|| AgentConfig {
        gateway_url: String::new(),
        machine: String::new(),
        token: None,
        defaults: Default::default(),
        update: Default::default(),
        tray: Default::default(),
    });
    let token = config::resolve_token(None, &lookup);
    match &token {
        Ok((_, source)) => println!("token:        {}", describe(source, &path)),
        Err(e) => println!("token:        missing ({e:#})"),
    }

    // Checked in a fresh instance of the app: from a terminal, this process's own grants would
    // be the terminal's.
    if cfg!(windows) {
        println!("permissions:  none needed on Windows");
    } else {
        match host::app_permissions(false) {
            Ok(p) => println!(
                "permissions:  screen recording {}, accessibility {} (RD Agent.app)",
                yes_no(p.screen),
                yes_no(p.input)
            ),
            Err(e) => println!("permissions:  unknown ({e:#})"),
        }
    }

    // The login service: a LaunchAgent on macOS (loaded into launchd), an autostart entry on
    // Windows (enabled at login).
    let (service, loaded_word) = if cfg!(windows) {
        ("autostart:   ", "enabled")
    } else {
        ("launch agent:", "loaded")
    };

    match host::service::status() {
        Ok(s) => {
            let installed = s
                .installed
                .as_ref()
                .map_or("not installed".to_owned(), |p| p.display().to_string());
            let loaded = match (s.loaded, s.state, s.pid) {
                // Windows: an agent can run without the autostart (started by hand).
                (false, Some(state), _) if cfg!(windows) => {
                    format!("not {loaded_word}; agent {state}")
                }
                (false, ..) => format!("not {loaded_word}"),
                (true, state, pid) => format!(
                    "{loaded_word}, {}{}",
                    state.as_deref().unwrap_or("state unknown"),
                    pid.map_or(String::new(), |p| format!(" (pid {p})"))
                ),
            };
            println!("{service} {installed}; {loaded}");
        }
        Err(e) => println!("{service} unknown ({e:#})"),
    }

    if let Some(mut cfg) = cfg {
        let check = match token {
            Ok((token, _)) => {
                cfg.token = Some(token);
                check_gateway(&cfg)
            }
            Err(e) => format!("not checked ({e:#})"),
        };
        println!("gateway:      {check}");
    }
    Ok(())
}

/// Where the token comes from, with what to do about it if anything.
fn describe(source: &TokenSource, config_path: &Path) -> String {
    match source {
        TokenSource::Env(var) => format!(
            "from the environment variable {var} (takes precedence over the config file and \
             the Keychain)"
        ),
        TokenSource::ConfigFile => match config::too_open(config_path) {
            None => format!("from the config file ({})", config_path.display()),
            Some(mode) => format!(
                "from the config file ({}), which others can read (mode {mode:o}): run \
                 chmod 600 on it",
                config_path.display()
            ),
        },
        TokenSource::Keychain => "from the Keychain (older install; run `rd-agent setup` again to \
             move it into the config file, which survives app rebuilds)"
            .to_owned(),
    }
}

fn check_gateway(cfg: &AgentConfig) -> String {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return format!("not checked ({e})"),
    };
    match runtime.block_on(rd_agent_core::runtime::check_gateway(cfg)) {
        Ok(()) => "reachable, token accepted".to_owned(),
        Err(e) => format!("FAILED: {e:#}"),
    }
}

fn yes_no(granted: bool) -> &'static str {
    if granted { "granted" } else { "NOT granted" }
}
