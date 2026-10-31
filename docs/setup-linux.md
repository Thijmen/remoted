# Linux agent setup (terminal and files)

On Linux, `rd-agent` has no screen. It serves the [terminal](terminal.md) and the
[file browser](files.md): a shell on the server in your browser tab, over the same port 443 as
everything else in rd. The server needs no SSH and no open ports; like the Mac and Windows
agents, it dials *out* to the gateway.

The portal shows the server with **Terminal** and **Files**, but no **Connect**. Shells run as
the user the agent runs as.

## Requirements

- Any Linux on x86-64 or ARM64 (the release builds are static: no particular distribution or
  glibc). Agent releases from 1.11.0 on include them.
- `curl`, and systemd for the user unit (without systemd, run `rd-agent run` from your own
  service manager).

## 1. Add the server in the portal

Click **Add device** in the portal ([devices.md](devices.md)). Use a machine id such as `box`
or the host name. The dialog shows the setup code, and under **Linux server** the whole install
command with the code filled in.

## 2. Install

On the server, as the user whose shell you want in the browser (not root):

```sh
curl -fsSL https://raw.githubusercontent.com/Thijmen/remoted/main/packaging/linux/install.sh | sh -s -- <setup code>
```

[`install.sh`](../packaging/linux/install.sh) downloads the newest agent release for your
architecture from GitHub, checks it against the release's `SHA256SUMS.txt`, installs it as
`~/.local/bin/rd-agent` and runs `rd-agent setup --code <setup code>`. Setup writes
`~/.config/rd-agent/config.toml` (mode 0600, with the token), and installs, enables and starts a
**systemd user unit**, `~/.config/systemd/user/rd-agent.service`. There are no permissions to
grant.

Without a setup code the script asks setup's questions instead. `RD_AGENT_VERSION=1.11.0`
installs a given version; `RD_INSTALL_DIR` puts the binary elsewhere.

**Updating:** the Linux agent does not update itself yet. Run the same command without the
setup code; it replaces the binary and restarts the agent:

```sh
curl -fsSL https://raw.githubusercontent.com/Thijmen/remoted/main/packaging/linux/install.sh | sh
```

**Building it yourself** instead: Rust (the version in `rust-toolchain.toml`), a C compiler and
CMake (for Opus), then `cargo build --release -p rd-agent`, copy `target/release/rd-agent` to
`~/.local/bin` and run `rd-agent setup`.

User services stop when you log out. To keep the agent running without a login, and start it at
boot:

```sh
sudo loginctl enable-linger "$USER"
```

## 3. Check

```sh
rd-agent status
journalctl --user -u rd-agent -f
```

Logs also go to `~/.local/state/rd-agent/logs/` (one file a day, 7 kept).

## The terminal on Linux

The terminal works as on the other agents ([terminal.md](terminal.md)): shells survive a dropped
connection and the `[terminal]` settings apply. The default shell is your login shell (`$SHELL`,
else the user database), started as a login shell with `TERM=xterm-256color`; set another
with `shell = "/usr/bin/fish"` under `[terminal]` in the config file.

## Remove

```sh
rd-agent uninstall          # stops and removes the systemd user unit
rm -r ~/.config/rd-agent ~/.local/state/rd-agent ~/.local/bin/rd-agent
```

Then remove the device in the portal.

## Local development

```sh
rd-gateway --dev                                           # http://127.0.0.1:8080, agents on :8081
rd-agent run --gateway ws://127.0.0.1:8081 --machine box   # with RD_AGENT_TOKEN=rd1.box.dev
```

`rd-agent run --terminal-only` on macOS or Windows runs an agent the same way: no screen,
only the terminal and files.
