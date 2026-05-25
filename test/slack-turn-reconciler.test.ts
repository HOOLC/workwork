import { describe, expect, it, vi } from "vitest";

import { SlackTurnReconciler } from "../src/services/slack/slack-turn-reconciler.js";
import type { SlackSessionRecord } from "../src/types.js";

describe("SlackTurnReconciler", () => {
  it("retains an active turn when thread/read temporarily omits it", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString()
    };

    const setActiveTurnId = vi.fn();
    const resetTurnBatchToPending = vi.fn();
    const readTurnSnapshot = vi.fn(async () => null);
    const ensureAgentSession = vi.fn(async () => session);

    const reconciler = new SlackTurnReconciler({
      sessions: {
        setActiveTurnId
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot
      } as never
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("retained");
    expect(ensureAgentSession).toHaveBeenCalledWith(session);
    expect(readTurnSnapshot).toHaveBeenCalledWith(session, "turn-1", {
      syncActiveTurn: true,
      treatMissingAsStale: false
    });
    expect(resetTurnBatchToPending).not.toHaveBeenCalled();
    expect(setActiveTurnId).not.toHaveBeenCalled();
  });

  it("clears an active turn when startup reconciliation treats a missing snapshot turn as stale", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString()
    };

    const setActiveTurnId = vi.fn(async () => session);
    const resetTurnBatchToPending = vi.fn();
    const readTurnSnapshot = vi.fn(async () => null);
    const ensureAgentSession = vi.fn(async () => session);

    const reconciler = new SlackTurnReconciler({
      sessions: {
        setActiveTurnId
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot
      } as never
    });

    await expect(reconciler.reconcileSingleActiveTurn(session, {
      treatMissingAsStale: true
    })).resolves.toBe("cleared");
    expect(readTurnSnapshot).toHaveBeenCalledWith(session, "turn-1", {
      syncActiveTurn: true,
      treatMissingAsStale: true
    });
    expect(resetTurnBatchToPending).toHaveBeenCalledWith(session, "turn-1");
    expect(setActiveTurnId).toHaveBeenCalledWith("C123", "111.222", undefined);
  });

  it("interrupts and resets a silent in-progress active turn after the stall timeout", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-05-25T09:00:00.000Z",
      createdAt: "2026-05-25T09:00:00.000Z",
      updatedAt: "2026-05-25T09:00:00.000Z"
    };

    const setActiveTurnId = vi.fn(async () => ({
      ...session,
      activeTurnId: undefined
    }));
    const resetTurnBatchToPending = vi.fn();
    const readTurnSnapshot = vi.fn(async () => ({
      status: "inProgress" as const,
      finalMessage: "",
      generatedImages: []
    }));
    const ensureAgentSession = vi.fn(async () => session);
    const interrupt = vi.fn(async () => undefined);

    const reconciler = new SlackTurnReconciler({
      activeTurnStallTimeoutMs: 60_000,
      now: () => Date.parse("2026-05-25T09:10:01.000Z"),
      sessions: {
        listAgentTraceEventsPage: vi.fn(() => ({
          hasMore: false,
          nextBeforeSequence: null,
          events: [
            {
              sessionKey: session.key,
              source: "agent_runtime",
              type: "agent_tool_result",
              at: "2026-05-25T09:01:00.000Z",
              sequence: 1,
              title: "tool result",
              summary: "done",
              turnId: "turn-1"
            },
            {
              sessionKey: session.key,
              source: "broker",
              type: "agent_input_delivered",
              at: "2026-05-25T09:10:00.000Z",
              sequence: 2,
              title: "input",
              summary: "joined",
              turnId: "turn-1"
            }
          ]
        })),
        setActiveTurnId
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot,
        interrupt
      } as never
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("cleared");
    expect(interrupt).toHaveBeenCalledWith(session);
    expect(resetTurnBatchToPending).toHaveBeenCalledWith(session, "turn-1");
    expect(setActiveTurnId).toHaveBeenCalledWith("C123", "111.222", undefined);
  });

  it("retains an in-progress active turn when recent agent-runtime activity exists", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-05-25T09:00:00.000Z",
      createdAt: "2026-05-25T09:00:00.000Z",
      updatedAt: "2026-05-25T09:00:00.000Z"
    };

    const setActiveTurnId = vi.fn();
    const resetTurnBatchToPending = vi.fn();
    const readTurnSnapshot = vi.fn(async () => ({
      status: "inProgress" as const,
      finalMessage: "",
      generatedImages: []
    }));
    const ensureAgentSession = vi.fn(async () => session);
    const interrupt = vi.fn();

    const reconciler = new SlackTurnReconciler({
      activeTurnStallTimeoutMs: 60_000,
      now: () => Date.parse("2026-05-25T09:10:01.000Z"),
      sessions: {
        listAgentTraceEventsPage: vi.fn(() => ({
          hasMore: false,
          nextBeforeSequence: null,
          events: [
            {
              sessionKey: session.key,
              source: "agent_runtime",
              type: "agent_token_count",
              at: "2026-05-25T09:09:30.000Z",
              sequence: 1,
              title: "usage",
              summary: "tokens",
              turnId: "turn-1"
            }
          ]
        })),
        setActiveTurnId
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot,
        interrupt
      } as never
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("retained");
    expect(interrupt).not.toHaveBeenCalled();
    expect(resetTurnBatchToPending).not.toHaveBeenCalled();
    expect(setActiveTurnId).not.toHaveBeenCalled();
  });
});
