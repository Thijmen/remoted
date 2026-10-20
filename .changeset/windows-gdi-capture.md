---
"rd-agent": patch
---

Windows: a PC without a graphics driver for its display (the Microsoft Basic Display Adapter
after a fresh install, a virtual machine, a USB/dock display) can connect now. Where Desktop
Duplication is unavailable the agent captures with GDI instead and encodes in software, at up
to 30 frames per second.
