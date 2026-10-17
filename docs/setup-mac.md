# Mac agent setup

How to put `RD Agent.app` on the Mac and keep it running. Do this at the Mac itself: the
permission dialogs need a person to approve them.

## 1. One-time: signing identity

```sh
packaging/macos/make-signing-identity.sh
```

This creates a self-signed code-signing certificate, "RD Local Code Signing", in the login
keychain. Every build is signed with it, so the app keeps the same identity across rebuilds,
and macOS keeps its Screen Recording / Accessibility grants. Without it, `build-app.sh` falls
back to ad-hoc signing, and macOS forgets the grants after every rebuild.

Keychain items do *not* survive rebuilds this way: a self-signed certificate has no Team ID,
so macOS ties an item to the exact build and asks "allow access?" again after every rebuild.
That is why the agent token lives in the config file (step 4), not in the Keychain.

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
(`mbp`) and the token (when the config file already has a token for that machine, Enter keeps
it), then:
- writes the config file with the token,
  `~/Library/Application Support/dev.thijmen.rd.agent/config.toml`. The file is mode 0600 and
  its directory 0700, so only your user can read it. Setup never prints the token;
- starts the app so that macOS shows its standard prompts for **Screen Recording** and
  **Accessibility**. Enable "RD Agent" in System Settings → Privacy & Security, then press
  Enter so setup checks again. The check runs as RD Agent.app, not as your terminal: macOS
  ties permissions to the app that is asking;
- installs the LaunchAgent.

Restart the agent after granting permissions:
`launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent`.

**Older installs** (set up before the token moved to the config file) have it in the Keychain
(service `dev.thijmen.rd.agent`, account `gateway-token`). The agent still reads it there when
neither `RD_AGENT_TOKEN` nor the config file has one, but after a rebuild that read waits on the
"allow access" dialog at the Mac. Run `setup` once more and enter the token, so it moves into
the config file; the Keychain item is then unused (`uninstall.sh --purge` removes it).

## 5. Check

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

See [recovery.md](recovery.md) for the locked-screen / reboot path.
