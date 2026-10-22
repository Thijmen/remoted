# rd-gateway — operator notes

The gateway joins a browser viewer's WebSocket to the agent's per-session WebSocket. It also
serves the portal, the viewer build and the network probe. The component contract lives in
[`protocol.md`](protocol.md). This page covers how to run and operate the gateway.

## Listeners

| Port | Default bind | Who talks to it | Routes |
|---|---|---|---|
| viewer | `0.0.0.0:8080` | browsers, via Traefik + Authentik | pages, `/assets/*`, `/api/*`, `/ws/view/{machine}`, `/ws/probe` |
| agent | `0.0.0.0:8081` | rd-agent, via Traefik (no Authentik) | `/ws/agent`, `/ws/agent/session/{session}`; everything else is 404 |
| metrics | `0.0.0.0:9090` | kubelet / Prometheus | `/healthz` → `ok`, `/metrics` (Prometheus text) |

Every accepted socket gets `TCP_NODELAY`. With `--dev` the default binds are `127.0.0.1`.

## Configuration

Every flag can also be set through the environment variable next to it.

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--dev` | – | off | Turns off **all auth** (user is `dev@localhost`), binds loopback by default, uses human-readable logs and honours `RD_CHAOS`. Binding a non-loopback address in dev mode logs a warning. |
| `--viewer-addr` | `RD_VIEWER_ADDR` | `0.0.0.0:8080` | |
| `--agent-addr` | `RD_AGENT_ADDR` | `0.0.0.0:8081` | |
| `--metrics-addr` | `RD_METRICS_ADDR` | `0.0.0.0:9090` | |
| `--public-origin` | `RD_PUBLIC_ORIGIN` | required unless `--dev` | e.g. `https://rd.example.com` (case-insensitive, trailing `/` ignored) |
| `--allowed-emails` | `RD_ALLOWED_EMAILS` | required unless `--dev` | comma separated, case-insensitive |
| `--allowed-uids` | `RD_ALLOWED_UIDS` | optional | comma separated `X-authentik-uid` values; when set, a viewer must match both lists (the uid is immutable, the email is not) |
| `--state-dir` | `RD_STATE_DIR` | – | writable directory for `devices.json`, the devices added in the portal (see [Devices](#devices)); without it *Add device* is off (in dev mode: devices are kept in memory) |
| `--agent-public-url` | `RD_AGENT_PUBLIC_URL` | – (dev: `ws://<agent listener>`) | where agents connect, e.g. `wss://rd-agent.example.com`; goes into setup codes, so *Add device* needs it |
| `--agent-tokens-dir` | `RD_AGENT_TOKENS_DIR` | `/etc/rd/agents` | provisioned tokens: one file per machine holding the hex SHA-256 of that agent's token; re-read on every auth, so rotation needs no restart |
| `--machines-file` | `RD_MACHINES_FILE` | – | `[{"machine":"mbp","name":"MacBook Pro","recovery":[{"label":"…","url":"…"}]}]`; these machines are listed even while offline. Machine ids are `[A-Za-z0-9_-]{1,64}`. |
| `--session-idle-timeout` | `RD_SESSION_IDLE_TIMEOUT` | `3600` | seconds without viewer input → close 4008 |
| `--chaos` | `RD_CHAOS` | – | dev only, e.g. `bw_kbps=5000,delay_ms=50` (see [Failure injection](#failure-injection)) |
| `--assets-dir` | `RD_ASSETS_DIR` | embedded | serve the viewer build from this directory instead of the copy embedded at compile time |
| – | `RUST_LOG` | `info` | tracing filter, e.g. `rd_gateway=debug` |

Startup fails if the production settings are missing, `RD_MACHINES_FILE`, `RD_AGENT_PUBLIC_URL`
or `devices.json` is invalid, `RD_STATE_DIR` cannot be created, or a listener cannot bind.

## Running in development

```sh
# 1. viewer build (re-run, or `bunx vite build --watch`, after viewer changes)
cd viewer && bun install && bun run build && cd ..
# 2. gateway: no auth, loopback only, assets read from disk
cargo run -p rd-gateway -- --dev --assets-dir viewer/dist
# portal: http://127.0.0.1:8080   agents connect to ws://127.0.0.1:8081
```

In dev mode the agent can send any bearer token. The machine id comes from the token when the
token has the form `rd1.<machine>.…`, and from `register` otherwise. *Add device* works too: its
setup codes point at the local agent listener, and its devices are kept in memory unless
`--state-dir` is given. The Vite dev server
(`bun run dev`, port 5173) proxies `/api` and `/ws` to `127.0.0.1:8080`.

Failure injection, for example a 5 Mbps bottleneck with 50 ms extra delay:

```sh
RD_CHAOS=bw_kbps=5000,delay_ms=50 cargo run -p rd-gateway -- --dev --assets-dir viewer/dist
```

## Authentication

**Agents** send `Authorization: Bearer rd1.<machine>.<secret>`. The gateway hashes the whole
token with SHA-256 and compares it in constant time with the device's hash in
`{RD_STATE_DIR}/devices.json`, or for a provisioned token with `{RD_AGENT_TOKENS_DIR}/<machine>`. The
`register.machine` must equal the machine in the token, or the control socket is closed with
4002. The data socket (`/ws/agent/session/{id}`) needs the same token. It must also match a
pending session of that machine that is no more than 10 s old, otherwise the gateway answers 404.

To provision a token and the file the gateway stores (the portal's *Add device* does this for
you):

```sh
TOKEN="rd1.mbp.$(openssl rand -hex 32)"           # goes into the agent's config (rd-agent setup)
printf '%s' "$TOKEN" | shasum -a 256 | cut -d' ' -f1 > mbp   # goes into the tokens secret
```

Failed agent auths are limited to **5 per minute per client IP**. The client IP is the
rightmost `X-Forwarded-For` entry (the one Traefik appended), or the TCP peer. Once an IP is
over the limit, every request from it gets `429` with `Retry-After`, even one with a valid
token, until the budget refills (one attempt every 12 s). Every `401`/`429` is logged as
`agent auth failed` / `agent auth rejected: rate limited`, with `ip`, `path` and `reason`.

**Viewers** need `X-authentik-email` to be in `RD_ALLOWED_EMAILS` on every viewer-port
request. WebSocket upgrades (`/ws/*`) also need `Origin` to equal `RD_PUBLIC_ORIGIN`. State-changing
requests (POST, DELETE) that carry a foreign `Origin` are refused as well. A refused request gets
`403` and a `viewer forbidden` log line with `ip`, `path` and `reason`.

Every response on the viewer port carries the CSP, COOP (`same-origin`), COEP
(`require-corp`), `nosniff` and `no-referrer` headers from `protocol.md`.

## Viewer port routes

| Route | Response |
|---|---|
| `/`, `/m/{machine}`, `/t/{machine}`, `/probe`, `/latency-target`, `/keytest` | `index.html`, `viewer.html`, `terminal.html`, `probe.html`, `latency.html`, `keytest.html` with `Cache-Control: no-cache` (and an `ETag` when embedded). A page missing from the build gets a 503 placeholder that explains how to build the viewer. |
| `/assets/*` | hashed Vite output, `Cache-Control: public, max-age=31536000, immutable` |
| other `GET /<file>` | other top-level files of the build (favicon, …), `no-cache`; else 404 |
| `GET /api/me` | `{"email":"…","uid":"…","dev":false,"devices":true}` (`uid` may be null) (uid = `X-authentik-uid`, for `RD_ALLOWED_UIDS`; `devices`: whether *Add device* is on) |
| `GET /api/machines` | see below |
| `POST /api/devices`, `POST /api/devices/{machine}/token`, `DELETE /api/devices/{machine}` | add a device, replace its token, remove it; see [Devices](#devices) |
| `POST /api/machines/{machine}/update` | tell the agent to install its update; see [Agent updates](#agent-updates) |
| `GET /ws/view/{machine}` | viewer session |
| `GET /ws/files/{machine}` | file channel; see [File channels](#file-channels) |
| `GET /ws/terminal/{machine}` | terminal channel; see [Terminal channels](#terminal-channels) |
| `GET /ws/probe`, `GET /api/probe/down`, `POST /api/probe/up`, `POST /api/probe/report` | network probe (`protocol.md`); the report is logged as one `probe_report {…}` line (≤ 1 MiB, else 413) |

`/api/machines` lists the machines from `RD_MACHINES_FILE` in file order, then any other machine
that has registered since start, sorted by id. Fields that are not known yet are `null`:
`since_ms` (online since / offline since, Unix ms), `os` and `agent_version` (from the last
registration, kept while offline), and `state` (last reported `AgentState`). `viewer` is
`{"user","since_ms"}` while a session is pending or active, else `null`. `recovery` comes from
the machines file (`[]` otherwise). `name` falls back to the device's name, then the machine id.
`update` is the agent's last `update_status` (fields in [protocol.md](protocol.md#updates-rd-agent-150-and-later),
empty ones left out), kept while offline and dropped when the agent registers again until it
reports anew; `null` for agents that never send one (before 1.5.0).
`files` is true when the agent listed `files` in `register.caps` (it can open file channels),
`terminal` when it listed `terminal` (it offers a terminal, [terminal.md](terminal.md)).
Devices added in the portal follow the machines file, in the order they were added, and have
`"managed": true`.

## Devices

The portal's *Add device* adds agent machines without touching the cluster: the gateway creates
each device's token, keeps only its SHA-256 and hands the token out once, inside a setup code.
It needs a writable `RD_STATE_DIR` (a persistent volume in Kubernetes) and
`RD_AGENT_PUBLIC_URL`; without them `/api/me` reports `"devices": false` and the routes answer
503. [devices.md](devices.md) is the user guide.

`{RD_STATE_DIR}/devices.json` holds `{"version":1,"devices":[…]}`, each device with `machine`,
`name`, `token_sha256`, `created_ms`, `created_by` and `token_ms`. Every change is written to a
temporary file that is renamed over it; a change that cannot be saved is undone (500).

| Route | Body | Answer |
|---|---|---|
| `POST /api/devices` | `{"machine":"studio","name":"Mac Studio"}` (`name` optional, ≤ 64 characters) | **201** `{"machine","name","gateway","token","setup_code"}` |
| `POST /api/devices/{machine}/token` | `{}` | **200**, same fields; the old token stops working |
| `DELETE /api/devices/{machine}` | – | **204** |

Bodies must be JSON (`content-type: application/json`, ≤ 4 KiB), which a cross-site page
cannot send without a CORS preflight; together with the `Origin` check this keeps other sites
from adding devices. Answers that carry a token are `Cache-Control: no-store`. Errors are
`{"error":"…"}` with **400** (invalid machine id or name), **404** (no such device), **409** (the
id exists, or has a provisioned token in `RD_AGENT_TOKENS_DIR`), **503** (turned off) or **500**
(the store could not be saved).

Replacing a token or removing a device disconnects a connected agent: its control socket is
closed with **4002** (`token replaced` / `device removed`), and a running session ends with
4003 for the viewer. Every change is an audit line (`target: audit`): `device_add`,
`device_token` or `device_remove`, with `user`, `ip` and `machine`.

## Agent updates

Agents 1.5.0 and later check for updates themselves and report what they found with
`update_status` ([updates.md](updates.md), [protocol.md](protocol.md#updates-rd-agent-150-and-later)).
The portal shows it from `/api/machines`; its **Update** button calls:

| Route | Body | Answer |
|---|---|---|
| `POST /api/machines/{machine}/update` | `{"version":"1.6.0"}` | **202** `{"ok":true}`: `{"t":"update","version":"1.6.0"}` was queued to the agent's control socket |

The body must be JSON (≤ 1 KiB), like the device routes, and the same `Origin` check applies.
The gateway checks the agent's last `update_status` first and answers `{"error":"…"}` with
**404** for an unknown machine, or **409** when the agent is offline, never reported an update
status (older agent), reports `can_install: false` (the error is its `cannot_install` text), has
no update available or another version than the one asked for, or is already `downloading` or
`installing`. The agent reports progress and the outcome with `update_status`; the 202 only means
the request was passed on.

Every request that is passed on is an audit line (`target: audit`): `agent_update` with `user`,
`ip`, `machine`, `from` (the agent's version) and `to`, and counts in
`rd_agent_updates_requested_total`. Refused requests log `agent update refused` with the reason.
From the agent's reports, the gateway logs `agent update phase` (info) when the phase changes and
`agent update outcome` (info, with `from`, `to`, `result`, `error`) once per new outcome; every
report is logged at debug as `agent update status`.

## Sessions

1. The viewer opens `/ws/view/{machine}`. If no agent control socket is registered for that
   machine, the viewer is closed with **4003**.
2. If another viewer holds the machine, that viewer gets **4001**. The agent then receives
   `session_end{reason:"taken_over"}` for the old session, **before** the new
   `session_request`.
3. The gateway sends `session_request{session (32 hex), user, client_ip}` and waits up to
   10 s for `/ws/agent/session/{session}`. If the agent doesn't dial in time, the viewer gets
   **4004**. If the agent's control socket goes away first, the viewer gets **4003**. A message
   the viewer sends before the join is held (at most one; the viewer is not read further) and
   forwarded first.
4. After the join the viewer receives `{"t":"session","machine","session","agent":{AgentState}}`.
   From then on, messages are copied unchanged in both directions.

How a session ends:

| Cause | Viewer close | Agent data socket | `session_end` to agent (reason) |
|---|---|---|---|
| viewer closes / its socket fails | – | 1000 | `viewer_closed` |
| viewer stops answering pings (45 s) | 1001 | 1000 | `viewer_unresponsive` |
| agent closes its data socket | **4006**, or the agent's own 4000–4999 code (e.g. 4005) | – | `agent_closed` |
| agent ends the session for an update (data socket closed with 4010, `session_end` reason `agent_update` on control) | **4010**, whichever arrives first | – or 1000 | `agent_update`, or not sent |
| agent data socket stops answering pings | **4006** | 1001 | `agent_unresponsive` |
| agent sends `session_end` on control | **4006** | 1000 | not sent (the agent already knows) |
| another viewer takes over | **4001** | 1000 | `taken_over` (sent by the take-over) |
| no input for `RD_SESSION_IDLE_TIMEOUT` | **4008** | 1000 | `idle` |
| agent did not dial within 10 s | **4004** | – | `agent_timeout` |
| agent offline (before or while pending) | **4003** | – | – |
| gateway shutdown | 1012 | 1012 | `gateway_shutdown` (best effort) |

Only viewer→agent binary frames whose first byte is an input type (`0x10..=0x15`) count as
input for the idle timeout. ACKs, pings and control JSON do not count.

A newer registration for the same machine replaces the old control socket, which is closed with
**4009**. Sessions already joined keep running on their own data sockets. Sessions still pending
end with 4003.

The audit log (`target: audit`) has a `session_start` line (user, ip, machine, session) when the
request goes out. It has a `session_end` line on every outcome, with the same fields plus
`duration_ms`, `bytes_up`, `bytes_down` and `reason`.

## File channels

A file channel ([files.md](files.md)) is joined like a session: the viewer opens
`/ws/files/{machine}`, the gateway sends the agent `files_request{channel, user, client_ip}`
and waits up to 10 s for `/ws/agent/files/{channel}`, then sends the viewer the same
`{"t":"session",…}` message (with the channel id as `session`) and copies messages unchanged.
The differences:

- A machine can have up to 4 file channels (pending and joined) next to its one screen session.
  Opening or ending either never ends the other. A fifth channel is closed with **1013**.
- Only agents that listed `files` in `register.caps` get a `files_request`; for others the viewer
  is closed with **4011**. Unknown or offline machines get **4003**, an agent that does not dial
  in time **4004**.
- Any message in either direction counts as activity for `RD_SESSION_IDLE_TIMEOUT` (**4008**).
- The gateway sends the agent no `session_end`: it sees its socket close (1000; 1001 when it
  stopped answering pings, 1012 at shutdown). Joined channels survive the control socket
  reconnecting; removing the device or replacing its token ends them (**4003**).
- Messages up to 8 MiB each way.

The audit log has a `files_open` line (user, ip, machine, channel) when the request goes out and
a `files_close` line with the same fields plus `duration_ms`, `bytes_up`, `bytes_down` and
`reason` (the session reasons above). What is transferred is not visible to the gateway; the
agent logs it.

## Terminal channels

A terminal channel ([terminal.md](terminal.md)) works like a file channel, with these
differences:

- The viewer opens `/ws/terminal/{machine}`, the agent gets `terminal_request{channel, user,
  client_ip}` and dials `/ws/agent/terminal/{channel}`.
- Up to 8 per machine (pending and joined), counted apart from the file channels; a ninth is
  closed with **1013**. Only agents that listed `terminal` in `register.caps` get a request;
  for others the viewer is closed with **4011**.
- Messages up to 1 MiB each way.
- A channel closing never ends its shell: the agent keeps it for a reattach.

The audit log has a `terminal_open` and a `terminal_close` line with the same fields as the file
channel's. What is typed or printed is not visible in the logs.

## Relay and keepalive

- Each direction is a loop that reads one message, then awaits its send to the other leg before
  reading the next. A slow viewer therefore stops the gateway from reading the agent, and TCP
  pushes back to the agent's `TCP_NOTSENT_LOWAT`-limited socket. Nothing is queued, dropped or
  reordered. WebSockets use `write_buffer_size(0)`. Per direction, the gateway holds at most one
  message plus the kernel socket buffers. The backpressure test measured about 3 MiB in flight
  over loopback.
- Message limits: 16 MiB agent→viewer, 64 KiB viewer→agent, 64 KiB on the control socket,
  8 MiB both ways on file channels, 1 MiB on terminal channels and on the probe socket.
- The gateway sends a WebSocket Ping every 15 s on every leg: control, data, file, terminal,
  viewer and probe.
  A leg with no Pong for 45 s is closed. Ping and pong frames are never forwarded between
  legs. A leg that stays blocked on backpressure for 45 s also counts as unresponsive.

## Failure injection

`RD_CHAOS` (only with `--dev`) models the agent→viewer path as a FIFO bottleneck link. A message
is delivered at `max(arrival + delay_ms, link free) + size / bw_kbps`. The delay line reads ahead
by at most `max(4 MiB, 2 × bandwidth·delay)`, so backpressure still reaches the agent, and
messages are never dropped or reordered. Viewer→agent traffic is not affected.

## Logs and metrics

Without `--dev`, logs are JSON lines on stdout, with fields flattened. Useful messages:
`rd-gateway listening`, `agent registered`, `agent control disconnected`, `session_start`,
`session_end`, `files_open`, `files_close`, `terminal_open`, `terminal_close`, `channel refused`, `viewer forbidden`, `agent auth failed`, `device_add`, `device_token`,
`device_remove`, `device store not saved`, `agent_update`, `agent update refused`,
`agent update phase`, `agent update outcome`, `probe run`, `probe_report {…}`.

`/metrics` exposes `rd_agents_online`, `rd_sessions_active`, `rd_sessions_started_total`,
`rd_relay_bytes_{down,up}_total`, `rd_files_channels_active`,
`rd_files_channels_started_total`, `rd_files_bytes_{down,up}_total` (file channels; not in
the relay totals), `rd_terminal_channels_active`, `rd_terminal_channels_started_total`,
`rd_terminal_bytes_{down,up}_total` (terminal channels, likewise), `rd_agent_auth_failures_total`,
`rd_viewer_auth_failures_total`, `rd_agent_registrations_total`,
`rd_agent_updates_requested_total` and `rd_probe_runs_total`.

## Shutdown

On SIGTERM or SIGINT the gateway stops accepting connections. It closes every WebSocket with
1012 (viewers and agents reconnect), waits up to 5 s for close handshakes and in-flight HTTP
requests, then exits 0. Long `/api/probe/down` streams end at shutdown.

## Container image

```sh
docker buildx build --platform linux/arm64 -f deploy/Dockerfile -t rd-gateway:dev .
```

The build runs in three stages. `oven/bun` builds `viewer/dist`. `rust` + `cargo-zigbuild`
then cross-compiles a static `aarch64-unknown-linux-musl` binary with the viewer embedded.
The result ships on `gcr.io/distroless/static-debian12:nonroot`. The image runs as non-root
(uid/gid 65532) and exposes 8080/8081/9090. Its only writable path is `RD_STATE_DIR`: mount a
persistent volume there (writable by gid 65532, e.g. `fsGroup: 65532`) and keep the root
filesystem read-only. Mount provisioned token hashes, if any, at `/etc/rd/agents`.

`.github/workflows/gateway-image.yml` builds and pushes `ghcr.io/thijmen/rd-gateway` on every
push to `main` that touches the gateway, protocol, flow control or viewer (`X.Y.Z` and
`latest`, git tag `gateway-vX.Y.Z`), and opens a PR on the GitOps repo that bumps the
deployed tag. To try a branch before it is merged, run the workflow by hand with **preview**:

```sh
gh workflow run gateway-image.yml --ref my-branch -f preview=true
```

That builds `X.Y.Z-preview.<sha>` from the branch, leaves `latest` and the release tags alone,
and opens the same bump PR, marked as a preview. Merging it deploys the preview. The next
build from `main` (or merging the previous tag back) replaces it.

## Tests

```sh
cargo test -p rd-gateway      # unit tests + tests/{auth,session,probe,http,devices,updates,backpressure}.rs
```

The integration tests start the gateway in-process on ephemeral loopback ports with short
timers (`rd_gateway::Timeouts`). They drive it with `tokio-tungstenite` and a hyper client.
