// Copies the rd Desktop version from clients/desktop/package.json (bumped by `changeset version`)
// into the Tauri crate, which is where the app, the bundles and the desktop-v* tag read it from.
//
//   bun tools/release/sync-desktop-version.ts           write Cargo.toml and Cargo.lock
//   bun tools/release/sync-desktop-version.ts --check   fail if they differ from package.json

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..", "..");
const pkgPath = join(root, "clients/desktop/package.json");
const tomlPath = join(root, "clients/desktop/src-tauri/Cargo.toml");
const lockPath = join(root, "clients/desktop/src-tauri/Cargo.lock");
const check = process.argv.includes("--check");

const version: string = JSON.parse(readFileSync(pkgPath, "utf8")).version;
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`${pkgPath}: not a semver version: ${version}`);
}

// The first `version = "..."` line in Cargo.toml is the [package] one.
const tomlRe = /^version = "([^"]*)"$/m;
// The rd-desktop entry in Cargo.lock: `name = "rd-desktop"` followed by its version line.
const lockRe = /^(name = "rd-desktop"\nversion = )"([^"]*)"$/m;

const toml = readFileSync(tomlPath, "utf8");
const lock = readFileSync(lockPath, "utf8");
const tomlVersion = toml.match(tomlRe)?.[1];
const lockVersion = lock.match(lockRe)?.[2];
if (tomlVersion === undefined) throw new Error(`${tomlPath}: no version line`);
if (lockVersion === undefined) throw new Error(`${lockPath}: no rd-desktop entry`);

if (check) {
  const stale = [
    tomlVersion !== version && `Cargo.toml has ${tomlVersion}`,
    lockVersion !== version && `Cargo.lock has ${lockVersion}`,
  ].filter(Boolean);
  if (stale.length > 0) {
    console.error(
      `rd Desktop version mismatch: package.json has ${version}, ${stale.join(", ")}.\n` +
        `Run: bun tools/release/sync-desktop-version.ts`,
    );
    process.exit(1);
  }
  console.log(`rd Desktop ${version}: package.json, Cargo.toml and Cargo.lock agree`);
} else {
  writeFileSync(tomlPath, toml.replace(tomlRe, `version = "${version}"`));
  writeFileSync(lockPath, lock.replace(lockRe, `$1"${version}"`));
  console.log(`rd Desktop ${version}: wrote Cargo.toml and Cargo.lock`);
}
