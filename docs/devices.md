# Adding devices

A *device* (or *machine*) is a Mac that runs `rd-agent` and shows up in the portal. This page
explains how the gateway recognises a device, and walks through adding, renaming, rotating
and removing one.

## How it works

Every device has a **machine id**, for example `mbp` or `studio`. The id is 1–64 characters
of `[A-Za-z0-9_-]` (no dots, no slashes). It appears in the portal URL (`/m/<machine>`), in
the token, and as a file name on the gateway, so pick something short and stable.

Each device has its own **token**:

```
rd1.<machine>.<secret>
```

The agent sends it as `Authorization: Bearer …` when it dials out. The gateway never stores
the token itself, only its SHA-256, in one file per machine:

```
/etc/rd/agents/<machine>      # hex SHA-256 of the whole token (RD_AGENT_TOKENS_DIR)
```

When an agent connects, the gateway:

1. reads the machine id from the token,
2. hashes the token and compares it with `/etc/rd/agents/<machine>`,
3. checks that the agent's `register` message names the same machine (otherwise close 4002).

The token file is read again on every connection attempt, so adding or rotating a device
**needs no gateway restart**.

A device appears in the portal in one of two ways:

| | Shown while offline | Friendly name | Recovery links | Needs restart |
|---|---|---|---|---|
| Listed in `RD_MACHINES_FILE` | ✅ always | ✅ | ✅ | yes, the file is read at start-up |
| Not listed | only after it has connected once since the gateway started | no, the id is shown | no | no |

The machines file is optional, but recommended for devices you use regularly.

## Add a device

### 1. Choose a machine id

For example `studio`. It must be unique on the gateway.

### 2. Create the token

On any machine with a checkout of this repo:

```sh
tools/new-agent-token.sh studio
```

It prints two values:

- the **token**: only for the agent, in step 4. Don't store it in the cluster or in git.
- the **sha256**: for the gateway, in step 3.

Without the script:

```sh
TOKEN="rd1.studio.$(openssl rand -hex 32)"
printf '%s' "$TOKEN" | shasum -a 256 | cut -d' ' -f1
```

### 3. Give the gateway the hash

Put the hash in a file named after the machine in `RD_AGENT_TOKENS_DIR`
(default `/etc/rd/agents`).

- **Kubernetes (our setup):** add a field `studio` with the hash to the 1Password item
  `rd-agent-tokens` (vault `k3s-vault`). The ExternalSecret mounts every field as
  `/etc/rd/agents/<field>`. Wait for it to sync (or force a refresh of the ExternalSecret).
- **Elsewhere:** write the file directly, for example
  `printf '%s\n' <sha256> > /etc/rd/agents/studio`.

Optionally add the device to `RD_MACHINES_FILE`, so it has a name and stays visible while
offline:

```json
[
  { "machine": "mbp",    "name": "MacBook Pro" },
  { "machine": "studio", "name": "Mac Studio", "recovery": [
      { "label": "Screen Sharing", "url": "https://guac.example.com/#/client/…" }
  ] }
]
```

Restart the gateway after changing this file. Startup fails on an invalid or duplicate id.

### 4. Install the agent on the device

At the Mac itself, follow [setup-mac.md](setup-mac.md) (signing identity, build, install). When
`rd-agent setup` asks:

- **Gateway URL**: the agent host, for example `wss://rd-agent.example.com`
  (not the portal URL).
- **Machine id**: `studio`. The default is `mbp`, so change it on every other device.
- **Token**: the token from step 2.

Setup can also take the first two as flags:

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" setup \
  --gateway wss://rd-agent.example.com --machine studio
```

### 5. Check

```sh
"/Applications/RD Agent.app/Contents/MacOS/rd-agent" status
```

`status` reports whether the gateway accepts the token. The device should now be online in the
portal, and the gateway logs `agent registered` with `machine=studio`.

## Rotate a token

1. `tools/new-agent-token.sh studio` for a new token and hash.
2. Replace the hash on the gateway (the 1Password field, or the file). The old token stops
   working on its next connection attempt.
3. Run `rd-agent setup` on the device again and enter the new token.

A control socket that is already connected stays up until it reconnects, so restart the agent
(`launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent`) to apply the change right away.

## Rename a device

The machine id is part of the token, so a rename is a new device: create a token for the new id,
add its hash (and machines-file entry), run `setup` with the new id, then remove the old one.

## Remove a device

1. Delete `/etc/rd/agents/<machine>` (or the 1Password field). The agent can no longer connect.
2. Remove the entry from `RD_MACHINES_FILE`, if there is one, and restart the gateway. Without
   a restart, a device that connected earlier stays in the portal list as offline.
3. On the Mac: `packaging/macos/uninstall.sh --purge`.

## Troubleshooting

Failed agent connections are logged as `agent auth failed` with a `reason`:

| Reason / close code | Cause | Fix |
|---|---|---|
| `unknown machine` | no `/etc/rd/agents/<machine>` file | check step 3; has the secret synced? |
| `token mismatch` | the hash does not match the token | regenerate, or re-enter the token in `setup` |
| `malformed token` | the token is not `rd1.<machine>.<secret>` | re-enter it in `setup` |
| `corrupt token file` | the file is not a 64-character hex hash | write only the hash into the file |
| close **4002** | `setup` machine id differs from the id in the token | run `setup` with the matching id |
| HTTP **429** | more than 5 failed attempts per minute from one IP | fix the cause, then wait about a minute |

Only one agent per machine id can be online. A second agent with the same id replaces the
first, which is closed with **4009**, so two devices that keep replacing each other means they
share an id.

In `--dev` mode the gateway skips the token check, so any `RD_AGENT_TOKEN=rd1.<machine>.dev`
works. See [setup-mac.md](setup-mac.md#local-development-no-cluster-no-permissions).
