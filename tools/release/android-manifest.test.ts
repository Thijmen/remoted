import { describe, expect, test } from "bun:test";

import { buildAndroidManifest, versionCode } from "./android-manifest";

const CHANGELOG = `# rd-android

## 0.2.0

### Minor Changes

- abc1234: The APK updates itself: a banner on the portal, a checked download and Android's installer.

## 0.1.0

### Minor Changes

- def5678: First release.
`;

function manifest(version = "0.2.0") {
  return buildAndroidManifest({
    version,
    repo: "Thijmen/remoted",
    apk: new Uint8Array([1, 2, 3, 4]),
    apkName: `rd-desktop-${version}-android-arm64-armv7.apk`,
    changelog: CHANGELOG,
    pubDate: new Date("2026-10-31T12:00:00.000Z"),
  });
}

describe("versionCode", () => {
  test("matches Tauri's derivation", () => {
    expect(versionCode("0.2.1")).toBe(2_001);
    expect(versionCode("1.7.1")).toBe(1_007_001);
    expect(versionCode("12.34.56")).toBe(12_034_056);
  });

  test("rejects something that is not a version", () => {
    expect(() => versionCode("v1")).toThrow();
  });
});

describe("buildAndroidManifest", () => {
  test("names the APK release and its digest", () => {
    const m = manifest();
    expect(m.version).toBe("0.2.0");
    expect(m.version_code).toBe(2_000);
    expect(m.pub_date).toBe("2026-10-31T12:00:00Z");
    expect(m.platforms["android-universal"]).toEqual({
      url: "https://github.com/Thijmen/remoted/releases/download/android-v0.2.0/rd-desktop-0.2.0-android-arm64-armv7.apk",
      sha256: "9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a",
    });
  });

  test("carries the notes of this and older versions, newest first", () => {
    const m = manifest();
    expect(m.notes).toBe(
      "- The APK updates itself: a banner on the portal, a checked download and Android's installer.",
    );
    expect(m.releases.map((r) => r.version)).toEqual(["0.2.0", "0.1.0"]);
    expect(m.releases[1].url).toBe("https://github.com/Thijmen/remoted/releases/tag/android-v0.1.0");
  });

  test("works before the first changelog exists", () => {
    const m = buildAndroidManifest({
      version: "0.1.0",
      repo: "Thijmen/remoted",
      apk: new Uint8Array([0]),
      apkName: "a.apk",
      changelog: "",
      pubDate: new Date("2026-10-31T12:00:00.000Z"),
    });
    expect(m.notes).toBe("");
    expect(m.releases).toEqual([]);
    expect(m.platforms["android-universal"].url).toContain("android-v0.1.0/a.apk");
  });
});
