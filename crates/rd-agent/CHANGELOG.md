# rd-agent

## 1.6.0

### Minor Changes

- ec33946: File transfer: browse this machine's files from the viewer, download files and folders (as a zip), upload files and folders, create folders, rename, and move things to the Trash or Recycle Bin. Transfers resume after a dropped connection and back off so the screen stays smooth. On a Mac, macOS asks once per folder (Desktop, Documents, Downloads) unless RD Agent has Full Disk Access.

## 1.5.0

### Minor Changes

- c374fa2: Update agents from the portal: a device whose agent is out of date shows **Update to X.Y.Z** with what's new and an **Update** button. The agent installs the new version itself and puts the previous one back if the new one doesn't reconnect within two minutes. The agent now has its own releases (`agent-vX.Y.Z`), reports its real version, and macOS gets a ready-made, signed `RD Agent.app` download. Agents up to 1.4.2 need to be updated by hand once.
