# Notes for coding agents

## Every change gets a changeset

Each PR includes a changeset: a `.changeset/*.md` file, committed with the change. Releases only
happen through them. Merging to `main` opens (or updates) the **Version packages** PR
(`.github/workflows/release.yml`); merging that PR tags `desktop-vX.Y.Z` and publishes the
release.

- Create one from the repository root with `bun install && bun changeset` (pick `rd-desktop`,
  then the bump), or write the file by hand:

  ```md
  ---
  "rd-desktop": minor
  ---

  One or two sentences for the changelog, written for the people who use rd.
  ```

- `rd-desktop` is the only package changesets knows (`lerna.json`), so every changeset names it.
- Bump: `patch` for fixes, `minor` for new features, `major` for incompatible changes.

Details: [clients/desktop/README.md](clients/desktop/README.md#releasing).
