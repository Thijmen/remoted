# rd Desktop

The normal rd viewer (`https://rd.example.com`) in its own window, for Windows and
macOS. While a session has the keyboard it also gets the shortcuts a browser tab never sees:

- Windows: Alt+Tab, the Windows key, Alt+Esc, Ctrl+Esc, Alt+F4, Caps Lock, …
- macOS: Cmd+Tab, Cmd+Space, Cmd+Q, Cmd+H, Ctrl+arrows (Spaces), Mission Control, …

all go to the remote machine.

Everything else (Authentik login, portal, video, settings) is the web viewer, loaded from the
gateway, so the app needs no update when the viewer changes. Design and the page ⇄ app bridge:
[docs/desktop-client.md](../../docs/desktop-client.md).

## Install

Download from the [GitHub Releases](https://github.com/Thijmen/remoted/releases) (tags
`desktop-v*`; the repository is private, so sign in to GitHub first), or from the artifacts of
a run of the **Desktop** workflow.

### Windows 10/11

| File | |
|---|---|
| `rd-desktop-<version>-x64-setup.exe` | per-user installer: no admin rights, Start menu entry, uninstall via Settings → Apps |
| `rd-desktop-<version>-x64-portable.exe` | the same app as a single exe, nothing to install |

The files are not code-signed, so SmartScreen warns the first time ("Windows protected your
PC"): click **More info → Run anyway**. (On a managed laptop a policy may block unsigned
programs outright; the app cannot work around that.)

The app uses the WebView2 runtime that ships with Windows 10/11 (the installer fetches it if it
is missing).

### macOS 12+ (Apple silicon and Intel)

`rd-desktop-<version>-macos-universal.dmg`: open it and drag **rd Desktop** to Applications.

- The app is not signed by an Apple developer account, so the first start is blocked ("Apple
  could not verify…"): click **Done**, then System Settings → Privacy & Security → scroll down →
  **Open Anyway** next to rd Desktop.
- The first time a session takes the keyboard, macOS asks to let rd Desktop "control this
  computer using accessibility features": **Open System Settings** → switch on **rd Desktop**
  under Accessibility. Click outside the rd window and back in (or reconnect) to start the grab.
  Until then the app works like a browser tab (Cmd+Tab stays on this Mac).
- After installing a **new version**, macOS no longer matches the old permission (each build
  has its own ad-hoc signature): in Settings → Accessibility select rd Desktop, remove it with
  **−**, and allow it again when asked.

## Use

Start **rd Desktop**, sign in with Authentik in the window, pick a machine and connect. When you
click into the remote screen, the keyboard belongs to the remote machine, shortcuts included.
To get out, click outside the window (or a viewer control such as the settings menu): the grab
ends at once, and keys work locally again.

- Windows: Ctrl+Alt+Del and Win+L always stay with Windows (it reserves them). Media and volume
  keys also go to the remote while the grab is on.
- macOS: **Ctrl+Cmd+Q** (lock screen) and **Cmd+Option+Esc** (Force Quit) always stay on this
  Mac, so you can never lock the remote Mac by accident or get stuck. Media, volume and
  brightness keys stay local too. Caps Lock is this Mac's: the remote lock follows it.
- Modifier mapping (settings menu): from Windows the default is "Windows keyboard → Mac" (Ctrl
  acts as Cmd, Alt+Tab as Cmd+Tab); from a Mac it is passthrough (Cmd is Cmd).
- Links to other sites open in your normal browser. Only the gateway and
  `*.example.com` (Authentik) load inside the window.

Data (the web profile with the login cookies) and the log `rd-desktop.log`:

- Windows: `%LOCALAPPDATA%\dev.thijmen.rd.desktop` (log in `logs\`).
- macOS: `~/Library/WebKit/dev.thijmen.rd.desktop` and
  `~/Library/Caches/dev.thijmen.rd.desktop`; log in `~/Library/Logs/dev.thijmen.rd.desktop/`.

Delete those to sign out completely. Keys are never logged; each grab logs only how many key
events it forwarded.

## Options

| | |
|---|---|
| `--url <URL>` / `RD_URL` | open another gateway, e.g. `http://127.0.0.1:18580` (the flag wins over the variable). That origin gets the `rd_set_grab` permission instead of only the production one. |
| `RD_LOG` | log level: `error`, `warn`, `info` (default), `debug`, `trace` |

## Local testing

Against a dev gateway and the synthetic agent:

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

On macOS the grab needs the Accessibility permission for the app bundle. Sign local builds
with a stable identity so one grant survives rebuilds (ad-hoc signatures change every build),
and start the bundle, not the bare binary (that would be attributed to your terminal):

```sh
APPLE_SIGNING_IDENTITY="RD Local Code Signing" bun run tauri build --debug --bundles app
open -n "src-tauri/target/debug/bundle/macos/rd Desktop.app" --env RD_DESKTOP_SELFTEST=1 \
  --args --url http://127.0.0.1:18580/m/mbp
```

`RD_DESKTOP_GRAB_INJECTED=1` (debug builds) also grabs synthesized keys, which the grab
normally lets through, e.g. to drive it with keys posted by an agent on the same Mac.

## Build

The Tauri CLI version is pinned in `package.json` (`bun.lock`); the Rust crate has its own
`Cargo.lock` and is not part of the root workspace, so `cargo test --workspace` and the Linux
CI need no WebView dependencies.

```sh
cd clients/desktop
bun install --frozen-lockfile
bun run tauri build --bundles nsis      # Windows: src-tauri/target/release/rd-desktop.exe + bundle/nsis/*-setup.exe
bun run tauri build --bundles app,dmg  # macOS: src-tauri/target/release/bundle/{macos,dmg}/
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/desktop.yml`) builds on `windows-latest` and `macos-15` (a universal
arm64 + x86_64 app, ad-hoc signed) for pushes to `main` that touch `clients/desktop/**` or
`crates/rd-proto/**`, and on demand. To release: bump `version` in `src-tauri/Cargo.toml`,
commit, then

```sh
git tag desktop-v0.2.0 && git push origin desktop-v0.2.0
```

The workflow checks that the tag matches the version and publishes both exes, the .dmg and
`SHA256SUMS.txt` as a GitHub Release.

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
- `src/grab_macos.rs`: per grab, a thread with its own run loop owning a `CGEventTap` at the
  HID level (session level if that is refused), placed first. It needs the Accessibility
  permission: without it the grab is refused and macOS's prompt is shown once per launch. It
  is installed when the page asks while the window is the key window, and comes off when the
  page says so, the window stops being the key window, a new page loads, the window closes,
  or macOS disables the tap (then the page gets `rdNative.release()`). Keys from other
  processes (`CGEventPost`: an agent on the same Mac, remapping tools) pass through, so an
  agent here cannot feed its own keys back. Refused while Secure Keyboard Entry is on (macOS
  hides keys from every tap then).
- `src/keys_mac.rs`: virtual keycode → DOM code via `rd_proto::keymap::by_mac_keycode`;
  modifier presses from `FlagsChanged` and the device-dependent side bits; keys held before the
  grab; Caps Lock passes through (macOS toggles it before any tap); Ctrl+Cmd+Q and
  Cmd+Option+Esc stay local (Q matched by character, so it works on any layout). Unit tested
  on every platform.
- `src/keys.rs`: scan code (+`0xE000` for `LLKHF_EXTENDED`) → DOM code via
  `rd_proto::keymap::by_win_scancode`, with the quirks: Right Shift is reported extended,
  NumLock (`E0 45`) vs Pause (`45`), Break (`E0 46`) and SysRq (`54`) map to Pause/PrintScreen,
  fake shifts (`E0 2A`/`E0 36`) and AltGr's fake LCtrl (scan code `0x21D`) are dropped. Unit
  tested on every platform.

## Limits

- Linux: the app is a plain wrapper and refuses `rd_set_grab` (`keyboard grab not supported on
  this platform`), so the page keeps using DOM keys.
- macOS: Touch ID / the lock key and the power button act before any tap and stay local. Keys
  arrive through the tap only while no app has Secure Keyboard Entry on.
- On macOS, WKWebView also asks the navigation policy about iframes: an iframe from another
  site would be opened in the browser instead. The viewer has none.
- Fullscreen: when the page enters fullscreen (its Fullscreen button), Tauri makes the window
  fullscreen. During a grab, Ctrl+Alt+Shift+F would reach the page without a user gesture
  (WebView2 then refuses `requestFullscreen`), so the app handles that chord itself: it toggles
  the window's fullscreen and does not pass the F to the page.
- Windows silently drops a low-level hook that is too slow (`LowLevelHooksTimeout`), and some
  security or input software (key remappers, software KVMs, anti-keyloggers) can bypass or block
  it. Typing keeps working then (the page handles the keys that reach it, and asks the app to
  install a fresh hook), but Alt+Tab and the Windows key act locally. The log line at the end of
  each grab shows how many keys the hook forwarded and how many passed it.
- With Num Lock on and **Right** Shift held, the fake Right Shift around the navigation keys may
  not be told apart from the real one.
- Unsigned (see SmartScreen above); on macOS ad-hoc signed, not notarized (see Open Anyway
  above), so the Accessibility grant has to be renewed after each update.
