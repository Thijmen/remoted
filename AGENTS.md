# Notes for coding agents

## Every change gets a changeset

Each PR includes a changeset: a `.changeset/*.md` file, committed with the change. Releases of
rd Desktop and the rd agent only happen through them. Merging to `main` opens (or updates) the
**Version packages** PR (`.github/workflows/release.yml`); merging that PR tags
`desktop-vX.Y.Z` and/or `agent-vX.Y.Z` and publishes those releases, which the installed apps
and agents then offer as updates ([docs/updates.md](docs/updates.md)). (The gateway image is
versioned separately: `.github/workflows/gateway-image.yml` builds and tags `gateway-vX.Y.Z` on
every push to `main` that touches the gateway, protocol, flow control or viewer.)

- Create one from the repository root with `bun install && bun changeset` (pick the package,
  then the bump), or write the file by hand:

  ```md
  ---
  "rd-desktop": minor
  ---

  One or two sentences for the changelog, written for the people who use rd.
  ```

- Changesets knows two packages (`lerna.json`); name the one whose users notice the change:
  - `rd-desktop` (`clients/desktop`): the desktop app. Its users see these notes in the app's
    update window.
  - `rd-agent` (`crates/rd-agent`): the agent on the machines you connect to (also for changes
    in `rd-agent-core`, `rd-platform-*`). Its notes show in the portal's **What's new**.
  - A change to both names both. A change only to the gateway or the viewer ships with the
    gateway image, not with an install: give it an empty changeset (`bun changeset --empty`),
    so nobody is asked to update an app or agent that did not change.
- Bump: `patch` for fixes, `minor` for new features, `major` for incompatible changes.
- Never edit the versions by hand: `bun run version` (what the Version packages PR runs) bumps
  `package.json` and `tools/release/sync-versions.ts` copies it into the Rust crates; CI checks
  they agree.

Details: [clients/desktop/README.md](clients/desktop/README.md#releasing),
[docs/updates.md](docs/updates.md).
