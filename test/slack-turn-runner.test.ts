import { EventEmitter } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { AgentRuntime } from "../src/services/agent-runtime/types.js";
import { SlackTurnRunner } from "../src/services/slack/slack-turn-runner.js";
import type { SlackSessionRecord } from "../src/types.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => fs.rm(dir, { recursive: true, force: true })));
  vi.restoreAllMocks();
});

function createRuntime(overrides: Partial<AgentRuntime>): AgentRuntime {
  const runtime = new EventEmitter() as AgentRuntime;
  const capabilities = {
    submitWhileActive: true,
    interrupt: true,
    readTurn: true,
    readSession: true,
    rawEvents: true,
    tokenUsage: "exact",
    toolCalls: true,
    systemPromptEcho: true
  } as const;
  runtime.getCapabilities = vi.fn(() => capabilities);
  runtime.start = vi.fn();
  runtime.stop = vi.fn();
  runtime.setSlackBotIdentity = vi.fn();
  runtime.ensureSession = vi.fn();
  runtime.submitInput = vi.fn();
  runtime.interrupt = vi.fn();
  runtime.readSession = vi.fn();
  runtime.readTurn = vi.fn();
  return Object.assign(runtime, overrides);
}

describe("SlackTurnRunner", () => {
  it("resets a missing stored agent session id and starts a fresh session", async () => {
    let currentSession: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-old",
      createdAt: "2026-03-16T00:00:00.000Z",
      updatedAt: "2026-03-16T00:00:00.000Z"
    };

    const ensureSession = vi.fn(async (session: SlackSessionRecord) => {
      if (session.agentSessionId === "thread-old") {
        throw new Error("no rollout found for thread id thread-old");
      }

      return {
        id: "thread-new",
        brokerSessionKey: session.key,
        runtime: "test",
        createdAt: "2026-03-16T00:00:01.000Z"
      };
    });

    const setActiveTurnId = vi.fn(async () => currentSession);
    const setAgentSessionId = vi.fn(async (_channelId: string, _rootThreadTs: string, agentSessionId: string | undefined) => {
      currentSession = {
        ...currentSession,
        agentSessionId
      };
      return currentSession;
    });

    const runner = new SlackTurnRunner({
      agentRuntime: createRuntime({ ensureSession }),
      slackApi: {
        getUserIdentity: vi.fn(),
        downloadFileAttachment: vi.fn()
      } as never,
      sessions: {
        setActiveTurnId,
        setAgentSessionId
      } as never,
      inboundStore: {} as never
    });

    const result = await runner.ensureAgentSession(currentSession);

    expect(ensureSession).toHaveBeenCalledTimes(2);
    expect(setActiveTurnId).toHaveBeenCalledWith("C123", "111.222", undefined);
    expect(setAgentSessionId).toHaveBeenNthCalledWith(1, "C123", "111.222", undefined);
    expect(setAgentSessionId).toHaveBeenNthCalledWith(2, "C123", "111.222", "thread-new");
    expect(result.agentSessionId).toBe("thread-new");
  });

  it("persists the active turn id before marking the Slack batch inflight", async () => {
    const calls: string[] = [];
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      createdAt: "2026-03-16T00:00:00.000Z",
      updatedAt: "2026-03-16T00:00:00.000Z"
    };
    const activeSession = {
      ...session,
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-03-16T00:00:01.000Z"
    };

    const runner = new SlackTurnRunner({
      agentRuntime: createRuntime({
        submitInput: vi.fn(async () => ({
          receipt: {
            agentSessionId: "thread-1",
            turnId: "turn-1",
            inputId: "input-1",
            delivery: "started_turn" as const,
            deliveredAt: "2026-03-16T00:00:01.000Z"
          },
          completion: Promise.resolve({
            agentSessionId: "thread-1",
            turnId: "turn-1",
            finalMessage: "",
            aborted: false
          })
        }))
      }),
      slackApi: {
        getUserIdentity: vi.fn(),
        downloadFileAttachment: vi.fn()
      } as never,
      sessions: {
        getSession: vi.fn(() => activeSession),
        setActiveTurnId: vi.fn(async (_channelId: string, _rootThreadTs: string, turnId: string | undefined) => {
          calls.push(turnId ? "set-active" : "clear-active");
          return turnId ? activeSession : session;
        }),
        clearActiveTurnIdIfMatches: vi.fn(async () => {
          calls.push("clear-active");
          return session;
        }),
        setAgentSessionId: vi.fn(),
        upsertAgentTurnUsage: vi.fn()
      } as never,
      inboundStore: {
        markMessagesInflightByTs: vi.fn(async () => {
          calls.push("mark-inflight");
        }),
        markTurnBatchDone: vi.fn(async () => {
          calls.push("mark-done");
          return activeSession;
        }),
        resetTurnBatchToPending: vi.fn()
      } as never
    });

    await runner.submitInputWithRecovery({
      session,
      sessionKey: session.key,
      senderUserId: "U123",
      input: [
        {
          type: "text",
          text: "hello",
          text_elements: []
        }
      ],
      messageTsList: ["111.223"]
    });

    expect(calls.slice(0, 2)).toEqual(["set-active", "mark-inflight"]);
  });

  it("downloads Slack attachments into the session workspace instead of sending image input items", async () => {
    const workspacePath = await fs.mkdtemp(path.join(os.tmpdir(), "slack-attachments-"));
    tempDirs.push(workspacePath);
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath,
      agentSessionId: "thread-1",
      createdAt: "2026-03-16T00:00:00.000Z",
      updatedAt: "2026-03-16T00:00:00.000Z"
    };
    const downloadFileAttachment = vi.fn(async () => ({
      bytes: Buffer.from("<svg/>"),
      contentType: "image/svg+xml"
    }));

    const runner = new SlackTurnRunner({
      agentRuntime: createRuntime({}),
      slackApi: {
        getUserIdentity: vi.fn(async () => null),
        downloadFileAttachment
      } as never,
      sessions: {} as never,
      inboundStore: {} as never
    });

    const input = await runner.buildTurnInput(session, {
      source: "thread_reply",
      channelId: "C123",
      rootThreadTs: "111.222",
      messageTs: "111.223",
      userId: "U123",
      senderKind: "user",
      text: "use the attached icon",
      images: [
        {
          fileId: "F123",
          name: "../screen.svg",
          mimetype: "image/svg+xml",
          url: "https://files.slack.test/screen.svg"
        }
      ]
    });

    expect(downloadFileAttachment).toHaveBeenCalledTimes(1);
    expect(input).toHaveLength(1);
    expect(input[0]).toMatchObject({ type: "text" });
    expect(input.some((item) => item.type === "image")).toBe(false);
    const text = input[0]?.type === "text" ? input[0].text : "";
    const expectedPath = path.join(workspacePath, ".slack-attachments", "111.223", "F123-screen.svg");
    expect(text).toContain("\"attachments\": [");
    expect(text).toContain(`"local_path": "${expectedPath}"`);
    expect(await fs.readFile(expectedPath, "utf8")).toBe("<svg/>");
  });
});
