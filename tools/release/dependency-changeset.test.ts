import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { affectedPackages, changesetContent, closure, parseCargoLock } from "./dependency-changeset";

const root = join(import.meta.dir, "..", "..");
const read = (path: string) => readFileSync(join(root, path), "utf8");
const agentLock = read("Cargo.lock");
const desktopLock = read("clients/desktop/src-tauri/Cargo.lock");

/** Bumps a crate's version in a lock file, like a Renovate update would. */
function bump(lock: string, crate: string): string {
  const re = new RegExp(`^(name = "${crate}"\\nversion = )"([^"]*)"$`, "m");
  expect(lock).toMatch(re);
  return lock.replace(re, `$1"999.0.0"`);
}

const unchanged = { "rd-agent": { base: agentLock, head: agentLock }, "rd-desktop": { base: desktopLock, head: desktopLock } };

describe("parseCargoLock / closure", () => {
  test("reads the workspace lock", () => {
    const lock = parseCargoLock(agentLock);
    const agent = lock.find((p) => p.name === "rd-agent")!;
    expect(agent.source).toBeUndefined();
    expect(agent.dependencies).toContain("rd-proto");
    const names = closure(lock, "rd-agent").map((p) => p.name);
    expect(names).toContain("rd-agent-core");
    expect(names).toContain("tokio");
    expect(names).not.toContain("rd-gateway");
  });
});

describe("affectedPackages", () => {
  test("desktop JS dependencies ship in rd-desktop; the viewer in neither", () => {
    expect(
      affectedPackages(["clients/desktop/bun.lock", "clients/desktop/package.json", "viewer/bun.lock", "viewer/package.json"], {}),
    ).toEqual(["rd-desktop"]);
    expect(affectedPackages(["viewer/bun.lock", "viewer/package.json"], {})).toEqual([]);
  });

  test("Docker, CI and root tooling ship in neither", () => {
    expect(affectedPackages(["deploy/Dockerfile", ".github/workflows/ci.yml", "package.json", "bun.lock"], {})).toEqual([]);
  });

  test("a workspace lock update of an agent dependency ships in rd-agent", () => {
    const files = ["Cargo.lock", "Cargo.toml"];
    const head = bump(agentLock, "tokio");
    expect(affectedPackages(files, { ...unchanged, "rd-agent": { base: agentLock, head } })).toEqual(["rd-agent"]);
  });

  test("a gateway-only lock update ships in neither", () => {
    const gatewayOnly = parseCargoLock(agentLock)
      .filter((p) => p.source !== undefined)
      .map((p) => p.name)
      .find((name) => !closure(parseCargoLock(agentLock), "rd-agent").some((p) => p.name === name));
    expect(gatewayOnly).toBeDefined();
    const head = bump(agentLock, gatewayOnly!);
    expect(
      affectedPackages(["Cargo.lock", "crates/rd-gateway/Cargo.toml"], { ...unchanged, "rd-agent": { base: agentLock, head } }),
    ).toEqual([]);
  });

  test("a shared workspace crate ships in both", () => {
    expect(affectedPackages(["crates/rd-proto/Cargo.toml"], unchanged)).toEqual(["rd-desktop", "rd-agent"]);
    expect(affectedPackages(["crates/rd-platform-macos/Cargo.toml"], unchanged)).toEqual(["rd-agent"]);
  });

  test("the desktop's own lock and the toolchain", () => {
    expect(affectedPackages(["clients/desktop/src-tauri/Cargo.lock"], unchanged)).toEqual(["rd-desktop"]);
    expect(affectedPackages(["rust-toolchain.toml"], {})).toEqual(["rd-desktop", "rd-agent"]);
  });
});

test("changesetContent", () => {
  expect(changesetContent(["rd-agent"], "Update Rust crate tokio to v1.48")).toBe(
    `---\n"rd-agent": patch\n---\n\nUpdate Rust crate tokio to v1.48\n`,
  );
  expect(changesetContent([], "Update Docker and CI")).toBe(`---\n---\n\nUpdate Docker and CI\n`);
});
