import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { MAX_RELEASES, buildManifest, changelogSections, compareVersions, signedVersion } from "./manifest";

const CHANGELOG = `# rd-desktop

## 1.6.0

### Minor Changes

- abc1234: Cmd+\` cycles the remote Mac's windows.
- 0123456789abcdef0123456789abcdef01234567: Two lines,
  continued here.

### Patch Changes

- def5678: A fix.

## 1.5.1


### Patch Changes

- 1111111: Only a fix.

## 1.5.0

### Minor Changes

- 2222222: Updates.
`;

/** A .sig as `tauri signer sign --app-version` writes it (base64 of the minisign file). */
function sig(version: string | null): string {
  const trusted = ["timestamp:1792443754", "file:x"].concat(version ? [`version:${version}`] : []).join("\t");
  const file = `untrusted comment: signature from tauri secret key\nRUQ=\ntrusted comment: ${trusted}\nUoh=\n`;
  return `${Buffer.from(file).toString("base64")}\n`;
}
const SIG = sig("1.6.0");

describe("changelogSections", () => {
  test("drops changeset headings and commit hashes", () => {
    const sections = changelogSections(CHANGELOG);
    expect(sections.map((s) => s.version)).toEqual(["1.6.0", "1.5.1", "1.5.0"]);
    expect(sections[0].notes).toBe(
      "- Cmd+` cycles the remote Mac's windows.\n- Two lines,\n  continued here.\n\n- A fix.",
    );
    expect(sections[1].notes).toBe("- Only a fix.");
  });

  test("reads the real changelogs", () => {
    const root = join(import.meta.dir, "..", "..");
    // Until its first own release the agent's changelog is only a title.
    for (const path of ["clients/desktop/CHANGELOG.md", "crates/rd-agent/CHANGELOG.md"]) {
      const sections = changelogSections(readFileSync(join(root, path), "utf8"));
      if (path.startsWith("clients/")) expect(sections.length).toBeGreaterThan(0);
      for (const s of sections) {
        expect(s.notes).not.toMatch(/^###/m);
        expect(s.notes).not.toMatch(/^- [0-9a-f]{7,40}: /m);
      }
    }
  });
});

describe("buildManifest", () => {
  const input = {
    pkg: "rd-desktop",
    version: "1.6.0",
    repo: "Thijmen/remoted",
    changelog: CHANGELOG,
    platforms: {
      "windows-x86_64": { file: "out/rd-desktop-1.6.0-x64-setup.exe", signature: SIG },
      "darwin-aarch64": { file: "out/rd-desktop-1.6.0-macos-universal.app.tar.gz", signature: SIG },
      "darwin-x86_64": { file: "out/rd-desktop-1.6.0-macos-universal.app.tar.gz", signature: SIG },
    },
    pubDate: new Date("2026-10-20T12:00:00.123Z"),
  };

  test("Tauri's static format plus the notes of every version", () => {
    const m = buildManifest(input);
    expect(m.version).toBe("1.6.0");
    expect(m.pub_date).toBe("2026-10-20T12:00:00Z");
    expect(m.notes).toStartWith("- Cmd+`");
    expect(m.platforms["windows-x86_64"]).toEqual({
      url: "https://github.com/Thijmen/remoted/releases/download/desktop-v1.6.0/rd-desktop-1.6.0-x64-setup.exe",
      signature: SIG.trim(),
    });
    expect(m.platforms["darwin-x86_64"].url).toBe(m.platforms["darwin-aarch64"].url);
    expect(m.releases.map((r) => r.version)).toEqual(["1.6.0", "1.5.1", "1.5.0"]);
    expect(m.releases[2].url).toBe("https://github.com/Thijmen/remoted/releases/tag/desktop-v1.5.0");
  });

  test("agent tags", () => {
    const m = buildManifest({ ...input, pkg: "rd-agent" });
    expect(m.platforms["windows-x86_64"].url).toContain("/download/agent-v1.6.0/");
    expect(m.releases[0].url).toEndWith("/tag/agent-v1.6.0");
  });

  test("an older release leaves later sections out", () => {
    const platforms = { "windows-x86_64": { file: "x.exe", signature: sig("1.5.1") } };
    const m = buildManifest({ ...input, version: "1.5.1", platforms });
    expect(m.releases.map((r) => r.version)).toEqual(["1.5.1", "1.5.0"]);
  });

  test("at most MAX_RELEASES versions", () => {
    let log = "# rd-desktop\n";
    for (let i = 40; i >= 1; i--) log += `\n## 1.${i}.0\n\n- ${i}\n`;
    const platforms = { "windows-x86_64": { file: "x.exe", signature: sig("1.40.0") } };
    const m = buildManifest({ ...input, version: "1.40.0", changelog: log, platforms });
    expect(m.releases).toHaveLength(MAX_RELEASES);
    expect(m.releases[0].version).toBe("1.40.0");
  });

  test("refuses what would make a broken manifest", () => {
    expect(() => buildManifest({ ...input, version: "9.9.9" })).toThrow(/no "## 9.9.9" section/);
    expect(() => buildManifest({ ...input, platforms: {} })).toThrow(/no --platform/);
    expect(() =>
      buildManifest({ ...input, platforms: { "macos-universal": input.platforms["darwin-aarch64"] } }),
    ).toThrow(/bad platform key/);
    expect(() => buildManifest({ ...input, pkg: "rd-gateway" })).toThrow(/unknown package/);
    // Signatures must name the released version, or every client refuses the update.
    for (const signature of [sig("1.5.1"), sig(null), "not base64 at all"]) {
      expect(() =>
        buildManifest({ ...input, platforms: { "windows-x86_64": { file: "x.exe", signature } } }),
      ).toThrow(/the signature is for version/);
    }
  });

  test("signedVersion", () => {
    expect(signedVersion(sig("1.6.0"))).toBe("1.6.0");
    expect(signedVersion(sig(null))).toBeUndefined();
    expect(signedVersion("")).toBeUndefined();
  });
});

test("compareVersions", () => {
  expect(compareVersions("1.10.0", "1.9.0")).toBeGreaterThan(0);
  expect(compareVersions("1.5.0", "1.5.0")).toBe(0);
  expect(compareVersions("1.5.0-beta.2", "1.5.0")).toBeLessThan(0);
  expect(compareVersions("1.5.0-beta.10", "1.5.0-beta.2")).toBeGreaterThan(0);
  expect(compareVersions("1.5.0-alpha", "1.5.0-beta")).toBeLessThan(0);
  expect(() => compareVersions("1.5", "1.5.0")).toThrow();
});
