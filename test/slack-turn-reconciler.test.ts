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

  it("resets a long-silent in-progress active turn back to pending", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-05-27T00:00:00.000Z",
      createdAt: "2026-05-27T00:00:00.000Z",
      updatedAt: "2026-05-27T00:00:00.000Z"
    };

    const setActiveTurnId = vi.fn(async () => ({
      ...session,
      activeTurnId: undefined,
      activeTurnStartedAt: undefined
    }));
    const listAgentTraceEventsPage = vi.fn(() => ({
      events: [
        {
          id: "trace-1",
          sessionKey: session.key,
          source: "agent_runtime" as const,
          type: "agent_turn_started",
          at: "2026-05-27T00:00:00.000Z",
          sequence: 1,
          title: "turn started",
          summary: "started",
          turnId: "turn-1",
          createdAt: "2026-05-27T00:00:00.000Z",
          updatedAt: "2026-05-27T00:00:00.000Z"
        }
      ],
      hasMore: false,
      nextBeforeSequence: null
    }));
    const resetTurnBatchToPending = vi.fn();
    const interrupt = vi.fn();
    const readTurnSnapshot = vi.fn(async () => ({ status: "inProgress" }));
    const ensureAgentSession = vi.fn(async () => session);

    const reconciler = new SlackTurnReconciler({
      sessions: {
        setActiveTurnId,
        listAgentTraceEventsPage
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot,
        interrupt
      } as never,
      staleActiveTurnAfterMs: 10 * 60_000,
      now: () => new Date("2026-05-27T00:30:00.000Z")
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("cleared");
    expect(readTurnSnapshot).toHaveBeenCalledWith(session, "turn-1", {
      syncActiveTurn: true,
      treatMissingAsStale: false
    });
    expect(interrupt).toHaveBeenCalledWith(session);
    expect(resetTurnBatchToPending).toHaveBeenCalledWith(session, "turn-1");
    expect(setActiveTurnId).toHaveBeenCalledWith("C123", "111.222", undefined);
  });

  it("retains an in-progress active turn while trace activity is recent", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-05-27T00:00:00.000Z",
      createdAt: "2026-05-27T00:00:00.000Z",
      updatedAt: "2026-05-27T00:00:00.000Z"
    };

    const setActiveTurnId = vi.fn();
    const listAgentTraceEventsPage = vi.fn(() => ({
      events: [
        {
          id: "trace-1",
          sessionKey: session.key,
          source: "agent_runtime" as const,
          type: "agent_tool_call",
          at: "2026-05-27T00:25:00.000Z",
          sequence: 10,
          title: "tool",
          summary: "running",
          turnId: "turn-1",
          createdAt: "2026-05-27T00:25:00.000Z",
          updatedAt: "2026-05-27T00:25:00.000Z"
        }
      ],
      hasMore: false,
      nextBeforeSequence: null
    }));
    const resetTurnBatchToPending = vi.fn();
    const interrupt = vi.fn();
    const readTurnSnapshot = vi.fn(async () => ({ status: "inProgress" }));
    const ensureAgentSession = vi.fn(async () => session);

    const reconciler = new SlackTurnReconciler({
      sessions: {
        setActiveTurnId,
        listAgentTraceEventsPage
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot,
        interrupt
      } as never,
      staleActiveTurnAfterMs: 10 * 60_000,
      now: () => new Date("2026-05-27T00:30:00.000Z")
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("retained");
    expect(interrupt).not.toHaveBeenCalled();
    expect(resetTurnBatchToPending).not.toHaveBeenCalled();
    expect(setActiveTurnId).not.toHaveBeenCalled();
  });

  it("still resets a stale in-progress turn when runtime interrupt fails", async () => {
    const session: SlackSessionRecord = {
      key: "C123:111.222",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      agentSessionId: "thread-1",
      activeTurnId: "turn-1",
      activeTurnStartedAt: "2026-05-27T00:00:00.000Z",
      createdAt: "2026-05-27T00:00:00.000Z",
      updatedAt: "2026-05-27T00:00:00.000Z"
    };

    const setActiveTurnId = vi.fn(async () => ({
      ...session,
      activeTurnId: undefined,
      activeTurnStartedAt: undefined
    }));
    const listAgentTraceEventsPage = vi.fn(() => ({
      events: [],
      hasMore: false,
      nextBeforeSequence: null
    }));
    const resetTurnBatchToPending = vi.fn();
    const interrupt = vi.fn(async () => {
      throw new Error("runtime is unreachable");
    });
    const readTurnSnapshot = vi.fn(async () => ({ status: "unknown" }));
    const ensureAgentSession = vi.fn(async () => session);

    const reconciler = new SlackTurnReconciler({
      sessions: {
        setActiveTurnId,
        listAgentTraceEventsPage
      } as never,
      inboundStore: {
        resetTurnBatchToPending
      } as never,
      turnRunner: {
        ensureAgentSession,
        readTurnSnapshot,
        interrupt
      } as never,
      staleActiveTurnAfterMs: 10 * 60_000,
      now: () => new Date("2026-05-27T00:30:00.000Z")
    });

    await expect(reconciler.reconcileSingleActiveTurn(session)).resolves.toBe("cleared");
    expect(interrupt).toHaveBeenCalledWith(session);
    expect(resetTurnBatchToPending).toHaveBeenCalledWith(session, "turn-1");
    expect(setActiveTurnId).toHaveBeenCalledWith("C123", "111.222", undefined);
  });
});
