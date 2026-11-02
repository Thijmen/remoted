---
"rd-desktop": patch
---

macOS: when a page's web process keeps crashing, rd Desktop no longer reloads it in an endless loop (which froze the app with a spinning cursor and flooded the gateway with requests). It reloads at most twice a minute, then shows a page with Try again, and notes each crash in its log.
