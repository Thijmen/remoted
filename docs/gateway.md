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
| `--agent-tokens-dir` | `RD_AGENT_TOKENS_DIR` | `/etc/rd/agents` | one file per machine holding the hex SHA-256 of that agent's token; re-read on every auth, so rotation needs no restart |
| `--machines-file` | `RD_MACHINES_FILE` | – | `[{"machine":"mbp","name":"MacBook Pro","recovery":[{"label":"…","url":"…"}]}]`; these machines are listed even while offline. Machine ids are `[A-Za-z0-9_-]{1,64}`. |
| `--session-idle-timeout` | `RD_SESSION_IDLE_TIMEOUT` | `3600` | seconds without viewer input → close 4008 |
| `--chaos` | `RD_CHAOS` | – | dev only, e.g. `bw_kbps=5000,delay_ms=50` (see [Failure injection](#failure-injection)) |
| `--assets-dir` | `RD_ASSETS_DIR` | embedded | serve the viewer build from this directory instead of the copy embedded at compile time |
| – | `RUST_LOG` | `info` | tracing filter, e.g. `rd_gateway=debug` |

Startup fails if the production settings are missing, `RD_MACHINES_FILE` is invalid, or a
listener cannot bind.

## Running in development

```sh
# 1. viewer build (re-run, or `bunx vite build --watch`, after viewer changes)
cd viewer && bun install && bun run build && cd ..
# 2. gateway: no auth, loopback only, assets read from disk
cargo run -p rd-gateway -- --dev --assets-dir viewer/dist
# portal: http://127.0.0.1:8080   agents connect to ws://127.0.0.1:8081
```

In dev mode the agent can send any bearer token. The machine id comes from the token when the
token has the form `rd1.<machine>.…`, and from `register` otherwise. The Vite dev server
(`bun run dev`, port 5173) proxies `/api` and `/ws` to `127.0.0.1:8080`.

Failure injection, for example a 5 Mbps bottleneck with 50 ms extra delay:

```sh
RD_CHAOS=bw_kbps=5000,delay_ms=50 cargo run -p rd-gateway -- --dev --assets-dir viewer/dist
```

## Authentication

**Agents** send `Authorization: Bearer rd1.<machine>.<secret>`. The gateway hashes the whole
token with SHA-256 and compares it in constant time with `{RD_AGENT_TOKENS_DIR}/<machine>`. The
`register.machine` must equal the machine in the token, or the control socket is closed with
4002. The data socket (`/ws/agent/session/{id}`) needs the same token. It must also match a
pending session of that machine that is no more than 10 s old, otherwise the gateway answers 404.

To create a token and the file the gateway stores:

```sh
TOKEN="rd1.mbp.$(openssl rand -hex 32)"           # goes into the agent's Keychain
printf '%s' "$TOKEN" | shasum -a 256 | cut -d' ' -f1 > mbp   # goes into the tokens secret
```

Failed agent auths are limited to **5 per minute per client IP**. The client IP is the
rightmost `X-Forwarded-For` entry (the one Traefik appended), or the TCP peer. Once an IP is
over the limit, every request from it gets `429` with `Retry-After`, even one with a valid
token, until the budget refills (one attempt every 12 s). Every `401`/`429` is logged as
`agent auth failed` / `agent auth rejected: rate limited`, with `ip`, `path` and `reason`.

**Viewers** need `X-authentik-email` to be in `RD_ALLOWED_EMAILS` on every viewer-port
request. WebSocket upgrades (`/ws/*`) also need `Origin` to equal `RD_PUBLIC_ORIGIN`. State-changing
requests (POST) that carry a foreign `Origin` are refused as well. A refused request gets
`403` and a `viewer forbidden` log line with `ip`, `path` and `reason`.

Every response on the viewer port carries the CSP, COOP (`same-origin`), COEP
(`require-corp`), `nosniff` and `no-referrer` headers from `protocol.md`.

## Viewer port routes

| Route | Response |
|---|---|
| `/`, `/m/{machine}`, `/probe`, `/latency-target`, `/keytest` | `index.html`, `viewer.html`, `probe.html`, `latency.html`, `keytest.html` with `Cache-Control: no-cache` (and an `ETag` when embedded). A page missing from the build gets a 503 placeholder that explains how to build the viewer. |
| `/assets/*` | hashed Vite output, `Cache-Control: public, max-age=31536000, immutable` |
| other `GET /<file>` | other top-level files of the build (favicon, …), `no-cache`; else 404 |
| `GET /api/me` | `{"email":"…","dev":false}` |
| `GET /api/machines` | see below |
| `GET /ws/view/{machine}` | viewer session |
| `GET /ws/probe`, `GET /api/probe/down`, `POST /api/probe/up`, `POST /api/probe/report` | network probe (`protocol.md`); the report is logged as one `probe_report {…}` line (≤ 1 MiB, else 413) |

`/api/machines` lists the machines from `RD_MACHINES_FILE` in file order, then any other machine
that has registered since start, sorted by id. Fields that are not known yet are `null`:
`since_ms` (online since / offline since, Unix ms), `os` and `agent_version` (from the last
registration, kept while offline), and `state` (last reported `AgentState`). `viewer` is
`{"user","since_ms"}` while a session is pending or active, else `null`. `recovery` comes from
the machines file (`[]` otherwise). `name` falls back to the machine id.

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

## Relay and keepalive

- Each direction is a loop that reads one message, then awaits its send to the other leg before
  reading the next. A slow viewer therefore stops the gateway from reading the agent, and TCP
  pushes back to the agent's `TCP_NOTSENT_LOWAT`-limited socket. Nothing is queued, dropped or
  reordered. WebSockets use `write_buffer_size(0)`. Per direction, the gateway holds at most one
  message plus the kernel socket buffers. The backpressure test measured about 3 MiB in flight
  over loopback.
- Message limits: 16 MiB agent→viewer, 64 KiB viewer→agent, 64 KiB on the control socket,
  1 MiB on the probe socket.
- The gateway sends a WebSocket Ping every 15 s on every leg: control, data, viewer and probe.
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
`session_end`, `viewer forbidden`, `agent auth failed`, `probe run`, `probe_report {…}`.

`/metrics` exposes `rd_agents_online`, `rd_sessions_active`, `rd_sessions_started_total`,
`rd_relay_bytes_{down,up}_total`, `rd_agent_auth_failures_total`,
`rd_viewer_auth_failures_total`, `rd_agent_registrations_total` and `rd_probe_runs_total`.

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
The result ships on `gcr.io/distroless/static-debian12:nonroot`. The image runs as non-root,
needs no writable filesystem, and exposes 8080/8081/9090. Mount the token hashes at
`/etc/rd/agents`.

## Tests

```sh
cargo test -p rd-gateway      # unit tests + tests/{auth,session,probe,http,backpressure}.rs
```

The integration tests start the gateway in-process on ephemeral loopback ports with short
timers (`rd_gateway::Timeouts`). They drive it with `tokio-tungstenite` and a hyper client.
