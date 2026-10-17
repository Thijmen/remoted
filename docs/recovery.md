# Recovery path (Track R)

rd's agent only runs inside a logged-in macOS session. Everything else — the lock
screen, the login window, a macOS permission re-approval prompt, a crashed agent,
a reboot with FileVault — goes through **Apple Screen Sharing / SSH via Guacamole**,
with `guacd` running at home so the heavy VNC leg stays on the LAN.

```
browser → Guacamole (K3s, Authentik) → Tailscale → guacd-home (home-server) → LAN → Mac 192.168.1.50:5900 / :22
```

## One-time setup

1. **Mac**
   - DHCP reservation for `192.168.1.50` on the router.
   - Power: `sudo pmset -c sleep 0 displaysleep 15 womp 1` (on charger: never sleep, Wake-on-LAN on).
   - System Settings → General → Software Update → Automatic updates: turn **off**
     "Install macOS updates" (keep security responses). Update manually when at home.
   - Keep the lid open (or use an HDMI dummy plug). Ethernet recommended.
   - Screen Sharing and Remote Login are already enabled.
2. **home-server**: `docker compose -f packaging/home/guacd/compose.yml up -d`.
3. **Tailscale ACL**: allow `tag:k3s-private` → `home-server:4822` (and nothing else to 4822).
4. **Cluster**: GitOps repo branch `rd-remote-desktop` adds the `guacd-home` egress
   Service in `services-guacamole`.
5. **Guacamole connections** (Settings → Connections), both with
   *Guacamole proxy parameters (guacd)* → hostname `guacd-home.services-guacamole.svc.cluster.local`, port `4822`:
   - **Mac – recovery VNC**: protocol VNC, host `192.168.1.50`, port `5900`, no stored password,
     encodings `zrle hextile zlib copyrect raw`, color depth 24 (try 16 if slow).
   - **Mac – SSH**: protocol SSH, host `192.168.1.50`, port `22`, no stored password.
6. For a usable VNC picture, temporarily switch the Mac to a non-HiDPI ~2 MP mode
   (or connect via the dummy plug), and enable Reduce Transparency / Reduce Motion.

## Situations

| Situation | What to do |
|---|---|
| Mac locked (rd portal shows "locked") | Mac – recovery VNC → type password → back to rd |
| Screen-recording re-approval prompt (rd shows `tcc_screen`) | Mac – recovery VNC → approve the prompt |
| Agent crashed / hung | Mac – SSH → `launchctl kickstart -k gui/$(id -u)/dev.thijmen.rd.agent` |
| Reboot with FileVault | Mac – SSH to the pre-boot unlock prompt (macOS 26 "unlock FileVault over SSH"; reportedly needs ≥ 26.5 on Wi-Fi — verify), then recovery VNC to log in |
| Mac asleep | Wake-on-LAN from another home machine, or it should not happen on AC with the pmset settings above |
