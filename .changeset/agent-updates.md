---
"rd-agent": minor
---

Update agents from the portal: a device whose agent is out of date shows **Update to X.Y.Z** with what's new and an **Update** button. The agent installs the new version itself and puts the previous one back if the new one doesn't reconnect within two minutes. The agent now has its own releases (`agent-vX.Y.Z`), reports its real version, and macOS gets a ready-made, signed `RD Agent.app` download. Agents up to 1.4.2 need to be updated by hand once.
