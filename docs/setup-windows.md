# Windows agent setup

How to put `rd-agent` on a Windows PC and keep it running, so you can reach the PC from a Mac
(or from any browser). Unlike macOS, Windows asks for no permissions: the agent captures the
screen and injects input for whoever is logged in.

## Requirements

- Windows 11, or Windows 10 version 1703 or later. (Tested on Windows 11 25H2.)
- A GPU with a hardware H.264 encoder is strongly recommended: NVIDIA (NVENC), Intel (Quick
  Sync) or AMD (AMF), through their Media Foundation encoders. Without one the agent uses
  Microsoft's software encoder, which is fine up to about 1080p on a fast CPU.
- Media Foundation. It is part of Windows, except on the **N editions** (install the *Media
  Feature Pack* under Settings → Apps → Optional features) and on **Windows Server** (install
  the *Server Media Foundation* feature). Without it `rd-agent.exe` does not start
  (`MFPlat.DLL was not found`).
- The agent runs in your logged-in session, not as a service: someone has to be logged in.

## 1. Install

Download `rd-agent-<version>-windows-x64-setup.exe` from the
[GitHub Releases](https://github.com/Thijmen/remoted/releases) and run it. It installs for your
account only (no administrator rights) into `%LOCALAPPDATA%\Programs\rd-agent`, adds Start menu
shortcuts (*rd agent setup*, *rd agent status*, *rd agent logs*) and an entry in Settings → Apps,
and offers the setup (step 3) at the end. The installer is not signed, so SmartScreen asks once:
*More info* → *Run anyway*.

Running a newer installer updates the agent in place: it stops the running agent, replaces the
programs and starts it again with the existing configuration. Silent installs work too:
`setup.exe /S`, then `rd-agent.exe setup` in a terminal.

Without the installer: every release also has `rd-agent-<version>-windows-x64.zip` with the same
programs (the zip from 1.1.0 on, the installer from 1.2.0 on). Unblock the zip before extracting
it (right-click → Properties → Unblock, or `Unblock-File`), so SmartScreen does not stop the
unsigned programs. Or build them, on any Windows machine with the Rust toolchain from
`rust-toolchain.toml`:

```powershell
cargo build --release -p rd-agent
```

which puts them in `target\release`. Either way, keep the two programs together in one folder,
for example `%LOCALAPPDATA%\Programs\rd-agent\`:

- `rd-agent.exe`: the agent and its commands (`setup`, `status`, …);
- `rd-agentw.exe`: the background launcher the autostart runs (see below).

## 2. Token

On any machine with bash (Git Bash works):

```sh
tools/new-agent-token.sh pc
```

Store the **sha256** where the gateway reads agent hashes (see [gateway.md](gateway.md)); only
the agent gets the **token** (next step). Every device needs its own machine id and token;
[devices.md](devices.md) explains how they work, and how to rotate or remove them.

## 3. Setup (in person)

The installer's finish page and the *rd agent setup* shortcut run it; from a terminal:

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" setup
```

Setup asks for the gateway URL (the agent host of your gateway, e.g.
`wss://rd-agent.example.com`), the machine id (it suggests the computer name) and the token,
then:

- writes the config file with the token, `%LOCALAPPDATA%\rd-agent\config.toml`. It lives in
  your local (not roaming) profile, which only you and administrators can read. Setup never
  prints the token;
- installs the autostart and starts the agent in the background.

## 4. Check

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" status
```

`status` shows the config, where the token comes from, the autostart and whether the agent
runs, and whether the gateway accepts the token. Logs: `%LOCALAPPDATA%\rd-agent\logs\`
(daily `rd-agent.*.log` files, plus `agent-stderr.log` from the launcher).

`rd-agent selftest` captures and encodes the screen for ten seconds and reports frame rate,
encode latency and the stream's parameters; `--synthetic` encodes a test pattern instead,
`--encoder hardware|software` picks the encoder.

## Autostart

`rd-agent install` (which `setup` runs) adds `rd-agent` to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing at `rd-agentw.exe`. At login
the launcher starts `rd-agent run` without a console window and restarts it 10 s after it fails,
like launchd's KeepAlive on macOS. There is one agent per session: a second `rd-agent run`
refuses to start.

- `rd-agent install` again restarts the agent (it stops the running one first).
- `rd-agent uninstall` removes the autostart and stops the agent.
- Updating: `uninstall`, replace both programs, `install`.

## Connecting from a Mac

Open the gateway in a browser, or use [rd Desktop](../clients/desktop/README.md) for macOS,
which also captures the shortcuts a browser never sees.

- **Modifier keys**: the viewer's default, *Automatic*, uses *Mac keyboard → Windows* when a Mac
  drives a Windows PC: Cmd acts as Ctrl (so Cmd+C, Cmd+V, Cmd+Z, … do what you expect), Ctrl
  acts as the Windows key, Option is Alt. With rd Desktop, Cmd+Tab switches apps on the PC
  (it is sent as Alt+Tab). Pick *Pass through* in the settings to send Cmd as the Windows key.
- **Keys** arrive as scan codes, so the PC's own keyboard layout applies, as with a keyboard
  plugged into it.
- **Trackpad** scrolling arrives as smooth, high-resolution wheel input.
- **Displays**: every monitor can be picked; the primary one is the default. Coordinates are
  physical pixels, so scaling (125 %, 150 %, …) does not shift clicks.

## Limits

- **Lock screen, UAC prompts, Ctrl+Alt+Del** show on Windows' secure desktop, which no program
  in your session can capture or control. While it is up the viewer reports the PC as locked,
  keeps the last picture and drops input; capture resumes when you are back on the desktop.
  Unlock at the PC. Ctrl+Alt+Del cannot be sent.
  There is no remote recovery path yet (the Mac's is Screen Sharing; see
  [recovery.md](recovery.md)).
- **Elevated windows** (run as administrator) ignore input from a program that is not
  elevated. The autostart runs the agent unelevated; run `rd-agent run` from an elevated
  terminal if you need to control such windows (it still cannot reach the secure desktop).
- **Rotated (portrait) displays** are captured in their landscape scan-out orientation.
- **HDR desktops** are converted to SDR by Windows.
- During a session the agent keeps the display and the PC awake, and wakes a dark display when
  a viewer connects: Desktop Duplication only delivers frames from a lit display.

## Local development (no cluster)

```powershell
cd viewer; bun install; bun run build; cd ..
cargo run --release -p rd-gateway -- --dev --assets-dir viewer/dist
$env:RD_AGENT_TOKEN = "rd1.pc.dev"
cargo run --release -p rd-agent -- run --gateway ws://127.0.0.1:8081 --machine pc
```

Then open <http://127.0.0.1:8080/>. Add `--source synthetic` to stream a test pattern instead
of the screen. Viewing the PC from a browser on the same PC works, but do not click into the
picture: the input loops back into the desktop you are watching.

Useful while working on `crates/rd-platform-windows`:

- `cargo run -p rd-platform-windows --example probe` lists monitors, GPUs, their outputs and the
  H.264 encoders Media Foundation offers for each.
- `cargo run --release -p rd-platform-windows --example animate` animates a window and the
  pointer on the primary display for a few seconds; run it next to `rd-agent selftest` for
  capture measurements on an otherwise static desktop.
- `cargo test -p rd-platform-windows -- --ignored` also runs the tests that touch the real
  desktop: a screen capture of the primary display and pointer moves (the pointer is put back).
- `RD_MF_ENCODER=hardware|software` forces the encoder choice for `run`.

## Uninstall

Installed with the installer: remove *rd agent* in Settings → Apps. That stops the agent,
removes the autostart, the programs and the shortcuts, and asks whether to delete the
configuration (with the token) and the logs as well.

Unpacked from the zip:

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" uninstall
```

Then delete the programs' folder, and `%LOCALAPPDATA%\rd-agent` for the config file (with the
token) and the logs.
