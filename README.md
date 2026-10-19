# rd

A remote desktop that runs in a plain browser tab over one WebSocket on port 443. It is
meant for a work laptop that can't run remote-desktop clients and only reaches the outside
world over 80/443.

```
browser (WebCodecs) ──WSS──▶ rd-gateway (K3s, behind Traefik + Authentik) ◀──WSS── rd-agent (Mac; Windows later)
```

- **rd-agent** captures the screen (ScreenCaptureKit), encodes hardware H.264
  (VideoToolbox) and injects mouse/keyboard input. It dials *out* to the gateway, so there
  are no inbound ports at home.
- **rd-gateway** serves the viewer, checks who you are, and joins the viewer and agent
  sockets without buffering.
- **viewer** decodes with WebCodecs and acknowledges every frame. The agent drops frames
  *before* encoding when the link can't keep up, so latency stays low instead of queueing.

The lock screen, the login window and reboots are handled by Apple Screen Sharing through
Guacamole: see [docs/recovery.md](docs/recovery.md).

| | |
|---|---|
| [docs/protocol.md](docs/protocol.md) | wire protocol v1 |
| [docs/gateway.md](docs/gateway.md) | gateway configuration and operation |
| [docs/setup-mac.md](docs/setup-mac.md) | installing the Mac agent, local development |
| [docs/recovery.md](docs/recovery.md) | recovery path (Screen Sharing / SSH via Guacamole) |
| [docs/desktop-client.md](docs/desktop-client.md) | rd Desktop: Windows/macOS app that also grabs Alt+Tab, Cmd+Tab, the Windows key, … ([install, build](clients/desktop/README.md)) |

## Layout

```
crates/rd-proto            wire protocol (shared by agent and gateway), HID keymap
crates/rd-flow             flow control: in-flight window + delay-based bitrate controller
crates/rd-agent-core       agent runtime (platform independent)
crates/rd-platform-macos   ScreenCaptureKit, VideoToolbox, CGEvent, IOKit, Keychain
crates/rd-agent            the agent binary (run | setup | install | uninstall | status | selftest)
crates/rd-gateway          relay, portal, network probe; embeds viewer/dist
viewer/                    TypeScript viewer (Vite + Bun)
clients/desktop            rd Desktop (Tauri): the viewer in a window + keyboard grab (Windows, macOS); own Cargo.lock
tools/gen-keymap           Chromium key table -> Rust/TS keymaps
tools/release              changesets helper: rd Desktop version -> Cargo.toml/Cargo.lock
.changeset                 pending release notes (changesets)
packaging/macos            app bundle, signing, LaunchAgent
deploy/Dockerfile          gateway image (linux/arm64)
```

## Checks

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
(cd viewer && bun run typecheck && bun run test && bun run build)
bun tools/gen-keymap/gen.ts --check
```

## Releases

rd Desktop is released with [changesets](https://github.com/changesets/changesets): add one
with `bun changeset` in a PR, merge the generated *Version packages* PR to tag and publish.
See [clients/desktop/README.md](clients/desktop/README.md#releasing). The gateway image keeps
its own versioning (`gateway-v*`, on every push to main; `.github/workflows/gateway-image.yml`).
