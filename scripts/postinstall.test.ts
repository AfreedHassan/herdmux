import { describe, expect, test } from "bun:test";

import { artifactName, releaseUrl, targetTriple } from "./postinstall";

describe("herdmux-sidebar postinstall", () => {
  test("maps supported npm platforms to Rust target triples", () => {
    expect(targetTriple("darwin", "arm64")).toBe("aarch64-apple-darwin");
    expect(targetTriple("darwin", "x64")).toBe("x86_64-apple-darwin");
    expect(targetTriple("linux", "x64")).toBe("x86_64-unknown-linux-gnu");
    expect(targetTriple("linux", "arm64")).toBe("aarch64-unknown-linux-gnu");
    expect(() => targetTriple("win32", "x64")).toThrow("Unsupported platform: win32-x64");
  });

  test("builds release artifact URL from package version", () => {
    expect(artifactName("aarch64-apple-darwin")).toBe("herdmux-sidebar-aarch64-apple-darwin.tar.gz");
    expect(releaseUrl("0.2.0-alpha.5", "aarch64-apple-darwin")).toBe(
      "https://github.com/AfreedHassan/herdmux/releases/download/v0.2.0-alpha.5/herdmux-sidebar-aarch64-apple-darwin.tar.gz",
    );
  });
});
