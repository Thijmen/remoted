# rd — protocol & component contracts (v1)

This document is the contract between the components. Source of truth for message layouts:
`crates/rd-proto/src/bin.rs` (binary) and `crates/rd-proto/src/control.rs` (JSON).
Golden vectors shared by Rust and TypeScript: `crates/rd-proto/testvectors/binary.json`
(u64 fields are JSON strings; `payload_hex` is the bytes after the header).

## Topology

```
browser (viewer) --WSS--> Traefik --> rd-gateway :8080  (Authentik forwardAuth + email allowlist + Origin check)
rd-agent (Mac/Win) --WSS--> Traefik --> rd-gateway :8081  (per-agent bearer token; no Authentik)
```

- Agents dial OUT. One long-lived **control** WS per agent: `GET {gateway_url}/ws/agent`.
- Per viewer session the agent dials a NEW **data** WS: `GET {gateway_url}/ws/agent/session/{session}`.
- Viewer connects `GET /ws/view/{machine}` on the viewer origin. The gateway joins viewer WS ⇄ agent
  data WS and copies messages unchanged in both directions (it only parses JSON it owns and peeks the
  first byte of binary frames for input-idle tracking / byte counters).
- Production hosts: viewer `https://rd.example.com`, agents `wss://rd-agent.example.com`.
- Dev: `rd-gateway --dev` serves both on localhost: viewer `http://127.0.0.1:8080`, agents `ws://127.0.0.1:8081`.

## Auth

**Agents** — header `Authorization: Bearer rd1.<machine>.<secret>`.
- Gateway hashes the whole token with SHA-256 and compares (constant time) to the hex digest in
  `{RD_AGENT_TOKENS_DIR}/<machine>` (default `/etc/rd/agents`). `<machine>` from the token must match
  the `register.machine`.
- Rate limit: 5 failed auths / minute / client IP (client IP = rightmost `X-Forwarded-For` entry, else peer addr).
- `--dev`: any token accepted; machine taken from the token if it has the `rd1.<machine>.` form, else from `register`.

**Viewers** — the request must carry `X-authentik-email` ∈ `RD_ALLOWED_EMAILS` (comma separated,
case-insensitive) and, for WebSocket upgrades, `Origin == RD_PUBLIC_ORIGIN`. Otherwise 403.
- `--dev`: no header checks; user = `dev@localhost`.

## Agent ⇄ gateway control socket (`/ws/agent`)

1. Agent → `{"t":"register","machine":"mbp","agent_version":"0.1.0","os":"macOS 26.3 (25D125)","gw_proto":1,"state":{...}}`
2. Gateway → `{"t":"registered","gw_proto":1}`. A newer registration for the same machine replaces the old
   socket (old one closed with code 4009).
3. Agent → `{"t":"state", ...AgentState}` whenever lock/perms/in_session change (checked every 2 s).
4. Gateway → `{"t":"session_request","session":"<32 hex>","user":"<email>","client_ip":"<ip>"}`.
   Agent must dial `/ws/agent/session/<session>` (same bearer token) within **10 s**. One session per
   machine: a new request implicitly ends the previous one.
5. Either side → `{"t":"session_end","session":"…","reason":"…"}`.
6. Keepalive: the gateway sends WebSocket Ping frames every 15 s on every leg and closes a leg after
   45 s without Pong. The agent reconnects its control socket if it receives nothing for 30 s.
   Agent reconnect backoff: 1 s → 30 s exponential with ±20 % jitter.

## Viewer session (`/ws/view/{machine}`)

1. Viewer connects. Gateway checks auth. If the machine is unknown/offline → close **4003**.
   If another viewer holds the machine, that viewer is closed with **4001** (take-over) and its agent
   session ended.
2. Gateway sends `session_request` to the agent; waits ≤ 10 s for the data WS (else close viewer **4004**).
3. Gateway joins the sockets and sends the viewer `{"t":"session","machine":"mbp","session":"…","agent":{AgentState}}`.
4. Viewer → agent `{"t":"hello","proto":1,"min_proto":1,"viewer":{"ua":"…","dpr":2,"viewport":[1536,750],"decoders":{"avc":true,"hevc":false}}}`
5. Agent → viewer `{"t":"hello","proto":1,"agent":{"os":"macos","version":"0.1.0","displays":[{"id":0,"w_pt":1728,"h_pt":1117,"scale":2.0}]},"caps":["audio","refine"]}`
   (`proto = min(maxes)`; if incompatible agent sends `{"t":"error","code":"proto_mismatch"}` and closes 4005.)
6. Viewer → `{"t":"start","width":1728,"height":1118,"fps":60,"codec":"avc","start_kbps":8000,"min_kbps":1000,"max_kbps":20000,"adaptive":true,"audio":false,"display":0}`
7. Agent → `{"t":"video_config","epoch":1,"codec":"avc1.640033","width":1728,"height":1118,"description":"<base64 avcC>"}`
   then VIDEO frames (first has `FIRST_OF_EPOCH|KEY`), `{"t":"stats",…}` every second,
   `{"t":"state",…}` on change, `{"t":"error","code":…}` on problems.
8. Viewer sends VIDEO_ACK after each decoder output (or discard), input events, `keyframe_req`
   (agent rate-limits to 1 per 500 ms), `pause`/`resume` (tab hidden/visible), `set` (partial params;
   size/display/codec change ⇒ new epoch ⇒ new `video_config` + keyframe).
9. PING/PONG both ways: the agent pings every 500 ms (RTT → flow window); the viewer pings every 1 s
   (clock offset). Every PING must be answered immediately with a PONG
   (`t_origin_us` echoed, `t_reply_us` = responder clock).
10. Close codes: 4001 kicked (don't auto-reconnect), 4003 offline, 4004 agent timeout, 4005 protocol,
    4006 session ended by agent, 4008 idle (no input for `RD_SESSION_IDLE_TIMEOUT`, default 3600 s), 4009 replaced.

Agent side: the session socket is the only one carrying media. When it closes the agent stops the
pipeline, releases all keys/buttons and drops the keep-awake guard.

### Clocks
- Agent timestamps: `rd_agent_core::clock::now_us()` (monotonic µs since agent start).
- Viewer timestamps: `Math.round(performance.now() * 1000)`.
- Viewer clock offset (agent − viewer) from the min-RTT PONG of the last 10 s:
  `offset = t_reply − (t_origin + t_recv) / 2`. capture→paint = `(paint_us + offset) − capture_us`.

## Flow control (agent) — see `crates/rd-flow`
- Window: `W_frames = clamp(ceil(fps·(rtt_min + 40ms)) + 1, 2, 16)`, `W_bytes = target_bps/8·(rtt_min + 40ms)`.
- Capture callback: `gate.admit()` → `None` = drop before encode. Always admit if nothing in flight.
- Encoded frames are never dropped (not by the agent, not by the gateway). If one ever must be, the
  next frame is forced IDR (`FrameGate::post_encode_drop`).
- Viewer ACKs after **decoder output** (so decoder backlog counts toward the window), cumulative.
- Stall: oldest in-flight > 1.5 s → log; > 8 s → agent closes the session (4006), viewer reconnects.
- Bitrate: delay-based controller ticking every 250 ms (`BitrateController`), applies to the encoder only
  on > 5 % change.

## Gateway HTTP surface (viewer port 8080)

| Route | What |
|---|---|
| `GET /` | portal (`index.html`) |
| `GET /m/{machine}` | viewer page (`viewer.html`) — the page reads the machine from `location.pathname` |
| `GET /probe` | network probe page (`probe.html`) |
| `GET /latency-target` | latency target page (`latency.html`), opened ON the host to be filmed/detected |
| `GET /keytest` | key test page (`keytest.html`), opened ON the host |
| `GET /assets/*` | hashed Vite assets (`Cache-Control: public, max-age=31536000, immutable`) |
| `GET /api/me` | `{"email":"…","uid":"…","dev":false}` (`uid` may be null) (uid = `X-authentik-uid`, for `RD_ALLOWED_UIDS`) |
| `GET /api/machines` | `[{"machine":"mbp","name":"MacBook Pro","online":true,"since_ms":1729160000000,"os":"macOS 26.3","agent_version":"0.1.0","state":{AgentState},"viewer":null\|{"user":"…","since_ms":…},"recovery":[{"label":"Screen Sharing (VNC)","url":"https://…"}]}]` |
| `GET /ws/view/{machine}` | viewer session WS |
| `GET /ws/probe` | probe WS (below) |
| `POST /api/probe/report` | JSON body (≤ 1 MiB) logged as one line `probe_report {...}`; → 204 |
| `GET /api/probe/down?kbps=10000&seconds=10&chunk_ms=20` | streamed HTTP body; each chunk: `[u64 LE server_us][u32 LE seq][filler]` sized `kbps·chunk_ms/8` bytes |
| `POST /api/probe/up` | streamed request body; → `{"bytes":N,"elapsed_ms":N}` |

HTML pages are served with `Cache-Control: no-cache`. All responses on 8080 carry
`Content-Security-Policy: default-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'`, `X-Frame-Options: DENY`,
`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`.

The viewer build (`viewer/dist`) must therefore contain: `index.html`, `viewer.html`, `probe.html`,
`latency.html`, `keytest.html`, `assets/…`. The gateway embeds `viewer/dist` (rust-embed,
`allow_missing`) and `--assets-dir <dir>` overrides it at runtime.

Agent port 8081: `GET /ws/agent`, `GET /ws/agent/session/{session}` only (everything else 404).
Metrics port 9090: `GET /healthz` → `ok`, `GET /metrics` (Prometheus text, optional).

### Probe socket (`/ws/probe`)
- On connect gateway → `{"t":"hello","server_time_us":…,"client_ip":"…","user":"…"}`.
- Viewer binary PING → gateway PONG immediately (also while a test runs, so RTT-under-load is measured
  through the same TCP stream).
- `{"t":"run","test":"down","kbps":10000,"fps":60,"seconds":20,"windowed":false}`: gateway sends VIDEO
  frames (`flags=0, epoch=0, seq=n, capture_us=gateway clock, encode_us=0`, random payload of
  `kbps·1000/8/fps` bytes) paced at `fps`. Viewer sends VIDEO_ACK for each on receipt. With
  `windowed:true` frames are gated by `rd_flow::FlowGate` (+ `BitrateController` adapting `kbps` as the
  max) exactly like the agent. End → `{"t":"done","test":"down","elapsed_ms":…,"frames":…,"bytes":…,"window_drops":…}`.
- `{"t":"run","test":"up","kbps":1000,"seconds":30}`: viewer sends PROBE_UP frames; gateway counts until
  `seconds` elapsed → `done` with received totals.
- `{"t":"run","test":"idle","seconds":600}`: nothing sent; `done` after `seconds` (tests idle survival).
- `{"t":"stop"}` → current run ends with `done{stopped:true}`.
- Limits: kbps ≤ 100000, seconds ≤ 7200, one run at a time per socket.

## Gateway config (env / flags)

| Env | Default | |
|---|---|---|
| `RD_VIEWER_ADDR` | `0.0.0.0:8080` | |
| `RD_AGENT_ADDR` | `0.0.0.0:8081` | |
| `RD_METRICS_ADDR` | `0.0.0.0:9090` | |
| `RD_PUBLIC_ORIGIN` | – (required unless `--dev`) | e.g. `https://rd.example.com` |
| `RD_ALLOWED_EMAILS` | – (required unless `--dev`) | comma separated |
| `RD_ALLOWED_UIDS` | – (optional) | comma separated `X-authentik-uid` values; when set, a viewer must match both lists |
| `RD_AGENT_TOKENS_DIR` | `/etc/rd/agents` | one file per machine with hex sha256 |
| `RD_MACHINES_FILE` | – | JSON `[{"machine":"mbp","name":"MacBook Pro","recovery":[{"label":"…","url":"…"}]}]` |
| `RD_SESSION_IDLE_TIMEOUT` | `3600` | seconds without input |
| `RD_CHAOS` | – | dev only: `bw_kbps=5000,delay_ms=50` applied agent→viewer in the join loop |
| `RUST_LOG` | `info` | JSON logs unless `--dev` |

## `.rdv` recording format
Header `RDV1` (4 bytes), then records until EOF:
`[kind u8: 0=text(JSON) 1=binary][t_us u64 LE (agent clock)][len u32 LE][bytes]`.
Messages are exactly the agent→viewer wire messages (`video_config`, VIDEO, AUDIO…), so a recording can
be replayed by a fake agent or decoded by the viewer's `/dev/decoder-test`.

## Keys
Wire = USB HID usage `(page << 16) | usage`. Tables generated from Chromium `dom_code_data.inc` by
`tools/gen-keymap` into `crates/rd-proto/src/keymap_gen.rs` and `viewer/src/input/keymap.gen.ts`.
Viewer: `KeyboardEvent.code` → HID. macOS: HID → kVK. Windows: HID → scancode (`0xE0xx` extended).
