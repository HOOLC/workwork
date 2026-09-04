import fs from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vite-plus/test";

const manifestPath = fileURLToPath(new URL("../slack/app-manifest.yaml", import.meta.url));

async function botScopes(): Promise<string[]> {
  const manifest = await fs.readFile(manifestPath, "utf8");
  const block = manifest.match(/oauth_config:\n\s+scopes:\n\s+bot:\n((?:\s+- [^\n]+\n)+)/)?.[1];
  if (!block) throw new Error("Slack manifest is missing oauth_config.scopes.bot");
  return block
    .trim()
    .split("\n")
    .map((line) => line.replace(/^\s*-\s*/, ""));
}

describe("Slack app manifest permissions", () => {
  it("grants only the additional Bot scopes required by implemented behavior", async () => {
    const scopes = await botScopes();

    expect(scopes).toContain("channels:join");
    expect(scopes).toContain("assistant:write");
    expect(scopes).not.toContain("chat:write.public");
  });
});
