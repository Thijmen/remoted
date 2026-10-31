//! `rd-agent setup`: interactive first-time setup, run in person at the host. Asks for the setup
//! code from the portal's *Add device* (or, without one, the gateway URL, machine id and token),
//! writes config.toml with the token (readable only by the user), on macOS shows the permission
//! prompts (not with `[terminal] only = true`), and installs the login service (LaunchAgent /
//! autostart / systemd user unit).
//!
//! On macOS the token is not kept in the Keychain: the app is signed with a self-signed
//! certificate (no Team ID), so macOS ties a Keychain item to the exact build, and every
//! rebuild would make the LaunchAgent wait on an unseen "allow access" dialog.

use std::io::{BufRead as _, Write as _};

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::config::AgentConfig;
use rd_proto::setup_code::{SetupCode, valid_machine_id};

use crate::{SetupArgs, config, host};

/// The machine id offered when there is no config yet: the computer name on Windows, the host
/// name on Linux.
fn default_machine() -> String {
    #[cfg(windows)]
    if let Some(id) = std::env::var("COMPUTERNAME")
        .ok()
        .and_then(|n| machine_id(&n))
    {
        return id;
    }
    #[cfg(target_os = "linux")]
    if let Some(id) = std::fs::read_to_string("/etc/hostname")
        .ok()
        .and_then(|n| machine_id(n.split('.').next().unwrap_or_default()))
    {
        return id;
    }
    "mbp".to_owned()
}

/// `name` as a machine id: lowercase, `[a-z0-9_-]`, at most 64 characters.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn machine_id(name: &str) -> Option<String> {
    let id: String = name
        .trim()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .take(64)
        .collect();
    (!id.is_empty()).then_some(id)
}

pub fn run(args: &SetupArgs) -> anyhow::Result<()> {
    let path = config::path(args.config.config.as_deref())?;
    let existing = path
        .is_file()
        .then(|| AgentConfig::load(&path))
        .and_then(Result::ok);
    println!("rd-agent setup (config: {})", path.display());

    // A setup code answers all three questions; flags mean a scripted setup without one.
    let code = if args.gateway.is_none() && args.machine.is_none() {
        prompt_setup_code()?
    } else {
        None
    };
    let (gateway_url, machine, token) = match code {
        Some(code) => {
            println!(
                "setup code for machine {} at {}",
                code.machine(),
                code.gateway
            );
            let machine = code.machine().to_owned();
            (code.gateway, machine, code.token)
        }
        None => ask_connection(args, existing.as_ref())?,
    };

    let (defaults, update, terminal, tray) = existing
        .map(|c| (c.defaults, c.update, c.terminal, c.tray))
        .unwrap_or_default();
    let cfg = AgentConfig {
        gateway_url,
        machine,
        token: Some(token),
        defaults,
        update,
        terminal,
        tray,
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

    // Without a screen there is nothing to grant.
    if cfg!(target_os = "linux") || cfg.terminal.only {
        println!("no screen (terminal and files only): no permissions to grant");
    } else {
        permissions()?;
    }

    if args.no_launch_agent {
        println!("skipping the login service (--no-launch-agent)");
    } else {
        host::service::install()?;
    }
    Ok(())
}

/// The setup code from the portal, if the user has one (Enter skips). Asks again after a code
/// that does not parse.
fn prompt_setup_code() -> anyhow::Result<Option<SetupCode>> {
    loop {
        print!(
            "Setup code (from Add device in the portal; Enter to type the gateway, machine id \
             and token instead): "
        );
        let answer = read_line()?;
        if answer.is_empty() {
            return Ok(None);
        }
        match SetupCode::decode(&answer) {
            Ok(code) => return Ok(Some(code)),
            Err(e) => println!("{e}"),
        }
    }
}

/// Gateway URL, machine id and token, asked one by one (no setup code).
fn ask_connection(
    args: &SetupArgs,
    existing: Option<&AgentConfig>,
) -> anyhow::Result<(String, String, String)> {
    let gateway_url = match &args.gateway {
        Some(g) => g.clone(),
        None => prompt_required(
            "Gateway URL (e.g. wss://rd-agent.example.com)",
            existing.map(|c| c.gateway_url.as_str()),
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
            &existing.map_or_else(default_machine, |c| c.machine.clone()),
        )?,
    };
    ensure!(
        valid_machine_id(&machine),
        "machine ids are 1-64 characters of [A-Za-z0-9_-]"
    );

    // Enter keeps the token already in the config file (if it is for this machine).
    let current = existing
        .and_then(|c| c.token.as_deref())
        .map(str::trim)
        .filter(|t| config::token_machine(t) == Some(machine.as_str()))
        .map(str::to_owned);
    let token = prompt_token(&machine, current)?;
    Ok((gateway_url, machine, token))
}

/// Nothing to grant on Windows.
#[cfg(windows)]
fn permissions() -> anyhow::Result<()> {
    println!(
        "Windows needs no permission prompts: the agent captures the screen and injects input \
         in your session. (Windows ignores injected input while an elevated window has the \
         focus; see docs/setup-windows.md.)"
    );
    Ok(())
}

/// Screen Recording and Accessibility, asked for by a fresh instance of the app (via
/// LaunchServices), so the grants go to RD Agent.app and not to the terminal this runs in.
#[cfg(not(windows))]
fn permissions() -> anyhow::Result<()> {
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
    Ok(())
}

#[cfg(not(windows))]
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

/// Like [`prompt`], but without a default an empty answer asks again.
fn prompt_required(label: &str, default: Option<&str>) -> anyhow::Result<String> {
    if let Some(default) = default {
        return prompt(label, default);
    }
    loop {
        print!("{label}: ");
        let answer = read_line()?;
        if !answer.is_empty() {
            return Ok(answer);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_names_become_machine_ids() {
        assert_eq!(
            machine_id("DESKTOP-4K2Q9ZB").as_deref(),
            Some("desktop-4k2q9zb")
        );
        assert_eq!(machine_id(" Thijs PC ").as_deref(), Some("thijs-pc"));
        assert_eq!(machine_id(""), None);
        assert_eq!(machine_id(&"x".repeat(80)).map(|id| id.len()), Some(64));
    }
}
