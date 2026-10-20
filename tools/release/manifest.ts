// Writes an update manifest (docs/updates.md): Tauri's static update JSON for one release, plus
// the release notes of every version from the package's CHANGELOG.md.
//
//   bun tools/release/manifest.ts --package rd-desktop --version 1.6.0 \
//     --platform windows-x86_64=out/rd-desktop-1.6.0-x64-setup.exe \
//     --platform darwin-aarch64,darwin-x86_64=out/rd-desktop-1.6.0-macos-universal.app.tar.gz \
//     --out desktop.json
//
// Each platform file needs its signature next to it (`<file>.sig`, as `tauri signer sign`
// writes it). URLs point at the package's GitHub Release (`--repo`, default $GITHUB_REPOSITORY).

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

const root = join(import.meta.dir, "..", "..");

/** Released packages: their changelog and tag prefix. */
export const PACKAGES: Record<string, { changelog: string; tagPrefix: string }> = {
  "rd-desktop": { changelog: "clients/desktop/CHANGELOG.md", tagPrefix: "desktop-v" },
  "rd-agent": { changelog: "crates/rd-agent/CHANGELOG.md", tagPrefix: "agent-v" },
};

/** At most this many versions of notes go into a manifest. */
export const MAX_RELEASES = 30;

export interface Release {
  version: string;
  notes: string;
  url: string;
}

export interface Manifest {
  version: string;
  notes: string;
  pub_date: string;
  platforms: Record<string, { url: string; signature: string }>;
  releases: Release[];
}

/**
 * The sections of a changesets CHANGELOG.md (`## 1.2.0` headings), newest first as written,
 * cleaned for people who use the app: no `### Minor Changes`-style headings and no commit
 * hashes in front of the entries.
 */
export function changelogSections(markdown: string): { version: string; notes: string }[] {
  const sections: { version: string; lines: string[] }[] = [];
  for (const line of markdown.split(/\r?\n/)) {
    const heading = /^## (\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\s*$/.exec(line);
    if (heading) {
      sections.push({ version: heading[1], lines: [] });
    } else if (sections.length > 0) {
      sections[sections.length - 1].lines.push(line);
    }
  }
  return sections.map(({ version, lines }) => ({ version, notes: cleanNotes(lines) }));
}

function cleanNotes(lines: string[]): string {
  const kept = lines
    .filter((l) => !/^###\s+(Major|Minor|Patch)\s+Changes\s*$/i.test(l))
    // "- 2b1bbb7: Text" -> "- Text" (changesets puts the commit in front of every entry).
    .map((l) => l.replace(/^(\s*[-*]\s+)[0-9a-f]{7,40}:\s+/, "$1"));
  // Collapse runs of blank lines and trim the ends.
  const out: string[] = [];
  for (const l of kept) {
    const blank = l.trim() === "";
    if (blank && (out.length === 0 || out[out.length - 1] === "")) continue;
    out.push(blank ? "" : l.trimEnd());
  }
  while (out.length > 0 && out[out.length - 1] === "") out.pop();
  return out.join("\n");
}

export function releaseUrl(repo: string, tag: string): string {
  return `https://github.com/${repo}/releases/tag/${encodeURIComponent(tag)}`;
}

export function assetUrl(repo: string, tag: string, file: string): string {
  return `https://github.com/${repo}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(basename(file))}`;
}

export interface ManifestInput {
  pkg: string;
  version: string;
  repo: string;
  changelog: string;
  /** Platform keys (e.g. `darwin-aarch64`) -> the signed file. */
  platforms: Record<string, { file: string; signature: string }>;
  pubDate: Date;
}

export function buildManifest(input: ManifestInput): Manifest {
  const { tagPrefix } = PACKAGES[input.pkg] ?? fail(`unknown package ${input.pkg}`);
  const sections = changelogSections(input.changelog);
  const current = sections.find((s) => s.version === input.version);
  if (!current) fail(`the changelog has no "## ${input.version}" section`);
  // Only versions up to the one being released (a changelog may already list a later one).
  const releases = sections
    .filter((s) => compareVersions(s.version, input.version) <= 0)
    .sort((a, b) => compareVersions(b.version, a.version))
    .slice(0, MAX_RELEASES)
    .map((s) => ({ version: s.version, notes: s.notes, url: releaseUrl(input.repo, tagPrefix + s.version) }));
  const tag = tagPrefix + input.version;
  const platforms: Manifest["platforms"] = {};
  for (const [key, { file, signature }] of Object.entries(input.platforms)) {
    if (!/^(windows|darwin|linux)-(x86_64|aarch64|i686|armv7)$/.test(key)) fail(`bad platform key ${key}`);
    const signed = signedVersion(signature);
    if (signed !== input.version) {
      // The clients refuse a signature that does not name the offered version
      // (requireSignedVersion), so publishing this would stop every update.
      fail(`${basename(file)}: the signature is for version ${signed ?? "(none)"}, not ${input.version}`);
    }
    platforms[key] = { url: assetUrl(input.repo, tag, file), signature: signature.trim() };
  }
  if (Object.keys(platforms).length === 0) fail("no --platform given");
  return {
    version: input.version,
    notes: current.notes,
    // RFC 3339 without milliseconds, as Tauri's examples.
    pub_date: input.pubDate.toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms,
    releases,
  };
}

/**
 * The `version:` field of a signature's trusted comment. `signature` is a `.sig` file as Tauri
 * writes it: base64 of a minisign signature file, whose trusted comment `tauri build` and
 * `tauri signer sign --app-version` fill with `timestamp:…\tfile:…\tversion:X.Y.Z`.
 */
export function signedVersion(signature: string): string | undefined {
  let text: string;
  try {
    text = Buffer.from(signature.trim(), "base64").toString("utf8");
  } catch {
    return undefined;
  }
  const trusted = text.split(/\r?\n/).find((l) => l.startsWith("trusted comment: "));
  const field = trusted
    ?.slice("trusted comment: ".length)
    .split("\t")
    .find((f) => f.startsWith("version:"));
  return field?.slice("version:".length);
}

/** Semver precedence for X.Y.Z[-pre] (build metadata is not used here). */
export function compareVersions(a: string, b: string): number {
  const parse = (v: string) => {
    const m = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/.exec(v) ?? fail(`not a version: ${v}`);
    return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4]?.split(".") ?? [] };
  };
  const x = parse(a);
  const y = parse(b);
  for (let i = 0; i < 3; i++) if (x.core[i] !== y.core[i]) return x.core[i] - y.core[i];
  // A pre-release is lower than the release.
  if (x.pre.length === 0 || y.pre.length === 0) return y.pre.length - x.pre.length;
  for (let i = 0; i < Math.min(x.pre.length, y.pre.length); i++) {
    const [p, q] = [x.pre[i], y.pre[i]];
    if (p === q) continue;
    const [pn, qn] = [/^\d+$/.test(p), /^\d+$/.test(q)];
    if (pn && qn) return Number(p) - Number(q);
    if (pn !== qn) return pn ? -1 : 1;
    return p < q ? -1 : 1;
  }
  return x.pre.length - y.pre.length;
}

function fail(msg: string): never {
  throw new Error(msg);
}

function parseArgs(argv: string[]) {
  const args = { pkg: "", version: "", out: "", repo: process.env.GITHUB_REPOSITORY ?? "", platforms: [] as string[] };
  for (let i = 0; i < argv.length; i++) {
    const value = () => argv[++i] ?? fail(`${argv[i - 1]} needs a value`);
    switch (argv[i]) {
      case "--package":
        args.pkg = value();
        break;
      case "--version":
        args.version = value();
        break;
      case "--out":
        args.out = value();
        break;
      case "--repo":
        args.repo = value();
        break;
      case "--platform":
        args.platforms.push(value());
        break;
      default:
        fail(`unknown argument ${argv[i]}`);
    }
  }
  if (!args.pkg || !args.version || !args.out || !args.repo) {
    fail("usage: manifest.ts --package <name> --version <x.y.z> --platform <keys>=<file> ... --out <file> [--repo owner/name]");
  }
  return args;
}

if (import.meta.main) {
  const args = parseArgs(process.argv.slice(2));
  const pkg = PACKAGES[args.pkg] ?? fail(`unknown package ${args.pkg}`);
  const platforms: ManifestInput["platforms"] = {};
  for (const spec of args.platforms) {
    const eq = spec.indexOf("=");
    if (eq < 0) fail(`--platform wants <keys>=<file>, got ${spec}`);
    const file = spec.slice(eq + 1);
    const sigPath = `${file}.sig`;
    if (!existsSync(file)) fail(`${file} does not exist`);
    if (!existsSync(sigPath)) fail(`${sigPath} does not exist (sign the file first)`);
    const signature = readFileSync(sigPath, "utf8");
    for (const key of spec.slice(0, eq).split(",")) platforms[key.trim()] = { file, signature };
  }
  const manifest = buildManifest({
    pkg: args.pkg,
    version: args.version,
    repo: args.repo,
    changelog: readFileSync(join(root, pkg.changelog), "utf8"),
    platforms,
    pubDate: new Date(),
  });
  writeFileSync(args.out, `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`${args.out}: ${args.pkg} ${manifest.version}, ${Object.keys(manifest.platforms).join(", ")}, notes of ${manifest.releases.length} versions`);
}
