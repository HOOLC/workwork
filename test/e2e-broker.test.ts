import fs from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { once } from "node:events";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

import { afterEach, describe, expect, it } from "vitest";

import type { CodexInputItem } from "../src/services/codex/app-server-client.js";
import { SessionManager } from "../src/services/session-manager.js";
import { StateStore } from "../src/store/state-store.js";
import type { PersistedAgentTraceEvent, PersistedInboundMessage, SlackSessionRecord } from "../src/types.js";
import { MockCodexAppServer } from "./helpers/mock-codex-app-server.js";
import { MockSlackServer } from "./manual/mock-slack-server.js";

const brokerRoot = path.dirname(fileURLToPath(new URL("../package.json", import.meta.url)));
const DEFAULT_E2E_TIMEOUT_MS = 30_000;
const DAY_MS = 24 * 60 * 60 * 1000;

describe.sequential("slack-codex-broker e2e", () => {
  const cleanups: Array<() => Promise<void>> = [];

  afterEach(async () => {
    while (cleanups.length > 0) {
      const cleanup = cleanups.pop();
      await cleanup?.();
    }
  });

  it("shows Slack assistant thread status while a turn is running and clears it after replying", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    let brokerBaseUrl = "";
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP",
      channels: [
        {
          id: "C123",
          name: "deep-review",
          is_channel: true
        }
      ]
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        await waitFor(() => {
          return mockSlack.assistantStatusUpdates.some((update) => update.status === "Thinking...");
        }, "assistant thinking status");

        const response = await fetch(`${brokerBaseUrl}/slack/post-message`, {
          method: "POST",
          headers: {
            "content-type": "application/x-www-form-urlencoded; charset=utf-8"
          },
          body: new URLSearchParams({
            channel_id: "C123",
            thread_ts: "110.220",
            text: "STATUS_REPLY_OK",
            kind: "final"
          }).toString()
        });
        if (!response.ok) {
          throw new Error(`Failed to post broker Slack reply: ${response.status}`);
        }

        context.complete("");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    brokerBaseUrl = broker.baseUrl;
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-status-mention", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "110.220",
      ts: "110.221",
      text: "<@UBOT> status test"
    });

    await waitFor(() => {
      return mockSlack.assistantStatusUpdates.some((update) => update.status === "Thinking...");
    }, "assistant thinking status call");
    await waitFor(() => {
      return mockSlack.postedMessages.some((message) => message.text === "STATUS_REPLY_OK");
    }, "broker-posted Slack reply");
    await waitFor(() => {
      return mockSlack.assistantStatusUpdates.some((update) => update.status === "");
    }, "assistant status clear");

    expect(mockSlack.assistantStatusUpdates).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          channel: "C123",
          threadTs: "110.220",
          status: "Thinking...",
          loadingMessages: "Thinking..."
        }),
        expect.objectContaining({
          channel: "C123",
          threadTs: "110.220",
          status: ""
        })
      ])
    );
  }, 90_000);

  it("posts a session permalink when the bot starts processing a Slack thread", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        ADMIN_BASE_URL: "https://admin.example.test"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session-link", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "991.220",
      ts: "991.221",
      text: "<@UBOT> trace link"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "first turn start");
    await waitFor(
      () => mockSlack.postedMessages.some((message) =>
        message.threadTs === "991.220" &&
          message.text.includes("查看会话活动时间线") &&
          message.text.includes("https://admin.example.test/admin/sessions/C123%3A991.220")
      ),
      "session permalink startup message"
    );
    await waitForSessionIdle(tempRoot, "C123:991.220");

    const postedLinks = mockSlack.postedMessages.filter((message) =>
      message.threadTs === "991.220" && message.text.includes("/admin/sessions/C123%3A991.220")
    );
    expect(postedLinks).toHaveLength(1);
    expect(postedLinks[0]!.text).toContain("<https://admin.example.test/admin/sessions/C123%3A991.220|查看会话活动时间线>");
    expect(postedLinks[0]!.text).toContain("<https://admin.example.test/admin/sessions/C123%3A991.220/github/bind|绑定 GitHub>");
    expect(postedLinks[0]!.text).not.toContain("已开始处理");
    expect(postedLinks[0]!.text).not.toContain("Bot");
    const startupMessages = mockSlack.postedMessages.filter((message) => message.threadTs === "991.220");
    expect(startupMessages).toEqual([postedLinks[0]]);
    await expect(readSessionRecord(tempRoot, "C123:991.220")).resolves.toMatchObject({
      sessionPageLinkPostedAt: expect.any(String)
    });
  }, 60_000);

  it("records the session starter and warns about default GitHub PR fallback in the session link", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        ADMIN_BASE_URL: "https://admin.example.test",
        BROKER_DEFAULT_GITHUB_LOGIN: "default-bot",
        BROKER_DEFAULT_GITHUB_TOKEN: "default-token"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session-github-starter", {
      type: "app_mention",
      user: "U_STARTER",
      channel: "C123",
      thread_ts: "992.220",
      ts: "992.221",
      text: "<@UBOT> open a PR"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "first turn start");
    await waitFor(
      () => mockSlack.postedMessages.some((message) =>
        message.threadTs === "992.220" &&
          message.text.includes("查看会话活动时间线") &&
          message.text.includes("当前发起人还没有绑定 GitHub 账号") &&
          message.text.includes("默认账号 default-bot") &&
          message.text.includes("https://admin.example.test/admin/sessions/C123%3A992.220/github/bind")
      ),
      "session permalink GitHub fallback message"
    );
    await waitForSessionIdle(tempRoot, "C123:992.220");
    await expect(readSessionRecord(tempRoot, "C123:992.220")).resolves.toMatchObject({
      initiatorUserId: "U_STARTER",
      initiatorMessageTs: "992.221",
      initiatorCapturedAt: expect.any(String)
    });

    await mockSlack.sendEvent("evt-session-github-later", {
      type: "message",
      user: "U_LATER",
      channel: "C123",
      thread_ts: "992.220",
      ts: "992.222",
      text: "follow-up"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 2, "second turn start");
    await waitForSessionIdle(tempRoot, "C123:992.220");
    await expect(readSessionRecord(tempRoot, "C123:992.220")).resolves.toMatchObject({
      initiatorUserId: "U_STARTER",
      initiatorMessageTs: "992.221"
    });
  }, 60_000);

  it("asks the session starter to bind GitHub even when no default GitHub account is configured", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        ADMIN_BASE_URL: "https://admin.example.test"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session-github-no-default", {
      type: "app_mention",
      user: "U_STARTER",
      channel: "C123",
      thread_ts: "993.220",
      ts: "993.221",
      text: "<@UBOT> open a PR"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "first turn start");
    await waitFor(
      () => mockSlack.postedMessages.some((message) =>
        message.threadTs === "993.220" &&
          message.text.includes("查看会话活动时间线") &&
          message.text.includes("当前发起人还没有绑定 GitHub 账号") &&
          message.text.includes("当前没有默认 GitHub PR 账号") &&
          message.text.includes("https://admin.example.test/admin/sessions/C123%3A993.220/github/bind")
      ),
      "session permalink GitHub bind message without default"
    );
    await waitForSessionIdle(tempRoot, "C123:993.220");
    await expect(readSessionRecord(tempRoot, "C123:993.220")).resolves.toMatchObject({
      initiatorUserId: "U_STARTER",
      initiatorMessageTs: "993.221"
    });
  }, 60_000);

  it("falls back to an eyes reaction when Slack assistant status is unavailable", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    let brokerBaseUrl = "";
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP",
      assistantStatusError: "unknown_method"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        await waitFor(() => {
          return mockSlack.reactionOperations.some((operation) => (
            operation.action === "add" &&
            operation.channel === "C123" &&
            operation.timestamp === "120.220" &&
            operation.name === "eyes"
          ));
        }, "assistant fallback reaction add");

        const response = await fetch(`${brokerBaseUrl}/slack/post-message`, {
          method: "POST",
          headers: {
            "content-type": "application/x-www-form-urlencoded; charset=utf-8"
          },
          body: new URLSearchParams({
            channel_id: "C123",
            thread_ts: "120.220",
            text: "FALLBACK_REPLY_OK",
            kind: "final"
          }).toString()
        });
        if (!response.ok) {
          throw new Error(`Failed to post broker Slack reply: ${response.status}`);
        }

        context.complete("");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    brokerBaseUrl = broker.baseUrl;
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-fallback-mention", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "120.220",
      ts: "120.221",
      text: "<@UBOT> fallback test"
    });

    await waitFor(() => {
      return mockSlack.reactionOperations.some((operation) => (
        operation.action === "add" &&
        operation.channel === "C123" &&
        operation.timestamp === "120.220" &&
        operation.name === "eyes"
      ));
    }, "assistant fallback reaction add");
    await waitFor(() => {
      return mockSlack.postedMessages.some((message) => message.text === "FALLBACK_REPLY_OK");
    }, "fallback broker-posted Slack reply");
    await waitFor(() => {
      return mockSlack.reactionOperations.some((operation) => (
        operation.action === "remove" &&
        operation.channel === "C123" &&
        operation.timestamp === "120.220" &&
        operation.name === "eyes"
      ));
    }, "assistant fallback reaction clear");

    expect(mockSlack.assistantStatusUpdates).toHaveLength(0);
  }, 90_000);

  it("starts a new session, backfills history, and forwards selected Slack card payloads", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP",
      channels: [
        {
          id: "C123",
          name: "deep-review",
          is_channel: true
        }
      ]
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-pre-root", {
      type: "message",
      user: "U123",
      channel: "C123",
      ts: "111.220",
      text: "ROOT_CONTEXT_ABC"
    });
    await mockSlack.sendEvent("evt-pre-recent", {
      type: "message",
      user: "U234",
      channel: "C123",
      thread_ts: "111.220",
      ts: "111.221",
      text: "RECENT_CONTEXT_DEF"
    });
    await mockSlack.sendEvent("evt-mention", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "111.220",
      ts: "111.222",
      text: "<@UBOT> 看看 <@U234> 这条 thread"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "first turn start");
    await waitForSessionIdle(tempRoot, "C123:111.220");
    await expect(readSessionRecord(tempRoot, "C123:111.220")).resolves.toMatchObject({
      channelName: "deep-review",
      channelType: "channel"
    });
    const firstTurnText = collectTextInput(mockCodex.turnsStarted[0]!.input);
    expect(firstTurnText).toContain("ROOT_CONTEXT_ABC");
    expect(firstTurnText).toContain("RECENT_CONTEXT_DEF");
    expect(firstTurnText).toContain("structured_message_json");
    expect(firstTurnText).toContain("\"text_with_resolved_mentions\": \"@Mock Bot 看看 @Mock Display 234 这条 thread\"");

    const sessionListResponse = await fetch(`${broker.baseUrl}/admin/api/sessions`);
    expect(sessionListResponse.ok).toBe(true);
    const sessionList = await sessionListResponse.json() as {
      readonly sessions?: Array<{
        readonly key?: string;
        readonly firstUserMessage?: { readonly textPreview?: string };
        readonly lastUserMessage?: { readonly textPreview?: string };
      }>;
    };
    expect(sessionList.sessions?.find((session) => session.key === "C123:111.220")).toMatchObject({
      firstUserMessage: {
        textPreview: "@Mock Bot 看看 @Mock Display 234 这条 thread"
      },
      lastUserMessage: {
        textPreview: "@Mock Bot 看看 @Mock Display 234 这条 thread"
      }
    });

    await mockSlack.sendEvent("evt-linear-card", {
      type: "message",
      channel: "C123",
      thread_ts: "111.220",
      ts: "111.223",
      subtype: "bot_message",
      bot_id: "BLINEAR",
      app_id: "ALINEAR",
      username: "Linear",
      text: "",
      blocks: [
        {
          type: "section",
          text: {
            type: "mrkdwn",
            text: "*CUE-1180* 感觉 ai chat webview 帧率很低"
          }
        }
      ],
      attachments: [
        {
          title: "CUE-1180 感觉 ai chat webview 帧率很低",
          title_link: "https://linear.app/cue/issue/CUE-1180",
          text: "State: Backlog"
        }
      ]
    });

    await waitFor(() => {
      const deliveredTexts = [
        ...mockCodex.turnsStarted.map((turn) => collectTextInput(turn.input)),
        ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
      ];
      return deliveredTexts.some((text) => text.includes("\"bot_id\": \"BLINEAR\""));
    }, "delivery of bot card payload");
    const deliveredTexts = [
      ...mockCodex.turnsStarted.map((turn) => collectTextInput(turn.input)),
      ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
    ];
    const botCardText = deliveredTexts.find((text) => text.includes("\"bot_id\": \"BLINEAR\"")) ?? "";
    expect(botCardText).toContain("\"attachments\"");
    expect(botCardText).toContain("https://linear.app/cue/issue/CUE-1180");
  }, 90_000);

  it("backfills Slack channel names for persisted sessions on startup", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const seedStore = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
    const seedSessions = new SessionManager({
      stateStore: seedStore,
      sessionsRoot: path.join(tempRoot, "sessions")
    });
    await seedSessions.load();
    await seedSessions.ensureSession("CBACK", "222.333");
    const now = new Date().toISOString();
    await seedSessions.upsertInboundMessage({
      key: "CBACK:222.333:222.334",
      sessionKey: "CBACK:222.333",
      channelId: "CBACK",
      rootThreadTs: "222.333",
      messageTs: "222.334",
      source: "thread_reply",
      userId: "U123",
      text: "<@U234> 旧消息",
      senderKind: "user",
      mentionedUserIds: ["U234"],
      status: "pending",
      createdAt: now,
      updatedAt: now
    });
    seedStore.close();

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP",
      channels: [
        {
          id: "CBACK",
          name: "admin-trace",
          is_channel: true
        }
      ]
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await waitFor(async () => {
      const session = await readSessionRecord(tempRoot, "CBACK:222.333");
      return session.channelName === "admin-trace" && session.channelType === "channel";
    }, "persisted session channel metadata backfill");
    await waitFor(async () => {
      const inbound = await readInboundMessages(tempRoot, "CBACK:222.333");
      return inbound[0]?.mentionedUsers?.[0]?.displayName === "Mock Display 234";
    }, "persisted inbound mention identity backfill");
  }, 60_000);

  it("replays missed thread messages after restart as a single recovered batch", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const port = await getFreePort();
    const broker = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "222.220",
      ts: "222.221",
      text: "<@UBOT> 开个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "session bootstrap turn");
    await waitForSessionIdle(tempRoot, "C123:222.220");
    await broker.stop();
    cleanups.pop();

    mockSlack.recordThreadMessage({
      channel: "C123",
      threadTs: "222.220",
      ts: "222.222",
      text: "漏掉的第一条",
      user: "U123"
    });
    mockSlack.recordThreadMessage({
      channel: "C123",
      threadTs: "222.220",
      ts: "222.223",
      text: "漏掉的第二条",
      user: "U234"
    });

    const restarted = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => restarted.stop());

    await waitFor(() => {
      const deliveredTexts = [
        ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
        ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
      ];
      return deliveredTexts.some((text) => text.includes("recovered_message_batch_json"));
    }, "recovered batch turn");
    const deliveredTexts = [
      ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
      ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
    ];
    const recoveredText = deliveredTexts.find((text) => text.includes("recovered_message_batch_json")) ?? "";
    expect(recoveredText).toContain("recovered_message_batch_json");
    expect(recoveredText).toContain("漏掉的第一条");
    expect(recoveredText).toContain("漏掉的第二条");
    expect(recoveredText).toContain("\"batch_message_count\": 2");
  }, 90_000);

  it("starts a fresh turn instead of resyncing back to an older active turn after a active input mismatch reset", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    let turnStartCount = 0;
    let releaseFirstTurn: (() => void) | undefined;
    const firstTurnGate = new Promise<void>((resolve) => {
      releaseFirstTurn = resolve;
    });
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnStartCount += 1;
        if (turnStartCount === 1) {
          await firstTurnGate;
          return;
        }
        if (turnStartCount >= 2) {
          context.complete("RECOVERED_AFTER_MISMATCH");
        }
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });
    cleanups.push(async () => {
      releaseFirstTurn?.();
    });

    const port = await getFreePort();
    const sessionKey = "C123:223.220";
    const broker = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        SLACK_ACTIVE_TURN_RECONCILE_INTERVAL_MS: "100",
        SLACK_MISSED_THREAD_RECOVERY_INTERVAL_MS: "100"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-active-input-mismatch-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "223.220",
      ts: "223.221",
      text: "<@UBOT> keep this turn running"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "initial active turn");
    await waitForSessionActive(tempRoot, sessionKey);
    await broker.stop();
    cleanups.pop();

    const writerStore = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
    const writerSessions = new SessionManager({
      stateStore: writerStore,
      sessionsRoot: path.join(tempRoot, "sessions")
    });
    await writerSessions.load();
    const existingSession = writerSessions.getSession("C123", "223.220");
    expect(existingSession?.activeTurnId).toBeTruthy();

    const fakeTurnId = "turn-fake-new";
    const fakeActiveSession = await writerSessions.setActiveTurnId("C123", "223.220", fakeTurnId);
    expect(fakeActiveSession.activeTurnId).toBe(fakeTurnId);
    const inflightMessages = writerSessions.listInboundMessages({
      channelId: "C123",
      rootThreadTs: "223.220",
      status: "inflight"
    });
    expect(inflightMessages.length).toBeGreaterThan(0);
    await writerSessions.updateInboundMessagesForBatch(
      "C123",
      "223.220",
      inflightMessages.map((message) => message.messageTs),
      {
        status: "inflight",
        batchId: fakeTurnId
      }
    );

    mockSlack.recordThreadMessage({
      channel: "C123",
      threadTs: "223.220",
      ts: "223.222",
      text: "MISSED_AFTER_MISMATCH",
      user: "U234"
    });
    const codexThread = existingSession?.agentSessionId ? mockCodex.getThread(existingSession.agentSessionId) : undefined;
    if (codexThread) {
      codexThread.activeTurnId = undefined;
      for (const turn of codexThread.turns) {
        if (turn.status === "inProgress") {
          turn.status = "interrupted";
        }
      }
    }

    const restarted = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        SLACK_ACTIVE_TURN_RECONCILE_INTERVAL_MS: "100",
        SLACK_MISSED_THREAD_RECOVERY_INTERVAL_MS: "100"
      }
    });
    cleanups.push(() => restarted.stop());

    try {
      await waitFor(() => mockCodex.turnsStarted.length >= 2, "replacement turn after active input mismatch", 60_000);
    } catch (error) {
      console.error(restarted.logs.join("").slice(-8_000));
      throw error;
    }
    await waitForSessionIdle(tempRoot, sessionKey);

    const recoveredTurnText = collectTextInput(mockCodex.turnsStarted[1]!.input);
    expect(recoveredTurnText).toContain("recovered_message_batch_json");
    expect(recoveredTurnText).toContain("MISSED_AFTER_MISMATCH");

    await waitFor(async () => {
      const session = await readSessionRecord(tempRoot, sessionKey);
      return !session.activeTurnId && session.lastDeliveredMessageTs === "223.222";
    }, "active-input-mismatch recovered delivery cursor");
    const finalSession = await readSessionRecord(tempRoot, sessionKey);
    expect(finalSession.activeTurnId).toBeUndefined();
    expect(finalSession.lastDeliveredMessageTs).toBe("223.222");

    await waitFor(async () => {
      const inbound = await readInboundMessages(tempRoot, sessionKey);
      return inbound.every((message) => message.status === "done");
    }, "all recovered active-input-mismatch inbound messages done");
    const finalInbound = await readInboundMessages(tempRoot, sessionKey);
    expect(finalInbound.filter((message) => message.status !== "done")).toHaveLength(0);
  }, 90_000);

  it("recovers a silent in-progress active turn after the stall timeout", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    let turnStartCount = 0;
    let releaseFirstTurn: (() => void) | undefined;
    const firstTurnGate = new Promise<void>((resolve) => {
      releaseFirstTurn = resolve;
    });
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnStartCount += 1;
        if (turnStartCount === 1) {
          await firstTurnGate;
          return;
        }

        context.complete("RECOVERED_AFTER_STALE_ACTIVE_TURN");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      releaseFirstTurn?.();
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const sessionKey = "C123:224.220";
    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        SLACK_ACTIVE_TURN_RECONCILE_INTERVAL_MS: "100",
        SLACK_ACTIVE_TURN_STALL_TIMEOUT_MS: "300",
        SLACK_MISSED_THREAD_RECOVERY_INTERVAL_MS: "100"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-stale-active-turn-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "224.220",
      ts: "224.221",
      text: "<@UBOT> START_STALE_ACTIVE_TURN"
    });

    await waitFor(() => mockCodex.turnsStarted.length >= 1, "initial stale active turn");
    await waitForSessionActive(tempRoot, sessionKey);
    await waitFor(() => mockCodex.interrupts.length >= 1, "stale active turn interrupt", 60_000);
    await waitFor(() => mockCodex.turnsStarted.length >= 2, "replacement turn after stale active recovery", 60_000);
    await waitFor(() => mockCodex.turnsStarted[1]?.status === "completed", "replacement turn completion", 60_000);
    await waitFor(async () => {
      const session = await readSessionRecord(tempRoot, sessionKey);
      return !session.activeTurnId && session.lastDeliveredMessageTs === "224.221";
    }, "stale active recovery delivery cursor", 60_000);

    const replacementTurnText = collectTextInput(mockCodex.turnsStarted[1]!.input);
    expect(replacementTurnText).toContain("START_STALE_ACTIVE_TURN");

    const finalSession = await readSessionRecord(tempRoot, sessionKey);
    expect(finalSession.activeTurnId).toBeUndefined();
    expect(finalSession.lastDeliveredMessageTs).toBe("224.221");

    const finalInbound = await readInboundMessages(tempRoot, sessionKey);
    expect(finalInbound.filter((message) => message.status !== "done")).toHaveLength(0);
  }, 90_000);

  it("periodically recovers missed thread replies without requiring a socket reconnect", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const port = await getFreePort();
    const broker = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        SLACK_ACTIVE_TURN_RECONCILE_INTERVAL_MS: "100",
        SLACK_MISSED_THREAD_RECOVERY_INTERVAL_MS: "100"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-periodic-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "333.220",
      ts: "333.221",
      text: "<@UBOT> 开个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "session bootstrap turn");
    await waitForSessionIdle(tempRoot, "C123:333.220");

    mockSlack.recordThreadMessage({
      channel: "C123",
      threadTs: "333.220",
      ts: "333.222",
      text: "漏掉的周期性恢复消息",
      user: "U234"
    });

    await waitFor(() => {
      const deliveredTexts = [
        ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
        ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
      ];
      return deliveredTexts.some((text) => text.includes("漏掉的周期性恢复消息"));
    }, "periodic recovered thread reply");

    const deliveredTexts = [
      ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
      ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
    ];
    const recoveredText = deliveredTexts.find((text) => text.includes("漏掉的周期性恢复消息")) ?? "";
    expect(recoveredText).toContain("recovered_message_batch_json");
    expect(recoveredText).toContain("\"recovery_kind\": \"missed_thread_messages\"");
    expect(recoveredText).toContain("漏掉的周期性恢复消息");
  }, 90_000);

  it("recovers persisted pending backlog on startup when a session has no active turn", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const port = await getFreePort();
    const broker = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "666.220",
      ts: "666.221",
      text: "<@UBOT> 开个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "session bootstrap turn");
    await waitForSessionIdle(tempRoot, "C123:666.220");
    await broker.stop();
    cleanups.pop();

    const stateStore = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot: path.join(tempRoot, "sessions")
    });
    await sessions.load();
    const session = sessions.getSession("C123", "666.220");
    expect(session).toBeTruthy();

    const now = new Date().toISOString();
    const pendingMessage: PersistedInboundMessage = {
      key: `C123:666.220:666.222`,
      sessionKey: "C123:666.220",
      channelId: "C123",
      rootThreadTs: "666.220",
      messageTs: "666.222",
      source: "thread_reply",
      userId: "U234",
      text: "BOOT_PENDING_RECOVERY",
      senderKind: "user",
      mentionedUserIds: [],
      images: [],
      slackMessage: {
        type: "message",
        user: "U234",
        ts: "666.222",
        text: "BOOT_PENDING_RECOVERY",
        thread_ts: "666.220",
        channel: "C123"
      },
      status: "pending",
      createdAt: now,
      updatedAt: now
    };
    await sessions.upsertInboundMessage(pendingMessage);

    const restarted = await startBrokerProcess({
      port,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => restarted.stop());

    await waitFor(() => {
      const deliveredTexts = mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input));
      return deliveredTexts.some((text) => text.includes("BOOT_PENDING_RECOVERY"));
    }, "startup recovery of persisted pending backlog");
  }, 90_000);

  it("reclaims sessions older than the hard protection window even when they still look active", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const stateStore = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot: path.join(tempRoot, "sessions")
    });
    await sessions.load();

    const oldAt = new Date(Date.now() - 72 * 60 * 60 * 1000).toISOString();
    const protectedAt = new Date(Date.now() - 36 * 60 * 60 * 1000).toISOString();
    const recentStateWriteAt = new Date().toISOString();
    const staleSession = await sessions.ensureSession("CSTALE", "777.100");
    await stateStore.upsertSession({
      ...staleSession,
      activeTurnId: "turn-stale",
      activeTurnStartedAt: oldAt,
      createdAt: oldAt,
      updatedAt: recentStateWriteAt
    });
    await fs.writeFile(path.join(staleSession.workspacePath, "marker.txt"), "stale active session");

    const staleJobDir = path.join(tempRoot, "jobs", "job-stale-active");
    await fs.mkdir(staleJobDir, { recursive: true });
    const staleJobScript = path.join(staleJobDir, "run.sh");
    await fs.writeFile(staleJobScript, "#!/bin/sh\nsleep 300\n");
    await fs.chmod(staleJobScript, 0o755);
    await sessions.upsertBackgroundJob({
      id: "job-stale-active",
      token: "token-stale-active",
      sessionKey: staleSession.key,
      channelId: staleSession.channelId,
      rootThreadTs: staleSession.rootThreadTs,
      kind: "watch_ci",
      shell: "sh",
      cwd: staleSession.workspacePath,
      scriptPath: staleJobScript,
      restartOnBoot: true,
      status: "running",
      createdAt: oldAt,
      updatedAt: oldAt,
      startedAt: oldAt,
      heartbeatAt: oldAt
    });

    const protectedSession = await sessions.ensureSession("CPROTECTED", "888.100");
    await stateStore.upsertSession({
      ...protectedSession,
      createdAt: protectedAt,
      updatedAt: protectedAt
    });
    await fs.writeFile(path.join(protectedSession.workspacePath, "marker.txt"), "protected job session");

    const protectedJobDir = path.join(tempRoot, "jobs", "job-protected");
    await fs.mkdir(protectedJobDir, { recursive: true });
    const protectedJobScript = path.join(protectedJobDir, "run.sh");
    await fs.writeFile(protectedJobScript, "#!/bin/sh\nsleep 300\n");
    await fs.chmod(protectedJobScript, 0o755);
    await sessions.upsertBackgroundJob({
      id: "job-protected",
      token: "token-protected",
      sessionKey: protectedSession.key,
      channelId: protectedSession.channelId,
      rootThreadTs: protectedSession.rootThreadTs,
      kind: "watch_ci",
      shell: "sh",
      cwd: protectedSession.workspacePath,
      scriptPath: protectedJobScript,
      restartOnBoot: true,
      status: "running",
      createdAt: protectedAt,
      updatedAt: protectedAt,
      startedAt: protectedAt,
      heartbeatAt: protectedAt
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        DISK_CLEANUP_DRY_RUN: "false",
        DISK_CLEANUP_MIN_FREE_BYTES: "1000000000000000",
        DISK_CLEANUP_TARGET_FREE_BYTES: "1000000000000000",
        DISK_CLEANUP_INACTIVE_SESSION_MS: String(DAY_MS),
        DISK_CLEANUP_JOB_PROTECTION_MS: String(2 * DAY_MS),
        DISK_CLEANUP_OLD_LOG_MS: String(DAY_MS)
      }
    });
    cleanups.push(() => broker.stop());

    await waitFor(async () => !(await pathExists(staleSession.workspacePath)), "stale active session cleanup");
    await stateStore.load();

    expect(sessions.getSessionByKey(staleSession.key)).toBeUndefined();
    expect(sessions.getBackgroundJob("job-stale-active")).toBeUndefined();
    expect(await pathExists(staleJobDir)).toBe(false);
    expect(sessions.getSessionByKey(protectedSession.key)).toBeDefined();
    expect(await pathExists(protectedSession.workspacePath)).toBe(true);
    expect(await pathExists(protectedJobDir)).toBe(true);
  }, 90_000);

  it("dry-runs expired session cache cleanup on startup without deleting artifacts", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const stateStore = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot: path.join(tempRoot, "sessions")
    });
    await sessions.load();

    const oldAt = new Date(Date.now() - 8 * DAY_MS).toISOString();
    const staleSession = await sessions.ensureSession("CCACHE", "999.100");
    await stateStore.upsertSession({
      ...staleSession,
      createdAt: oldAt,
      updatedAt: oldAt
    });
    const nodeModulesPath = path.join(staleSession.workspacePath, "web/node_modules/pkg/index.js");
    await fs.mkdir(path.dirname(nodeModulesPath), { recursive: true });
    await fs.writeFile(nodeModulesPath, "cached dependency");

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        DISK_CLEANUP_DRY_RUN: "true",
        DISK_CLEANUP_MIN_FREE_BYTES: "0",
        DISK_CLEANUP_TARGET_FREE_BYTES: "0",
        DISK_CLEANUP_SESSION_CACHE_TTL_MS: String(DAY_MS)
      }
    });
    cleanups.push(() => broker.stop());

    await waitFor(() => {
      return broker.logs.some((line) =>
        line.includes("Disk cleanup session cache candidate") &&
        line.includes(staleSession.key) &&
        line.includes("\"dryRun\":true")
      );
    }, "session cache dry-run candidate log");

    expect(await pathExists(path.join(staleSession.workspacePath, "web/node_modules"))).toBe(true);
  }, 90_000);

  it("injects background job events back into the same session", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "333.220",
      ts: "333.221",
      text: "<@UBOT> 先起一个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "initial turn");
    await waitForSessionIdle(tempRoot, "C123:333.220");

    const registerResponse = await fetch(`${broker.baseUrl}/jobs/register`, {
      method: "POST",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        channel_id: "C123",
        thread_ts: "333.220",
        kind: "watch_ci",
        script: "#!/bin/sh\nsleep 30"
      })
    });
    const registerBody = await registerResponse.json() as {
      job?: { id: string; token: string };
    };
    expect(registerResponse.ok).toBe(true);
    expect(registerBody.job?.id).toBeTruthy();
    expect(registerBody.job?.token).toBeTruthy();

    await postJson(`${broker.baseUrl}/jobs/${registerBody.job!.id}/event`, {
      token: registerBody.job!.token,
      event_kind: "state_changed",
      summary: "CI turned green."
    });
    await postJson(`${broker.baseUrl}/jobs/${registerBody.job!.id}/complete`, {
      token: registerBody.job!.token,
      summary: "job done"
    });

    await waitFor(() => {
      const deliveredTexts = [
        ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
        ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
      ];
      return deliveredTexts.some((text) => text.includes("background_job_event_json"));
    }, "background job event delivery");
    const deliveredTexts = [
      ...mockCodex.turnsStarted.slice(1).map((turn) => collectTextInput(turn.input)),
      ...mockCodex.steers.map((steer) => collectTextInput(steer.input))
    ];
    expect(deliveredTexts.some((text) => text.includes("background_job_event_json"))).toBe(true);
    expect(deliveredTexts.some((text) => text.includes("CI turned green."))).toBe(true);
    expect(deliveredTexts.some((text) => text.includes("\"job_kind\": \"watch_ci\""))).toBe(true);
  }, 60_000);

  it("delivers idle input and active follow-up input through one broker agent input contract", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const releaseTurn = createDeferred<void>();
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        await releaseTurn.promise;
        context.complete("CONTRACT_DONE");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      releaseTurn.resolve();
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const sessionKey = "C123:445.220";
    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot,
      extraEnv: {
        SLACK_ACTIVE_TURN_RECONCILE_INTERVAL_MS: "100"
      }
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-contract-initial", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "445.220",
      ts: "445.221",
      text: "<@UBOT> INITIAL_CONTRACT_INPUT"
    });

    await waitFor(() => mockCodex.turnsStarted.length === 1, "initial input starts one turn");
    await waitForSessionActive(tempRoot, sessionKey);

    await mockSlack.sendEvent("evt-contract-follow-up", {
      type: "message",
      user: "U234",
      channel: "C123",
      thread_ts: "445.220",
      ts: "445.222",
      text: "FOLLOW_UP_ACTIVE_INPUT"
    });

    await waitFor(
      () => mockCodex.steers.some((steer) => collectTextInput(steer.input).includes("FOLLOW_UP_ACTIVE_INPUT")),
      "active follow-up delivered immediately"
    );

    expect(mockCodex.turnsStarted).toHaveLength(1);
    expect(mockCodex.interrupts).toHaveLength(0);
    const inflightBeforeCompletion = await readInboundMessages(tempRoot, sessionKey);
    expect(inflightBeforeCompletion.find((message) => message.messageTs === "445.222")?.status).toBe("inflight");

    releaseTurn.resolve();
    await waitForSessionIdle(tempRoot, sessionKey);

    const traceEvents = await readAgentTraceEvents(tempRoot, sessionKey);
    const deliveredEvents = traceEvents.filter((event) => event.type === "agent_input_delivered");
    expect(deliveredEvents).toEqual(expect.arrayContaining([
      expect.objectContaining({
        status: "started_turn",
        metadata: expect.objectContaining({
          delivery: "started_turn"
        })
      }),
      expect.objectContaining({
        status: "joined_active_turn",
        metadata: expect.objectContaining({
          delivery: "joined_active_turn"
        })
      })
    ]));
    expect(deliveredEvents.filter((event) => event.status === "joined_active_turn")).toHaveLength(1);
    expect(traceEvents.map((event) => event.type)).toEqual(expect.arrayContaining([
      "agent_input_received",
      "agent_input_delivered",
      "agent_turn_started",
      "agent_turn_completed"
    ]));
  }, 90_000);

  it("queues active Slack follow-up input when immediate active delivery fails", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const releaseInitialTurn = createDeferred<void>();
    const releaseFollowUpTurn = createDeferred<void>();
    let steerFailureInjected = false;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnSteerRequest: (request) => {
        if (!steerFailureInjected && collectTextInput(request.input).includes("FOLLOW_UP_QUEUED_AFTER_ACTIVE_DELIVERY_FAILURE")) {
          steerFailureInjected = true;
          return "temporary active input delivery failure";
        }
        return undefined;
      },
      onTurnStart: async (context) => {
        if (mockCodex.turnsStarted.length === 1) {
          await releaseInitialTurn.promise;
          context.complete("INITIAL_DONE");
          return;
        }

        await releaseFollowUpTurn.promise;
        context.complete("FOLLOW_UP_DONE");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      releaseInitialTurn.resolve();
      releaseFollowUpTurn.resolve();
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const sessionKey = "C123:446.220";
    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-active-delivery-fallback-initial", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "446.220",
      ts: "446.221",
      text: "<@UBOT> INITIAL_ACTIVE_DELIVERY_FAILURE_TEST"
    });

    await waitFor(() => mockCodex.turnsStarted.length === 1, "initial turn before active delivery failure");
    await waitForSessionActive(tempRoot, sessionKey);

    await mockSlack.sendEvent("evt-active-delivery-fallback-follow-up", {
      type: "message",
      user: "U234",
      channel: "C123",
      thread_ts: "446.220",
      ts: "446.222",
      text: "FOLLOW_UP_QUEUED_AFTER_ACTIVE_DELIVERY_FAILURE"
    });

    await waitFor(() => steerFailureInjected, "active delivery failure injected");
    expect(mockCodex.steers).toHaveLength(0);

    releaseInitialTurn.resolve();
    await waitFor(
      () => mockCodex.turnsStarted.some((turn) => collectTextInput(turn.input).includes("FOLLOW_UP_QUEUED_AFTER_ACTIVE_DELIVERY_FAILURE")),
      "follow-up starts as queued turn after active delivery failure"
    );

    const followUpTurn = mockCodex.turnsStarted.find((turn) =>
      collectTextInput(turn.input).includes("FOLLOW_UP_QUEUED_AFTER_ACTIVE_DELIVERY_FAILURE")
    );
    expect(followUpTurn).toBeTruthy();
    expect(mockSlack.postedMessages.map((message) => message.text)).not.toContain(
      "I hit an internal issue while working on this thread. Send a quick follow-up and I will continue from the latest state."
    );
  }, 90_000);

  it("wakes a turn that ends without an explicit final, block, or wait state", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    let turnCount = 0;
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnCount += 1;
        context.complete("");
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: await getFreePort(),
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "666.220",
      ts: "666.221",
      text: "<@UBOT> 继续把这个做完"
    });

    let wakeText = "";
    await waitFor(() => {
      wakeText = findStartedTurnTextContaining(mockCodex, "explicit final, block, or wait state") ?? "";
      return Boolean(wakeText);
    }, "unexpected stop wake turn", 120_000);
    expect(wakeText).toContain("unexpected_turn_stop_json");
    expect(wakeText).toContain("explicit final, block, or wait state");
    await waitForSessionIdle(tempRoot, "C123:666.220");
    expect(turnCount).toBeGreaterThanOrEqual(2);
  }, 150_000);

  it("wakes a wait turn when no running async job backs that wait state", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    let turnCount = 0;
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnCount += 1;
        if (turnCount === 1) {
          await waitForSessionActive(tempRoot, "C123:777.220");
          await postJson(`${brokerBaseUrl}/slack/post-state`, {
            channel_id: "C123",
            thread_ts: "777.220",
            kind: "wait",
            reason: "waiting for async job"
          });
          context.complete("");
        }
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "777.220",
      ts: "777.221",
      text: "<@UBOT> 盯一下这个"
    });

    let wakeText = "";
    await waitFor(() => {
      wakeText = findStartedTurnTextContaining(mockCodex, "there is no running broker-managed async job") ?? "";
      return Boolean(wakeText);
    }, "wait-without-job wake turn");
    expect(wakeText).toContain("unexpected_turn_stop_json");
    expect(wakeText).toContain("there is no running broker-managed async job");
    await waitForSessionIdle(tempRoot, "C123:777.220");
    expect(turnCount).toBeGreaterThanOrEqual(2);
  }, 60_000);

  it("does not wake a silent wait turn when a running async job backs that wait state", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    let turnCount = 0;
    const firstTurnCompleted = createDeferred<void>();
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnCount += 1;
        if (turnCount === 1) {
          try {
            await waitForSessionActive(tempRoot, "C123:778.220");
            const registerResponse = await fetch(`${brokerBaseUrl}/jobs/register`, {
              method: "POST",
              headers: {
                "content-type": "application/json"
              },
              body: JSON.stringify({
                channel_id: "C123",
                thread_ts: "778.220",
                kind: "watch_ci",
                script: "#!/bin/sh\nsleep 30"
              })
            });
            expect(registerResponse.ok).toBe(true);

            await postJson(`${brokerBaseUrl}/slack/post-state`, {
              channel_id: "C123",
              thread_ts: "778.220",
              kind: "wait",
              reason: "waiting for async job"
            });
            context.complete("");
            firstTurnCompleted.resolve();
          } catch (error) {
            firstTurnCompleted.reject(error);
            throw error;
          }
        }
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "778.220",
      ts: "778.221",
      text: "<@UBOT> 盯一下这个"
    });

    await firstTurnCompleted.promise;
    await waitForSessionIdle(tempRoot, "C123:778.220", 60_000);
    const postedMessageCountAfterIdle = mockSlack.postedMessages.length;
    await delay(1_000);
    expect(turnCount).toBe(1);
    expect(mockSlack.postedMessages).toHaveLength(postedMessageCountAfterIdle);
  }, 90_000);

  it("does not wake a silent block turn that already recorded its blocker", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    let turnCount = 0;
    const firstTurnCompleted = createDeferred<void>();
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnCount += 1;
        if (turnCount === 1) {
          try {
            await waitForSessionActive(tempRoot, "C123:779.220");
            await postJson(`${brokerBaseUrl}/slack/post-state`, {
              channel_id: "C123",
              thread_ts: "779.220",
              kind: "block",
              reason: "waiting for user approval"
            });
            context.complete("");
            firstTurnCompleted.resolve();
          } catch (error) {
            firstTurnCompleted.reject(error);
            throw error;
          }
        }
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "779.220",
      ts: "779.221",
      text: "<@UBOT> 这步先停住"
    });

    await firstTurnCompleted.promise;
    await waitForSessionIdle(tempRoot, "C123:779.220", 60_000);
    const postedMessageCountAfterIdle = mockSlack.postedMessages.length;
    await delay(1_000);
    expect(turnCount).toBe(1);
    expect(mockSlack.postedMessages).toHaveLength(postedMessageCountAfterIdle);
  }, 90_000);

  it("does not wake a silent final turn or replay stale watcher events after completion", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    let turnCount = 0;
    let registeredJobId = "";
    let registeredJobToken = "";
    const firstTurnCompleted = createDeferred<void>();
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async (context) => {
        turnCount += 1;
        if (turnCount === 1) {
          try {
            await waitForSessionActive(tempRoot, "C123:780.220");
            const registerResponse = await fetch(`${brokerBaseUrl}/jobs/register`, {
              method: "POST",
              headers: {
                "content-type": "application/json"
              },
              body: JSON.stringify({
                channel_id: "C123",
                thread_ts: "780.220",
                kind: "watch_ci",
                script: "#!/bin/sh\nsleep 30"
              })
            });
            expect(registerResponse.ok).toBe(true);
            const registerJson = await registerResponse.json() as {
              job: { id: string; token: string };
            };
            registeredJobId = registerJson.job.id;
            registeredJobToken = registerJson.job.token;

            await postJson(`${brokerBaseUrl}/slack/post-state`, {
              channel_id: "C123",
              thread_ts: "780.220",
              kind: "final"
            });
            context.complete("");
            firstTurnCompleted.resolve();
          } catch (error) {
            firstTurnCompleted.reject(error);
            throw error;
          }
        }
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "780.220",
      ts: "780.221",
      text: "<@UBOT> 合并之后继续盯一下"
    });

    await firstTurnCompleted.promise;
    await waitForSessionIdle(tempRoot, "C123:780.220", 60_000);
    expect(turnCount).toBe(1);
    expect(registeredJobId).not.toBe("");
    expect(registeredJobToken).not.toBe("");

    const postedMessageCountAfterIdle = mockSlack.postedMessages.length;
    const eventResponse = await fetch(`${brokerBaseUrl}/jobs/${registeredJobId}/event`, {
      method: "POST",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        token: registeredJobToken,
        event_kind: "state_changed",
        summary: "PR merged on main"
      })
    });
    expect(eventResponse.ok).toBe(true);

    await delay(1_000);
    expect(turnCount).toBe(1);
    expect(mockSlack.postedMessages).toHaveLength(postedMessageCountAfterIdle);
  }, 90_000);

  it("does not recover the broker's own Slack messages as inbound work", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer({
      onTurnStart: async () => {
        await postJson(`${brokerBaseUrl}/slack/post-message`, {
          channel_id: "C123",
          thread_ts: "555.220",
          text: "broker self reply"
        });
      }
    });
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "555.220",
      ts: "555.221",
      text: "<@UBOT> 触发一次回复"
    });
    await waitFor(
      () => mockSlack.postedMessages.some((message) => message.text === "broker self reply"),
      "bot reply",
      30_000
    );
    await waitForSessionIdle(tempRoot, "C123:555.220", 30_000);
    const turnCountBeforeRestart = mockCodex.turnsStarted.length;

    await broker.stop();
    cleanups.pop();

    const restarted = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => restarted.stop());

    await delay(2_000);
    expect(mockCodex.turnsStarted).toHaveLength(turnCountBeforeRestart);
  }, 60_000);

  it("converts markdownish Slack posts to mrkdwn before delivery", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-format-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "777.220",
      ts: "777.221",
      text: "<@UBOT> 开个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "format session bootstrap turn");
    await waitForSessionIdle(tempRoot, "C123:777.220");

    await postJson(`${brokerBaseUrl}/slack/post-message`, {
      channel_id: "C123",
      thread_ts: "777.220",
      text: "## Summary\n- **done**\n- [docs](https://example.com)\n- `https://linear.app/settings/api`"
    });

    await waitFor(
      () => mockSlack.postedMessages.some((message) => message.threadTs === "777.220" && message.text.includes("*Summary*")),
      "converted slack markdown post"
    );

    const posted = mockSlack.postedMessages.find((message) => message.threadTs === "777.220" && message.text.includes("*Summary*"));
    expect(posted?.text).toBe(
      "*Summary*\n• *done*\n• <https://example.com|docs>\n• `https://linear.\u200Bapp/settings/api`"
    );
  }, 60_000);

  it("chunks long Slack posts after markdownish conversion", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-codex-broker-e2e-"));
    cleanups.push(async () => {
      await removeTempRoot(tempRoot);
    });

    const brokerPort = await getFreePort();
    const brokerBaseUrl = `http://127.0.0.1:${brokerPort}`;
    const mockSlack = new MockSlackServer("UBOT", {
      botId: "BBOT",
      appId: "AAPP"
    });
    const mockCodex = new MockCodexAppServer();
    const slackPort = await mockSlack.start();
    const codexUrl = await mockCodex.start();
    cleanups.push(async () => {
      await mockCodex.stop();
      await mockSlack.stop();
    });

    const broker = await startBrokerProcess({
      port: brokerPort,
      slackPort,
      codexUrl,
      tempRoot
    });
    cleanups.push(() => broker.stop());

    await mockSlack.sendEvent("evt-long-format-session", {
      type: "app_mention",
      user: "U123",
      channel: "C123",
      thread_ts: "888.220",
      ts: "888.221",
      text: "<@UBOT> 开个 session"
    });
    await waitFor(() => mockCodex.turnsStarted.length >= 1, "long format session bootstrap turn");
    await waitForSessionIdle(tempRoot, "C123:888.220");

    const markdownUnit = "1. **item**\n";
    const mrkdwnUnit = "1. *item*\n";
    const markdown = markdownUnit.repeat(400).trimEnd();

    await postJson(`${brokerBaseUrl}/slack/post-message`, {
      channel_id: "C123",
      thread_ts: "888.220",
      text: markdown
    });

    await waitFor(
      () =>
        mockSlack.postedMessages.filter(
          (message) => message.threadTs === "888.220" && message.text.startsWith("1. *item*")
        ).length >= 2,
      "multi-chunk converted slack post"
    );

    const posted = mockSlack.postedMessages.filter(
      (message) => message.threadTs === "888.220" && message.text.startsWith("1. *item*")
    );
    expect(posted).toHaveLength(2);
    expect(posted[0]?.text).toBe(mrkdwnUnit.repeat(350));
    expect(posted[1]?.text).toBe(mrkdwnUnit.repeat(49) + "1. *item*");
    expect(posted[0]?.text).not.toContain("**");
    expect(posted[1]?.text).not.toContain("**");
  }, 60_000);
});

async function startBrokerProcess(options: {
  readonly port: number;
  readonly slackPort: number;
  readonly codexUrl: string;
  readonly tempRoot: string;
  readonly extraEnv?: Record<string, string>;
}): Promise<{
  readonly baseUrl: string;
  readonly stop: () => Promise<void>;
  readonly logs: readonly string[];
}> {
  const logs: string[] = [];
  const child = spawn("pnpm", ["exec", "tsx", "src/index.ts"], {
    cwd: brokerRoot,
    env: {
      ...process.env,
      ...options.extraEnv,
      SLACK_APP_TOKEN: "xapp-test",
      SLACK_BOT_TOKEN: "xoxb-test",
      SLACK_API_BASE_URL: `http://127.0.0.1:${options.slackPort}/api`,
      SLACK_SOCKET_OPEN_URL: "apps.connections.open",
      SLACK_INITIAL_THREAD_HISTORY_COUNT: "8",
      SLACK_HISTORY_API_MAX_LIMIT: "50",
      STATE_DIR: path.join(options.tempRoot, "state"),
      SESSIONS_ROOT: path.join(options.tempRoot, "sessions"),
      REPOS_ROOT: path.join(options.tempRoot, "repos"),
      JOBS_ROOT: path.join(options.tempRoot, "jobs"),
      LOG_DIR: path.join(options.tempRoot, "logs"),
      CODEX_HOME: path.join(options.tempRoot, "codex-home"),
      PORT: String(options.port),
      BROKER_HTTP_BASE_URL: `http://127.0.0.1:${options.port}`,
      CODEX_APP_SERVER_URL: options.codexUrl,
      DEBUG: "1"
    },
    stdio: ["ignore", "pipe", "pipe"]
  });

  child.stdout.on("data", (chunk) => {
    logs.push(chunk.toString());
  });
  child.stderr.on("data", (chunk) => {
    logs.push(chunk.toString());
  });

  await waitForHttpReady(`http://127.0.0.1:${options.port}`, logs);

  return {
    baseUrl: `http://127.0.0.1:${options.port}`,
    logs,
    stop: async () => {
      if (child.exitCode !== null || child.signalCode !== null) {
        return;
      }

      child.kill("SIGTERM");
      const graceful = await Promise.race([
        once(child, "exit").then(() => true),
        delay(5_000).then(() => false)
      ]);
      if (graceful) {
        return;
      }

      child.kill("SIGKILL");
      await once(child, "exit");
    }
  };
}

async function waitForHttpReady(url: string, logs: readonly string[], timeoutMs = DEFAULT_E2E_TIMEOUT_MS): Promise<void> {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);
      if (response.ok) {
        return;
      }
    } catch {
      // ignore and retry
    }

    await delay(200);
  }

  throw new Error(`Timed out waiting for broker readiness: ${url}\n${logs.join("")}`);
}

async function waitFor(predicate: () => boolean | Promise<boolean>, label: string, timeoutMs = DEFAULT_E2E_TIMEOUT_MS): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if (await predicate()) {
        return;
      }
    } catch (error) {
      if (!isTransientSqliteLock(error)) {
        throw error;
      }
    }
    await delay(100);
  }

  throw new Error(`Timed out waiting for ${label}`);
}

function isTransientSqliteLock(error: unknown): boolean {
  return error instanceof Error && /database is locked/i.test(error.message);
}

async function waitForSessionIdle(
  tempRoot: string,
  sessionKey: string,
  timeoutMs = DEFAULT_E2E_TIMEOUT_MS
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastSession: SlackSessionRecord | undefined;

  while (Date.now() < deadline) {
    try {
      const session = await readSessionRecord(tempRoot, sessionKey);
      lastSession = session;
      if (!session.activeTurnId) {
        return;
      }
    } catch {
      // session file may not exist yet
    }

    await delay(100);
  }

  throw new Error(`Timed out waiting for session idle: ${sessionKey}; lastSession=${JSON.stringify({
    activeTurnId: lastSession?.activeTurnId ?? null,
    lastTurnSignalKind: lastSession?.lastTurnSignalKind ?? null,
    lastTurnSignalTurnId: lastSession?.lastTurnSignalTurnId ?? null
  })}`);
}

async function waitForSessionActive(
  tempRoot: string,
  sessionKey: string,
  timeoutMs = DEFAULT_E2E_TIMEOUT_MS
): Promise<void> {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    try {
      const session = await readSessionRecord(tempRoot, sessionKey);
      if (session.activeTurnId) {
        return;
      }
    } catch {
      // session file may not exist yet
    }

    await delay(100);
  }

  throw new Error(`Timed out waiting for session active: ${sessionKey}`);
}

async function readSessionRecord(tempRoot: string, sessionKey: string): Promise<SlackSessionRecord> {
  const store = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
  await store.load();
  try {
    const session = store.getSession(sessionKey);
    if (!session) {
      throw new Error(`Unknown session: ${sessionKey}`);
    }
    return session;
  } finally {
    store.close();
  }
}

async function readInboundMessages(tempRoot: string, sessionKey: string): Promise<PersistedInboundMessage[]> {
  const store = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
  await store.load();
  try {
    return store.listInboundMessages({ sessionKey });
  } finally {
    store.close();
  }
}

async function readAgentTraceEvents(tempRoot: string, sessionKey: string): Promise<PersistedAgentTraceEvent[]> {
  const store = new StateStore(path.join(tempRoot, "state"), path.join(tempRoot, "sessions"));
  await store.load();
  try {
    return store.listAgentTraceEvents(sessionKey);
  } finally {
    store.close();
  }
}

async function delay(timeoutMs: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, timeoutMs));
}

async function pathExists(targetPath: string): Promise<boolean> {
  try {
    await fs.access(targetPath);
    return true;
  } catch {
    return false;
  }
}

async function removeTempRoot(tempRoot: string): Promise<void> {
  let lastError: unknown = undefined;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      await fs.rm(tempRoot, { force: true, recursive: true });
      return;
    } catch (error) {
      lastError = error;
      await delay(100 * (attempt + 1));
    }
  }

  throw lastError;
}

async function getFreePort(): Promise<number> {
  const server = http.createServer();
  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve());
  });

  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("failed to allocate free port");
  }

  const port = address.port;
  await new Promise<void>((resolve) => {
    server.close(() => resolve());
  });
  return port;
}

function collectTextInput(input: readonly CodexInputItem[]): string {
  return input
    .filter((item): item is Extract<CodexInputItem, { type: "text" }> => item.type === "text")
    .map((item) => item.text)
    .join("\n");
}

function findStartedTurnTextContaining(mockCodex: MockCodexAppServer, needle: string): string | undefined {
  return mockCodex.turnsStarted.map((turn) => collectTextInput(turn.input)).find((text) => text.includes(needle));
}

async function postJson(url: string, payload: Record<string, unknown>): Promise<void> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify(payload)
  });

  if (!response.ok) {
    throw new Error(`HTTP ${response.status} for ${url}: ${await response.text()}`);
  }
}

function createDeferred<T>(): {
  readonly promise: Promise<T>;
  readonly resolve: (value: T | PromiseLike<T>) => void;
  readonly reject: (reason?: unknown) => void;
} {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((innerResolve, innerReject) => {
    resolve = innerResolve;
    reject = innerReject;
  });

  return {
    promise,
    resolve,
    reject
  };
}
