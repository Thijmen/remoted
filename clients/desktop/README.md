# rd Desktop

The normal rd viewer (loaded from your gateway) in its own window, for Windows and macOS. While a session has the keyboard it also gets the shortcuts a browser tab never sees:

- Windows: Alt+Tab, the Windows key, Alt+Esc, Ctrl+Esc, Alt+F4, Caps Lock, …
- macOS: Cmd+Tab, Cmd+Space, Cmd+Q, Cmd+H, Ctrl+arrows (Spaces), Mission Control, …

all go to the remote machine.

Everything else (Authentik login, portal, video, settings) is the web viewer, loaded from the
gateway, so the app needs no update when the viewer changes. The app itself updates from the
GitHub releases ([Updates](#updates)). Design and the page ⇄ app bridge:
[docs/desktop-client.md](../../docs/desktop-client.md); updates:
[docs/updates.md](../../docs/updates.md).

## Install

Download from the [GitHub Releases](https://github.com/Thijmen/remoted/releases) (tags
`desktop-v*`), or from the artifacts of a run of the **Desktop** workflow.

### Windows 10/11

| File | |
|---|---|
| `rd-desktop-<version>-x64-setup.exe` | per-user installer: no admin rights, Start menu entry, uninstall via Settings → Apps. Updates itself. |
| `rd-desktop-<version>-x64-portable.exe` | the same app as a single exe, nothing to install. Says when there is a new version, but you download it yourself. |

The files are not code-signed, so SmartScreen warns the first time ("Windows protected your
PC"): click **More info → Run anyway**. (On a managed laptop a policy may block unsigned
programs outright; the app cannot work around that.) Updates the app downloads itself run
without that prompt.

The app uses the WebView2 runtime that ships with Windows 10/11 (the installer fetches it if it
is missing).

### macOS 12+ (Apple silicon and Intel)

`rd-desktop-<version>-macos-universal.dmg`: open it and drag **rd Desktop** to Applications.
Start it from there: run from the disk image, or from a folder you cannot write, it cannot
update itself.

- The app is not signed by an Apple developer account, so the first start is blocked ("Apple
  could not verify…"): click **Done**, then System Settings → Privacy & Security → scroll down →
  **Open Anyway** next to rd Desktop. Updates the app installs itself are not blocked.
- The first time a session takes the keyboard, macOS asks to let rd Desktop "control this
  computer using accessibility features": **Open System Settings** → switch on **rd Desktop**
  under Accessibility. Click outside the rd window and back in (or reconnect) to start the grab.
  Until then the app works like a browser tab (Cmd+Tab stays on this Mac).
- Every release is signed with the same certificate, so the permission stays when the app
  updates. Coming from 1.4.x or older (ad-hoc signed), macOS does not recognize the new version
  once: in Settings → Accessibility select rd Desktop, remove it with **−**, and allow it again
  when asked.

## Use

Start **rd Desktop**. The first time, it asks for:

- **Gateway URL**: where your rd gateway serves the viewer, e.g. `https://rd.example.com`
  (`https://` is added if you leave it out; plain `http://` only for `localhost`).
- **Sign-in domain** (optional): if the gateway sends you to a login page on another host (e.g.
  Authentik at `auth.example.com`), enter `example.com` so that page loads in the window
  instead of your browser.

Both are saved in `settings.json` (below). To change them later, start the app with `--setup`
(Windows: `rd-desktop.exe --setup`; macOS: `open -a "rd Desktop" --args --setup`), or delete
the file.

Then sign in in the window, pick a machine and connect. When you
click into the remote screen, the keyboard belongs to the remote machine, shortcuts included.
To get out, click outside the window (or a viewer control such as the settings menu): the grab
ends at once, and keys work locally again.

- Windows: Ctrl+Alt+Del and Win+L always stay with Windows (it reserves them). Media and volume
  keys also go to the remote while the grab is on.
- macOS: **Ctrl+Cmd+Q** (lock screen) and **Cmd+Option+Esc** (Force Quit) always stay on this
  Mac, so you can never lock the remote Mac by accident or get stuck. Media, volume and
  brightness keys stay local too. Caps Lock is this Mac's: the remote lock follows it.
- Modifier mapping (settings menu): the default, "Automatic", follows the keyboard and the remote
  OS. Windows → Mac: Ctrl acts as Cmd, Alt+Tab as Cmd+Tab. Mac → Windows: Cmd acts as Ctrl,
  Cmd+Tab as Alt+Tab. The same OS on both sides passes keys through.
- Links to other sites open in your normal browser. Only the gateway and hosts under the
  sign-in domain load inside the window.

### Updates

rd Desktop looks for a new version on GitHub 10 s after it starts and then every 6 hours. It
downloads it in the background and checks its signature, then opens a small window with what
changed in every version since yours and one button, **Restart & update**:

- Windows: the app closes, the installer shows a progress bar, then starts the new version.
- macOS: the app replaces itself and restarts.

The window never opens over a session (a machine's page in the main window): it waits until you
leave it. Closing the window means "later": it comes back at the next check or start, and the
tray menu offers **Restart to Update to X.Y.Z…** meanwhile. After the update, the main window
shows "Updated to X.Y.Z · What's new" for 20 s; **What's new** opens the notes again.

The tray icon (macOS: menu bar) shows the version and has **Check for Updates…**, **Check for
updates automatically**, **Show rd** and **Quit**. **Check for Updates…** is also in the macOS
app menu, and in the Windows title-bar menu (right-click the title bar, or Alt+Space) next to
**About rd Desktop**. Closing the main window still quits the app.

The portable exe, and a macOS app that cannot replace itself (run from the disk image, from
where it was downloaded instead of Applications, or from a folder you cannot write), show the
same window with **Download** instead: it opens the release page.

Settings (`settings.json`: `gateway_url`, `sign_in_domain`, and `"update_check": false` when
automatic update checks are off):

- Windows: `%APPDATA%\dev.thijmen.rd.desktop\settings.json`
- macOS: `~/Library/Application Support/dev.thijmen.rd.desktop/settings.json`

Data (the web profile with the login cookies, `state.json` with the version that ran last) and
the log `rd-desktop.log`:

- Windows: `%LOCALAPPDATA%\dev.thijmen.rd.desktop` (log in `logs\`).
- macOS: `~/Library/WebKit/dev.thijmen.rd.desktop` and
  `~/Library/Caches/dev.thijmen.rd.desktop`; `state.json` next to `settings.json`; log in
  `~/Library/Logs/dev.thijmen.rd.desktop/`.

Delete those to sign out completely. Keys are never logged; each grab logs only how many key
events it forwarded.

## Options

| | |
|---|---|
| `--setup` | show the setup page, to change the saved gateway |
| `--url <URL>` / `RD_URL` | open this gateway for this run only, without saving it, e.g. `http://127.0.0.1:18580` (the flag wins over the variable). That origin gets the `rd_set_grab` permission instead of the saved one. |
| `RD_LOG` | log level: `error`, `warn`, `info` (default), `debug`, `trace` |
| `RD_UPDATE_CHECK=0` | no automatic update checks, whatever the tray menu says (managed machines). **Check for Updates…** still works. |
| `RD_UPDATE_URL` | read the update manifest from this URL instead of GitHub (testing, forks): `https://`, or plain `http://` to localhost in debug builds. Debug builds check automatically only when it is set. |

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

Updates, end to end, with a throwaway key (never the release key) and a test identifier, so
the test copies keep their settings and web data apart from yours:

```sh
bun run tauri signer generate --ci -w /tmp/rdkey -p pw
# conf-0.0.2.json: {"version": "0.0.2", "identifier": "dev.thijmen.rd.desktop.test",
#   "bundle": {"createUpdaterArtifacts": true},
#   "plugins": {"updater": {"pubkey": "<contents of /tmp/rdkey.pub>",
#                           "dangerousInsecureTransportProtocol": true}}}
TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/rdkey)" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=pw \
  bun run tauri build --bundles app --config conf-0.0.2.json
```

Put `rd Desktop.app.tar.gz` and a `desktop.json` ([docs/updates.md](../../docs/updates.md):
its `url`, and the `.sig` file's contents as `signature`) in a folder, serve it with
`python3 -m http.server --bind 127.0.0.1 8123`, build 0.0.1 the same way, copy it to a folder
of its own and start it with `RD_UPDATE_URL=http://127.0.0.1:8123/desktop.json`.
`dangerousInsecureTransportProtocol` lets a release build use plain http; never ship it. Start
the binary inside the bundle (`…/Contents/MacOS/rd-desktop`) from a path without symlinks
(`/private/tmp`, not `/tmp`): Tauri does not trust an executable path through a symlink, so
such a copy only offers the download.

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

A local `tauri build` is ad-hoc signed and has no updater files: those need the private update
key (`TAURI_SIGNING_PRIVATE_KEY` and `_PASSWORD`, and `--config
'{"bundle":{"createUpdaterArtifacts":true}}'`). See [Local testing](#local-testing) for an
end-to-end update with a throwaway key.

CI (`.github/workflows/desktop.yml`) builds on `windows-latest` and `macos-15` (a universal
arm64 + x86_64 app) for pull requests and pushes to `main` that touch `clients/desktop/**`,
`crates/rd-proto/**` or the release tooling, and on demand. With the repository's secrets it
signs the macOS app with the project's "rd Code Signing" certificate (so the Accessibility
permission survives updates) and writes the signed updater files (the installer's `.sig`, the
macOS `.app.tar.gz` and its `.sig`); without them (pull requests from forks) the builds are
ad-hoc signed and have none. Keys and certificates: [docs/updates.md](../../docs/updates.md#signing).

### Releasing

Releases go through [changesets](https://github.com/changesets/changesets). The version lives in
`package.json`; `tools/release/sync-versions.ts` copies it into `src-tauri/Cargo.toml` and
`Cargo.lock` (CI fails if they differ). The rd agent is released the same way but on its own
(package `rd-agent`, tags `agent-vX.Y.Z`, `.github/workflows/agent.yml`).

1. In a PR that changes the app, add a changeset from the repository root:
   ```sh
   bun install && bun changeset   # pick rd-desktop, patch/minor/major, one line for the changelog
   ```
   This writes a `.changeset/*.md` file; commit it with the change. Its text is what users see
   in the update window, so write it for them.
2. After merging, `.github/workflows/release.yml` opens (or updates) a **Version packages** PR
   that bumps the version, writes `CHANGELOG.md` and syncs the Rust crate. Several changesets
   collect into one release.
3. Merging the version PR tags `desktop-vX.Y.Z` (and `agent-vX.Y.Z` when the agent has a new
   version too) and starts the Desktop workflow on the tag. It checks that the tag matches the
   version and publishes the installer, the portable exe, the .dmg, the updater files and
   `SHA256SUMS.txt` as a GitHub Release, with this version's changelog section as notes. Then
   it writes `desktop.json` (`tools/release/manifest.ts`) to the `updates` release, and from
   that moment installed apps offer the update ([docs/updates.md](../../docs/updates.md)).

A manual `bun run version` (from the root) does step 2's edits locally. Pushing a
`desktop-v*` tag by hand still works too.

Icons: `icon.svg` is the source; `bun run tauri icon icon.svg -o src-tauri/icons`, then delete
the generated `android/`, `ios/`, `Square*`, `StoreLogo.png` and `64x64.png`. The macOS menu bar
icon is a template image from `tray-icon.svg`: `rsvg-convert -w 36 -h 36 tray-icon.svg -o
src-tauri/icons/tray-template.png`.

## How it works

- `src/main.rs`: the window (title "rd", 1280×800, remembers nothing), navigation policy, the
  `rd_set_grab` command and the setup commands. They are app commands with ACL permissions
  (`build.rs`: `AppManifest`). `rd_set_grab` and `rd_whats_new` are granted only by a
  capability added at runtime for the gateway origin in use (saved, or `--url`), window
  `main`. The setup page (`dist/index.html`, bundled) gets only `rd_setup_info`,
  `rd_save_gateway` and `rd_whats_new` (`capabilities/setup.json`, local pages); the update
  window (`dist/update.html`, window `update`) only `rd_update_state`, `rd_update_check` and
  `rd_update_install` (`capabilities/update.json`). No other command or plugin permission is
  exposed: the updater plugin is used from Rust only.
- `src/update.rs`: the update checks (timer thread; wall-clock time, so a sleeping laptop
  checks when it wakes), the download, the update window (state pushed to the page as
  `window.rdUpdate(view)`; its links open in the system browser), install and restart. The
  updater plugin reads the manifest (`plugins.updater` in `tauri.conf.json`, or
  `RD_UPDATE_URL`) and verifies the download against `plugins.updater.pubkey`; the signature
  must name the announced version (`requireSignedVersion`). The window waits while the main
  window shows a `/m/…` page of the gateway.
- `src/notes.rs`: the manifest's `releases`, the versions between two versions, and their
  Markdown as HTML without raw HTML, images or non-http(s) links (the manifest is not signed).
- `src/installable.rs`: can this copy replace itself (Windows: `uninstall.exe` next to the exe;
  macOS: an `.app` bundle, not translocated, its folder and itself writable)?
- `src/notice.rs`: `state.json` (the version that ran last) and the "Updated to X.Y.Z" notice,
  injected into the main window's page with `eval` in a closed shadow root.
- `src/menus.rs`: the tray icon and menu (macOS: a template image, `tray-icon.svg`), and "Check
  for Updates…" in Tauri's default macOS menu. `src/sysmenu_windows.rs`: the Windows title-bar
  menu items (system menu + window subclass).
- `src/config.rs`: `--url`/`--setup`, the settings file and its validation, and which URLs may
  load in the window.
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
- Windows: not code-signed (see SmartScreen above). macOS: signed with a self-signed
  certificate, not notarized (see Open Anyway above).
- Updates: no beta channel, and no update is forced. Only files signed with the release key
  are installed. Linux builds do not update themselves. The portable exe and macOS copies that
  cannot replace themselves only link to the download.
