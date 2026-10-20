// Copies the version of each released package from its package.json (bumped by `changeset
// version`) into its Rust crate, which is where the binaries, the bundles and the tags read it
// from:
//
//   rd-desktop  clients/desktop/package.json -> clients/desktop/src-tauri/Cargo.toml + its Cargo.lock
//   rd-agent    crates/rd-agent/package.json -> crates/rd-agent/Cargo.toml + the workspace Cargo.lock
//
//   bun tools/release/sync-versions.ts           write Cargo.toml and Cargo.lock
//   bun tools/release/sync-versions.ts --check   fail if they differ from package.json
//   bun tools/release/sync-versions.ts --print rd-agent   print a package's version

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "..", "..");

/** A released package and where its version is copied to. */
interface Package {
  name: string;
  packageJson: string;
  cargoToml: string;
  cargoLock: string;
}

export const PACKAGES: Package[] = [
  {
    name: "rd-desktop",
    packageJson: "clients/desktop/package.json",
    cargoToml: "clients/desktop/src-tauri/Cargo.toml",
    cargoLock: "clients/desktop/src-tauri/Cargo.lock",
  },
  {
    name: "rd-agent",
    packageJson: "crates/rd-agent/package.json",
    cargoToml: "crates/rd-agent/Cargo.toml",
    cargoLock: "Cargo.lock",
  },
];

// The first `version = "..."` line in Cargo.toml is the [package] one.
const tomlRe = /^version = "([^"]*)"$/m;
// The crate's entry in Cargo.lock: `name = "<crate>"` followed by its version line.
const lockRe = (crate: string) => new RegExp(`^(name = "${crate}"\\nversion = )"([^"]*)"$`, "m");

export function packageVersion(pkg: Package): string {
  const path = join(root, pkg.packageJson);
  const version: string = JSON.parse(readFileSync(path, "utf8")).version;
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`${path}: not a semver version: ${version}`);
  }
  return version;
}

/** Returns what is out of date (empty when in sync); with `write`, fixes it. */
function sync(pkg: Package, write: boolean): string[] {
  const version = packageVersion(pkg);
  const tomlPath = join(root, pkg.cargoToml);
  const lockPath = join(root, pkg.cargoLock);
  // A Windows checkout may have CRLF line endings; the version lines below are matched with
  // LF, and a write normalizes the file (Cargo accepts either).
  const toml = readFileSync(tomlPath, "utf8").replace(/\r\n/g, "\n");
  const lock = readFileSync(lockPath, "utf8").replace(/\r\n/g, "\n");
  const tomlVersion = toml.match(tomlRe)?.[1];
  const lockVersion = lock.match(lockRe(pkg.name))?.[2];
  if (tomlVersion === undefined) throw new Error(`${tomlPath}: no version line`);
  if (lockVersion === undefined) throw new Error(`${lockPath}: no ${pkg.name} entry`);
  const stale = [
    tomlVersion !== version && `${pkg.cargoToml} has ${tomlVersion}`,
    lockVersion !== version && `${pkg.cargoLock} has ${lockVersion}`,
  ].filter((s): s is string => Boolean(s));
  if (write && stale.length > 0) {
    writeFileSync(tomlPath, toml.replace(tomlRe, `version = "${version}"`));
    writeFileSync(lockPath, lock.replace(lockRe(pkg.name), `$1"${version}"`));
  }
  return stale;
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  if (args[0] === "--print") {
    const pkg = PACKAGES.find((p) => p.name === args[1]);
    if (!pkg) throw new Error(`unknown package ${args[1]}; one of ${PACKAGES.map((p) => p.name).join(", ")}`);
    console.log(packageVersion(pkg));
    process.exit(0);
  }
  // A beta build (`.github/workflows/beta.yml`) stamps its own version into a checkout without
  // touching the committed one: set package.json, then copy it into Cargo.toml and Cargo.lock.
  if (args[0] === "--set") {
    const pkg = PACKAGES.find((p) => p.name === args[1]);
    if (!pkg) throw new Error(`unknown package ${args[1]}; one of ${PACKAGES.map((p) => p.name).join(", ")}`);
    const version = args[2];
    if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
      throw new Error(`not a semver version: ${version ?? ""}`);
    }
    const path = join(root, pkg.packageJson);
    const json = JSON.parse(readFileSync(path, "utf8"));
    json.version = version;
    writeFileSync(path, JSON.stringify(json, null, 2) + "\n");
    sync(pkg, true);
    console.log(`${pkg.name} ${version}: wrote ${pkg.packageJson}, ${pkg.cargoToml} and ${pkg.cargoLock}`);
    process.exit(0);
  }
  const check = args.includes("--check");
  let failed = false;
  for (const pkg of PACKAGES) {
    const version = packageVersion(pkg);
    const stale = sync(pkg, !check);
    if (stale.length === 0) {
      console.log(`${pkg.name} ${version}: package.json, Cargo.toml and Cargo.lock agree`);
    } else if (check) {
      console.error(`${pkg.name} version mismatch: package.json has ${version}, ${stale.join(", ")}.`);
      failed = true;
    } else {
      console.log(`${pkg.name} ${version}: wrote ${pkg.cargoToml} and ${pkg.cargoLock}`);
    }
  }
  if (failed) {
    console.error("Run: bun tools/release/sync-versions.ts");
    process.exit(1);
  }
}
