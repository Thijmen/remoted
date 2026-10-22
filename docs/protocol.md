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
- File channels (browse and transfer files, [files.md](files.md)) are joined the same way:
  viewer `GET /ws/files/{machine}`, agent `GET /ws/agent/files/{channel}`. So are terminal
  channels (a shell, [terminal.md](terminal.md)): viewer `GET /ws/terminal/{machine}`, agent
  `GET /ws/agent/terminal/{channel}`.
- Viewer connects `GET /ws/view/{machine}` on the viewer origin. The gateway joins viewer WS ⇄ agent
  data WS and copies messages unchanged in both directions (it only parses JSON it owns and peeks the
  first byte of binary frames for input-idle tracking / byte counters).
- Hosts (examples): viewer `https://rd.example.com`, agents `wss://rd-agent.example.com`.
- Dev: `rd-gateway --dev` serves both on localhost: viewer `http://127.0.0.1:8080`, agents `ws://127.0.0.1:8081`.

## Auth

**Agents** — header `Authorization: Bearer rd1.<machine>.<secret>`.
- Gateway hashes the whole token with SHA-256 and compares (constant time) to the device's digest in
  `{RD_STATE_DIR}/devices.json` (devices added in the portal), else to the hex digest in
  `{RD_AGENT_TOKENS_DIR}/<machine>` (default `/etc/rd/agents`). `<machine>` from the token must match
  the `register.machine`.
- Rate limit: 5 failed auths / minute / client IP (client IP = rightmost `X-Forwarded-For` entry, else peer addr).
- `--dev`: any token accepted; machine taken from the token if it has the `rd1.<machine>.` form, else from `register`.

**Viewers** — the request must carry `X-authentik-email` ∈ `RD_ALLOWED_EMAILS` (comma separated,
case-insensitive) and, for WebSocket upgrades, `Origin == RD_PUBLIC_ORIGIN`. Otherwise 403.
- `--dev`: no header checks; user = `dev@localhost`.

## Agent ⇄ gateway control socket (`/ws/agent`)

1. Agent → `{"t":"register","machine":"mbp","agent_version":"1.5.0","os":"macOS 26.3 (25D125)","gw_proto":1,"state":{...},"caps":["files"]}`
   `caps` (optional, added after gw_proto 1 shipped; older gateways ignore it): what the agent
   supports beyond screen sessions. `files`: file channels. `terminal`: terminal channels
   ([terminal.md](terminal.md); only while the terminal is enabled in the agent's config).
2. Gateway → `{"t":"registered","gw_proto":1}`. A newer registration for the same machine replaces the old
   socket (old one closed with code 4009).
3. Agent → `{"t":"state", ...AgentState}` whenever lock/perms/in_session/secure/disconnected
   change (checked every 2 s). `AgentState` is
   `{"locked":bool,"perms":{"screen":bool,"input":bool},"in_session":bool,"secure":"unavailable"|"starting"|"ready"|"failed","disconnected":bool}`;
   `secure` is the Windows secure-desktop helper (docs/setup-windows.md), absent on older
   agents and on macOS. `disconnected` (Windows) means the agent's Remote Desktop session is
   disconnected: Windows draws nothing for it, so there is nothing to show until someone
   reconnects; absent on older agents.
4. Gateway → `{"t":"session_request","session":"<32 hex>","user":"<email>","client_ip":"<ip>"}`.
   Agent must dial `/ws/agent/session/<session>` (same bearer token) within **10 s**. One session per
   machine: a new request implicitly ends the previous one.
5. Either side → `{"t":"session_end","session":"…","reason":"…"}`.
   Gateway → `{"t":"files_request","channel":"<32 hex>","user":"<email>","client_ip":"<ip>"}`
   (only to agents with the `files` cap): dial `/ws/agent/files/<channel>` within **10 s**. A
   machine has up to 4 file channels next to its session; they are independent of it (no
   `session_end`, the channel ends when its sockets close). See [files.md](files.md).
   Gateway → `{"t":"terminal_request","channel":"<32 hex>","user":"<email>","client_ip":"<ip>"}`
   (only to agents with the `terminal` cap): dial `/ws/agent/terminal/<channel>` within
   **10 s**; up to 8 per machine, likewise independent. See [terminal.md](terminal.md).
6. Keepalive: the gateway sends WebSocket Ping frames every 15 s on every leg and closes a leg after
   45 s without Pong. The agent reconnects its control socket if it receives nothing for 30 s.
   Agent reconnect backoff: 1 s → 30 s exponential with ±20 % jitter, reset after a registration.
   How the control upgrade's answer changes that:
   - 401/403, and closes 4002 (wrong machine, token replaced or device removed) / 4009 (replaced):
     a refusal, wait 30 s.
   - 429 (failed-login rate limit): wait `Retry-After` (≤ 10 min), else 30 s.
   - Any other HTTP answer (the proxy's 502/503/504, or 404 when the gateway's route is gone,
     while the gateway pod restarts): retry every 1 s (±20 %) for up to 2 min, then exponential.
   - No answer (TCP/TLS failure, timeout): exponential.

### Updates (rd-agent 1.5.0 and later)

How agents update themselves is in [updates.md](updates.md). Two control messages carry it:

- Agent → `{"t":"update_status","available":{"version":"1.6.0","notes":"## 1.6.0\n\n- …","url":"https://github.com/Thijmen/remoted/releases/tag/agent-v1.6.0"},"can_install":true,"phase":"idle","checked_ms":1729160000000,"last":{"from":"1.5.0","to":"1.5.1","result":"ok","at_ms":1729100000000}}`,
  after every `registered` and whenever it changes. Empty fields are left out.
  - `available`: a newer version from the update manifest; absent when up to date, not checked
    yet, or checks are off. `notes`: the release notes of every version newer than the running
    one, newest first, as Markdown (at most 16 KiB). `url`: the release page.
  - `can_install`: the agent can install updates itself. When false, `cannot_install` says why
    (for the portal).
  - `phase`: `idle`, `downloading` or `installing` (the update helper took over; the agent
    restarts shortly). `checking` is defined but not sent: the periodic checks would otherwise
    make two reports every 6 hours.
  - `error`: why the last check or update request failed.
  - `checked_ms`: the last successful check, Unix ms.
  - `last`: the last update attempt, kept until the next one: `from`, `to`, `result` (`ok`,
    `rolled_back`: the new version did not come back and the old one was put back, `failed`:
    nothing was installed), `error`, `at_ms`.
- Gateway → `{"t":"update","version":"1.6.0"}`: the portal's **Update** button. The gateway sends
  it only when the agent's last `update_status` offers exactly that version, with `can_install`
  and no update in progress (`POST /api/machines/{machine}/update`). The agent reads the manifest
  again: the version must still be offered, and its file signed for that version; otherwise it
  reports an `error`. An `update` that arrives while one is in progress is ignored. The agent
  reports `downloading` first, downloads and checks the file, and only then ends its session: it
  closes the data socket with **4010** and sends `session_end` with reason `agent_update`
  (viewers wait until the agent is back and no longer reports an update in progress). Sessions
  requested while it updates are ended the same way. A `session_end` from the gateway for that
  already-ended session is ignored.

The gateway keeps the last `update_status` while the agent is offline (so the portal can show an
install in progress) and drops it when the agent registers again; the new connection reports
its own.

Compatibility: both messages are additive and `gw_proto` stays 1. Older gateways ignore
`update_status` (an unknown `t`). Older agents never send it, so the gateway never sends them
`update`. Receivers ignore unknown fields; unknown `phase` and `result` values are read as
`unknown`.

## Viewer session (`/ws/view/{machine}`)

1. Viewer connects. Gateway checks auth. If the machine is unknown/offline → close **4003**.
   If another viewer holds the machine, that viewer is closed with **4001** (take-over) and its agent
   session ended.
2. Gateway sends `session_request` to the agent; waits ≤ 10 s for the data WS (else close viewer **4004**).
3. Gateway joins the sockets and sends the viewer `{"t":"session","machine":"mbp","session":"…","agent":{AgentState}}`.
4. Viewer → agent `{"t":"hello","proto":1,"min_proto":1,"viewer":{"ua":"…","dpr":2,"viewport":[1536,750],"decoders":{"avc":true,"hevc":false}}}`
5. Agent → viewer `{"t":"hello","proto":1,"agent":{"os":"macos","version":"1.5.0","displays":[{"id":0,"w_pt":1728,"h_pt":1117,"scale":2.0}]},"caps":["audio","refine"]}`
   (`proto = min(maxes)`; if incompatible agent sends `{"t":"error","code":"proto_mismatch"}` and closes 4005.)
6. Viewer → `{"t":"start","width":1728,"height":1118,"fps":60,"codec":"avc","start_kbps":8000,"min_kbps":1000,"max_kbps":20000,"adaptive":true,"audio":false,"display":0}`
   `width`×`height` is a bounding box: the agent always streams the display's own aspect ratio,
   scaled to the largest even size that fits in the box (never up; at least 320×200), e.g.
   1728×1118 on a 16:9 display gives 1728×972. `video_config` carries the size actually sent.
   A `start`/`set` that does not parse (e.g. an unknown codec) or that the agent cannot serve is
   answered with `{"t":"error","code":"bad_request","msg":…}`; messages with an unknown `t` are
   ignored.
7. Agent → `{"t":"video_config","epoch":1,"codec":"avc1.640033","width":1728,"height":1118,"description":"<base64 avcC>"}`
   then VIDEO frames (first has `FIRST_OF_EPOCH|KEY`), `{"t":"stats",…}` every second,
   `{"t":"state",…}` on change, `{"t":"error","code":…}` on problems.
8. Viewer sends VIDEO_ACK after each decoder output (or discard), input events, `keyframe_req`
   (see Keyframes below), `pause`/`resume` (tab hidden/visible; the agent sends a keyframe on
   resume by itself, so the viewer need not ask), `set` (partial params; size/display/codec
   change ⇒ new epoch ⇒ new `video_config` + keyframe), and `sas` (Ctrl+Alt+Del on the Windows
   secure desktop; the agent answers `{"t":"error","code":"sas"}` when policy blocks it).
9. PING/PONG both ways: the agent pings every 500 ms (RTT → flow window); the viewer pings every 1 s
   (clock offset). Every PING must be answered immediately with a PONG
   (`t_origin_us` echoed, `t_reply_us` = responder clock).
10. Close codes: 4001 kicked (don't auto-reconnect), 4003 offline, 4004 agent timeout, 4005 protocol,
    4006 session ended by agent, 4008 idle (no input for `RD_SESSION_IDLE_TIMEOUT`, default 3600 s), 4009 replaced,
    4010 agent updating (it comes back by itself, usually within a minute: keep reconnecting, and treat the 4003
    and 4004 that follow while it restarts as part of the update), 4011 not supported by the agent (a file
    channel to an agent without the `files` cap, or a terminal to one without `terminal`), 1013 try again later
    (a fifth file channel, a ninth terminal).

Agent side: the session socket is the only one carrying media. When it closes the agent stops the
pipeline, releases all keys/buttons and drops the keep-awake guard.

- **Locked host:** without the secure-desktop helper the agent never injects input into the
  lock screen. While the host is locked (checked at most every 200 ms) every input event except
  RELEASE_ALL is dropped; keys/buttons still held when the lock is noticed are released. The
  first dropped event of a locked period makes the agent send the viewer
  `{"t":"state",…,"locked":true}` (once per period). With `secure:"ready"` the agent swaps to
  the helper's pipeline, shows the real lock screen, and forwards input to the helper instead
  of dropping it (docs/setup-windows.md).
- **Capture stopped:** if capture stops on its own (permission revoked, display gone, capture
  service error) the agent sends `{"t":"error","code":"capture_stopped","msg":…}` once (checked
  every second) and keeps the session open; it is reported again only after capture ran again.

### Clocks
- Agent timestamps: `rd_agent_core::clock::now_us()` (monotonic µs since agent start).
- Viewer timestamps: `Math.round(performance.now() * 1000)`.
- Viewer clock offset (agent − viewer) from the min-RTT PONG of the last 10 s:
  `offset = t_reply − (t_origin + t_recv) / 2`. capture→paint = `(paint_us + offset) − capture_us`.

## Flow control (agent) — see `crates/rd-flow`
- **Window:** `horizon = rtt_min + enc + 40 ms`,
  `W_frames = clamp(ceil(fps·horizon) + 1, 2, 16)`, `W_bytes = target_bps/8 · horizon`.
  - `rtt_min`: lowest agent PING RTT (every 500 ms) over the last 10 s; 50 ms until measured.
  - `enc`: median encoder time (`encode_us`, VideoToolbox submit → output) of the last 64
    non-key frames, clamped to `min(25 ms, 1/fps)`; 10 ms until measured (0 in the gateway's
    synthetic probe). Frames count from admission, so their time in the encoder must fit in the
    budget too. Time in the agent's own send queues is deliberately not part of it (it would
    widen the window exactly when the socket is congested). In `stats` as `window_enc_ms`.
  - Frames still in the encoder count against `W_bytes` at the average frame size
    (`target_bps/8/fps`).
- Capture callback: `gate.admit()` → `None` = drop before encode. Always admit if nothing is in
  flight (a keyframe larger than `W_bytes` cannot deadlock). Paused: admit nothing.
- Encoded frames are never dropped (not by the agent, not by the gateway). The queue between the
  encoder and the sender holds 32 frames (2 × the largest window), so frames the window admitted
  always fit, even while the socket is blocked. If one ever must be dropped, the next frame is
  forced IDR (`FrameGate::post_encode_drop`). If the encoder drops a frame that was to carry a
  requested IDR (`cancel`), the next frame carries it instead.
- Viewer ACKs after **decoder output** (so decoder backlog counts toward the window), cumulative.
  A window reset (resume after > 1.5 s with unacknowledged frames) forgets in-flight frames and
  `rtt_min` (not `enc`) and forces an IDR.
- **Keyframes** are forced for a new stream, on resume, on `keyframe_req` and after a window
  reset or post-encode drop. A `keyframe_req` is dropped when a keyframe the viewer has not seen
  yet is already on its way: requested but not yet admitted, in the encoder or the agent's
  queues, or handed to the socket less than `rtt_min` ago (the request crossed it). Otherwise at
  most one per 500 ms; requests inside the interval are coalesced into one at its end. A static
  screen produces no captures, so the pipeline polls `FrameGate::refresh_wanted()` (a keyframe
  is wanted, not paused, room in the window; false once a frame is admitted) and re-submits its
  most recent frame.
- Stall: oldest in-flight > 1.5 s → log; > 8 s → agent closes the session (4006), viewer
  reconnects (not while paused).
- **Bitrate:** delay-based `BitrateController`, ticking every 250 ms:
  ```text
  serialization_i = bytes_i · 8 / max(delivery_rate, 1 Mbps)
  q_i = sent_to_ack_i − rtt_min − serialization_i − decode_i       (sent non-key frames)
  q   = median(q_i over the last 500 ms)
  delivery_rate = bits ACKed in the last 1 s;  app_limited = encoded rate (1 s) < 0.7 · target
  drop_ratio = window drops / captured frames, last 1 s

  q > 150 ms                                  → target = max(min, 0.7 · delivery); hold 2 s
  q > 50 ms for 2 ticks, or (q ≥ 15 ms and drop_ratio > 10 %),
    or (q ≥ 5 ms with window drops for 4 ticks)
                                              → target = max(min, min(0.85 · target, 0.95 · delivery)); hold 2 s
  startup (first 3 s), q < 15 ms, !app_limited → target · 1.25 per tick (window drops ignored)
  after the hold, q < 15 ms, !app_limited, no window drop for 2 s
                                              → every 500 ms: target = min(max, 1.06 · target + 200 kbps)
  ```
  - `q` and the drop ratio only use frames sent / captured after the most recent cut, so stale
    samples cannot cut twice. Cuts never raise the target.
  - Until the first full second has elapsed, rates are averaged over the elapsed time.
  - A change is pushed to the encoder only when it is > 5 % from the last one pushed; in fixed
    mode (`adaptive:false`) the target stays at `start_kbps` and every change of it is pushed.
  - Why each refinement exists: `crates/rd-flow/src/bitrate.rs` (tuned with `tests/sim.rs`).

## Gateway HTTP surface (viewer port 8080)

| Route | What |
|---|---|
| `GET /` | portal (`index.html`) |
| `GET /m/{machine}` | viewer page (`viewer.html`) — the page reads the machine from `location.pathname` |
| `GET /probe` | network probe page (`probe.html`) |
| `GET /latency-target` | latency target page (`latency.html`), opened ON the host to be filmed/detected |
| `GET /keytest` | key test page (`keytest.html`), opened ON the host |
| `GET /assets/*` | hashed Vite assets (`Cache-Control: public, max-age=31536000, immutable`) |
| `GET /api/me` | `{"email":"…","uid":"…","dev":false,"devices":true}` (`uid` may be null) (uid = `X-authentik-uid`, for `RD_ALLOWED_UIDS`; `devices` = *Add device* available) |
| `GET /api/machines` | `[{"machine":"mbp","name":"MacBook Pro","managed":false,"online":true,"since_ms":1729160000000,"os":"macOS 26.3","agent_version":"1.5.0","state":{AgentState},"viewer":null\|{"user":"…","since_ms":…},"recovery":[{"label":"Screen Sharing (VNC)","url":"https://…"}],"update":null\|{UpdateStatus},"files":true}]` (`update`: the agent's last `update_status`, null when it never sent one; `files`: the agent supports file channels) |
| `POST /api/machines/{machine}/update` | JSON `{"version":"1.6.0"}` → 202 `{"ok":true}` once `update` is queued to the agent; 404 unknown machine; 409 `{"error":"…"}` when the agent is offline, does not report updates, cannot install them, has no or another update available, or is already updating |
| `POST /api/devices` | JSON `{"machine":"studio","name":"Mac Studio"}` → 201 `{"machine","name","gateway","token","setup_code"}` (no-store) |
| `POST /api/devices/{machine}/token` | JSON `{}` → 200, same fields; the agent's control socket is closed with 4002 |
| `DELETE /api/devices/{machine}` | → 204; the agent's control socket is closed with 4002 |
| `GET /ws/view/{machine}` | viewer session WS |
| `GET /ws/files/{machine}` | file channel WS ([files.md](files.md)) |
| `GET /ws/probe` | probe WS (below) |
| `POST /api/probe/report` | JSON body (≤ 1 MiB) logged as one line `probe_report {...}`; → 204 |
| `GET /api/probe/down?kbps=10000&seconds=10&chunk_ms=20` | streamed HTTP body; each chunk: `[u64 LE server_us][u32 LE seq][filler]` sized `kbps·chunk_ms/8` bytes |
| `POST /api/probe/up` | streamed request body; → `{"bytes":N,"elapsed_ms":N}` |

HTML pages are served with `Cache-Control: no-cache`. All responses on 8080 carry
`Content-Security-Policy: default-src 'self'; connect-src 'self' ipc: http://ipc.localhost; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; frame-ancestors 'none'`
(`ipc:` / `http://ipc.localhost`: Tauri v2 IPC for the desktop app, see `docs/desktop-client.md`), `X-Frame-Options: DENY`,
`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`.

The viewer build (`viewer/dist`) must therefore contain: `index.html`, `viewer.html`, `probe.html`,
`latency.html`, `keytest.html`, `assets/…`. The gateway embeds `viewer/dist` (rust-embed,
`allow_missing`) and `--assets-dir <dir>` overrides it at runtime.

Device routes take JSON bodies only (≤ 4 KiB) and answer errors as `{"error":"…"}`: 400 invalid id/name,
404 unknown device, 409 id exists or has a provisioned token, 503 no `RD_STATE_DIR`/`RD_AGENT_PUBLIC_URL`,
500 store not saved. The update route takes JSON only too (≤ 1 KiB). Details: `docs/gateway.md`.

### Setup code
What *Add device* hands out and `rd-agent setup` reads (`crates/rd-proto/src/setup_code.rs`):
`rds1_` + base64url (no padding) of `{"gateway":"wss://rd-agent.example.com","token":"rd1.<machine>.<secret>"}`.
`gateway` is `ws://` or `wss://`; the machine id is the token's. Whitespace inside the code is ignored
(line breaks from copying). The secret of a token the gateway creates is 32 random bytes, base64url.

Agent port 8081: `GET /ws/agent`, `GET /ws/agent/session/{session}`, `GET /ws/agent/files/{channel}`
only (everything else 404).
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
| `RD_STATE_DIR` | – | writable dir for `devices.json` (portal devices); none → *Add device* off (dev: in memory) |
| `RD_AGENT_PUBLIC_URL` | – (dev: `ws://<agent addr>`) | agent URL put into setup codes, e.g. `wss://rd-agent.example.com` |
| `RD_AGENT_TOKENS_DIR` | `/etc/rd/agents` | provisioned tokens: one file per machine with hex sha256 |
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
Viewer: `KeyboardEvent.code` → HID. macOS: HID → kVK. Windows: HID → scancode (`0xE0xx` extended),
injected with `SendInput`. The table follows the low-level keyboard hook's convention, which differs
from injection for Num Lock (table `E0 45`, injected as `45`) and Pause (table `45`, injected as
`VK_PAUSE`).
