import { logger } from "../../logger.js";
import type { PersistedAgentTraceEvent, SlackSessionRecord } from "../../types.js";
import { SessionManager } from "../session-manager.js";
import { SlackInboundStore } from "./slack-inbound-store.js";
import { SlackTurnRunner } from "./slack-turn-runner.js";

const ACTIVE_TURN_TRACE_LOOKBACK_LIMIT = 500;

export class SlackTurnReconciler {
  readonly #sessions: SessionManager;
  readonly #turnRunner: SlackTurnRunner;
  readonly #inboundStore: SlackInboundStore;
  readonly #activeTurnStallTimeoutMs: number;
  readonly #now: () => number;

  constructor(options: {
    readonly sessions: SessionManager;
    readonly turnRunner: SlackTurnRunner;
    readonly inboundStore: SlackInboundStore;
    readonly activeTurnStallTimeoutMs?: number | undefined;
    readonly now?: (() => number) | undefined;
  }) {
    this.#sessions = options.sessions;
    this.#turnRunner = options.turnRunner;
    this.#inboundStore = options.inboundStore;
    this.#activeTurnStallTimeoutMs = options.activeTurnStallTimeoutMs ?? Number.POSITIVE_INFINITY;
    this.#now = options.now ?? Date.now;
  }

  async reconcileSingleActiveTurn(
    session: SlackSessionRecord,
    options?: {
      readonly treatMissingAsStale?: boolean | undefined;
    }
  ): Promise<"cleared" | "retained"> {
    if (!session.agentSessionId || !session.activeTurnId) {
      await this.#sessions.setActiveTurnId(session.channelId, session.rootThreadTs, undefined);
      return "cleared";
    }

    const hydratedSession = await this.#turnRunner.ensureAgentSession(session);
    const activeTurnId = hydratedSession.activeTurnId!;
    const snapshot = await this.#turnRunner.readTurnSnapshot(hydratedSession, activeTurnId, {
      syncActiveTurn: true,
      treatMissingAsStale: options?.treatMissingAsStale ?? false
    });

    if (!snapshot) {
      if (options?.treatMissingAsStale) {
        logger.info("Active agent turn missing from thread snapshot during stale-turn reconciliation", {
          sessionKey: session.key,
          turnId: activeTurnId,
          reason: "turn_missing_from_snapshot"
        });
        await this.#inboundStore.resetTurnBatchToPending(hydratedSession, activeTurnId);
        await this.#sessions.setActiveTurnId(
          hydratedSession.channelId,
          hydratedSession.rootThreadTs,
          undefined
        );
        return "cleared";
      }

      logger.debug("Active agent turn not present in thread snapshot yet; retaining turn", {
        sessionKey: session.key,
        turnId: activeTurnId,
        reason: "turn_missing_from_snapshot"
      });
      return "retained";
    }

    if (snapshot.status === "inProgress" || snapshot.status === "unknown") {
      const stale = this.#getStalledActiveTurn(session, activeTurnId);
      if (stale) {
        logger.warn("Detected stalled active agent turn; resetting broker runtime state", {
          sessionKey: session.key,
          turnId: activeTurnId,
          status: snapshot.status,
          lastAgentRuntimeActivityAt: stale.lastActivityAt,
          inactiveMs: stale.inactiveMs,
          staleAfterMs: this.#activeTurnStallTimeoutMs
        });
        await this.#inboundStore.resetTurnBatchToPending(hydratedSession, activeTurnId);
        await this.#sessions.setActiveTurnId(
          hydratedSession.channelId,
          hydratedSession.rootThreadTs,
          undefined
        );
        try {
          await this.#turnRunner.interrupt(hydratedSession);
        } catch (error) {
          logger.warn("Failed to interrupt stalled active agent turn during reconciliation", {
            sessionKey: session.key,
            turnId: activeTurnId,
            error: error instanceof Error ? error.message : String(error)
          });
        }
        return "cleared";
      }
      return "retained";
    }

    logger.info("Reconciling terminal agent turn state from snapshot", {
      sessionKey: session.key,
      turnId: activeTurnId,
      status: snapshot.status
    });

    if (snapshot.status === "completed" || snapshot.status === "interrupted") {
      await this.#inboundStore.markTurnBatchDone(hydratedSession, activeTurnId);
    } else {
      await this.#inboundStore.resetTurnBatchToPending(hydratedSession, activeTurnId);
    }

    await this.#sessions.setActiveTurnId(
      hydratedSession.channelId,
      hydratedSession.rootThreadTs,
      undefined
    );
    return "cleared";
  }

  #getStalledActiveTurn(
    session: SlackSessionRecord,
    turnId: string
  ): {
    readonly lastActivityAt: string;
    readonly inactiveMs: number;
  } | null {
    if (!Number.isFinite(this.#activeTurnStallTimeoutMs) || this.#activeTurnStallTimeoutMs < 0) {
      return null;
    }

    const lastActivityAt = this.#latestAgentRuntimeActivityAt(session, turnId);
    if (!lastActivityAt) {
      return null;
    }

    const lastActivityMs = Date.parse(lastActivityAt);
    if (!Number.isFinite(lastActivityMs)) {
      return null;
    }

    const inactiveMs = this.#now() - lastActivityMs;
    return inactiveMs >= this.#activeTurnStallTimeoutMs
      ? {
          lastActivityAt,
          inactiveMs
        }
      : null;
  }

  #latestAgentRuntimeActivityAt(session: SlackSessionRecord, turnId: string): string | undefined {
    const traceEvents = this.#sessions.listAgentTraceEventsPage(session.key, {
      limit: ACTIVE_TURN_TRACE_LOOKBACK_LIMIT
    }).events;
    const latestTrace = traceEvents
      .filter((event) => event.turnId === turnId && event.source === "agent_runtime")
      .sort(compareTraceActivity)
      .at(-1);

    return latestTrace?.at ?? session.activeTurnStartedAt ?? session.createdAt;
  }
}

function compareTraceActivity(left: PersistedAgentTraceEvent, right: PersistedAgentTraceEvent): number {
  const leftAt = Date.parse(left.at);
  const rightAt = Date.parse(right.at);
  if (Number.isFinite(leftAt) && Number.isFinite(rightAt) && leftAt !== rightAt) {
    return leftAt - rightAt;
  }
  if (left.sequence !== right.sequence) {
    return left.sequence - right.sequence;
  }
  return (left.id ?? "").localeCompare(right.id ?? "");
}
