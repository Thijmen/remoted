# Updates

rd Desktop and the rd agent find new versions on GitHub, show what changed, and install
themselves when you say so. The gateway (and with it the viewer) is not covered: it is
deployed with your own tooling (`gateway-v*` images).

| | Checks | Asks | Installs |
|---|---|---|---|
| rd Desktop (Windows installer, macOS app) | at launch and every 6 h | its own small window, never during a session | Restart & update |
| rd agent (Windows, macOS) | at start and every 6 h | the portal's device card | the portal's **Update** button; rolls back if the new version does not come back |

## Versions and releases

rd Desktop and the rd agent are released separately, each through
[changesets](https://github.com/changesets/changesets):

| Package | Directory | Tag | Release assets |
|---|---|---|---|
| `rd-desktop` | `clients/desktop` | `desktop-vX.Y.Z` | Windows setup + portable exe, macOS .dmg, updater files |
| `rd-agent` | `crates/rd-agent` | `agent-vX.Y.Z` | Windows setup + zip, macOS zip (`RD Agent.app`), updater signatures |

Each has its own `CHANGELOG.md` and version. `tools/release/sync-versions.ts` copies the
version from the package's `package.json` into its `Cargo.toml` and the matching `Cargo.lock`.
Both continued from the shared 1.4.2, so no version ever goes down for anyone who installed one.

Merging the **Version packages** PR makes `.github/workflows/release.yml` tag every package
whose tag does not exist yet and start its workflow on the tag: `desktop.yml` for
`desktop-v*`, `agent.yml` for `agent-v*`. Each publishes its GitHub Release and then its
update manifest.

## Update manifests

GitHub has one "latest release" per repository, which cannot serve two release streams. The
manifests therefore live as assets of one fixed pre-release, tagged `updates`, that the release
workflows overwrite (it is never marked latest):

- `https://github.com/Thijmen/remoted/releases/download/updates/desktop.json`
- `https://github.com/Thijmen/remoted/releases/download/updates/agent.json`

`tools/release/manifest.ts` writes them from the package's `CHANGELOG.md` and the signatures.
The format is Tauri's static update JSON plus the release notes of every version:

```json
{
  "version": "1.6.0",
  "notes": "Cmd+` cycles the remote Mac's windows.",
  "pub_date": "2026-10-20T12:00:00Z",
  "platforms": {
    "windows-x86_64": { "url": "https://github.com/…/desktop-v1.6.0/rd-desktop-1.6.0-x64-setup.exe", "signature": "…" },
    "darwin-aarch64": { "url": "https://github.com/…/desktop-v1.6.0/rd-desktop-1.6.0-macos-universal.app.tar.gz", "signature": "…" },
    "darwin-x86_64": { "url": "…same universal archive…", "signature": "…" }
  },
  "releases": [
    { "version": "1.6.0", "notes": "- Cmd+` cycles the remote Mac's windows.", "url": "https://github.com/Thijmen/remoted/releases/tag/desktop-v1.6.0" },
    { "version": "1.5.0", "notes": "- …", "url": "…" }
  ]
}
```

- `platforms` keys are `<os>-<arch>` as Tauri names them (`windows-x86_64`, `darwin-aarch64`,
  `darwin-x86_64`). A universal macOS file is listed under both macOS keys.
- `signature`: the contents of the `.sig` file that `tauri signer sign` writes (base64 of a
  minisign signature).
- `releases`: newest first, at most 30. `notes` is the version's changelog section as Markdown,
  without the changeset headings (`### Minor Changes`) and commit hashes. Clients show every
  version newer than their own.
- Agent: `windows-x86_64` is `rd-agent-X.Y.Z-windows-x64-setup.exe`; the macOS keys point to
  `rd-agent-X.Y.Z-macos-universal.zip` (a zip of `RD Agent.app`, made with `ditto -c -k
  --keepParent`).

## Signing

**Update signatures.** Every update file is signed with one minisign key (Tauri's updater
format). The apps install only files signed by it. The public key is built into rd Desktop
(`plugins.updater.pubkey` in `tauri.conf.json`) and into rd-agent (`crates/rd-agent/src/update/`).
The private key and its password are the GitHub Actions secrets `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`; keep a backup outside GitHub. Without the key no
installed copy can be updated any more: they would have to be reinstalled by hand with a build
that carries a new public key.

**macOS code signing.** rd Desktop and RD Agent.app are signed with one self-signed
certificate, "rd Code Signing" (secrets `MACOS_CERTIFICATE`, the base64 of a .p12, and
`MACOS_CERTIFICATE_PASSWORD`). macOS ties the Accessibility and Screen Recording permissions to
the signature's designated requirement (bundle id + this certificate), so they survive updates.
An ad-hoc signature changes with every build and would lose them each time. The certificate
is not trusted by Gatekeeper, so the first install still needs System Settings → Privacy &
Security → **Open Anyway**; files the apps download themselves are not quarantined, so updates
do not.

Windows files are not code-signed. SmartScreen asks once at the first install; updates are
downloaded by the app or agent itself and run without the prompt.

## rd Desktop

**When.** 10 s after launch, then every 6 hours, unless automatic checks are off: the tray
menu's **Check for updates automatically**, or `"update_check": false` in `settings.json`. The
environment variable `RD_UPDATE_CHECK=0` turns them off regardless (for managed machines).
**Check for Updates…** always works.

**What happens.** A newer version is downloaded in the background and its signature checked.
Then the update window opens, unless a session is open in the main window (a `/m/…` page of the
gateway): then it waits until the session page is left, or for the next check. Closing the
window means "later": it opens again at the next check or launch, and the tray menu offers
**Restart to Update to X.Y.Z** in the meantime.

**The window** (`dist/update.html`, a bundled page in its own small window) shows the new
version, the installed one, the notes of every version in between (newest first), a link to the
GitHub release, and one button, **Restart & update**:

- macOS: the app is replaced in place and restarted.
- Windows: the app quits and the installer runs with a progress bar only (`passive`), then
  starts the new version.

**Can't install here.** The portable exe (no `uninstall.exe` next to it), and a macOS app that
cannot be replaced (run from the disk image, or translocated by Gatekeeper, or a folder the
user cannot write) get the same window, but the button is **Download** and opens the release
page.

**After an update** the main window shows a small notice for 20 s: "Updated to X.Y.Z ·
What's new". **What's new** opens the window with the notes.

**Menus.** A tray (macOS: menu bar) icon while the app runs: the version, **Check for
Updates…** (or **Restart to Update to X.Y.Z**), **Check for updates automatically**, **Show
rd**, **Quit**. Closing the window still quits the app. macOS: the app menu has **About rd
Desktop** and **Check for Updates…**. Windows: the window's title-bar menu (right-click the
title bar, or Alt+Space) has **About rd Desktop** and **Check for Updates…**.

**Testing.** `RD_UPDATE_URL=<manifest URL>` replaces the manifest URL (also plain `http://` to
localhost in debug builds).

## rd agent

**Checks.** 1 minute after start, then every 6 hours, the agent reads `agent.json`, and reports
what it found to the gateway (`update_status`, [protocol.md](protocol.md)). `[update] check =
false` in its `config.toml` turns this off; `RD_AGENT_UPDATE_URL` replaces the manifest URL.

**Portal.** A device with a newer version shows **Update to X.Y.Z** with **What's new** (the
notes since its version). While a session to that machine is open, the portal asks first: the
session ends and the viewer reconnects by itself when the agent is back (about 15 s). The
gateway sends the agent `update {version}` and logs `agent_update` in the audit log.

**Installing.** The agent ends its session, downloads the file, checks its signature, copies
the running version aside, and starts the **update helper**: `rd-agent update-helper` from
that copy, as its own process outside the agent's login service. The helper

1. installs: Windows runs `setup.exe /S` (the installer stops the agent, replaces it and starts
   it again); macOS swaps `RD Agent.app` and restarts the LaunchAgent;
2. waits up to 2 minutes for the new version to register with the gateway (the new agent
   writes a marker once the gateway accepted it);
3. if it does not, rolls back: puts the previous version back and restarts it;
4. records the outcome, which the agent reports on its next connection; the portal shows a
   failed or rolled-back update on the device card.

Only agents installed the normal way update themselves: on Windows by the installer
(`uninstall.exe` next to `rd-agent.exe`), on macOS `RD Agent.app` started by its LaunchAgent.
Others report `can_install: false`; the portal then says to install by hand.

`rd-agent update` does the same from a terminal (`--check` only reports).

## Existing installs

- rd Desktop 1.4.x and earlier have no updater: install 1.5.0 by hand once.
- Windows agents 1.4.x and earlier: run the 1.5.0 installer once.
- Mac agents built with `build-app.sh` are signed with a local certificate: switching to the
  released, CI-signed RD Agent.app needs Screen Recording and Accessibility granted once more.
  After that, updates keep them.
