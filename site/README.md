# rd documentation site

The site published to GitHub Pages (`https://thijmen.github.io/remoted/`), built with
[Astro Starlight](https://starlight.astro.build/) and Bun. Astro 7 needs Node 22.22 or later.

```sh
bun install --frozen-lockfile
bun run dev       # http://localhost:4321/remoted/ (live reload, also when docs/*.md change)
bun run build     # → dist/
bun run preview   # serve dist/
```

## Where the pages come from

| Pages | Source | Edit |
|---|---|---|
| Home, Introduction, Quick start, How it works, Using rd, Development | `src/content/docs/*.mdx` | here |
| Gateway, Mac agent, Windows agent, Devices, rd Desktop, Updates, Recovery, Wire protocol, Desktop bridge, Viewer internals | `docs/*.md`, `clients/desktop/README.md`, `viewer/README.md` | the source file |

The second group is synced, not copied by hand: `integrations/sync-docs.mjs` writes each file
listed in `integrations/pages.mjs` to `src/content/docs/<slug>.md` (title and description
from `pages.mjs`, the edit link and last-updated date from the source) every time Astro starts.
Those folders (`src/content/docs/guides/`, `src/content/docs/reference/`) are generated and
ignored by git: don't put hand-written pages there. To publish another Markdown file from the
repository, add it to `pages.mjs` and to the sidebar in `astro.config.mjs`.

`integrations/markdown-links.mjs` handles links:

- in synced pages, relative links are resolved against the source file, so
  `[updates.md](updates.md#signing)` becomes `/remoted/guides/updates/#signing`, and a link to a
  file that is not a page (code, scripts, workflows) points to it on GitHub;
- in every page, root-relative links (`/quick-start/`) get the base path. Write links that way.
  In MDX components, use `src/components/LinkCard.astro` (it does the same for `href`), or
  `import.meta.env.BASE_URL`.

## Hosting

`.github/workflows/pages.yml` builds the site on pull requests that touch it (or the synced
files) and deploys it from `main`. It passes the origin and base path that GitHub reports as
`SITE_ORIGIN` and `SITE_BASE_PATH`, so a custom domain needs no change here. One-time setup:
Settings → Pages → Source: **GitHub Actions**.

## Screenshots

`src/assets/screenshots/*.webp` are the real portal and viewer, taken with headless Chrome
(`playwright-core`, which uses the installed Google Chrome) at 2× from a dev gateway with a
test-pattern agent. To retake them, from the repository root:

```sh
(cd viewer && bun install && bun run build)
cargo build -p rd-gateway -p rd-agent
mkdir -p /tmp/rd-shots/state /tmp/rd-shots/logs
printf '[{"machine":"mbp","name":"MacBook Pro"},{"machine":"studio","name":"Mac Studio","recovery":[{"label":"Screen Sharing (VNC)","url":"https://guac.example.com/"}]}]' > /tmp/rd-shots/machines.json
printf 'gateway_url = "ws://127.0.0.1:18581"\nmachine = "mbp"\n\n[update]\ncheck = false\n' > /tmp/rd-shots/config.toml

target/debug/rd-gateway --dev --viewer-addr 127.0.0.1:18580 --agent-addr 127.0.0.1:18581 \
  --metrics-addr 127.0.0.1:18590 --assets-dir viewer/dist --state-dir /tmp/rd-shots/state \
  --machines-file /tmp/rd-shots/machines.json --agent-public-url wss://rd-agent.example.com &
RD_AGENT_TOKEN=rd1.mbp.dev RD_AGENT_LOG_DIR=/tmp/rd-shots/logs \
  target/debug/rd-agent run --config /tmp/rd-shots/config.toml --source synthetic &

cd site && bun run screenshots            # or: bun run screenshots portal session
```

The temporary config and log folder keep the test agent away from a real agent's files on the
same machine. The script shows `you@example.com` instead of the dev user, and adds and removes a
"Windows PC" device for the *Add device* shot.
