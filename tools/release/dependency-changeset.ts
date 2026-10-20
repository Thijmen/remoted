// Adds a changeset to a Renovate pull request, so dependency updates get released like any
// other change (AGENTS.md). Run by .github/workflows/dependency-changesets.yml.
//
// It decides which released package ships the update (PACKAGES in sync-versions.ts):
//
//   - a file in the package's own directory changed (clients/desktop/**, crates/rd-agent/**);
//   - the package's Cargo.lock changed a crate the package depends on;
//   - a workspace crate the package depends on changed (crates/<crate>/**);
//   - rust-toolchain.toml changed (it builds both).
//
// Each gets a patch bump. An update that ships in neither (gateway, viewer, CI, tooling) gets an
// empty changeset, so the PR still has one but nobody is asked to update.
//
// It only talks to the GitHub API and never runs code from the pull request.
//
//   PR_NUMBER=32 GITHUB_REPOSITORY=owner/repo GH_TOKEN=... bun tools/release/dependency-changeset.ts
//
// With DRY_RUN=1 it prints the changeset instead of committing it.

import { dirname } from "node:path";

import { PACKAGES } from "./sync-versions";

export const changesetPath = (pr: number) => `.changeset/renovate-${pr}.md`;

/** A Cargo.lock entry. Dependencies are as written: "name", "name version" or "name version (source)". */
interface LockPackage {
  name: string;
  version: string;
  source?: string;
  dependencies: string[];
}

export function parseCargoLock(text: string): LockPackage[] {
  const packages: LockPackage[] = [];
  let current: LockPackage | null = null;
  let inDeps = false;
  for (const line of text.split("\n")) {
    if (line === "[[package]]") {
      current = { name: "", version: "", dependencies: [] };
      packages.push(current);
      inDeps = false;
      continue;
    }
    if (!current) continue;
    if (inDeps) {
      if (line.startsWith("]")) inDeps = false;
      else {
        const m = line.match(/^\s*"(.*)",?$/);
        if (m) current.dependencies.push(m[1]);
      }
      continue;
    }
    const kv = line.match(/^(\w+) = (.*)$/);
    if (!kv) continue;
    const [, key, value] = kv;
    if (key === "name") current.name = JSON.parse(value);
    else if (key === "version") current.version = JSON.parse(value);
    else if (key === "source") current.source = JSON.parse(value);
    else if (key === "dependencies") {
      if (value === "[") inDeps = true;
      else current.dependencies.push(...(JSON.parse(value) as string[]));
    }
  }
  return packages;
}

const lockKey = (p: LockPackage) => `${p.name} ${p.version} ${p.source ?? ""}`;

/** Every package `root` depends on, itself included (all targets and dependency kinds). */
export function closure(lock: LockPackage[], root: string): LockPackage[] {
  const resolve = (dep: string): LockPackage[] => {
    const [name, version] = dep.split(" ");
    return lock.filter((p) => p.name === name && (version === undefined || p.version === version));
  };
  const seen = new Map<string, LockPackage>();
  const queue = lock.filter((p) => p.name === root && p.source === undefined);
  while (queue.length > 0) {
    const p = queue.pop()!;
    if (seen.has(lockKey(p))) continue;
    seen.set(lockKey(p), p);
    for (const dep of p.dependencies) queue.push(...resolve(dep));
  }
  return [...seen.values()];
}

/** The lock files of a released package, before and after the pull request (null: missing). */
export interface Locks {
  base: string | null;
  head: string | null;
}

/** Which released packages a change to `files` ships in. */
export function affectedPackages(files: string[], locks: Record<string, Locks>): string[] {
  const affected: string[] = [];
  for (const pkg of PACKAGES) {
    const dir = `${dirname(pkg.packageJson)}/`;
    const { base, head } = locks[pkg.name] ?? { base: null, head: null };
    const baseLock = base === null ? [] : parseCargoLock(base);
    const headLock = head === null ? [] : parseCargoLock(head);
    const deps = [...closure(baseLock, pkg.name), ...closure(headLock, pkg.name)];
    const depKeys = new Set(deps.map(lockKey));
    // Workspace crates (no source) live in crates/<name>.
    const crateDirs = new Set(deps.filter((p) => p.source === undefined).map((p) => `crates/${p.name}/`));

    const baseKeys = new Set(baseLock.map(lockKey));
    const headKeys = new Set(headLock.map(lockKey));
    const lockChangedDep =
      files.includes(pkg.cargoLock) &&
      [...depKeys].some((key) => baseKeys.has(key) !== headKeys.has(key));

    const ships =
      lockChangedDep ||
      files.some(
        (f) =>
          f.startsWith(dir) ||
          f === "rust-toolchain.toml" ||
          [...crateDirs].some((crateDir) => f.startsWith(crateDir)),
      );
    if (ships) affected.push(pkg.name);
  }
  return affected;
}

export function changesetContent(packages: string[], title: string): string {
  const frontmatter = packages.map((p) => `"${p}": patch\n`).join("");
  return `---\n${frontmatter}---\n\n${title}\n`;
}

// --- GitHub API -------------------------------------------------------------------------------

const repo = process.env.GITHUB_REPOSITORY;
const token = process.env.GH_TOKEN;

async function api(path: string, init: RequestInit = {}, accept = "application/vnd.github+json") {
  const res = await fetch(`https://api.github.com/repos/${repo}${path}`, {
    ...init,
    headers: {
      Accept: accept,
      Authorization: `Bearer ${token}`,
      "X-GitHub-Api-Version": "2022-11-28",
      ...init.headers,
    },
  });
  return res;
}

async function json<T>(path: string): Promise<T> {
  const res = await api(path);
  if (!res.ok) throw new Error(`GET ${path}: ${res.status} ${await res.text()}`);
  return (await res.json()) as T;
}

/** A file's contents at `ref`, or null when it does not exist there. */
async function fileAt(path: string, ref: string): Promise<string | null> {
  const res = await api(
    `/contents/${path}?ref=${encodeURIComponent(ref)}`,
    {},
    "application/vnd.github.raw+json",
  );
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`GET ${path}@${ref}: ${res.status} ${await res.text()}`);
  return res.text();
}

interface PullRequest {
  number: number;
  state: string;
  title: string;
  head: { ref: string; sha: string; repo: { full_name: string } | null };
  base: { sha: string };
}

async function main() {
  const number = Number(process.env.PR_NUMBER);
  if (!repo || !token || !Number.isInteger(number)) {
    throw new Error("Needs PR_NUMBER, GITHUB_REPOSITORY and GH_TOKEN");
  }
  const pr = await json<PullRequest>(`/pulls/${number}`);
  if (pr.state !== "open" || pr.head.repo?.full_name !== repo) {
    console.log("Not an open pull request from this repository; nothing to do.");
    return;
  }

  const files: string[] = [];
  for (let page = 1; ; page++) {
    const batch = await json<{ filename: string }[]>(`/pulls/${number}/files?per_page=100&page=${page}`);
    files.push(...batch.map((f) => f.filename));
    if (batch.length < 100) break;
  }

  const path = changesetPath(number);
  const others = files.filter((f) => /^\.changeset\/.*\.md$/.test(f) && f !== path);
  if (others.length > 0) {
    console.log(`The pull request already has a changeset (${others.join(", ")}); leaving it alone.`);
    return;
  }
  const changed = files.filter((f) => f !== path);

  // Only fetch the lock files when a Rust change could matter.
  const locks: Record<string, Locks> = {};
  if (changed.some((f) => f.endsWith("Cargo.lock") || f.startsWith("crates/"))) {
    for (const pkg of PACKAGES) {
      locks[pkg.name] = {
        base: await fileAt(pkg.cargoLock, pr.base.sha),
        head: await fileAt(pkg.cargoLock, pr.head.sha),
      };
    }
  }

  const packages = affectedPackages(changed, locks);
  const content = changesetContent(packages, pr.title);
  console.log(`Ships in: ${packages.join(", ") || "nothing (empty changeset)"}`);
  if (process.env.DRY_RUN) {
    console.log(`${path}:\n${content}`);
    return;
  }

  const existing = await api(`/contents/${path}?ref=${encodeURIComponent(pr.head.ref)}`);
  let sha: string | undefined;
  if (existing.ok) {
    const file = (await existing.json()) as { sha: string; content: string };
    if (Buffer.from(file.content, "base64").toString("utf8") === content) {
      console.log(`${path} is up to date.`);
      return;
    }
    sha = file.sha;
  } else if (existing.status !== 404) {
    throw new Error(`GET ${path}: ${existing.status} ${await existing.text()}`);
  }

  // Renovate ignores commits by this author (gitIgnoredAuthors in renovate.json), so it keeps
  // rebasing the branch; when it does, this workflow runs again and re-adds the changeset.
  const bot = { name: "github-actions[bot]", email: "41898282+github-actions[bot]@users.noreply.github.com" };
  const res = await api(`/contents/${path}`, {
    method: "PUT",
    body: JSON.stringify({
      message: "Add changeset for dependency update",
      content: Buffer.from(content).toString("base64"),
      branch: pr.head.ref,
      sha,
      author: bot,
      committer: bot,
    }),
  });
  if (!res.ok) throw new Error(`PUT ${path}: ${res.status} ${await res.text()}`);
  console.log(`Wrote ${path} to ${pr.head.ref}.`);
}

if (import.meta.main) await main();
