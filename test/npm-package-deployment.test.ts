import fs from "node:fs/promises";

import { describe, expect, it } from "vitest";

describe("npm package deployment contract", () => {
  it("documents the package release target and privacy-safe test shape", async () => {
    const doc = await fs.readFile(new URL("../docs/npm-package-deployment.md", import.meta.url), "utf8");
    expect(doc).toContain("Ship broker releases as built npm packages, with separate admin and worker");
    expect(doc).toContain("`@agent-session-broker/admin` contains the admin HTTP entry point");
    expect(doc).toContain("`@agent-session-broker/worker` contains the worker entry point");
    expect(doc).toContain("Production must not be a build machine");
    expect(doc).toContain("Rollback requires a target");
    expect(doc).toContain("Avoid tests like");
    expect(doc).toContain("where `X` is a real private value");
  });

  it("defines explicit split runtime package boundaries for public releases", async () => {
    const packageJson = JSON.parse(await fs.readFile(new URL("../package.json", import.meta.url), "utf8")) as {
      readonly name?: string;
      readonly private?: boolean;
      readonly repository?: { readonly url?: string };
      readonly bugs?: { readonly url?: string };
      readonly homepage?: string;
      readonly scripts?: Record<string, string>;
    };
    const adminPackageJson = JSON.parse(await fs.readFile(new URL("../packages/admin/package.json", import.meta.url), "utf8")) as {
      readonly name?: string;
      readonly repository?: { readonly url?: string };
      readonly bugs?: { readonly url?: string };
      readonly homepage?: string;
      readonly publishConfig?: Record<string, string>;
      readonly files?: readonly string[];
      readonly bin?: Record<string, string>;
    };
    const workerPackageJson = JSON.parse(await fs.readFile(new URL("../packages/worker/package.json", import.meta.url), "utf8")) as {
      readonly name?: string;
      readonly repository?: { readonly url?: string };
      readonly bugs?: { readonly url?: string };
      readonly homepage?: string;
      readonly publishConfig?: Record<string, string>;
      readonly files?: readonly string[];
    };

    expect(packageJson).toMatchObject({
      name: "workwork",
      private: true,
    });
    expect(packageJson.repository?.url).toBe("git+https://github.com/HOOLC/workwork.git");
    expect(packageJson.bugs?.url).toBe("https://github.com/HOOLC/workwork/issues");
    expect(packageJson.homepage).toBe("https://github.com/HOOLC/workwork#readme");
    expect(adminPackageJson).toMatchObject({
      name: "@agent-session-broker/admin",
      homepage: "https://github.com/HOOLC/workwork#readme",
      bugs: {
        url: "https://github.com/HOOLC/workwork/issues",
      },
      repository: {
        url: "git+https://github.com/HOOLC/workwork.git",
      },
      bin: {
        "agent-session-broker-macos-bootstrap": "./scripts/ops/macos-bootstrap.mjs",
      },
    });
    expect(workerPackageJson).toMatchObject({
      name: "@agent-session-broker/worker",
      homepage: "https://github.com/HOOLC/workwork#readme",
      bugs: {
        url: "https://github.com/HOOLC/workwork/issues",
      },
      repository: {
        url: "git+https://github.com/HOOLC/workwork.git",
      },
    });
    expect(adminPackageJson.publishConfig).toMatchObject({
      access: "public",
      registry: "https://registry.npmjs.org/",
    });
    expect(workerPackageJson.publishConfig).toMatchObject({
      access: "public",
      registry: "https://registry.npmjs.org/",
    });
    expect(adminPackageJson.files).toEqual(expect.arrayContaining(["dist/src/", "dist/admin-ui/", "scripts/ops/lib.mjs", "scripts/ops/macos-bootstrap.mjs", "scripts/ops/macos-launchd-launcher.mjs", "scripts/ops/macos-launchd-restart.mjs"]));
    expect(workerPackageJson.files).toEqual(expect.arrayContaining(["dist/src/", "scripts/ops/macos-launchd-launcher.mjs", "scripts/ops/macos-launchd-restart.mjs"]));
    expect(workerPackageJson.files).not.toEqual(expect.arrayContaining(["dist/admin-ui/"]));
    expect(adminPackageJson.files).not.toEqual(expect.arrayContaining(["src/", "test/", "dist/test/", ".data/", ".data-agent-trace-preview/"]));
    expect(workerPackageJson.files).not.toEqual(expect.arrayContaining(["src/", "test/", "dist/test/", ".data/", ".data-agent-trace-preview/"]));
    expect(packageJson.scripts?.["release:stage"]).toContain("stage-npm-packages");
    expect(packageJson.scripts?.["release:pack"]).toContain("pnpm build");
    expect(packageJson.scripts?.["release:pack"]).toContain("npm pack artifacts/npm-packages/admin");
    expect(packageJson.scripts?.["release:pack"]).toContain("npm pack artifacts/npm-packages/worker");
  });

  it("stages script dependencies needed by published package binaries", async () => {
    const stageScript = await fs.readFile(new URL("../scripts/build/stage-npm-packages.mjs", import.meta.url), "utf8");
    expect(stageScript).toContain('"lib.mjs"');
    expect(stageScript).toContain('"macos-bootstrap.mjs"');
  });

  it("makes CI produce the same packed artifacts that deployment consumes", async () => {
    const workflow = await fs.readFile(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
    expect(workflow).toContain("pnpm build");
    expect(workflow).toContain("pnpm test");
    expect(workflow).toContain("pnpm release:stage");
    expect(workflow).toContain("npm pack artifacts/npm-packages/admin --pack-destination artifacts");
    expect(workflow).toContain("npm pack artifacts/npm-packages/worker --pack-destination artifacts");
    expect(workflow).toContain("actions/upload-artifact");
  });

  it("publishes npm releases only through an explicit release workflow", async () => {
    const workflow = await fs.readFile(new URL("../.github/workflows/npm-publish.yml", import.meta.url), "utf8");
    expect(workflow).toContain("workflow_dispatch");
    expect(workflow).toContain("tags:");
    expect(workflow).toContain("v*");
    expect(workflow).toContain("id-token: write");
    expect(workflow).toContain("registry-url: https://registry.npmjs.org");
    expect(workflow).toContain("pnpm install --frozen-lockfile");
    expect(workflow).toContain("pnpm build");
    expect(workflow).toContain("pnpm test");
    expect(workflow).toContain("pnpm release:stage");
    expect(workflow).toContain("npm pack artifacts/npm-packages/admin --pack-destination artifacts");
    expect(workflow).toContain("npm pack artifacts/npm-packages/worker --pack-destination artifacts");
    expect(workflow).toContain("npm publish artifacts/npm-packages/admin --access public --provenance");
    expect(workflow).toContain("npm publish artifacts/npm-packages/worker --access public --provenance");
    expect(workflow).toContain("NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}");
    expect(workflow).toContain("agent-session-broker-admin-npm-package");
    expect(workflow).toContain("agent-session-broker-worker-npm-package");
  });
});
