import { logger } from "../../logger.js";
import type { PersistedAgentTraceEvent, SlackSessionRecord } from "../../types.js";
import { SessionManager } from "../session-manager.js";
import { SlackInboundStore } from "./slack-inbound-store.js";
import { SlackTurnRunner } from "./slack-turn-runner.js";

const DEFAULT_TRACE_ACTIVITY_LOOKBACK = 500;

export class SlackTurnReconciler {
  readonly #sessions: SessionManager;
  readonly #turnRunner: SlackTurnRunner;
  readonly #inboundStore: SlackInboundStore;
  readonly #staleActiveTurnAfterMs: number;
  readonly #now: () => Date;

  constructor(options: {
    readonly sessions: SessionManager;
    readonly turnRunner: SlackTurnRunner;
    readonly inboundStore: SlackInboundStore;
    readonly staleActiveTurnAfterMs?: number | undefined;
    readonly now?: (() => Date) | undefined;
  }) {
    this.#sessions = options.sessions;
    this.#turnRunner = options.turnRunner;
    this.#inboundStore = options.inboundStore;
    this.#staleActiveTurnAfterMs = options.staleActiveTurnAfterMs ?? 0;
    this.#now = options.now ?? (() => new Date());
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
      const stale = this.#getStaleActiveTurn(session, activeTurnId);
      if (stale) {
        logger.warn("Resetting stale active agent turn after trace inactivity", {
          sessionKey: session.key,
          turnId: activeTurnId,
          snapshotStatus: snapshot.status,
          staleForMs: stale.staleForMs,
          staleAfterMs: this.#staleActiveTurnAfterMs,
          lastActivityAt: stale.lastActivityAt
        });
        try {
          await this.#turnRunner.interrupt(hydratedSession);
        } catch (error) {
          logger.warn("Failed to interrupt stale active agent turn before broker reset", {
            sessionKey: session.key,
            turnId: activeTurnId,
            error: error instanceof Error ? error.message : String(error)
          });
        }
        await this.#inboundStore.resetTurnBatchToPending(hydratedSession, activeTurnId);
        await this.#sessions.setActiveTurnId(
          hydratedSession.channelId,
          hydratedSession.rootThreadTs,
          undefined
        );
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

  #getStaleActiveTurn(session: SlackSessionRecord, turnId: string): {
    readonly lastActivityAt: string;
    readonly staleForMs: number;
  } | null {
    if (this.#staleActiveTurnAfterMs <= 0) {
      return null;
    }

    const lastActivityMs = this.#getLatestActiveTurnActivityMs(session, turnId);
    if (lastActivityMs === undefined) {
      return null;
    }

    const nowMs = this.#now().getTime();
    if (!Number.isFinite(nowMs)) {
      return null;
    }

    const staleForMs = nowMs - lastActivityMs;
    if (staleForMs < this.#staleActiveTurnAfterMs) {
      return null;
    }

    return {
      lastActivityAt: new Date(lastActivityMs).toISOString(),
      staleForMs
    };
  }

  #getLatestActiveTurnActivityMs(session: SlackSessionRecord, turnId: string): number | undefined {
    let latest = parseIsoTimestampMs(session.activeTurnStartedAt);
    const traceEvents = this.#sessions.listAgentTraceEventsPage(session.key, {
      limit: DEFAULT_TRACE_ACTIVITY_LOOKBACK
    }).events;

    for (const event of traceEvents) {
      if (event.turnId !== turnId) {
        continue;
      }

      latest = maxTimestampMs(latest, latestTraceEventTimestampMs(event));
    }

    return latest;
  }
}

function latestTraceEventTimestampMs(event: PersistedAgentTraceEvent): number | undefined {
  return maxTimestampMs(
    maxTimestampMs(parseIsoTimestampMs(event.at), parseIsoTimestampMs(event.updatedAt)),
    parseIsoTimestampMs(event.createdAt)
  );
}

function maxTimestampMs(
  left: number | undefined,
  right: number | undefined
): number | undefined {
  if (left === undefined) {
    return right;
  }
  if (right === undefined) {
    return left;
  }
  return Math.max(left, right);
}

function parseIsoTimestampMs(value: string | undefined): number | undefined {
  if (!value) {
    return undefined;
  }

  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : undefined;
}
