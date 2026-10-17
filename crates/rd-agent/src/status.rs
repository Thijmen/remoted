//! `rd-agent status`: what is configured and whether it works. Read-only: never prompts, never
//! registers with the gateway.

use std::path::Path;

use rd_agent_core::config::AgentConfig;

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

    let env_token = std::env::var(config::TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty());
    let keychain = match host::load_token() {
        Ok(Some(_)) => "present".to_owned(),
        Ok(None) => "missing".to_owned(),
        Err(e) => format!("unreadable ({e:#})"),
    };
    println!(
        "token:        Keychain {keychain}{}",
        if env_token {
            format!(", {} set (takes precedence)", config::TOKEN_ENV)
        } else {
            String::new()
        }
    );

    match host::permissions() {
        Ok(p) => println!(
            "permissions:  screen recording {}, accessibility {} (for this executable)",
            yes_no(p.screen),
            yes_no(p.input)
        ),
        Err(e) => println!("permissions:  unknown ({e:#})"),
    }

    match host::service::status() {
        Ok(s) => {
            let installed = s
                .installed
                .as_ref()
                .map_or("not installed".to_owned(), |p| p.display().to_string());
            let loaded = match (s.loaded, s.state, s.pid) {
                (false, ..) => "not loaded".to_owned(),
                (true, state, pid) => format!(
                    "loaded, {}{}",
                    state.as_deref().unwrap_or("state unknown"),
                    pid.map_or(String::new(), |p| format!(" (pid {p})"))
                ),
            };
            println!("launch agent: {installed}; {loaded}");
        }
        Err(e) => println!("launch agent: unknown ({e:#})"),
    }

    if let Some(mut cfg) = cfg {
        let check = match config::resolve_token(None, &cfg) {
            Ok(token) => {
                cfg.token = Some(token);
                check_gateway(&cfg)
            }
            Err(e) => format!("not checked ({e:#})"),
        };
        println!("gateway:      {check}");
    }
    Ok(())
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
