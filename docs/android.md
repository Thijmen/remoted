# Android app

rd Desktop also builds for Android: the same crate and setup page as the desktop app, in the
system WebView. It is a **client** — a phone or tablet that controls your Mac or PC. It is not
an rd agent for Android.

**Status: spike.** The app builds, installs, shows the setup page and loads the gateway. The
touch layer, soft-keyboard input, the phone layout, immersive mode, the back button and APK
updates are not in yet, and nothing is released: this is only enough to check that the WebView
decodes the stream, that Authentik works inside it, and how the soft keyboard behaves.

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
- Versions come from `tauri.properties` (`versionCode`/`versionName`), generated per build. The
  separate `android-v*` channel and the APK updater are part of the release work.
