---
"rd-desktop": minor
---

Add devices from the portal: **Add device** creates the device's token and shows a one-time setup code to paste into `rd-agent setup` (also on the Windows installer's last page). **New token** and **Remove** on a device's card replace its token or remove it, and disconnect its agent right away. The gateway needs `RD_STATE_DIR` (a persistent volume) and `RD_AGENT_PUBLIC_URL` for this; tokens provisioned in `RD_AGENT_TOKENS_DIR` keep working.
