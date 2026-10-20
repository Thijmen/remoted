# rd-desktop

## 1.5.0

### Minor Changes

- c374fa2: rd Desktop now updates itself. It checks for a new version at launch and every few hours, downloads it in the background and, when you're not in a session, shows what's new since your version with a **Restart & update** button. A new tray (menu bar) icon, the macOS app menu and the Windows title-bar menu have **Check for Updates…**; automatic checks can be turned off in the tray menu. On macOS the app is now signed with a stable certificate, so the Accessibility permission survives updates (grant it once more after installing this version).

## 1.4.2

### Patch Changes

- 2b1bbb7: The Windows agent installer builds again when Chocolatey is unavailable, so releases that include it are no longer held up.

## 1.4.1

### Patch Changes

- 461e19b: Fix the rd Desktop builds for macOS and Windows, which failed because the app's dependency lockfile was out of date.

## 1.4.0

### Minor Changes

- 275865c: Switch between the host's screens from the viewer: the session menu now has a display picker (and Ctrl+Alt+Shift+M
  jumps to the next display), and the chosen screen is remembered for the next connect. The portal, connect screen and
  session menu also got a cleaner look.

## 1.3.0

### Minor Changes

- 4f1cc49: Add devices from the portal: **Add device** creates the device's token and shows a one-time setup code to paste into `rd-agent setup` (also on the Windows installer's last page). **New token** and **Remove** on a device's card replace its token or remove it, and disconnect its agent right away. The gateway needs `RD_STATE_DIR` (a persistent volume) and `RD_AGENT_PUBLIC_URL` for this; tokens provisioned in `RD_AGENT_TOKENS_DIR` keep working.

## 1.2.0

### Minor Changes

- 385b13d: Windows agent installer: the release now includes `rd-agent-<version>-windows-x64-setup.exe`, a per-user installer (no admin rights) with Start menu shortcuts and an entry in Settings → Apps. It offers the setup at the end, updates a running agent in place, and its uninstaller removes the autostart.

## 1.1.0

### Minor Changes

- 2bf4fa4: Connect to Windows PCs: rd-agent now runs on Windows too, and the release includes it (`rd-agent-<version>-windows-x64.zip`; setup in docs/setup-windows.md). From a Mac, the new default "Automatic" modifier mapping makes Cmd act as Ctrl and Ctrl as the Windows key, and Cmd+Tab switches apps on the PC.
