// Writes android.json (docs/android.md): the update manifest the Android app reads. It is not
// Tauri's updater JSON, but keeps the same `version`/`platforms`/`releases` shape so the shared
// helpers and `tools/release/publish-manifest.sh` work with it. The APK's own signature is the
// trust anchor (Android refuses an update signed with another key); `sha256` catches a broken
// download and `publish-manifest.sh` refuses a manifest that does not parse.
//
//   bun tools/release/android-manifest.ts --version 0.2.0 \
//     --apk out/rd-desktop-0.2.0-android-arm64-armv7.apk --out android.json
//
// The version is the `rd-android` package version; versionCode is what Tauri derives from it
// (major * 1_000_000 + minor * 1_000 + patch). Notes come from clients/android/CHANGELOG.md
// when it exists (the first release is cut before changesets has written one).

import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

import {
  MAX_RELEASES,
  type Release,
  assetUrl,
  changelogSections,
  compareVersions,
  releaseUrl,
} from "./manifest";

const root = join(import.meta.dir, "..", "..");
const PACKAGE = "rd-android";
const TAG_PREFIX = "android-v";
const CHANGELOG = "clients/android/CHANGELOG.md";
/** The one platform key in the manifest: both ABIs travel in one APK. */
export const PLATFORM = "android-universal";

export interface AndroidManifest {
  version: string;
  version_code: number;
  notes: string;
  pub_date: string;
  platforms: Record<string, { url: string; sha256: string }>;
  releases: Release[];
}

/** Tauri's Android versionCode for `X.Y.Z` (build metadata and pre-release are ignored). */
export function versionCode(version: string): number {
  const m = /^(\d+)\.(\d+)\.(\d+)/.exec(version);
  if (!m) throw new Error(`not a version: ${version}`);
  return Number(m[1]) * 1_000_000 + Number(m[2]) * 1_000 + Number(m[3]);
}

export interface AndroidManifestInput {
  version: string;
  repo: string;
  /** The APK's bytes (only for its digest). */
  apk: Uint8Array;
  apkName: string;
  changelog: string;
  pubDate: Date;
}

export function buildAndroidManifest(input: AndroidManifestInput): AndroidManifest {
  const code = versionCode(input.version);
  const tag = TAG_PREFIX + input.version;
  const sections = changelogSections(input.changelog);
  const current = sections.find((s) => s.version === input.version);
  const releases = sections
    .filter((s) => compareVersions(s.version, input.version) <= 0)
    .sort((a, b) => compareVersions(b.version, a.version))
    .slice(0, MAX_RELEASES)
    .map((s) => ({
      version: s.version,
      notes: s.notes,
      url: releaseUrl(input.repo, TAG_PREFIX + s.version),
    }));
  return {
    version: input.version,
    version_code: code,
    notes: current?.notes ?? "",
    pub_date: input.pubDate.toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms: {
      [PLATFORM]: {
        // `assetUrl` uses the basename, which is the name the release workflow uploads.
        url: assetUrl(input.repo, tag, input.apkName),
        sha256: createHash("sha256").update(input.apk).digest("hex"),
      },
    },
    releases,
  };
}

export function buildFromFiles(args: {
  version: string;
  repo: string;
  apk: string;
  changelogPath?: string;
  pubDate?: Date;
}): AndroidManifest {
  if (!existsSync(args.apk)) throw new Error(`${args.apk} does not exist`);
  const changelogPath = args.changelogPath ?? join(root, CHANGELOG);
  return buildAndroidManifest({
    version: args.version,
    repo: args.repo,
    apk: readFileSync(args.apk),
    apkName: basename(args.apk),
    changelog: existsSync(changelogPath) ? readFileSync(changelogPath, "utf8") : "",
    pubDate: args.pubDate ?? new Date(),
  });
}

function parseArgs(argv: string[]) {
  const args = {
    version: "",
    apk: "",
    out: "",
    repo: process.env.GITHUB_REPOSITORY ?? "",
    changelog: "",
  };
  for (let i = 0; i < argv.length; i++) {
    const value = () => argv[++i] ?? fail(`${argv[i - 1]} needs a value`);
    switch (argv[i]) {
      case "--version":
        args.version = value();
        break;
      case "--apk":
        args.apk = value();
        break;
      case "--out":
        args.out = value();
        break;
      case "--repo":
        args.repo = value();
        break;
      case "--changelog":
        args.changelog = value();
        break;
      default:
        fail(`unknown argument ${argv[i]}`);
    }
  }
  if (!args.version || !args.apk || !args.out || !args.repo) {
    fail(
      "usage: android-manifest.ts --version <x.y.z> --apk <file> --out <file> [--repo owner/name] [--changelog <file>]",
    );
  }
  return args;
}

function fail(msg: string): never {
  throw new Error(msg);
}

if (import.meta.main) {
  const args = parseArgs(process.argv.slice(2));
  const manifest = buildFromFiles({
    version: args.version,
    repo: args.repo,
    apk: args.apk,
    changelogPath: args.changelog || undefined,
  });
  writeFileSync(args.out, JSON.stringify(manifest, null, 2) + "\n");
  console.log(`wrote ${args.out} (${PACKAGE} ${manifest.version}, code ${manifest.version_code})`);
}
