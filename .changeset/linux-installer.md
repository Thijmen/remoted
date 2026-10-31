---
"rd-agent": minor
---

Linux agents are now released as static binaries for x86_64 and ARM64, with a one-line installer: `curl -fsSL https://raw.githubusercontent.com/Thijmen/remoted/main/packaging/linux/install.sh | sh -s -- <setup code>`. The portal's Add device dialog shows that command with the code filled in. `rd-agent setup --code <setup code>` sets an agent up without questions.
