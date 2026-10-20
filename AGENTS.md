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

## Documentation site

`site/` (Astro Starlight, `bun run dev` / `bun run build` there) is published to GitHub Pages by
`.github/workflows/pages.yml`. Most of its guides are the Markdown files in `docs/`,
`clients/desktop/README.md` and `viewer/README.md`, synced at build time
(`site/integrations/pages.mjs`): keep editing those files, and use relative links between them
as before. A new file under `docs/` is published once it is added to `pages.mjs`; add it to the
sidebar in `site/astro.config.mjs` too, so readers can find it. The hand-written pages (home,
Introduction, Quick start, How it works, Using rd, Development) are in `site/src/content/docs/`;
update them when a change makes them wrong. Details: [site/README.md](site/README.md).
