# Mac agent setup

How to put `RD Agent.app` on the Mac and keep it running. Do this at the Mac itself: the
permission dialogs need a person to approve them.

## 1. Install

Download `rd-agent-<version>-macos-universal.zip` from the agent releases (`agent-v*`) on
[GitHub Releases](https://github.com/Thijmen/remoted/releases) (macOS 15 or later, Apple silicon
or Intel), unzip it and move **RD Agent.app** to `/Applications`. From then on it updates itself
from the portal ([updates.md](updates.md)).

The app is signed with the project's own certificate ("rd Code Signing"), not notarized by
Apple, so macOS blocks it the first time it runs ("Apple could not verify…"): run step 3's
`setup` command once, click **Done**, then System Settings → Privacy & Security → scroll down →
**Open Anyway** next to rd-agent, and run `setup` again. (Or clear the download mark before the
first run: `xattr -dr com.apple.quarantine "/Applications/RD Agent.app"`.)

Every release is signed with the same certificate. macOS ties the Screen Recording and
Accessibility permissions to it, so they survive updates. Building the agent yourself (below)
signs it with your own certificate instead: switching between your own builds and the releases
needs the permissions granted once more.

Older agents (built here with `build-app.sh` up to 1.4.2) do not update themselves: install the
release once as above (it replaces `/Applications/RD Agent.app`), run `setup` again (Enter keeps
the saved configuration) and grant both permissions again.

### Or build it yourself

One-time, a signing identity:

```sh
packaging/macos/make-signing-identity.sh
```

This creates a self-signed code-signing certificate, "RD Local Code Signing", in the login
keychain. Every build is signed with it, so the app keeps the same identity across rebuilds,
and macOS keeps its Screen Recording / Accessibility grants. Without it, `build-app.sh` falls
back to ad-hoc signing, and macOS forgets the grants after every rebuild.

Keychain items do *not* survive rebuilds this way: a self-signed certificate has no Team ID,
so macOS ties an item to the exact build and asks "allow access?" again after every rebuild.
That is why the agent token lives in the config file (step 3), not in the Keychain.

Then build and install:

```sh
packaging/macos/build-app.sh     # builds and signs packaging/macos/build/RD Agent.app (--universal: also Intel)
packaging/macos/install.sh       # copies it to /Applications, (re)loads the LaunchAgent
```

A self-built agent can still update itself to a release only if the release satisfies its
designated requirement, which a different certificate does not: the portal then says to
install by hand.

## 2. Add the Mac in the portal

Open your gateway's portal (for example `https://rd.example.com`), click **Add device** and give
the Mac a name. The portal shows a **setup code** (`rds1_…`) once: copy it for the next step.
It holds the gateway URL and the Mac's token, so treat it like a password. Every device gets its
own machine id and token; [devices.md](devices.md) explains how they work, how to rotate or
remove them, and how to provision a token without the portal.

## 3. Setup (in person)

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" setup
```

Setup asks for the setup code from step 2: paste it and press Enter. Without a code, press
Enter and it asks for the gateway URL (the agent host of your gateway, e.g.
`wss://rd-agent.example.com`), the machine id (`mbp`) and the token one by one (when the
config file already has a token for that machine, Enter keeps it). Then it:
- writes the config file with the token,
  `~/Library/Application Support/dev.thijmen.rd.agent/config.toml`. The file is mode 0600 and
  its directory 0700, so only your user can read it. Setup never prints the token;
- starts the app so that macOS shows its standard prompts for **Screen Recording** and
  **Accessibility**. Enable "RD Agent" in System Settings → Privacy & Security, then press
  Enter so setup checks again. The check runs as RD Agent.app, not as your terminal: macOS
  ties permissions to the app that is asking;
- installs the LaunchAgent (`~/Library/LaunchAgents/dev.thijmen.rd.agent.plist`). It runs the
  agent only in your logged-in GUI session (`LimitLoadToSessionType=Aqua`); launchd restarts it
  if it exits.

Restart the agent after granting permissions:
`launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent`.

**Older installs** (set up before the token moved to the config file) have it in the Keychain
(service `dev.thijmen.rd.agent`, account `gateway-token`). The agent still reads it there when
neither `RD_AGENT_TOKEN` nor the config file has one, but after a rebuild that read waits on the
"allow access" dialog at the Mac. Run `setup` once more (with a setup code, or the token), so
it moves into the config file; the Keychain item is then unused (`uninstall.sh --purge` removes
it).

## 4. Check

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" status
```

`status` shows the config, where the token comes from (the `RD_AGENT_TOKEN` environment
variable, the config file, or the Keychain of an older install; it warns when the config file is
readable by others), the app's permissions, the LaunchAgent state and whether the gateway
accepts the token. It never prints the token. The agent logs the same source at start-up
(`token_from=…`). Logs:
`~/Library/Logs/rd-agent/` (daily files), plus the unified log:
`log stream --predicate 'subsystem == "dev.thijmen.rd.agent"'`.

## File transfer

The agent serves the file browser ([files.md](files.md)) as your user: it sees what you can
see. macOS still asks before an app reads the Desktop, Documents and Downloads folders,
removable volumes and network shares: the first time the file browser opens one of them, the
Mac shows *"RD Agent" would like to access files in your … folder*. Until someone clicks
**Allow** (you can, from a screen session) that folder answers "access denied". To skip these
prompts, turn on **RD Agent** in System Settings → Privacy & Security → **Full Disk Access**
(then restart the agent: `launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent`). Deleted
files go to the Trash.

## Updates

RD Agent.app from a release updates itself from the portal: the Mac's card shows **Update
available: X.Y.Z**, **What's new** and **Update**. The agent downloads the new app, checks its
signature and that it is signed with the same certificate (so Screen Recording and
Accessibility carry over), ends an open session (the viewer reconnects by itself) and hands
over to an update helper. The helper swaps `/Applications/RD Agent.app` (the old one waits next
to it as `RD Agent.app.previous-X.Y.Z`), restarts the LaunchAgent and waits up to 2 minutes for
the new version to connect with the permissions the old one had. If it does not, the helper
puts the old app back. The card then shows what happened. Details: [updates.md](updates.md).

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" update --check   # is there a newer version? also shows the last update's outcome
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" update           # install it now and wait for the outcome
```

The helper logs to `~/Library/Logs/rd-agent/update-helper.log`. The agent checks for new versions
1 minute after it starts and then every 6 hours; `[update]` `check = false` in `config.toml`
turns that off (the portal then offers nothing, but `rd-agent update` still works).

- If an update fails with a hint about **App Management**, macOS did not let the helper replace
  the app in `/Applications`: allow RD Agent under System Settings → Privacy & Security → App
  Management and click **Update** again.
- An agent you built yourself is signed with your own certificate: the portal still offers the
  update, but the agent refuses it ("signed with a different certificate") and stays as it is.
  Install the release by hand once (step 1) to switch.

## Local development (no cluster, no permissions)

```sh
(cd viewer && bun install && bun run build)
cargo run --release -p rd-gateway -- --dev --assets-dir viewer/dist
RD_AGENT_TOKEN=rd1.mbp.dev cargo run --release -p rd-agent -- run \
  --gateway ws://127.0.0.1:8081 --machine mbp --source synthetic
open http://127.0.0.1:8080/
```

`--source synthetic` sends a moving test pattern through the real VideoToolbox encoder. It
needs no Screen Recording permission. For real capture, run the signed app from `/Applications`
(or `open -n "packaging/macos/build/RD Agent.app" --args run --config <dev config>`), so the
permission belongs to the app.

## Uninstall

```sh
packaging/macos/uninstall.sh            # unload + remove the LaunchAgent and the app
packaging/macos/uninstall.sh --purge    # also logs, the config file (with the token) and the privacy grants
```

Without the repository: `"/Applications/RD Agent.app/Contents/MacOS/rd-agent" uninstall` stops
the agent and removes the LaunchAgent; then move RD Agent.app to the Bin.

See [recovery.md](recovery.md) for the locked-screen / reboot path.
