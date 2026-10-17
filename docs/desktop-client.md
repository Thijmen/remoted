# Desktop client (rd Desktop)

A small Tauri app that opens the normal viewer (`https://rd.example.com`) in its own
window. It exists for one reason: a browser tab cannot see the shortcuts the operating system
keeps for itself (Alt+Tab, the Windows key, Alt+Esc, Ctrl+Esc, …) unless it is fullscreen with
Keyboard Lock. The desktop app grabs them with a low-level keyboard hook while you are in a
session and hands them to the page, which sends them to the remote machine like any other key.

Everything else — login (Authentik), the portal, video, input, settings — is the same web
viewer, loaded from the gateway. The app has no copy of the viewer and needs no update when the
viewer changes.

Target: Windows 10/11 (WebView2, preinstalled). Per-user, no admin rights. A macOS build works
as a plain wrapper (no keyboard grab yet).

## Bridge contract, version 1

The page and the app talk through two small channels. Nothing else crosses the boundary.

### Page → app: grab state

When the viewer runs inside the app, Tauri injects `window.__TAURI_INTERNALS__`. The page calls

```js
window.__TAURI_INTERNALS__.invoke('rd_set_grab', { active: true | false })
```

- `active: true` exactly while: a session is streaming, the page is visible, the document has
  focus, and keyboard input would go to the remote machine (the stage/canvas is focused, not a
  viewer control such as the settings menu).
- `active: false` on any change away from that (blur, tab hidden, viewer UI focused, disconnect,
  page unload). The page calls it on every transition (it may repeat a value; the app ignores
  repeats).
- Errors from `invoke` are ignored (logged once) — e.g. an older app without the command.

### App → page: keys

While the grab is active **and** the app window is the foreground window, the app's low-level
keyboard hook swallows every key (so Windows does not act on it) and forwards it with

```js
window.rdNative.key(code, down, repeat)
```

- `code`: the DOM `KeyboardEvent.code` string for the physical key (e.g. `"Tab"`, `"AltLeft"`,
  `"MetaLeft"`, `"KeyA"`), derived from the hardware scan code (the same table as the viewer's
  `keymap.gen.ts` / `rd-proto::keymap`). Keys without a code are dropped.
- `down`: true on press, false on release. `repeat`: true for auto-repeat presses (a press of a
  key the hook already saw down).
- The page treats these exactly like DOM keyboard events (modifier mapping, Caps Lock sync,
  chords such as Ctrl+Alt+Shift+D), and ignores real DOM key events while the grab is active (so
  nothing is sent twice).

```js
window.rdNative.release()
```

- Called when the app stops grabbing for its own reasons (window lost foreground, hook removed,
  app closing). The page sends `RELEASE_ALL` and forgets pressed keys, exactly as on window blur.

`window.rdNative` is defined by the viewer (always, also in a browser — it is then unused). The
app calls it with `eval` on the webview, which works whatever the page's CSP is.

### What can never be grabbed

Ctrl+Alt+Del and Win+L (Windows reserves them). A mouse click outside the window also ends the
grab, so you can never get stuck.

## Security notes

- The app only loads the configured gateway origin; navigation elsewhere opens the system
  browser. The remote capability that allows `rd_set_grab` is limited to that origin.
- The hook is installed only while the grab is active and the window is foreground; it never
  logs keys.
- The gateway CSP allows the Tauri IPC endpoints (`ipc:` and `http://ipc.localhost`) in
  `connect-src`; normal browsers have no such endpoints.
