# Mac agent setup

How to put `RD Agent.app` on the Mac and keep it running. Do this at the Mac itself: the
permission dialogs need a person to approve them.

## 1. One-time: signing identity

```sh
packaging/macos/make-signing-identity.sh
```

This creates a self-signed code-signing certificate, "RD Local Code Signing", in the login
keychain. Every build is signed with it, so the app keeps the same identity across rebuilds,
and macOS keeps its Screen Recording / Accessibility grants and Keychain access. Without it,
`build-app.sh` falls back to ad-hoc signing, and macOS forgets the grants after every rebuild.

## 2. Build and install

```sh
packaging/macos/build-app.sh     # builds and signs packaging/macos/build/RD Agent.app
packaging/macos/install.sh       # copies it to /Applications, (re)loads the LaunchAgent
```

The LaunchAgent (`~/Library/LaunchAgents/dev.thijmen.rd.agent.plist`) runs the agent only
in your logged-in GUI session (`LimitLoadToSessionType=Aqua`). launchd restarts it if it
exits.

## 3. Token

On any machine:

```sh
tools/new-agent-token.sh mbp
```

- Put the **sha256** in the 1Password item `rd-agent-tokens` (vault `k3s-vault`), in the
  field `mbp`. The gateway reads it as `/etc/rd/agents/mbp`.
- Only the agent gets the **token** (next step). It never goes into the cluster or git.

## 4. Setup (in person)

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" setup
```

Setup asks for the gateway URL (default `wss://rd-agent.example.com`), the machine id
(`mbp`) and the token, then:
- stores the token in the Keychain and writes the config file
  (`~/Library/Application Support/dev.thijmen.rd.agent/config.toml`);
- starts the app so that macOS shows its standard prompts for **Screen Recording** and
  **Accessibility**. Enable "RD Agent" in System Settings → Privacy & Security, then press
  Enter so setup checks again. The check runs as RD Agent.app, not as your terminal: macOS
  ties permissions to the app that is asking;
- installs the LaunchAgent.

Restart the agent after granting permissions:
`launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent`.

## 5. Check

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" status
```

`status` shows the config, the token (present or missing), the app's permissions, the
LaunchAgent state and whether the gateway accepts the token. Logs:
`~/Library/Logs/rd-agent/` (daily files), plus the unified log:
`log stream --predicate 'subsystem == "dev.thijmen.rd.agent"'`.

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
packaging/macos/uninstall.sh --purge    # also logs, the Keychain token and the privacy grants
```

See [recovery.md](recovery.md) for the locked-screen / reboot path.
