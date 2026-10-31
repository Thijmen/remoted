# Desktop client (rd Desktop)

A small Tauri app that opens the normal viewer (your gateway, e.g. `https://rd.example.com`; asked
for on first start) in its own window. It exists for one reason: a browser tab cannot see the shortcuts the operating system
keeps for itself (Windows: Alt+Tab, the Windows key, Alt+Esc, Ctrl+Esc, …; macOS: Cmd+Tab,
Cmd+Space, Ctrl+arrows, …) unless it is fullscreen with Keyboard Lock (Chrome and Edge only;
macOS never passes Cmd+Tab to a page). The desktop app grabs them while you are in a session
(Windows: a low-level keyboard hook; macOS: an event tap) and hands them to the page, which
sends them to the remote machine like any other key.

Everything else — login (Authentik), the portal, video, input, settings — is the same web
viewer, loaded from the gateway. The app has no copy of the viewer and needs no update when the
viewer changes. It updates itself from the GitHub releases: [updates.md](updates.md).

Targets: Windows 10/11 (WebView2, preinstalled; per-user, no admin rights) and macOS 12+
(WKWebView; universal app; the grab needs the Accessibility permission).

On **Android** the app is just the viewer in the system WebView: it loads the same gateway and
setup page, but refuses `rd_set_grab` (a phone has no system shortcuts to take) and has no
tray, updater plugin or second window. Its Troubleshoot screen shows **Android**, explains that
shortcuts stay on the phone, and offers **Change gateway** (`rd_open_setup`). Updates are a
banner on the portal (`rd_android_update`), installed by Android rather than by the app. See
[android.md](android.md).

The app: [clients/desktop](../clients/desktop/README.md) (install, options, build, release).

## Bridge contract, version 1

The page and the app talk through two small channels. Nothing else crosses the boundary.

### Page → app: grab state

When the viewer runs inside the app, Tauri injects `window.__TAURI_INTERNALS__`. The page calls

```js
window.__TAURI_INTERNALS__.invoke('rd_set_grab', { active: true | false, seq })
```

- `active: true` exactly while: a session is streaming, the page is visible, the document has
  focus, and keyboard input would go to the remote machine (the stage/canvas is focused, not a
  viewer control such as the settings menu).
- `active: false` on any change away from that (blur, tab hidden, viewer UI focused, disconnect,
  page unload). The page decides once focus has settled (a focus move fires focusout and focusin
  in one go).
- **One call at a time:** the page sends the next call only when the previous one is answered
  (or after 3 s without an answer), always with the latest wanted state, so the app never ends
  up in an older state than the page. `seq` increases with every call and starts at the clock
  (µs), so a new page outranks late calls of the previous one; the app applies a call only if
  no newer one was applied (Tauri runs commands concurrently). Calls without `seq` (older pages)
  are always applied.
- A refused `true` (e.g. the window was not in front yet) is asked again on the next transition
  or click into the video; until then DOM keys work. After `rdNative.release()` the page asks
  again as soon as the keys should be grabbed.
- Errors from `invoke` are logged once — e.g. an older app without the command.

### Page → app: Troubleshoot

The viewer's Settings › Troubleshoot screen (session menu) asks the app two more things. Both
are optional: an older app refuses them and the page falls back to the `rd_set_grab` error text.

```js
window.__TAURI_INTERNALS__.invoke('rd_keyboard_status')
// → { platform: "macos" | "windows" | "other", version, grab,
//     accessibility: bool | null, secureInput: bool | null }
window.__TAURI_INTERNALS__.invoke('rd_open_keyboard_settings')
```

- `rd_keyboard_status` is read-only: the app version, whether it can grab on this platform, and
  on macOS whether it has the Accessibility permission and whether Secure Keyboard Entry is on
  (`null` elsewhere).
- `rd_open_keyboard_settings` opens System Settings › Privacy & Security › Accessibility on
  macOS (that one page, nothing else); elsewhere it is refused.
- When a grab is refused, the page asks for the status. If the Accessibility permission is the
  reason, it shows a banner ("Shortcuts like Cmd+Tab stay on this Mac") with a button to the
  Troubleshoot screen. The screen also lists the modifier mapping in use and whether the remote
  machine may inject input.

### App → page: keys

While the grab is active **and** the app window is the foreground (macOS: key) window, the app
swallows every key (so the OS does not act on it) and forwards it with

```js
window.rdNative.key(code, down, repeat)
```

- `code`: the DOM `KeyboardEvent.code` string for the physical key (e.g. `"Tab"`, `"AltLeft"`,
  `"MetaLeft"`, `"KeyA"`), derived from the hardware scan code (the same table as the viewer's
  `keymap.gen.ts` / `rd-proto::keymap`). Keys without a code are dropped.
- `down`: true on press, false on release. `repeat`: true for auto-repeat presses (a press of a
  key the hook already saw down).
- The page treats these exactly like DOM keyboard events (modifier mapping, Caps Lock sync,
  chords such as Ctrl+Alt+Shift+D).
- The app swallows every key it forwards, so it never also arrives as a DOM event.
  - **Windows:** a DOM key the page still receives during a grab is one the hook did **not**
    forward (input other programs inject, keys held from before the grab, a key without a code,
    or a hook that Windows dropped or other software blocks). The page handles it like any DOM
    key, with the modifiers the hook holds added (Windows never saw those), instead of losing it;
    a press of a key already down through the hook is not sent again, and duplicates of releases
    are harmless. If such a press arrives while no key has come through the hook for 2 s, the
    page has the app install a fresh hook (`rd_set_grab` false then true), backing off from 5 s
    to 60 s.
  - **macOS:** the tap passes only what it leaves local on purpose (Ctrl+Cmd+Q, Cmd+Option+Esc,
    and keys other processes synthesize — an agent on the same Mac included, whose keys must not
    loop back), so the page drops DOM keys during a grab, except Caps Lock.
- Caps Lock: macOS toggles the lock before any tap sees the key, so the macOS app lets Caps Lock
  through and the page syncs the remote lock from that DOM event. (The Windows hook swallows
  Caps Lock and reports it through `rdNative.key`; the local lock does not change during a grab
  there.)

```js
window.rdNative.release()
```

- Called when the app stops grabbing for its own reasons (window lost foreground, hook removed,
  app closing). The page sends `RELEASE_ALL` and forgets pressed keys, exactly as on window blur.

`window.rdNative` is defined by the viewer (always, also in a browser — it is then unused). The
app calls it with `eval` on the webview, which works whatever the page's CSP is.

### What can never be grabbed

- Windows: Ctrl+Alt+Del and Win+L (Windows reserves them).
- macOS: Ctrl+Cmd+Q (lock screen) and Cmd+Option+Esc (Force Quit) are let through on purpose;
  Touch ID, the power button and media/brightness keys are not key events.

A mouse click outside the window also ends the grab, so you can never get stuck.

### Files: drops and downloads

The viewer's [file transfer](files.md) needs two things from the window, both plain web
behaviour in a browser:

- **Dropping files** onto the window reaches the page as HTML drag and drop (the app turns off
  Tauri's own drop handler, which would swallow them).
- **Downloads** the page hands over (a finished transfer is a `blob:` URL of the gateway page)
  are saved to the Downloads folder under a free name. Downloads from any other origin are
  refused. macOS has no download window: the viewer's transfer list says when a file was saved.

## Security notes

- The app only loads the configured gateway origin (plus, if set, hosts under the configured
  sign-in domain, for the login page); navigation elsewhere opens the system browser. The
  capability that allows `rd_set_grab` (and `rd_whats_new`, `rd_keyboard_status`,
  `rd_open_keyboard_settings`) is added at runtime for the gateway origin only. The
  bundled setup page may only read and save the gateway (`capabilities/setup.json`).
- `rd_whats_new` (no arguments) is allowed for the gateway origin (the same runtime
  capability) and the setup page. It opens the app's own release-notes window, reading the
  public update manifest if the app has not yet: a page can make that window appear, nothing
  more. The app calls it from the "Updated to X.Y.Z · What's new" notice it injects after an
  update (with `eval`, in a closed shadow root); where it is refused (a sign-in page), the
  notice opens the release page in the browser instead.
- The update window is a bundled page in its own window (`update`). It may only read the update
  state, start a check, and install the update the app downloaded and verified, or open its
  release page (`capabilities/update.json`). No page gets the updater plugin's commands; the
  app uses the plugin from Rust only.
- Updates are installed only if signed with the release key (`plugins.updater.pubkey`), and
  the signature must name the version the manifest announces (`requireSignedVersion`). The
  manifest itself is not signed: its release notes are rendered without raw HTML, images or
  non-http(s) links, links open in the system browser, and the window's CSP allows no remote
  content or inline script. `RD_UPDATE_URL` (another manifest) accepts https, or plain http to
  localhost in debug builds only.
- The hook/tap is installed only while the grab is active and the window is foreground; it
  never logs keys (only how many it forwarded).
- The gateway CSP allows the Tauri IPC endpoints (`ipc:` and `http://ipc.localhost`) in
  `connect-src`; normal browsers have no such endpoints.

## Implementation notes (rd Desktop)

How the app behaves within the contract above (Windows unless noted; the macOS differences
follow):

- `rd_set_grab({ active: true })` resolves only once the hook is installed. It is rejected when
  the window is not the foreground window, when the hook cannot be installed, and always on
  macOS/Linux (`keyboard grab not supported on this platform`); the page then keeps its DOM key
  listeners. `active: false` removes the hook and resolves; it never triggers `release()`.
- The app calls `release()` only when it stops grabbing on its own: the window lost the
  foreground, the input desktop switched (lock screen, UAC, Ctrl+Alt+Del), a new page loads in
  the window, or the window closes. It never re-installs the hook unasked; the page asks again.
- Every key is swallowed while grabbing, Caps Lock included. Injected input (`SendInput` from
  other programs) passes through, and so do keys without a DOM code; the page handles them as
  DOM keys. Keys that were already down when the hook went on pass through until released, so
  Windows never keeps them stuck; their release is still forwarded.
- Each grab logs `keyboard grab on` and, when it ends, `keyboard grab off (<why>): N key events
  forwarded; passed to Windows: … injected, … while not in front, … held from before, … without
  a key code` (info level; counts only, never keys). A grab that forwarded nothing although keys
  passed also logs a warning.
- Debug builds: `RD_DESKTOP_GRAB_INJECTED=1` grabs injected keys too, so tests can drive the
  hook with `SendInput`.
- AltGr: Windows' fake left Ctrl (scan code `0x21D`) is dropped, so the page sees `AltRight`
  only. Fake shifts (`E0 2A` / `E0 36`) are dropped too.
- Calls are `window.rdNative && window.rdNative.key(...)` (guarded; `rdNative` exists only on
  session pages), one `eval` per event, in order.
- Updates never interrupt a session: the update window opens only when the main window is not
  on a `/m/…` page of the gateway, and otherwise when that page is left (the next page load).

macOS:

- `rd_set_grab({ active: true })` is also rejected without the Accessibility permission (macOS's
  prompt is shown once per launch), while Secure Keyboard Entry is on, and when macOS refuses
  the tap. The tap sits at the HID level, falling back to the session level.
- `release()` also follows the window losing key status and macOS disabling the tap (timeout or
  user input).
- Keys posted by other processes (`kCGEventSourceUnixProcessID` ≠ 0) pass through: an rd agent
  on the same Mac, remapping tools. Hardware keys, including Karabiner's virtual keyboard, are
  grabbed.
- Caps Lock passes through (see above). Ctrl+Cmd+Q and Cmd+Option+Esc pass through and are not
  forwarded; Q is matched by the character it types, so this holds on any layout.
- Modifiers come from `FlagsChanged` events and the device-dependent flag bits (left/right).
  Keys already down when the grab starts pass through until released; their release is
  forwarded.
