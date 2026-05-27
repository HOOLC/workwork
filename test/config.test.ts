import { describe, expect, it } from "vitest";

import { loadConfig } from "../src/config.js";

describe("loadConfig", () => {
  it("throws when required variables are missing", () => {
    expect(() => loadConfig({} as NodeJS.ProcessEnv)).toThrowError(
      "Missing required environment variable: SLACK_APP_TOKEN"
    );
  });

  it("loads required configuration", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test"
    } as NodeJS.ProcessEnv);

    expect(config.slackAppToken).toBe("xapp-test");
    expect(config.slackBotToken).toBe("xoxb-test");
    expect(config.stateDir.endsWith(".data/state")).toBe(true);
    expect(config.sessionsRoot.endsWith(".data/sessions")).toBe(true);
    expect(config.reposRoot.endsWith(".data/repos")).toBe(true);
    expect(config.logDir.endsWith(".data/logs")).toBe(true);
    expect(config.codexHostHomePath).toBeUndefined();
    expect(config.codexTeamHomePath.endsWith(".data/team-codex-home")).toBe(true);
    expect(config.slackInitialThreadHistoryCount).toBe(8);
    expect(config.slackHistoryApiMaxLimit).toBe(50);
    expect(config.slackActiveTurnReconcileIntervalMs).toBe(15_000);
    expect(config.slackMissedThreadRecoveryIntervalMs).toBe(5 * 60_000);
    expect(config.slackStaleActiveTurnAfterMs).toBe(30 * 60_000);
    expect(config.logLevel).toBe("info");
    expect(config.logRawSlackEvents).toBe(true);
    expect(config.logRawCodexRpc).toBe(true);
    expect(config.logRawHttpRequests).toBe(true);
    expect(config.logRawMaxBytes).toBe(128 * 1024);
    expect(config.diskCleanupEnabled).toBe(true);
    expect(config.diskCleanupDryRun).toBe(true);
    expect(config.diskCleanupCheckIntervalMs).toBe(300_000);
    expect(config.diskCleanupMinFreeBytes).toBe(10 * 1024 * 1024 * 1024);
    expect(config.diskCleanupTargetFreeBytes).toBe(20 * 1024 * 1024 * 1024);
    expect(config.diskCleanupSessionCacheTtlMs).toBe(7 * 24 * 60 * 60 * 1000);
    expect(config.diskCleanupInactiveSessionMs).toBe(24 * 60 * 60 * 1000);
    expect(config.diskCleanupJobProtectionMs).toBe(48 * 60 * 60 * 1000);
    expect(config.diskCleanupOldLogMs).toBe(24 * 60 * 60 * 1000);
    expect(config.brokerAdminToken).toBeUndefined();
    expect(config.geminiHostHomePath).toBeUndefined();
    expect(config.geminiHttpProxy).toBeUndefined();
    expect(config.geminiHttpsProxy).toBeUndefined();
    expect(config.geminiAllProxy).toBeUndefined();
    expect(config.isolatedMcpServers).toEqual(["linear", "notion"]);
    expect(config.codexDisabledMcpServers).toEqual(["*", "linear", "notion"]);
    expect(config.tempadLinkServiceUrl).toBeUndefined();
    expect(config.githubApiBaseUrl).toBe("https://api.github.com");
    expect(config.githubOAuthScopes).toEqual(["repo", "read:user", "user:email"]);
    expect(config.defaultGitHubLogin).toBeUndefined();
    expect(config.defaultGitHubToken).toBeUndefined();
    expect(config.adminBaseUrl).toBe("http://127.0.0.1:3000");
  });

  it("rejects invalid numeric values", () => {
    expect(() =>
      loadConfig({
        SLACK_APP_TOKEN: "xapp-test",
        SLACK_BOT_TOKEN: "xoxb-test",
        PORT: "nope"
      } as NodeJS.ProcessEnv)
    ).toThrowError("Invalid numeric environment variable: PORT");
  });


  it("loads an explicit team codex home path", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      CODEX_TEAM_HOME: "/team-codex-home"
    } as NodeJS.ProcessEnv);

    expect(config.codexTeamHomePath).toBe("/team-codex-home");
  });

  it("loads an explicit host codex home path", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      CODEX_HOST_HOME_PATH: "/host-codex-home"
    } as NodeJS.ProcessEnv);

    expect(config.codexHostHomePath).toBe("/host-codex-home");
  });

  it("loads Gemini runtime configuration", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      GEMINI_HOST_HOME_PATH: "/host-gemini-home",
      GEMINI_HTTP_PROXY: "http://host.docker.internal:6152",
      GEMINI_HTTPS_PROXY: "http://host.docker.internal:6152",
      GEMINI_ALL_PROXY: "socks5://host.docker.internal:6153"
    } as NodeJS.ProcessEnv);

    expect(config.geminiHostHomePath).toBe("/host-gemini-home");
    expect(config.geminiHttpProxy).toBe("http://host.docker.internal:6152");
    expect(config.geminiHttpsProxy).toBe("http://host.docker.internal:6152");
    expect(config.geminiAllProxy).toBe("socks5://host.docker.internal:6153");
  });

  it("loads an explicit tempad link service url override", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      TEMPAD_LINK_SERVICE_URL: "http://host.docker.internal:4320"
    } as NodeJS.ProcessEnv);

    expect(config.tempadLinkServiceUrl).toBe("http://host.docker.internal:4320");
  });

  it("parses disabled MCP servers as a csv list and unions isolated MCP servers", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      CODEX_DISABLED_MCP_SERVERS: " github, linear ,, ",
      ISOLATED_MCP_SERVERS: " notion, linear ,, "
    } as NodeJS.ProcessEnv);

    expect(config.isolatedMcpServers).toEqual(["notion", "linear"]);
    expect(config.codexDisabledMcpServers).toEqual(["*", "github", "linear", "notion"]);
  });

  it("parses log configuration", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      LOG_LEVEL: "debug",
      LOG_RAW_SLACK_EVENTS: "false",
      LOG_RAW_CODEX_RPC: "false",
      LOG_RAW_HTTP_REQUESTS: "true",
      LOG_RAW_MAX_BYTES: "4096"
    } as NodeJS.ProcessEnv);

    expect(config.logLevel).toBe("debug");
    expect(config.logRawSlackEvents).toBe(false);
    expect(config.logRawCodexRpc).toBe(false);
    expect(config.logRawHttpRequests).toBe(true);
    expect(config.logRawMaxBytes).toBe(4096);
  });

  it("loads an optional broker admin token", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      BROKER_ADMIN_TOKEN: "secret-admin-token"
    } as NodeJS.ProcessEnv);

    expect(config.brokerAdminToken).toBe("secret-admin-token");
  });

  it("loads GitHub PR identity configuration", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      GITHUB_API_BASE_URL: "https://github.enterprise.test/api/v3",
      GITHUB_OAUTH_SCOPES: "repo, read:user, workflow",
      BROKER_DEFAULT_GITHUB_LOGIN: "default-bot",
      BROKER_DEFAULT_GITHUB_TOKEN: "default-token"
    } as NodeJS.ProcessEnv);

    expect(config.githubApiBaseUrl).toBe("https://github.enterprise.test/api/v3");
    expect(config.githubOAuthScopes).toEqual(["repo", "read:user", "workflow"]);
    expect(config.defaultGitHubLogin).toBe("default-bot");
    expect(config.defaultGitHubToken).toBe("default-token");
  });

  it("loads an explicit stale active turn watchdog threshold", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      SLACK_STALE_ACTIVE_TURN_AFTER_MS: "600000"
    } as NodeJS.ProcessEnv);

    expect(config.slackStaleActiveTurnAfterMs).toBe(600_000);
  });

  it("parses disk cleanup configuration", () => {
    const config = loadConfig({
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      DISK_CLEANUP_ENABLED: "false",
      DISK_CLEANUP_DRY_RUN: "false",
      DISK_CLEANUP_CHECK_INTERVAL_MS: "60000",
      DISK_CLEANUP_MIN_FREE_BYTES: "123",
      DISK_CLEANUP_TARGET_FREE_BYTES: "456",
      DISK_CLEANUP_SESSION_CACHE_TTL_MS: "654",
      DISK_CLEANUP_INACTIVE_SESSION_MS: "789",
      DISK_CLEANUP_JOB_PROTECTION_MS: "101112",
      DISK_CLEANUP_OLD_LOG_MS: "131415"
    } as NodeJS.ProcessEnv);

    expect(config.diskCleanupEnabled).toBe(false);
    expect(config.diskCleanupDryRun).toBe(false);
    expect(config.diskCleanupCheckIntervalMs).toBe(60_000);
    expect(config.diskCleanupMinFreeBytes).toBe(123);
    expect(config.diskCleanupTargetFreeBytes).toBe(456);
    expect(config.diskCleanupSessionCacheTtlMs).toBe(654);
    expect(config.diskCleanupInactiveSessionMs).toBe(789);
    expect(config.diskCleanupJobProtectionMs).toBe(101_112);
    expect(config.diskCleanupOldLogMs).toBe(131_415);
  });
});
