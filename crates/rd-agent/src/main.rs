//! rd-agent: host agent binary. TO BE IMPLEMENTED (subcommands below are the contract).
//!
//! rd-agent run        [--config <path>] [--gateway <url>] [--machine <id>] [--token-env]
//! rd-agent setup      (interactive, in person): read token from stdin, store in Keychain,
//!                     write config.toml, request permissions, install LaunchAgent
//! rd-agent install | uninstall   LaunchAgent management
//! rd-agent status     config, token present?, permissions, LaunchAgent state, gateway reachability
//! rd-agent selftest   --size 1728x1118 --fps 60 --seconds 10 [--bitrate-kbps 12000] [--record out.rdv]

fn main() {
    eprintln!("rd-agent: not implemented yet");
    std::process::exit(2);
}
