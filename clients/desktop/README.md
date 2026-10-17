# rd Desktop

The normal rd viewer (`https://rd.example.com`) in its own window, for Windows. While a
session has the keyboard it also gets the shortcuts a browser tab never sees: Alt+Tab, the
Windows key, Alt+Esc, Ctrl+Esc, Alt+F4, Caps Lock, … all go to the remote machine.

Everything else (Authentik login, portal, video, settings) is the web viewer, loaded from the
gateway, so the app needs no update when the viewer changes. Design and the page ⇄ app bridge:
[docs/desktop-client.md](../../docs/desktop-client.md).

## Install (Windows 10/11)

Download from the [GitHub Releases](https://github.com/Thijmen/remoted/releases) (tags
`desktop-v*`), or from the artifacts of a run of the **Desktop** workflow:

| File | |
|---|---|
| `rd-desktop-<version>-x64-setup.exe` | per-user installer: no admin rights, Start menu entry, uninstall via Settings → Apps |
| `rd-desktop-<version>-x64-portable.exe` | the same app as a single exe, nothing to install |

The files are not code-signed, so SmartScreen warns the first time ("Windows protected your
PC"): click **More info → Run anyway**. (On a managed laptop a policy may block unsigned
programs outright; the app cannot work around that.)

The app uses the WebView2 runtime that ships with Windows 10/11 (the installer fetches it if it
is missing).

## Use

Start **rd Desktop**, sign in with Authentik in the window, pick a machine and connect. When you
click into the remote screen, the keyboard belongs to the remote machine, shortcuts included.
To get out, click outside the window (or a viewer control such as the settings menu): the grab
ends at once, and keys work locally again.

- Ctrl+Alt+Del and Win+L always stay with Windows (it reserves them).
- Media and volume keys also go to the remote while the grab is on.
- Links to other sites open in your normal browser. Only the gateway and
  `*.example.com` (Authentik) load inside the window.

Data lives in `%LOCALAPPDATA%\dev.thijmen.rd.desktop` (the WebView2 profile with the login
cookies, and `logs\rd-desktop.log`). Delete that folder to sign out completely. Keys are never
logged.

## Options

| | |
|---|---|
| `--url <URL>` / `RD_URL` | open another gateway, e.g. `http://127.0.0.1:18580` (the flag wins over the variable). That origin gets the `rd_set_grab` permission instead of only the production one. |
| `RD_LOG` | log level: `error`, `warn`, `info` (default), `debug`, `trace` |

## Local testing

Against a dev gateway and the synthetic agent (works on macOS too, as a plain wrapper):

```sh
(cd viewer && bun run build) && cargo build --release -p rd-gateway -p rd-agent
target/release/rd-gateway --dev --viewer-addr 127.0.0.1:18580 --agent-addr 127.0.0.1:18581 \
  --metrics-addr 127.0.0.1:18590 --assets-dir viewer/dist &
RD_AGENT_TOKEN=rd1.mbp.dev target/release/rd-agent run --gateway ws://127.0.0.1:18581 \
  --machine mbp --source synthetic &

cd clients/desktop/src-tauri
cargo run -- --url http://127.0.0.1:18580/m/mbp
```

Debug builds log every grab change at `info`. `RD_DESKTOP_SELFTEST=1 cargo run -- --url …`
also makes the loaded page call `rd_set_grab(true)`/`(false)` itself and logs the outcome
(and whether `window.rdNative` exists and which IPC transport Tauri used).

## Build

The Tauri CLI version is pinned in `package.json` (`bun.lock`); the Rust crate has its own
`Cargo.lock` and is not part of the root workspace, so `cargo test --workspace` and the Linux
CI need no WebView dependencies.

```sh
cd clients/desktop
bun install --frozen-lockfile
bun run tauri build --bundles nsis   # Windows: src-tauri/target/release/rd-desktop.exe + bundle/nsis/*-setup.exe
bun run tauri build --bundles app    # macOS: src-tauri/target/release/bundle/macos/rd Desktop.app
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/desktop.yml`) builds on `windows-latest` for pushes to `main` that touch
`clients/desktop/**` or `crates/rd-proto/**`, and on demand. To release: bump `version` in
`src-tauri/Cargo.toml`, commit, then

```sh
git tag desktop-v0.1.0 && git push origin desktop-v0.1.0
```

The workflow checks that the tag matches the version and publishes both exes (plus
`SHA256SUMS.txt`) as a GitHub Release.

Icons: `icon.svg` is the source; `bun run tauri icon icon.svg -o src-tauri/icons`, then delete
the generated `android/`, `ios/`, `Square*`, `StoreLogo.png` and `64x64.png`.

## How it works

- `src/main.rs`: the window (title "rd", 1280×800, remembers nothing), navigation policy, the
  `rd_set_grab` command. The command is an app command with an ACL permission
  (`build.rs`: `AppManifest`), granted only by `capabilities/gateway.json` (remote origin
  `https://rd.example.com`, window `main`) or, with `--url`, a capability added at
  startup for that origin. No other command or plugin permission is exposed to the page.
- `src/grab_windows.rs`: a thread with a message loop owning a `WH_KEYBOARD_LL` hook. It is
  installed when the page asks (`rd_set_grab({active: true})`) while the window is the
  foreground window, and the call resolves only after that. It comes off when the page says so,
  or on its own when the window loses the foreground (`EVENT_SYSTEM_FOREGROUND`), the input
  desktop switches (lock screen, UAC, Ctrl+Alt+Del), a new page loads, or the window closes;
  then the page gets `rdNative.release()`. The app never re-installs it unasked.
- In the hook: injected input passes through; every other key is swallowed and handed to a
  forwarder thread over a channel (nothing else happens in the hook, so Windows never times it
  out), which calls `window.rdNative && window.rdNative.key(code, down, repeat)` on the
  webview. Keys already held when the hook went on pass through until released, so Windows
  never keeps them stuck.
- `src/keys.rs`: scan code (+`0xE000` for `LLKHF_EXTENDED`) → DOM code via
  `rd_proto::keymap::by_win_scancode`, with the quirks: Right Shift is reported extended,
  NumLock (`E0 45`) vs Pause (`45`), Break (`E0 46`) and SysRq (`54`) map to Pause/PrintScreen,
  fake shifts (`E0 2A`/`E0 36`) and AltGr's fake LCtrl (scan code `0x21D`) are dropped. Unit
  tested on every platform.

## Limits

- Windows only: on macOS (and Linux) the app is a plain wrapper and refuses `rd_set_grab`
  (`keyboard grab not supported on this platform`), so the page keeps using DOM keys.
- On macOS, WKWebView also asks the navigation policy about iframes: an iframe from another
  site would be opened in the browser instead. The viewer has none.
- Fullscreen: when the page enters fullscreen (e.g. from a click), Tauri makes the window
  fullscreen. The Ctrl+Alt+Shift+F chord during a grab arrives as forwarded keys without a
  user gesture, so WebView2 may refuse `requestFullscreen` (untested on Windows). Maximize
  the window, or use the viewer's controls, instead.
- Windows silently drops a low-level hook that is too slow (`LowLevelHooksTimeout`); the hook
  only does a channel send, but if keys ever start going to Windows again mid-session, click
  outside the window and back in.
- With Num Lock on and **Right** Shift held, the fake Right Shift around the navigation keys may
  not be told apart from the real one.
- Unsigned (see SmartScreen above).
