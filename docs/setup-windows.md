# Windows agent setup

How to put `rd-agent` on a Windows PC and keep it running, so you can reach the PC from a Mac
(or from any browser). Unlike macOS, Windows asks for no permissions: the agent captures the
screen and injects input for whoever is logged in.

## Requirements

- Windows 11, or Windows 10 version 1703 or later. (Tested on Windows 11 25H2.)
- A GPU with a hardware H.264 encoder is strongly recommended: NVIDIA (NVENC), Intel (Quick
  Sync) or AMD (AMF), through their Media Foundation encoders. Without one the agent uses
  Microsoft's software encoder, which is fine up to about 1080p on a fast CPU.
- A graphics driver for the display is recommended. With one the agent uses Desktop
  Duplication (the fastest capture path). Without one — the *Microsoft Basic Display Adapter*
  after a fresh install, a virtual machine's display adapter, a USB/dock display — the agent
  falls back to GDI capture and software encoding: it works, but at most 30 frames per second
  and with more CPU load. Update the display adapter's driver in Device Manager for the fast
  path.
- Media Foundation. It is part of Windows, except on the **N editions** (install the *Media
  Feature Pack* under Settings → Apps → Optional features) and on **Windows Server** (install
  the *Server Media Foundation* feature). Without it `rd-agent.exe` does not start
  (`MFPlat.DLL was not found`).
- The agent runs in your logged-in session, not as a service: someone has to be logged in.

## 1. Install

Download `rd-agent-<version>-windows-x64-setup.exe` from the agent releases (`agent-v*`; up
to 1.4.2 the agent came with the rd Desktop releases) on
[GitHub Releases](https://github.com/Thijmen/remoted/releases) and run it. It installs for your
account only (no administrator rights) into `%LOCALAPPDATA%\Programs\rd-agent`, adds Start menu
shortcuts (*rd agent*, which starts the agent; *rd agent setup*, *rd agent status*, *rd agent
logs*) and an entry in Settings → Apps, and offers the setup (step 3) at the end, so add the PC
in the portal (step 2) first. The installer is not signed, so SmartScreen asks once:
*More info* → *Run anyway*.

From 1.5.0 on, an agent installed this way updates itself: the portal lists the PC under
**Needs update**, and **Update now** in its details installs the new version: the agent installs the new version and puts the previous one back
if the new one does not reconnect within two minutes ([updates.md](updates.md)). Agents up to
1.4.2 have no updater: run the 1.5.0 installer once.

Running a newer installer by hand also updates the agent in place: it stops the running agent,
replaces the programs and starts it again with the existing configuration. Silent installs work
too: `setup.exe /S`, then `rd-agent.exe setup` in a terminal.

Without the installer: every release also has `rd-agent-<version>-windows-x64.zip` with the same
programs (the zip from 1.1.0 on, the installer from 1.2.0 on). An agent unpacked from the zip
does not update itself; the portal then says to update it by hand. Unblock the zip before
extracting it (right-click → Properties → Unblock, or `Unblock-File`), so SmartScreen does not
stop the unsigned programs. Or build them, on any Windows machine with the Rust toolchain from
`rust-toolchain.toml`:

```powershell
cargo build --release -p rd-agent
```

which puts them in `target\release`. Either way, keep the two programs together in one folder,
for example `%LOCALAPPDATA%\Programs\rd-agent\`:

- `rd-agent.exe`: the agent and its commands (`setup`, `status`, …);
- `rd-agentw.exe`: the background launcher the autostart runs (see below).

## 2. Add the PC in the portal

Open your gateway's portal (for example `https://rd.example.com`), click **Add device** and give
the PC a name. The portal shows a **setup code** (`rds1_…`) once: copy it for the next step.
It holds the gateway URL and the PC's token, so treat it like a password. Every device gets its
own machine id and token; [devices.md](devices.md) explains how they work, how to rotate or
remove them, and how to provision a token without the portal.

## 3. Setup (in person)

The installer's finish page and the *rd agent setup* shortcut run it; from a terminal:

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" setup
```

Setup asks for the setup code from step 2: paste it (right-click in the console window, or
Ctrl+V) and press Enter. Without a code, press Enter and it asks for the gateway URL (the agent
host of your gateway, e.g. `wss://rd-agent.example.com`), the machine id (it suggests the
computer name) and the token one by one. Then it:

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
- Updating by hand (the zip, or a build of your own): `uninstall`, replace both programs,
  `install`. The installer does the same.

## Notification area icon

While it runs, the agent shows the rd icon in the notification area (next to the clock; Windows
may put it under the **^** overflow first: drag it out to keep it visible). Its menu shows:
- the version and whether the PC is online (or why not);
- who is connected: someone viewing the screen or transferring files;
- **Pause remote access**: ends open sessions and disconnects from the gateway, so the portal
  shows the PC offline and nobody can connect, until you click **Resume remote access**. The
  pause lasts until the agent restarts (at the next login at the latest);
- **Check for updates**, or **Install update X.Y.Z** once one is found (as the portal's Update
  button);
- **Start at login**: adds or removes the autostart's Run value; the running agent keeps
  running;
- **Quit RD Agent**: stops the agent (the launcher does not restart it) until the next login,
  or until you start **rd agent** from the Start menu.

The icon is dimmed while the PC is not reachable (paused, offline, connecting) and gets a red
dot while someone is connected. `[tray]` `enabled = false` in `config.toml` (or `rd-agent run
--no-tray`) turns it off.

## Updates

An agent installed with the installer (in a folder with its `uninstall.exe`, started by the
autostart) updates itself from the portal: the PC's details show the new version with **What's new** and
**Update now**. The agent downloads the new installer, checks its signature, ends
an open session (the viewer reconnects by itself) and hands over to an update helper, which runs
the installer silently and waits up to 2 minutes for the new version to connect to the gateway.
If it does not, the helper puts the previous `rd-agent.exe` and `rd-agentw.exe` back and starts
them. The portal then shows what happened. Details: [updates.md](updates.md).

At the PC, from a terminal:

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" update --check   # is there a newer version? also shows the last update's outcome
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" update           # install it now
```

`update` returns at once (the installer has to replace `rd-agent.exe`); `update --check` shows
the outcome a minute later. The helper logs to `%LOCALAPPDATA%\rd-agent\logs\update-helper.log`.
The agent checks for new versions 1 minute after it starts and then every 6 hours;
`[update]` `check = false` in `config.toml` turns that off (the portal then offers nothing, but
`rd-agent update` still works).

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

## Lock screen and UAC (secure desktop)

Windows shows the lock screen and UAC prompts on a *secure desktop*
that a program in your session cannot capture or control. `rd-agent` can install a small
LocalSystem service that runs a helper in your session while it is locked, so the viewer shows
the real desktop and your input reaches it.

The installer offers it on its **Lock screen and UAC** page (checked by default): it asks for
administrator rights once and leaves the rest of the install per-user. To install it later, or
on a silent install:

```powershell
# In an elevated terminal (administrator):
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" secure install
# or, silently: rd-agent-<version>-windows-x64-setup.exe /S /SECURE
```

The service starts at boot; a running agent finds it within a few seconds. From then on, when
the PC is locked or a UAC prompt is up, the
viewer shows the actual screen: type the Windows password in the picture, click UAC buttons,
and use **Send Ctrl+Alt+Del** from the banner. Nothing is stored; the password only travels
through the session to the helper. The portal shows the secure state (`starting` while the
helper comes up, `ready` when it is in the picture, `failed` if it could not start).

- `rd-agent secure status` shows whether the service is installed and running;
  `rd-agent secure uninstall` stops and removes it. Both need an administrator. The service
  and helper log to `%ProgramData%\rd\agent\logs` (`secure-service.log`,
  `secure-helper.log`) for diagnosing a start failure.
- The install copies `rd-agent-svc.exe` and `rd-agent-sec.exe` to `%ProgramFiles%\rd-agent`.
  The per-user agent updates itself silently, and a silent update cannot ask for
  administrator rights, so it does not update those copies: run the installer interactively
  (or `secure install`) after an update to refresh them.
- `rd-agent secure status` exits 0 only when the service is running.
- Ctrl+Alt+Del needs
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`
  to allow services (`1` or `3`); the viewer reports an error when the policy blocks it.
- It works in the console session and in a locked Remote Desktop session (the agent's own
  session either way). A *disconnected* Remote Desktop session has no screen at all, so there
  is no lock screen to show; the viewer says so and does not start the helper. On a VM you
  reach through Remote Desktop (or Guacamole), either leave that session connected, or sign in
  on the VM's own console and run the agent there: a console session is always drawn.
- **Trust boundary:** the service accepts requests from a session's own signed-in user (it
  checks the caller's session and user SID) and only shows that session's secure desktop. Any
  code running as that user can therefore be shown it; use this on a machine you control. It does not cover BitLocker or the
  pre-boot screen, and v1 only serves a locked session with the agent running (not the
  signed-out screen after a reboot). The lock screen is captured from the primary display;
  the viewer's display picker does not move it on a multi-monitor PC.

## Limits

- **Lock screen, UAC prompts, Ctrl+Alt+Del** stay on Windows' secure desktop unless the
  secure-desktop service is installed (see above). Without it the viewer reports the PC as
  locked, keeps the last picture and drops input; capture resumes when you are back on the
  desktop, and you unlock at the PC. BitLocker and the pre-boot screen are never covered; a
  KVM/vPro or the recovery key is needed there (the Mac's path is Screen Sharing; see
  [recovery.md](recovery.md)).
- **Elevated windows** (run as administrator) ignore input from a program that is not
  elevated. The autostart runs the agent unelevated; run `rd-agent run` from an elevated
  terminal if you need to control such windows (it still cannot reach the secure desktop).
- **Rotated (portrait) displays** are captured in their landscape scan-out orientation.
- **HDR desktops** are converted to SDR by Windows.
- **Without a graphics driver** (see [Requirements](#requirements)) the agent captures with
  GDI and encodes in software: at most 30 frames per second, and more CPU load. Updating the
  display adapter's driver brings Desktop Duplication back.
- During a session the agent keeps the display and the PC awake, and wakes a dark display when
  a viewer connects: Desktop Duplication only delivers frames from a lit display.
- **File transfer** ([files.md](files.md)) runs as your user, unelevated: it reaches what you
  can reach in Explorer without "Continue" (administrator) prompts. The file browser offers
  your Desktop, Documents and Downloads where Windows keeps them (also when OneDrive moved
  them) and every drive. Deleted files go to the Recycle Bin; on drives without one (network
  shares) Windows deletes them for good.

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

- `cargo run -p rd-platform-windows --example probe` lists monitors, GPUs, their outputs,
  whether a D3D11 device can be created on each (Desktop Duplication's fast path) and the
  H.264 encoders Media Foundation offers for it.
- `cargo run --release -p rd-platform-windows --example animate` animates a window and the
  pointer on the primary display for a few seconds; run it next to `rd-agent selftest` for
  capture measurements on an otherwise static desktop.
- `cargo test -p rd-platform-windows -- --ignored` also runs the tests that touch the real
  desktop: a screen capture of the primary display and pointer moves (the pointer is put back).
- `RD_MF_ENCODER=hardware|software` forces the encoder choice for `run`.
- `RD_CAPTURE=gdi` skips Desktop Duplication (the fallback the agent takes by itself on a PC
  without a graphics driver), to try the GDI capture path on a PC that has one.

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
