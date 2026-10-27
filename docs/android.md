# Android app

rd Desktop also builds for Android: the same crate and setup page as the desktop app, in the
system WebView. It is a **client** — a phone or tablet that controls your Mac or PC. It is not
an rd agent for Android.

**Status: in progress.** The app builds, installs, shows the setup page and loads the gateway.
The viewer already has a touch layer (tap, long-press right click, drag, two-finger scroll), an
on-screen key bar (sticky modifiers, arrows, F1–F12) and soft-keyboard text via the `TEXT` wire
message. Immersive mode, the back button, changing the gateway in-app and APK updates are not in
yet, and nothing is released.

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
- There is no in-app way to change the gateway yet: clear the app's storage (Settings → Apps →
  rd Desktop → Storage → Clear storage) to get the setup page back.
- On Apple silicon, name the emulator's ABI in the build: the AVD is `arm64-v8a`, so build with
  `--target aarch64`.
- Against a local gateway: `adb reverse tcp:8080 tcp:8080`, then `http://127.0.0.1:8080` as the
  gateway URL. Localhost is a secure context, so WebCodecs works without TLS.

## Project

- `src-tauri/gen/android` is the Gradle project (`bun run tauri android init` generates it and it
  is committed). Identifier `dev.thijmen.rd.desktop`, `minSdk` 26.
- The Rust side is a library (`src/lib.rs`, `run()`) with a thin desktop `src/main.rs`; modules
  that only make sense on a desktop are `#[cfg(desktop)]`. Android has no keyboard grab (a phone
  has no system shortcuts to take), no tray, no second window and no updater plugin.
- The version lives in `clients/android/package.json` (`rd-android`), its own channel separate
  from rd Desktop. Tauri derives `versionCode` from it (`major * 1_000_000 + minor * 1_000 +
  patch`) and writes both into `tauri.properties` for the build. The APK updater is still to do.

## CI

`.github/workflows/android.yml`:

- **Pull requests and `main`** build a debug APK (debug-signed, so it installs) for arm64 + armv7
  and upload it as the `rd-android-apk` artifact, with `SHA256SUMS-android.txt`.
- **An `android-v*` tag** builds the release APK signed with the keystore and publishes it as a
  GitHub Release. The tag must match the version in `clients/android/package.json`; the workflow
  checks that. Without the secrets it fails rather than publishing an APK nobody can update.

The workflow installs the Android SDK packages the Gradle project builds against (see the `env`
at the top of `android.yml` and `compileSdk`/`targetSdk` in
`src-tauri/gen/android/app/build.gradle.kts`); keep those in step.

Release signing needs four repository secrets. Generate the key once and keep it — Android
refuses to update an app when the signing key changes:

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

Then bump `clients/android/package.json`, commit and tag:

```sh
git tag android-v0.1.0 && git push origin android-v0.1.0
```

`tauri android build` with no keystore (a local release build) produces an unsigned APK; local
debug builds are unaffected.
