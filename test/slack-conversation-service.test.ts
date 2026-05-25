import { EventEmitter } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { AppConfig } from "../src/config.js";
import { AuthProfileUnavailableError } from "../src/services/agent-runtime/session-auth-profile-runtime.js";
import { SlackConversationService } from "../src/services/slack/slack-conversation-service.js";
import { SlackApiError } from "../src/services/slack/slack-api.js";
import { SessionManager } from "../src/services/session-manager.js";
import { StateStore } from "../src/store/state-store.js";
import type { PersistedAgentTraceEvent, PersistedInboundMessage, SlackSessionRecord } from "../src/types.js";

const TEST_SESSION: SlackSessionRecord = {
  key: "C123:111.222",
  channelId: "C123",
  rootThreadTs: "111.222",
  workspacePath: "/tmp/workspace",
  agentSessionId: "thread-1",
  activeTurnId: "turn-1",
  createdAt: new Date().toISOString(),
  updatedAt: new Date().toISOString()
};

const TEST_CONFIG = {
  slackInitialThreadHistoryCount: 8,
  slackHistoryApiMaxLimit: 50,
  slackActiveTurnReconcileIntervalMs: 15_000,
  slackActiveTurnStallTimeoutMs: 10 * 60_000,
  slackMissedThreadRecoveryIntervalMs: 15_000,
  adminBaseUrl: "https://admin.example"
} as AppConfig;

afterEach(() => {
  vi.restoreAllMocks();
});

describe("SlackConversationService", () => {
  it("does not block startup on persisted active turn reconciliation", async () => {
    const agentRuntime = new EventEmitter();
    const never = new Promise<never>(() => {});
    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        listSessions: vi.fn(() => [TEST_SESSION]),
        getSessionByKey: vi.fn(() => TEST_SESSION),
        setActiveTurnId: vi.fn(),
        upsertAgentTraceEvent: vi.fn()
      } as never,
      agentRuntime: Object.assign(agentRuntime, {
        ensureSession: vi.fn(async () => ({ id: TEST_SESSION.agentSessionId })),
        readTurn: vi.fn(() => never)
      }) as never,
      slackApi: {
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    const startup = service.start().then(() => "started");
    await expect(Promise.race([
      startup,
      new Promise((resolve) => setTimeout(() => resolve("blocked"), 50))
    ])).resolves.toBe("started");

    await service.stop();
  });

  it("coalesces live active-turn reconcile timer ticks instead of overlapping passes", async () => {
    const source = await fs.readFile(
      new URL("../src/services/slack/slack-conversation-service.ts", import.meta.url),
      "utf8"
    );

    expect(source).toContain("#activeTurnReconcilePromise");
    expect(source).toContain("#runLiveActiveTurnReconcileOnce");
    expect(source).toMatch(/if \(\s*this\.#activeTurnReconcilePromise\s*\)/);
  });

  it("removes the agent runtime event listener on stop", async () => {
    const agentRuntime = new EventEmitter();
    const getSessionByKey = vi.fn(() => TEST_SESSION);
    const setAssistantThreadStatus = vi.fn(async () => undefined);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        getSessionByKey,
        upsertAgentTraceEvent: vi.fn()
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        setAssistantThreadStatus,
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    expect(agentRuntime.listenerCount("event")).toBe(1);

    agentRuntime.emit("event", {
      type: "agent.tool.started",
      agentSessionId: TEST_SESSION.agentSessionId,
      brokerSessionKey: TEST_SESSION.key,
      turnId: TEST_SESSION.activeTurnId,
      callId: "call-1",
      name: "exec_command",
      at: new Date().toISOString()
    });

    await vi.waitFor(() => {
      expect(setAssistantThreadStatus).toHaveBeenCalledTimes(1);
    });

    await service.stop();

    expect(agentRuntime.listenerCount("event")).toBe(0);
    expect(setAssistantThreadStatus).toHaveBeenCalledTimes(2);

    agentRuntime.emit("event", {
      type: "agent.tool.started",
      agentSessionId: TEST_SESSION.agentSessionId,
      brokerSessionKey: TEST_SESSION.key,
      turnId: TEST_SESSION.activeTurnId,
      callId: "call-2",
      name: "exec_command",
      at: new Date().toISOString()
    });

    await Promise.resolve();
    expect(setAssistantThreadStatus).toHaveBeenCalledTimes(2);
  });

  it("skips runtime events without a broker session key", async () => {
    const agentRuntime = new EventEmitter();
    const getSessionByKey = vi.fn(() => TEST_SESSION);
    const setAssistantThreadStatus = vi.fn(async () => undefined);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        getSessionByKey,
        upsertAgentTraceEvent: vi.fn()
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        setAssistantThreadStatus,
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    agentRuntime.emit("event", {
      type: "agent.error",
      code: "runtime_error",
      message: "missing session",
      recoverable: false,
      at: new Date().toISOString()
    });

    await Promise.resolve();

    expect(getSessionByKey).not.toHaveBeenCalled();
    expect(setAssistantThreadStatus).not.toHaveBeenCalled();

    await service.stop();
  });

  it("persists normalized agent runtime events as agent trace events", async () => {
    const agentRuntime = new EventEmitter();
    const records: PersistedAgentTraceEvent[] = [];
    const upsertAgentTraceEvent = vi.fn(async (record: PersistedAgentTraceEvent) => {
      records.push(record);
    });

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        getSessionByKey: vi.fn(() => TEST_SESSION),
        upsertAgentTraceEvent
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    agentRuntime.emit("event", {
      type: "agent.session.started",
      agentSessionId: TEST_SESSION.agentSessionId,
      brokerSessionKey: TEST_SESSION.key,
      systemPrompt: [
        "System instruction",
        "",
        "Personal long-lived memory from ~/.codex/AGENT.md:",
        "- remember the admin language",
        "",
        "Slack thread message model:",
        "live thread"
      ].join("\n"),
      memory: "- remember the admin language",
      at: new Date().toISOString()
    });
    agentRuntime.emit("event", {
      type: "agent.tool.started",
      agentSessionId: TEST_SESSION.agentSessionId,
      brokerSessionKey: TEST_SESSION.key,
      turnId: TEST_SESSION.activeTurnId,
      callId: "call-1",
      name: "exec_command",
      at: new Date().toISOString()
    });

    await vi.waitFor(() => {
      expect(upsertAgentTraceEvent).toHaveBeenCalledTimes(3);
    });
    expect(records.map((record) => record.type)).toEqual(expect.arrayContaining([
      "agent_system_prompt",
      "agent_memory",
      "agent_tool_call"
    ]));
    expect(records.find((record) => record.type === "agent_tool_call")).toEqual(expect.objectContaining({
      source: "agent_runtime",
      toolName: "exec_command"
    }));

    await service.stop();
  });

  it("converts file upload initial comments from markdownish to mrkdwn", async () => {
    const agentRuntime = new EventEmitter();
    const uploadThreadFile = vi.fn(async () => ({
      fileId: "F123"
    }));
    const setLastSlackReplyAt = vi.fn(async () => TEST_SESSION);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        setLastSlackReplyAt
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        uploadThreadFile,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    await service.postSlackFile({
      channelId: "C123",
      rootThreadTs: "111.222",
      contentBase64: Buffer.from("hello world").toString("base64"),
      filename: "report.txt",
      initialComment: "## Summary\n- **done**\n- [docs](https://example.com)"
    });

    expect(uploadThreadFile).toHaveBeenCalledWith(
      expect.objectContaining({
        initialComment: "*Summary*\n• *done*\n• <https://example.com|docs>"
      })
    );
    expect(setLastSlackReplyAt).toHaveBeenCalledTimes(1);

    await service.stop();
  });

  it("records a silent stop state without owning active turn completion", async () => {
    const agentRuntime = new EventEmitter();
    const recordTurnSignal = vi.fn(async () => TEST_SESSION);
    const setActiveTurnId = vi.fn(async () => ({
      ...TEST_SESSION,
      activeTurnId: undefined
    }));
    const listInboundMessages = vi.fn((): PersistedInboundMessage[] => []);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        getSession: vi.fn(() => TEST_SESSION),
        recordTurnSignal,
        setActiveTurnId,
        listInboundMessages,
        updateInboundMessagesForBatch: vi.fn(async () => []),
        setLastDeliveredMessageTs: vi.fn(async () => TEST_SESSION)
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    await service.postSlackState({
      channelId: "C123",
      rootThreadTs: "111.222",
      kind: "wait",
      reason: "waiting on async job"
    });

    expect(recordTurnSignal).toHaveBeenCalledWith("C123", "111.222", expect.objectContaining({
      turnId: "turn-1",
      kind: "wait",
      reason: "waiting on async job"
    }));
    expect(setActiveTurnId).not.toHaveBeenCalled();
    expect(listInboundMessages).toHaveBeenCalledWith(expect.objectContaining({
      channelId: "C123",
      rootThreadTs: "111.222",
      status: "inflight",
      batchId: "turn-1"
    }));

    await service.stop();
  });

  it("records a visible final Slack message without owning active turn completion", async () => {
    const agentRuntime = new EventEmitter();
    const recordTurnSignal = vi.fn(async () => TEST_SESSION);
    const setActiveTurnId = vi.fn(async () => ({
      ...TEST_SESSION,
      activeTurnId: undefined
    }));
    const postThreadMessage = vi.fn(async (_channelId: string, _threadTs: string, _text: string) => "333.444");
    const setLastSlackReplyAt = vi.fn(async () => TEST_SESSION);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions: {
        recordTurnSignal,
        setActiveTurnId,
        setLastSlackReplyAt,
        listInboundMessages: vi.fn((): PersistedInboundMessage[] => []),
        updateInboundMessagesForBatch: vi.fn(async () => []),
        setLastDeliveredMessageTs: vi.fn(async () => TEST_SESSION)
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        postThreadMessage,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {
        rememberPostedMessageTs: vi.fn()
      } as never
    });

    await service.postSlackMessage({
      channelId: "C123",
      rootThreadTs: "111.222",
      text: "done",
      kind: "final"
    });

    expect(postThreadMessage).toHaveBeenCalledTimes(1);
    expect(recordTurnSignal).toHaveBeenCalledWith("C123", "111.222", expect.objectContaining({
      turnId: "turn-1",
      kind: "final"
    }));
    expect(setActiveTurnId).not.toHaveBeenCalled();

    await service.stop();
  });

  it("stops missed-message recovery on Slack rate limit and backs off the next periodic scan", async () => {
    const agentRuntime = new EventEmitter();
    const sessions = [
      {
        ...TEST_SESSION,
        activeTurnId: undefined,
        lastObservedMessageTs: "111.223",
        updatedAt: new Date().toISOString()
      },
      {
        ...TEST_SESSION,
        key: "C123:222.333",
        rootThreadTs: "222.333",
        activeTurnId: undefined,
        lastObservedMessageTs: "222.334",
        updatedAt: new Date().toISOString()
      }
    ];
    const listThreadMessages = vi.fn(async () => {
      throw new SlackApiError({
        path: "conversations.replies",
        status: 429,
        statusText: "Too Many Requests",
        retryAfterMs: 120_000
      });
    });

    const service = new SlackConversationService({
      config: {
        ...TEST_CONFIG,
        slackMissedThreadRecoveryIntervalMs: 100,
        slackActiveTurnReconcileIntervalMs: 100
      } as AppConfig,
      sessions: {
        listSessions: vi.fn(() => sessions),
        getLatestSlackInboundMessageTs: vi.fn()
      } as never,
      agentRuntime: agentRuntime as never,
      slackApi: {
        listThreadMessages,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {
        shouldIgnoreThreadMessage: vi.fn(() => false)
      } as never
    });

    await service.recoverMissedThreadMessages("periodic");
    await service.recoverMissedThreadMessages("periodic");

    expect(listThreadMessages).toHaveBeenCalledTimes(1);
    expect(listThreadMessages).toHaveBeenCalledWith(expect.objectContaining({
      channelId: "C123",
      rootThreadTs: "111.222"
    }));

    await service.stop();
  });

  it("resets a session by dropping the old agent history and dispatching a fresh Slack-context wakeup", async () => {
    const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-reset-state-"));
    const sessionsRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-reset-sessions-"));
    const stateStore = new StateStore(stateDir, sessionsRoot);
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot
    });
    await sessions.load();
    let session = await sessions.ensureSession("C123", "111.222", {
      channelName: "bridge-app",
      channelType: "channel"
    });
    session = await sessions.setAgentSessionId(session.channelId, session.rootThreadTs, "thread-old");
    session = await sessions.setActiveTurnId(session.channelId, session.rootThreadTs, "turn-old");
    await sessions.upsertInboundMessage({
      key: `${session.key}:111.223`,
      sessionKey: session.key,
      channelId: session.channelId,
      channelType: session.channelType,
      rootThreadTs: session.rootThreadTs,
      messageTs: "111.223",
      source: "thread_reply",
      userId: "U123",
      text: "old pending",
      status: "pending",
      createdAt: "2026-03-19T00:00:01.000Z",
      updatedAt: "2026-03-19T00:00:01.000Z"
    });
    await sessions.upsertInboundMessage({
      key: `${session.key}:111.224`,
      sessionKey: session.key,
      channelId: session.channelId,
      channelType: session.channelType,
      rootThreadTs: session.rootThreadTs,
      messageTs: "111.224",
      source: "thread_reply",
      userId: "U123",
      text: "old inflight",
      status: "inflight",
      batchId: "turn-old",
      createdAt: "2026-03-19T00:00:02.000Z",
      updatedAt: "2026-03-19T00:00:02.000Z"
    });

    let submittedText = "";
    const agentRuntime = Object.assign(new EventEmitter(), {
      start: vi.fn(async () => undefined),
      stop: vi.fn(async () => undefined),
      setSlackBotIdentity: vi.fn(),
      getCapabilities: vi.fn(),
      ensureSession: vi.fn(async (nextSession: SlackSessionRecord) => {
        expect(nextSession.agentSessionId).toBeUndefined();
        expect(nextSession.activeTurnId).toBeUndefined();
        return {
          id: "thread-new",
          brokerSessionKey: nextSession.key,
          runtime: "test",
          createdAt: "2026-03-19T00:00:03.000Z"
        };
      }),
      submitInput: vi.fn(async (input: { readonly input: readonly { readonly type: string; readonly text?: string }[]; readonly inputId: string }) => {
        submittedText = input.input.find((item) => item.type === "text")?.text ?? "";
        return {
          receipt: {
            agentSessionId: "thread-new",
            turnId: "turn-new",
            inputId: input.inputId,
            delivery: "started_turn" as const,
            deliveredAt: "2026-03-19T00:00:04.000Z"
          },
          completion: Promise.resolve({
            agentSessionId: "thread-new",
            turnId: "turn-new",
            finalMessage: "",
            aborted: true
          })
        };
      }),
      interrupt: vi.fn(async () => undefined),
      readSession: vi.fn(),
      readTurn: vi.fn()
    });
    const listThreadMessages = vi.fn(async () => [
      {
        channelId: "C123",
        channelType: "channel",
        rootThreadTs: "111.222",
        messageTs: "111.222",
        userId: "U111",
        text: "原始需求",
        senderKind: "user" as const
      },
      {
        channelId: "C123",
        channelType: "channel",
        rootThreadTs: "111.222",
        messageTs: "111.225",
        userId: "U222",
        text: "最新补充",
        senderKind: "user" as const
      }
    ]);

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions,
      agentRuntime: agentRuntime as never,
      slackApi: {
        listThreadMessages,
        postThreadMessage: vi.fn(async () => "333.444"),
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn(),
        getUserIdentity: vi.fn(async (userId: string) => ({
          userId,
          mention: `<@${userId}>`,
          displayName: userId === "U222" ? "用户二" : "用户一"
        })),
        downloadImageAsDataUrl: vi.fn()
      } as never,
      selfMessageFilter: {
        rememberPostedMessageTs: vi.fn(),
        shouldIgnoreThreadMessage: vi.fn(() => false)
      } as never
    });

    const reset = await service.resetSession(session.key);
    await vi.waitFor(() => {
      expect(agentRuntime.submitInput).toHaveBeenCalledTimes(1);
    });

    expect(reset).toMatchObject({
      clearedInboundCount: 2,
      resumedCount: 1,
      interruptedActiveTurn: true,
      previousAgentSessionId: "thread-old",
      previousActiveTurnId: "turn-old",
      historyMessageCount: 2,
      authBlocked: false
    });
    expect(agentRuntime.interrupt).toHaveBeenCalledWith(expect.objectContaining({
      agentSessionId: "thread-old",
      activeTurnId: "turn-old"
    }));
    expect(submittedText).toContain("previous agent thread/history was intentionally discarded");
    expect(submittedText).toContain("原始需求");
    expect(submittedText).toContain("最新补充");

    const latest = sessions.getSessionByKey(session.key);
    expect(latest).toMatchObject({
      agentSessionId: "thread-new",
      activeTurnId: undefined
    });
    expect(sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: ["pending", "inflight"]
    })).toHaveLength(0);
    const resetMessage = sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      source: "admin_session_reset"
    })[0];
    expect(resetMessage).toMatchObject({
      messageTs: reset.resetMessageTs,
      status: "done",
      text: expect.stringContaining("丢弃旧 agent history")
    });
    expect(sessions.listAgentTraceEvents(session.key)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          type: "agent_session_reset",
          title: "Session 已重置",
          summary: "已清空 agent history 并重新唤起 bot"
        })
      ])
    );

    await service.stop();
    stateStore.close();
    await fs.rm(stateDir, { force: true, recursive: true });
    await fs.rm(sessionsRoot, { force: true, recursive: true });
  });

  it("deletes a session without ensuring a new agent session first", async () => {
    const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-delete-state-"));
    const sessionsRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-delete-sessions-"));
    const stateStore = new StateStore(stateDir, sessionsRoot);
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot
    });
    await sessions.load();
    let session = await sessions.ensureSession("C123", "111.222");
    session = await sessions.setAgentSessionId(session.channelId, session.rootThreadTs, "thread-old");
    session = await sessions.setActiveTurnId(session.channelId, session.rootThreadTs, "turn-old");

    const agentRuntime = Object.assign(new EventEmitter(), {
      ensureSession: vi.fn(async () => {
        throw new Error("delete should not ensure a replacement session");
      }),
      interrupt: vi.fn(async () => undefined),
      readSession: vi.fn(),
      readTurn: vi.fn()
    });
    const setAssistantThreadStatus = vi.fn(async () => undefined);
    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions,
      agentRuntime: agentRuntime as never,
      slackApi: {
        setAssistantThreadStatus,
        addReaction: vi.fn(),
        removeReaction: vi.fn()
      } as never,
      selfMessageFilter: {} as never
    });

    const deleted = await service.deleteSession(session.key);

    expect(deleted).toMatchObject({
      deleted: true,
      interruptedActiveTurn: true,
      previousAgentSessionId: "thread-old",
      previousActiveTurnId: "turn-old",
      clearedInboundCount: 0
    });
    expect(agentRuntime.ensureSession).not.toHaveBeenCalled();
    expect(agentRuntime.interrupt).toHaveBeenCalledWith(expect.objectContaining({
      agentSessionId: "thread-old",
      activeTurnId: "turn-old"
    }));
    expect(sessions.getSessionByKey(session.key)).toBeUndefined();

    await service.stop();
    stateStore.close();
    await fs.rm(stateDir, { force: true, recursive: true });
    await fs.rm(sessionsRoot, { force: true, recursive: true });
  });

  it("keeps Slack input pending and posts one session link when auth profile is unavailable", async () => {
    const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-auth-block-state-"));
    const sessionsRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-auth-block-sessions-"));
    const stateStore = new StateStore(stateDir, sessionsRoot);
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot
    });
    await sessions.load();
    let session = await sessions.ensureSession("C123", "111.222");
    session = await sessions.setSessionAuthProfile(session.key, "empty-profile", {
      boundAt: "2026-05-09T00:00:00.000Z"
    });
    const agentRuntime = Object.assign(new EventEmitter(), {
      start: vi.fn(async () => undefined),
      stop: vi.fn(async () => undefined),
      setSlackBotIdentity: vi.fn(),
      getCapabilities: vi.fn(),
      ensureSession: vi.fn(async () => {
        throw new AuthProfileUnavailableError({
          sessionKey: session.key,
          profileName: "empty-profile",
          reason: "primary_quota_exhausted"
        });
      }),
      submitInput: vi.fn(),
      interrupt: vi.fn(),
      readSession: vi.fn(),
      readTurn: vi.fn()
    });
    const postThreadMessage = vi.fn(async () => "333.444");

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions,
      agentRuntime: agentRuntime as never,
      slackApi: {
        postThreadMessage,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn(),
        getUserIdentity: vi.fn(async () => null)
      } as never,
      selfMessageFilter: {
        rememberPostedMessageTs: vi.fn(),
        shouldIgnoreThreadMessage: vi.fn(() => false)
      } as never
    });

    await service.acceptInboundMessage(session, {
      source: "thread_reply",
      channelId: "C123",
      rootThreadTs: "111.222",
      messageTs: "111.223",
      userId: "U123",
      text: "继续"
    });

    await vi.waitFor(() => {
      expect(postThreadMessage).toHaveBeenCalledTimes(2);
    });
    const postCalls = postThreadMessage.mock.calls as unknown as Array<[string, string, string]>;
    expect(postCalls[0]).toEqual([
      "C123",
      "111.222",
      "<https://admin.example/admin/sessions/C123%3A111.222|查看会话活动时间线>"
    ]);
    expect(postCalls[1]?.[2]).toContain("账号额度不可用");
    expect(postCalls[1]?.[2]).toContain("https://admin.example/admin/sessions/C123%3A111.222");

    expect(sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: "pending"
    })).toHaveLength(1);
    expect(sessions.getSessionByKey(session.key)).toMatchObject({
      authProfileName: "empty-profile",
      authBlockReason: "primary_quota_exhausted",
      authBlockedNoticePostedAt: expect.any(String)
    });

    await service.acceptInboundMessage(sessions.getSessionByKey(session.key)!, {
      source: "thread_reply",
      channelId: "C123",
      rootThreadTs: "111.222",
      messageTs: "111.224",
      userId: "U123",
      text: "再发一条"
    });
    await vi.waitFor(() => {
      expect(agentRuntime.ensureSession).toHaveBeenCalledTimes(2);
    });
    expect(postThreadMessage).toHaveBeenCalledTimes(2);

    await service.stop();
    stateStore.close();
    await fs.rm(stateDir, { force: true, recursive: true });
    await fs.rm(sessionsRoot, { force: true, recursive: true });
  });

  it("keeps Slack input pending without asking for manual switch when auth profile status reads fail", async () => {
    const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-auth-probe-state-"));
    const sessionsRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-auth-probe-sessions-"));
    const stateStore = new StateStore(stateDir, sessionsRoot);
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot
    });
    await sessions.load();
    let session = await sessions.ensureSession("C123", "111.222");
    session = await sessions.setSessionAuthProfile(session.key, "bound-profile", {
      boundAt: "2026-05-09T00:00:00.000Z"
    });
    const agentRuntime = Object.assign(new EventEmitter(), {
      start: vi.fn(async () => undefined),
      stop: vi.fn(async () => undefined),
      setSlackBotIdentity: vi.fn(),
      getCapabilities: vi.fn(),
      ensureSession: vi.fn(async () => {
        throw new AuthProfileUnavailableError({
          sessionKey: session.key,
          profileName: "bound-profile",
          reason: "account_probe_failed"
        });
      }),
      submitInput: vi.fn(),
      interrupt: vi.fn(),
      readSession: vi.fn(),
      readTurn: vi.fn()
    });
    const postThreadMessage = vi.fn(async () => "333.444");

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions,
      agentRuntime: agentRuntime as never,
      slackApi: {
        postThreadMessage,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn(),
        getUserIdentity: vi.fn(async () => null)
      } as never,
      selfMessageFilter: {
        rememberPostedMessageTs: vi.fn(),
        shouldIgnoreThreadMessage: vi.fn(() => false)
      } as never
    });

    await service.acceptInboundMessage(session, {
      source: "thread_reply",
      channelId: "C123",
      rootThreadTs: "111.222",
      messageTs: "111.223",
      userId: "U123",
      text: "继续"
    });

    await vi.waitFor(() => {
      expect(agentRuntime.ensureSession).toHaveBeenCalledTimes(1);
    });
    const postedTexts = (postThreadMessage.mock.calls as unknown as Array<[string, string, string]>)
      .map((call) => String(call[2] ?? ""));
    expect(postedTexts.some((text) => text.includes("账号额度不可用"))).toBe(false);
    expect(postedTexts.some((text) => text.includes("手动切换账号"))).toBe(false);
    expect(sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: "pending"
    })).toHaveLength(1);
    expect(sessions.getSessionByKey(session.key)).toMatchObject({
      authProfileName: "bound-profile",
      authBlockedAt: undefined,
      authBlockReason: undefined,
      authBlockedNoticePostedAt: undefined
    });

    await service.stop();
    stateStore.close();
    await fs.rm(stateDir, { force: true, recursive: true });
    await fs.rm(sessionsRoot, { force: true, recursive: true });
  });

  it("posts the session activity link once when two inbound messages race", async () => {
    const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-session-link-race-state-"));
    const sessionsRoot = await fs.mkdtemp(path.join(os.tmpdir(), "slack-conversation-session-link-race-sessions-"));
    const stateStore = new StateStore(stateDir, sessionsRoot);
    const sessions = new SessionManager({
      stateStore,
      sessionsRoot
    });
    await sessions.load();
    const session = await sessions.ensureSession("C123", "111.222");
    const agentRuntime = Object.assign(new EventEmitter(), {
      start: vi.fn(async () => undefined),
      stop: vi.fn(async () => undefined),
      setSlackBotIdentity: vi.fn(),
      getCapabilities: vi.fn(),
      ensureSession: vi.fn(async () => ({
        id: "agent-session-1",
        brokerSessionKey: session.key,
        runtime: "test",
        createdAt: "2026-05-09T00:00:00.000Z"
      })),
      submitInput: vi.fn(async (input: { readonly inputId: string }) => ({
        receipt: {
          agentSessionId: "agent-session-1",
          turnId: `turn-${input.inputId}`,
          inputId: input.inputId,
          delivery: "started_turn",
          deliveredAt: "2026-05-09T00:00:00.000Z"
        },
        completion: Promise.resolve({
          agentSessionId: "agent-session-1",
          turnId: `turn-${input.inputId}`,
          finalMessage: "",
          aborted: false
        })
      })),
      interrupt: vi.fn(),
      readSession: vi.fn(),
      readTurn: vi.fn()
    });
    const postThreadMessage = vi.fn(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
      return `link-${postThreadMessage.mock.calls.length}`;
    });

    const service = new SlackConversationService({
      config: TEST_CONFIG,
      sessions,
      agentRuntime: agentRuntime as never,
      slackApi: {
        postThreadMessage,
        setAssistantThreadStatus: vi.fn(),
        addReaction: vi.fn(),
        removeReaction: vi.fn(),
        getUserIdentity: vi.fn(async () => null)
      } as never,
      selfMessageFilter: {
        rememberPostedMessageTs: vi.fn(),
        shouldIgnoreThreadMessage: vi.fn(() => false)
      } as never
    });

    await Promise.all([
      service.acceptInboundMessage(session, {
        source: "thread_reply",
        channelId: "C123",
        rootThreadTs: "111.222",
        messageTs: "111.223",
        userId: "U123",
        text: "第一条"
      }),
      service.acceptInboundMessage(session, {
        source: "thread_reply",
        channelId: "C123",
        rootThreadTs: "111.222",
        messageTs: "111.224",
        userId: "U123",
        text: "第二条"
      })
    ]);

    const linkPosts = (postThreadMessage.mock.calls as unknown as Array<[string, string, string]>)
      .filter((call) => call[2].includes("查看会话活动时间线"));
    expect(linkPosts).toHaveLength(1);
    expect(sessions.getSessionByKey(session.key)).toMatchObject({
      sessionPageLinkPostedAt: expect.any(String)
    });
    await vi.waitFor(() => {
      expect(agentRuntime.submitInput.mock.calls.length).toBeGreaterThan(0);
      expect(sessions.listInboundMessages({
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        status: "done"
      }).length).toBeGreaterThan(0);
    });

    await service.stop();
    stateStore.close();
    await fs.rm(stateDir, { force: true, recursive: true });
    await fs.rm(sessionsRoot, { force: true, recursive: true });
  });
});
