# rd-agent

## 1.8.0

### Minor Changes

- b5f2e0f: Terminal: open a shell on this machine from the portal or the session menu (zsh on a Mac, PowerShell on Windows), over the same connection as the screen. The shell keeps running when the connection drops, and the terminal picks up where it left off. Turn it off with `[terminal] enabled = false` in the agent's config.
- bf576eb: The agent now has an icon in the macOS menu bar and the Windows notification area. Its menu shows whether the machine is online and who is connected, and lets you pause remote access, check for and install updates, turn start at login on or off, and quit the agent. RD Agent.app also has an icon now, and opening it starts the agent again after a quit.

## 1.7.0

### Minor Changes

- b9fa9ed: Windows: show the real lock screen and UAC prompts in the viewer, so a locked PC can be
  unlocked remotely with its normal password. Install the secure-desktop service once from an
  elevated terminal with `rd-agent secure install`.

## 1.6.1

### Patch Changes

- f668d92: Windows: a PC without a graphics driver for its display (the Microsoft Basic Display Adapter
  after a fresh install, a virtual machine, a USB/dock display) can connect now. Where Desktop
  Duplication is unavailable the agent captures with GDI instead and encodes in software, at up
  to 30 frames per second.

## 1.6.0

### Minor Changes

- ec33946: File transfer: browse this machine's files from the viewer, download files and folders (as a zip), upload files and folders, create folders, rename, and move things to the Trash or Recycle Bin. Transfers resume after a dropped connection and back off so the screen stays smooth. On a Mac, macOS asks once per folder (Desktop, Documents, Downloads) unless RD Agent has Full Disk Access.

## 1.5.0

### Minor Changes

- c374fa2: Update agents from the portal: a device whose agent is out of date shows **Update to X.Y.Z** with what's new and an **Update** button. The agent installs the new version itself and puts the previous one back if the new one doesn't reconnect within two minutes. The agent now has its own releases (`agent-vX.Y.Z`), reports its real version, and macOS gets a ready-made, signed `RD Agent.app` download. Agents up to 1.4.2 need to be updated by hand once.
