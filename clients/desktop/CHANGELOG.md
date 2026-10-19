# rd-desktop

## 1.2.0

### Minor Changes

- 385b13d: Windows agent installer: the release now includes `rd-agent-<version>-windows-x64-setup.exe`, a per-user installer (no admin rights) with Start menu shortcuts and an entry in Settings → Apps. It offers the setup at the end, updates a running agent in place, and its uninstaller removes the autostart.

## 1.1.0

### Minor Changes

- 2bf4fa4: Connect to Windows PCs: rd-agent now runs on Windows too, and the release includes it (`rd-agent-<version>-windows-x64.zip`; setup in docs/setup-windows.md). From a Mac, the new default "Automatic" modifier mapping makes Cmd act as Ctrl and Ctrl as the Windows key, and Cmd+Tab switches apps on the PC.
