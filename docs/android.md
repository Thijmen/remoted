# Android app

rd Desktop also runs on Android: the same crate and setup page as the desktop app, in the
system WebView. It is a **client** — a phone that controls your Mac or PC. It is not an rd
agent for Android.

Status: the app builds, installs, shows the setup page, loads the gateway and updates itself
from GitHub. It has no Play Store listing: the APK is sideloaded and updates itself over its
own `android-v*` release channel (below). Until the first such tag exists, build it yourself.

## Install

Download `rd-desktop-<version>-android-arm64-armv7.apk` from the
[releases](https://github.com/Thijmen/remoted/releases) tagged `android-v<version>`, or from a
run of the **Android** workflow. Android 8.0 (API 26) or newer; the APK has 64-bit and 32-bit
ARM code.

The APK is not signed by Google Play and is not in the Play Store, so Android treats it as an
unknown source:

- The first install asks to allow **installs from this source** (Chrome or your file manager).
  Allow it, then tap the APK again.
- Play Protect may warn that the app was not scanned; choose **Install anyway**. It is signed
  with the project's own key, the same key every later update uses.

## Use

**First start** shows the setup page, exactly like rd Desktop: enter the gateway (e.g.
`https://rd.example.com`) and, if your login page is on another host, its domain. Sign in with
Authentik (password + TOTP) in the app's window, pick a machine and connect.

To change the gateway later: in a session, open the menu (the ▾ tab) → **Troubleshoot** →
**Change gateway**. (On the desktop, start the app with `--setup` instead.)

A **phone has no mouse or modifier keys**, so the viewer has a touch layer and an on-screen key
bar:

| Gesture | |
|---|---|
| Tap | left click |
| Two rapid taps | double click |
| Long press (0.5 s) | right click |
| One-finger drag | drag with the left button |
| Two-finger drag | scroll (content follows your fingers) |

The key bar above the screen sends sticky **Ctrl**, **Alt**, **Shift** and **Cmd** (the Meta
key; the label follows the remote OS), **Esc**, **Tab**, **Backspace**, **Enter**, the arrow
keys, and an **Fn** row with F1–F12, Home/End/Page Up/Page Down and Delete. The keyboard button
opens the phone's soft keyboard; what you type (including accents, emoji and CJK, through the
host's IME) goes to the remote machine as Unicode text. Text needs an agent that advertises the
`text` capability; the button is hidden otherwise.

The first visit to a machine on a touch screen shows the stream settings (resolution, frame
rate, bitrate, audio) instead of hiding them under **Stream settings**; the choice is saved per
machine. The default is 30 fps and 10 Mbps, and the picture is fit to the screen.

While a session is on screen the app is fullscreen (the system bars come back with a swipe from
the edge) and the screen stays awake. The back gesture leaves the session. File transfer and
multiple displays use the same menu as in a browser.

The **terminal** (menu → **Terminal**) uses the same bar while it has the keyboard: Esc, Tab,
arrows, Home/End, Page Up/Down, Delete and F1–F12 go to the shell, and a sticky **Ctrl** or
**Alt** applies to the next character you type on the phone (Ctrl+C, Alt+B).

## Updates

Release builds check `android.json` once at start (debug builds do not). When there is a newer
version, a banner appears on any page that is **not** a session — the portal, the machine's
connect page or the setup page — so an update never interrupts a session. **Update** downloads
the APK, checks the SHA-256 the manifest announces, and hands it to Android's installer;
**Later** hides the banner until the next launch. Android verifies that the APK is signed with
the same key as the installed app before it installs, so a tampered download cannot replace the
app. The first update also asks to allow installs from rd (Settings → Apps → rd → Install
unknown apps), after which **Update** works directly.

The update check is at start only; the app does not poll. If an update fails, the banner says
why and can be retried.

## Build

Prerequisites:

- Android SDK with NDK 27 and platform 36/37, and a JDK 17 or 21 (`JAVA_HOME`; the default
  JDK 25 is too new for Gradle).
- The Rust targets: `rustup target add aarch64-linux-android armv7-linux-androideabi`.

Then, from `clients/desktop`:

```sh
export ANDROID_HOME="$HOME/Library/Android/sdk"
export NDK_HOME="$ANDROID_HOME/ndk/27.1.12297006"
export JAVA_HOME="$(/usr/libexec/java_home -v 21)"
bun install
bun run tauri android build --apk --debug --target aarch64
```

The APK lands in `src-tauri/gen/android/app/build/outputs/apk/universal/debug/`; install it with
`adb install -r …`, or run `bun run tauri android dev` against a device or emulator.

Use `bun run tauri …`, not the CLI binary directly: the generated Gradle task calls the CLI back
the way it was first invoked, and a direct `./node_modules/.bin/tauri android init` leaves it
calling `node tauri`, which does not exist.

A universal debug APK carries all four ABIs with debug symbols (about 500 MB); `--target`
limits it to one ABI, which is what an emulator install needs.

## Testing

- The first start shows the setup page (no gateway configured). Enter the gateway there; the app
  remembers it and loads it from then on.
- On Apple silicon, name the emulator's ABI in the build: the AVD is `arm64-v8a`, so build with
  `--target aarch64`.
- Against a local gateway: `adb reverse tcp:8080 tcp:8080`, then `http://127.0.0.1:8080` as the
  gateway URL. Localhost is a secure context, so WebCodecs works without TLS. A debug APK allows
  cleartext, a release APK does not.
- Logs: `adb logcat | grep -iE 'rd_desktop|rd-desktop'` (the Rust log goes to logcat in Android builds).
- The update banner only exists in release builds. To try it, put an `android.json` and an APK
  on a local HTTPS server, or build with a higher version and publish the manifest where the
  constant in `src/android.rs` points.

## Project

- `src-tauri/gen/android` is the Gradle project (`bun run tauri android init` generates it and it
  is committed). Identifier `dev.thijmen.rd.desktop`, `minSdk` 26.
- The Rust side is a library (`src/lib.rs`, `run()`) with a thin desktop `src/main.rs`; modules
  that only make sense on a desktop are `#[cfg(desktop)]`. Android has no keyboard grab (a phone
  has no system shortcuts to take), no tray, no second window and no updater plugin.
- `src/android.rs` plus `gen/android/.../RdAndroidPlugin.kt` are the Android-only shell: the
  system bars and screen-on state, opening links in the browser, the setup page, and the APK
  update (check, download, verify, install).
- The version lives in `clients/android/package.json` (`rd-android`), its own channel separate
  from rd Desktop. Tauri derives `versionCode` from it (`major * 1_000_000 + minor * 1_000 +
  patch`) and writes both into `tauri.properties` for the build; `src/android.rs` and
  `tools/release/android-manifest.ts` derive the same code. CI passes the version with
  `--config '{"version":"…"}'`, because the shared crate otherwise carries the desktop version.

## CI

`.github/workflows/android.yml`:

- **Pull requests and `main`** build a debug APK (debug-signed, so it installs) for arm64 + armv7
  and upload it as the `rd-android-apk` artifact, with `SHA256SUMS-android.txt`.
- **An `android-v*` tag** builds the release APK signed with the keystore, publishes it as a
  GitHub Release, then writes `android.json` and uploads it to the `updates` pre-release (where
  `desktop.json` and `agent.json` live; docs/updates.md). The tag must match the version in
  `clients/android/package.json`.

The workflow installs the Android SDK packages the Gradle project builds against (see the `env`
at the top of `android.yml` and `compileSdk`/`targetSdk` in
`src-tauri/gen/android/app/build.gradle.kts`); keep those in step.

Release signing needs four repository secrets. Generate the key once and keep it — Android
refuses to update an app when the signing key changes, so losing it means every user must
uninstall and install again:

```sh
keytool -genkeypair -keystore rd-android.jks -alias rd -keyalg RSA -keysize 2048 -validity 10000
base64 -i rd-android.jks | pbcopy   # macOS; Linux: base64 -w0 rd-android.jks
```

| Secret | |
|---|---|
| `ANDROID_KEY_BASE64` | the base64 above |
| `ANDROID_KEY_ALIAS` | `rd` (or the alias you chose) |
| `ANDROID_KEY_PASSWORD` | the key password |
| `ANDROID_STORE_PASSWORD` | the keystore password |

Then bump `clients/android/package.json` (a changeset naming `rd-android` does it), merge, and
tag the version changesets writes:

```sh
git tag android-v0.1.0 && git push origin android-v0.1.0
```

`tauri android build` with no keystore (a local release build) produces an unsigned APK; local
debug builds are unaffected.

## Limits

- No file transfer or audio-specific work on Android yet; the existing menus still offer files
  when the agent has it, but downloads in a WebView need native handling (not wired up).
- No pinch-to-zoom: the picture is fit to the screen.
- No keyboard grab: a phone cannot capture system shortcuts, so the key bar is the way to send
  modifiers; a Bluetooth keyboard sends every key the WebView sees.
- The system WebView is not Chrome; H.264 hardware decode and frame pacing depend on the device.
  If a phone decodes poorly, open the gateway in Chrome instead — the same viewer has the touch
  layer there.
- The app is not in the Play Store, and nothing is code-signed beyond the self-signed Android
  key (a Play Protect warning on install is expected).
