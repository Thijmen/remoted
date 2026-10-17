//! `rd-agent setup`: interactive first-time setup, run in person at the Mac. Writes config.toml
//! with the token (readable only by the user), shows the permission prompts and installs the
//! LaunchAgent.
//!
//! The token is not kept in the Keychain: the app is signed with a self-signed certificate (no
//! Team ID), so macOS ties a Keychain item to the exact build, and every rebuild would make
//! the LaunchAgent wait on an unseen "allow access" dialog.

use std::io::{BufRead as _, Write as _};

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::config::AgentConfig;

use crate::{SetupArgs, config, host};

const DEFAULT_GATEWAY: &str = "wss://rd-agent.example.com";
const DEFAULT_MACHINE: &str = "mbp";

pub fn run(args: &SetupArgs) -> anyhow::Result<()> {
    let path = config::path(args.config.config.as_deref())?;
    let existing = path
        .is_file()
        .then(|| AgentConfig::load(&path))
        .and_then(Result::ok);
    println!("rd-agent setup (config: {})", path.display());

    let gateway_url = match &args.gateway {
        Some(g) => g.clone(),
        None => prompt(
            "Gateway URL",
            existing
                .as_ref()
                .map_or(DEFAULT_GATEWAY, |c| &c.gateway_url),
        )?,
    };
    let url = url::Url::parse(&gateway_url).context("invalid gateway URL")?;
    ensure!(
        matches!(url.scheme(), "ws" | "wss"),
        "the gateway URL must start with ws:// or wss://"
    );
    let machine = match &args.machine {
        Some(m) => m.clone(),
        None => prompt(
            "Machine id",
            existing.as_ref().map_or(DEFAULT_MACHINE, |c| &c.machine),
        )?,
    };
    ensure!(
        !machine.is_empty()
            && machine.len() <= 64
            && machine
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "machine ids are 1-64 characters of [A-Za-z0-9_-]"
    );

    // Enter keeps the token already in the config file (if it is for this machine).
    let current = existing
        .as_ref()
        .and_then(|c| c.token.as_deref())
        .map(str::trim)
        .filter(|t| config::token_machine(t) == Some(machine.as_str()))
        .map(str::to_owned);
    let token = prompt_token(&machine, current)?;

    let cfg = AgentConfig {
        gateway_url,
        machine,
        token: Some(token),
        defaults: existing.map(|c| c.defaults).unwrap_or_default(),
    };
    cfg.save(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    if args.config.config.is_none()
        && let Some(dir) = path.parent()
    {
        config::restrict_app_dir(dir)?;
    }
    println!(
        "wrote {} (with the token; readable only by you)",
        path.display()
    );

    // Asked for by a fresh instance of the app (via LaunchServices), so the grants go to
    // RD Agent.app and not to the terminal this runs in.
    println!(
        "asking macOS for Screen Recording and Accessibility for RD Agent; approve them in the \
         dialogs / System Settings > Privacy & Security"
    );
    let mut perms = host::app_permissions(true)?;
    while !(perms.screen && perms.input) {
        println!(
            "screen recording: {}, accessibility: {}",
            granted(perms.screen),
            granted(perms.input)
        );
        print!(
            "enable \"RD Agent\" under System Settings > Privacy & Security > Screen & System \
             Audio Recording and > Accessibility, then press Enter to re-check (or type skip): "
        );
        if read_line()?.eq_ignore_ascii_case("skip") {
            break;
        }
        perms = host::app_permissions(false)?;
    }
    println!(
        "screen recording: {}, accessibility: {}",
        granted(perms.screen),
        granted(perms.input)
    );

    if args.no_launch_agent {
        println!("skipping the LaunchAgent (--no-launch-agent)");
    } else {
        host::service::install()?;
    }
    Ok(())
}

fn granted(yes: bool) -> &'static str {
    if yes { "granted" } else { "NOT granted" }
}

fn read_line() -> anyhow::Result<String> {
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("stdin closed");
    }
    Ok(line.trim().to_owned())
}

fn prompt(label: &str, default: &str) -> anyhow::Result<String> {
    print!("{label} [{default}]: ");
    let answer = read_line()?;
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer
    })
}

/// Asks for the token; with `current`, an empty answer keeps it. Never prints a token.
fn prompt_token(machine: &str, current: Option<String>) -> anyhow::Result<String> {
    match &current {
        Some(_) => print!("Agent token (rd1.{machine}.<secret>) [Enter keeps the current one]: "),
        None => print!("Agent token (rd1.{machine}.<secret>): "),
    }
    let token = read_line()?;
    if token.is_empty()
        && let Some(current) = current
    {
        return Ok(current);
    }
    match config::token_machine(&token) {
        Some(m) if m == machine => Ok(token),
        Some(m) => bail!("this token is for machine {m:?}, not {machine:?}"),
        None => bail!("expected a token of the form rd1.<machine>.<secret>"),
    }
}
