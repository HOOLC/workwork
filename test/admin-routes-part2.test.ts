import http from "node:http";

import fs from "node:fs/promises";

import { afterEach, describe, expect, it } from "vitest";

import { loadConfig } from "../src/config.js";

import { stableSessionOrder } from "../src/admin-ui/session-order.js";

import { renderAdminPage } from "../src/http/admin-page.js";

import { deferUntilResponseFinished } from "../src/http/response-deferred-tasks.js";

import { createHttpHandler } from "../src/http/router.js";

import { waitFor } from "./admin-routes-helpers.js";

describe("admin routes", () => {
  const cleanups: Array<() => Promise<void>> = [];

  afterEach(async () => {
    while (cleanups.length > 0) {
      await cleanups.pop()?.();
    }
  });

  async function startAdminServer(configEnv: NodeJS.ProcessEnv, adminService: Record<string, unknown>): Promise<string> {
    const config = loadConfig(configEnv);
    const server = http.createServer(
      createHttpHandler({
        adminService: adminService as never,
        bridge: {} as never,
        isolatedMcp: {} as never,
        jobManager: {} as never,
        config,
      }),
    );
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    cleanups.push(async () => {
      await new Promise<void>((resolve, reject) => {
        server.close((error) => {
          if (error) {
            reject(error);
            return;
          }
          resolve();
        });
      });
    });

    const address = server.address();
    if (!address || typeof address === "string") {
      throw new Error("failed to start test server");
    }
    return `http://127.0.0.1:${address.port}`;
  }

  it("keeps the session list order stable while the same view is being refreshed", () => {
    const initial = stableSessionOrder({ viewKey: "", keys: [] }, "ongoing\n", ["a", "b", "c"]);
    expect(initial.keys).toEqual(["a", "b", "c"]);

    const refreshed = stableSessionOrder(initial, "ongoing\n", ["c", "a", "d", "b"]);
    expect(refreshed.keys).toEqual(["a", "b", "c", "d"]);

    const removed = stableSessionOrder(refreshed, "ongoing\n", ["d", "a"]);
    expect(removed.keys).toEqual(["a", "d"]);

    const changedView = stableSessionOrder(removed, "usage\n", ["d", "a"]);
    expect(changedView.keys).toEqual(["d", "a"]);
  });

  it("uses the Vite dev server assets when admin ui dev origin is configured", () => {
    const previous = process.env.ADMIN_UI_DEV_ORIGIN;
    process.env.ADMIN_UI_DEV_ORIGIN = "http://127.0.0.1:5173/";
    try {
      const html = renderAdminPage({ serviceName: "workwork" });
      expect(html).toContain("http://127.0.0.1:5173/admin/@react-refresh");
      expect(html).toContain("__vite_plugin_react_preamble_installed__");
      expect(html).toContain("http://127.0.0.1:5173/admin/@vite/client");
      expect(html).toContain("http://127.0.0.1:5173/admin/main.tsx");
      expect(html).not.toContain("/admin/assets/admin-ui.css");
      expect(html).not.toContain("/admin/assets/admin-ui.js");
    } finally {
      if (previous == null) {
        delete process.env.ADMIN_UI_DEV_ORIGIN;
      } else {
        process.env.ADMIN_UI_DEV_ORIGIN = previous;
      }
    }
  });

  it("accepts auth profile creation without an explicit name", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true, status: "admin-ok" }),
        addAuthProfile: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true, status: { ok: true } };
        },
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/auth-profiles`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        auth_json_content: '{"tokens":{"account_id":"acc-1"}}',
      }),
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        name: undefined,
        authJsonContent: '{"tokens":{"account_id":"acc-1"}}',
      },
    ]);
  });

  it("forwards auth profile device-code start and completion to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true, status: "admin-ok" }),
        addAuthProfile: async () => ({ ok: true }),
        startAuthProfileDeviceCode: async () => {
          calls.push({ type: "start" });
          return {
            ok: true,
            deviceCode: {
              deviceAuthId: "device-1",
              userCode: "ABCD-EFGH",
            },
          };
        },
        completeAuthProfileDeviceCode: async (payload: Record<string, unknown>) => {
          calls.push({ type: "complete", ...payload });
          return {
            ok: true,
            deviceCode: {
              status: "pending",
            },
          };
        },
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const start = await fetch(`${baseUrl}/admin/api/auth-profiles/device-code/start`, {
      method: "POST",
    });
    expect(start.status).toBe(200);

    const complete = await fetch(`${baseUrl}/admin/api/auth-profiles/device-code/complete`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        device_auth_id: "device-1",
        user_code: "ABCD-EFGH",
        retry_after_seconds: 8,
      }),
    });
    expect(complete.status).toBe(200);
    expect(calls).toEqual([
      {
        type: "start",
      },
      {
        type: "complete",
        name: undefined,
        deviceAuthId: "device-1",
        userCode: "ABCD-EFGH",
        retryAfterSeconds: 8,
      },
    ]);
  });

  it("forwards GitHub author mapping upserts to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true, status: { ok: true } };
        },
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/github-authors`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        slack_user_id: "U123",
        github_author: "Alice Example <alice@example.com>",
      }),
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        slackUserId: "U123",
        githubAuthor: "Alice Example <alice@example.com>",
      },
    ]);
  });

  it("forwards default GitHub PR account selection to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        setDefaultGitHubPrAccount: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true, status: { ok: true } };
        },
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const missing = await fetch(`${baseUrl}/admin/api/github-accounts/default-pr`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({}),
    });
    expect(missing.status).toBe(400);

    const response = await fetch(`${baseUrl}/admin/api/github-accounts/default-pr`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        slack_user_id: "U123",
      }),
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        slackUserId: "U123",
      },
    ]);
  });

  it("forwards automatic session auth profile switches without requiring a profile name", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
        switchSessionAuthProfile: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true };
        },
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/sessions/${encodeURIComponent("C123:111.222")}/auth-profile`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        mode: "auto",
      }),
    });

    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        sessionKey: "C123:111.222",
        mode: "auto",
      },
    ]);
  });

  it("serves the React admin client without the legacy inline script", async () => {
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true, status: "admin-ok" }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const page = await fetch(`${baseUrl}/admin`);
    const html = await page.text();
    expect(html).not.toMatch(/<script>[\s\S]*switchAdminView[\s\S]*<\/script>/);
    expect(html).toContain("/admin/assets/admin-ui.js");
    const adminShellSource = await fs.readFile(new URL("../src/admin-ui/admin-shell.tsx", import.meta.url), "utf8");
    expect(adminShellSource).toContain("export function AdminShell");
    expect(adminShellSource).not.toContain("initAdminPage");
  });

  it("forwards deploy requests to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true };
        },
        rollbackRelease: async () => ({ ok: true }),
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/deploy`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        target: "worker",
        version: "0.2.0",
        allow_active: true,
      }),
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        target: "worker",
        version: "0.2.0",
        allowActive: true,
      },
    ]);
  });

  it("forwards rollback requests to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true };
        },
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/rollback`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        target: "admin",
        version: "0.1.0",
        allow_active: false,
      }),
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        target: "admin",
        version: "0.1.0",
        allowActive: false,
      },
    ]);
  });

  it("forwards session delete requests to the admin service", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        activateAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
        deleteSession: async (payload: Record<string, unknown>) => {
          calls.push(payload);
          return { ok: true };
        },
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/sessions/${encodeURIComponent("C123:111.222")}`, {
      method: "DELETE",
    });
    expect(response.status).toBe(200);
    expect(calls).toEqual([
      {
        sessionKey: "C123:111.222",
      },
    ]);
  });

  it("maps missing session delete failures to 404", async () => {
    const baseUrl = await startAdminServer(
      {
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
      } as NodeJS.ProcessEnv,
      {
        getStatus: async () => ({ ok: true }),
        addAuthProfile: async () => ({ ok: true }),
        upsertGitHubAuthorMapping: async () => ({ ok: true }),
        deleteGitHubAuthorMapping: async () => ({ ok: true }),
        deleteAuthProfile: async () => ({ ok: true }),
        activateAuthProfile: async () => ({ ok: true }),
        deployRelease: async () => ({ ok: true }),
        rollbackRelease: async () => ({ ok: true }),
        deleteSession: async () => {
          throw new Error("Session not found: C123:missing");
        },
      },
    );

    const response = await fetch(`${baseUrl}/admin/api/sessions/${encodeURIComponent("C123:missing")}`, {
      method: "DELETE",
    });
    expect(response.status).toBe(404);
    await expect(response.json()).resolves.toMatchObject({
      ok: false,
      error: "Session not found: C123:missing",
    });
  });
});
