# Adding devices

A *device* (or *machine*) is a Mac or Windows PC that runs `rd-agent` and shows up in the
portal. This page explains how the gateway recognises a device, and walks through adding,
renaming, rotating and removing one.

## How it works

Every device has a **machine id**, for example `mbp` or `studio`. The id is 1–64 characters
of `[A-Za-z0-9_-]` (no dots, no slashes). It appears in the portal URL (`/m/<machine>`) and in
the token, so pick something short and stable.

Each device has its own **token**:

```
rd1.<machine>.<secret>
```

The agent sends it as `Authorization: Bearer …` when it dials out. The gateway never stores
the token itself, only its SHA-256, in one of two places:

- **Added in the portal** (*Add device*, the usual way): the gateway creates the token, keeps
  its hash in `RD_STATE_DIR/devices.json` and shows the token once, inside a **setup code**
  that you paste into `rd-agent setup` on the device.
- **Provisioned**: you create the token yourself and put its hash in a file named after the
  machine in `RD_AGENT_TOKENS_DIR` (default `/etc/rd/agents`), for example from a Kubernetes
  secret. See [Provisioned tokens](#provisioned-tokens).

A machine id belongs to one of the two: the portal refuses to add an id that has a token file.

When an agent connects, the gateway:

1. reads the machine id from the token,
2. hashes the token and compares it with the device's hash (`devices.json`, else the token
   file),
3. checks that the agent's `register` message names the same machine (otherwise close 4002).

Both are read again on every connection attempt, so adding, rotating or removing a device
**needs no gateway restart**.

A device appears in the portal in one of these ways:

| | Shown while offline | Friendly name | Recovery links | Needs restart |
|---|---|---|---|---|
| Added in the portal | ✅ always | ✅ the name you gave it | no | no |
| Listed in `RD_MACHINES_FILE` | ✅ always | ✅ | ✅ | yes, the file is read at start-up |
| Neither (a provisioned token only) | only after it has connected once since the gateway started | no, the id is shown | no | no |

A device added in the portal can also be listed in the machines file, for its recovery links;
the file's name then wins.

## Add a device

### 1. Add it in the portal

Open the portal (for example `https://rd.example.com`) and click **Add device**. Give the
device a name, for example *Mac Studio*. The machine id follows the name (`mac-studio`); change
it before you click **Add** if you want a shorter one, because it cannot change later.

The portal then shows the device's **setup code**, a single line starting with `rds1_`. It holds
the gateway URL agents connect to and the device's token. It is shown **only once** and works
like a password: copy it and keep it until step 3, but don't store it anywhere else. Lost it?
Click **New token** on the device's card for a new one.

*Add device* is missing when the gateway has no `RD_STATE_DIR` or no `RD_AGENT_PUBLIC_URL`
(see [gateway.md](gateway.md#devices)).

### 2. Install the agent on the device

At the device itself, follow [setup-mac.md](setup-mac.md) (signing identity, build, install) or
[setup-windows.md](setup-windows.md) (the installer).

### 3. Run setup with the code

`rd-agent setup` starts by asking for the setup code: paste it and press Enter. Setup reads the
gateway URL, machine id and token from it, writes the config file and installs the login
service. On Windows the installer's last page runs setup for you; the *rd agent setup* shortcut
runs it again later.

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" setup     # Mac
```

```powershell
& "$env:LOCALAPPDATA\Programs\rd-agent\rd-agent.exe" setup     # Windows
```

Without a code (press Enter at the prompt, or pass `--gateway` / `--machine`), setup asks for
the **gateway URL** (the agent host, for example `wss://rd-agent.example.com`, not the portal
URL), the **machine id** and the **token** one by one. The portal shows these three under *Set
up without the code*.

### 4. Check

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" status
```

(`rd-agent.exe status` on Windows.) `status` reports whether the gateway accepts the token.
The device should now be online in the portal, and the gateway logs `agent registered` with
`machine=mac-studio`.

## Rotate a token

Click **New token** on the device's card. The old token stops working at once: a connected agent
is disconnected (close 4002) and stays offline. Run `rd-agent setup` on the device again with
the new setup code.

## Rename a device

The machine id is part of the token, so a rename is a new device: add the device again with the
new id, run `setup` with its code, then remove the old one.

## Remove a device

1. Click **Remove** on the device's card. Its token stops working, a connected agent is
   disconnected and the device leaves the portal.
2. On the device: `packaging/macos/uninstall.sh --purge` on a Mac; on Windows uninstall *rd
   agent* under Settings → Apps ([setup-windows.md](setup-windows.md#uninstall)).

The portal logs every change in the audit log (`target: audit`) as `device_add`,
`device_token` or `device_remove`, with the user, IP and machine id.

## Provisioned tokens

For devices whose token you manage outside the gateway, for example in a password manager that
syncs into a Kubernetes secret. The portal shows these devices without *New token* and *Remove*.

1. Choose a machine id, for example `studio`. It must be unique on the gateway.
2. Create the token, on any machine with a checkout of this repo:

   ```sh
   tools/new-agent-token.sh studio
   ```

   It prints the **token** (only for the agent, in step 4; don't store it in the cluster or in
   git) and its **sha256** (for the gateway, in step 3). Without the script:

   ```sh
   TOKEN="rd1.studio.$(openssl rand -hex 32)"
   printf '%s' "$TOKEN" | shasum -a 256 | cut -d' ' -f1
   ```

3. Put the hash in a file named after the machine in `RD_AGENT_TOKENS_DIR`:
   - **Kubernetes (our setup):** add a field `studio` with the hash to the 1Password item
     `rd-agent-tokens` (vault `k3s-vault`). The ExternalSecret mounts every field as
     `/etc/rd/agents/<field>`. Wait for it to sync (or force a refresh of the ExternalSecret).
   - **Elsewhere:** write the file directly, for example
     `printf '%s\n' <sha256> > /etc/rd/agents/studio`.

   Optionally add the device to `RD_MACHINES_FILE`, so it has a name and stays visible while
   offline (restart the gateway after changing the file; start-up fails on an invalid or
   duplicate id):

   ```json
   [
     { "machine": "mbp",    "name": "MacBook Pro" },
     { "machine": "studio", "name": "Mac Studio", "recovery": [
         { "label": "Screen Sharing", "url": "https://guac.example.com/#/client/…" }
     ] }
   ]
   ```

4. Run `rd-agent setup` on the device, press Enter at the setup code prompt and enter the
   gateway URL, `studio` and the token. Setup can also take the first two as flags:

   ```sh
   "/Applications/RD Agent.app/Contents/MacOS/rd-agent" setup \
     --gateway wss://rd-agent.example.com --machine studio
   ```

**Rotate:** create a new token and hash, replace the hash, and run `setup` again with the new
token. The old token stops working on its next connection attempt; a control socket that is
already connected stays up until it reconnects, so restart the agent to apply the change right
away (`launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent` on a Mac,
`rd-agent.exe install` on Windows).

**Remove:** delete `/etc/rd/agents/<machine>` (or the 1Password field), and the entry in
`RD_MACHINES_FILE` if there is one, then restart the gateway. Without a restart, a device that
connected earlier stays in the portal list as offline.

## Troubleshooting

Failed agent connections are logged as `agent auth failed` with a `reason`:

| Reason / close code | Cause | Fix |
|---|---|---|
| `unknown machine` | no device and no `/etc/rd/agents/<machine>` file | add the device; for a provisioned token, has the secret synced? |
| `token mismatch` | the token is not the device's current one (replaced in the portal, or a wrong hash) | run `setup` with the current setup code or token |
| `malformed token` | the token is not `rd1.<machine>.<secret>` | run `setup` again |
| `corrupt token file` | the file is not a 64-character hex hash | write only the hash into the file |
| close **4002** | the token was replaced or the device removed in the portal, or the `setup` machine id differs from the id in the token | run `setup` with a new setup code |
| HTTP **429** | more than 5 failed attempts per minute from one IP | fix the cause, then wait about a minute |

Only one agent per machine id can be online. A second agent with the same id replaces the
first, which is closed with **4009**, so two devices that keep replacing each other means they
share an id.

In `--dev` mode the gateway skips the token check, so any `RD_AGENT_TOKEN=rd1.<machine>.dev`
works, and *Add device* keeps devices in memory unless `--state-dir` is set. See
[setup-mac.md](setup-mac.md#local-development-no-cluster-no-permissions) or
[setup-windows.md](setup-windows.md#local-development-no-cluster).
