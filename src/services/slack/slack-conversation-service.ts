import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";

import { buildAdminSessionUrl } from "../../admin-session-url.js";
import type { AppConfig } from "../../config.js";
import { logger } from "../../logger.js";
import { SessionManager } from "../session-manager.js";
import type {
  BackgroundJobEventPayload,
  PersistedInboundMessage,
  ResolvedSlackThreadMessage,
  SlackInputMessage,
  SlackSessionRecord,
  SlackThreadMessage,
  SlackTurnSignalKind
} from "../../types.js";
import type { AgentRuntime, AgentRuntimeEvent } from "../agent-runtime/types.js";
import {
  isAuthProfileUnavailableError,
  type AuthProfileUnavailableError
} from "../agent-runtime/session-auth-profile-runtime.js";
import { isAuthProfileProbeFailureReason } from "../session-auth-profile-selector.js";
import { AgentTraceRecorder } from "../agent-runtime/agent-trace-recorder.js";
import {
  SlackApi,
  isSlackRateLimitError,
  type SlackUploadedFile
} from "./slack-api.js";
import { SlackAssistantStatusController } from "./slack-assistant-status.js";
import {
  createSlackInputFromThreadMessage,
  isSlackMessageEffectivelyEmpty,
  parseSlackTextMetadata
} from "./slack-event-parser.js";
import {
  chunkSlackMessage,
  clampHistoryLimit,
  compareIsoTimestamp,
  createSyntheticMessageTs,
  createSlackFailureFingerprint,
  formatSlackRunFailureMessage,
  isBeforeSlackTs,
  isMissingAgentSessionError,
  isRecoverableAgentTurnFailure,
  parseActiveTurnMismatch,
  isMissingActiveTurnInputError,
  isSlackMessageAfterCursor,
  shouldResetConflictingActiveTurnMismatch,
  shouldForceResetStaleIdleRuntime,
  shouldPostSlackRunFailure,
  shouldNotifySlackFailure,
  shouldAutoRecoverSession
} from "./slack-conversation-utils.js";
import { SlackInboundStore } from "./slack-inbound-store.js";
import {
  formatSlackHistoryContextForAgent
} from "./slack-message-format.js";
import { markdownishToMrkdwn } from "./slack-mrkdwn.js";
import { SlackSelfMessageFilter } from "./slack-self-filter.js";
import { SlackCoauthorService } from "./slack-coauthor-service.js";
import type { GitHubPrIdentityService } from "../github-pr-identity-service.js";
import { planCompletedTurnDisposition } from "./slack-turn-disposition.js";
import { SlackTurnReconciler } from "./slack-turn-reconciler.js";
import { SlackTurnRunner } from "./slack-turn-runner.js";

interface RuntimeSessionState {
  readonly queue: PendingDispatchRequest[];
  processing: boolean;
  generation: number;
  autoResumeTimer?: NodeJS.Timeout | undefined;
  blockedUntilMs?: number | undefined;
  blockedFailureFingerprint?: string | undefined;
  lastFailureNotificationFingerprint?: string | undefined;
  lastFailureNotificationAtMs?: number | undefined;
}

interface PendingDispatchRequest {
  readonly kind: "dispatch_pending";
  readonly recoveryKind?: "missed_thread_messages" | undefined;
}

const AUTO_RESUME_AFTER_FAILURE_MS = 5_000;
const NONRECOVERABLE_DISPATCH_RETRY_COOLDOWN_MS = 5 * 60 * 1_000;
const MISSED_THREAD_RECOVERY_RATE_LIMIT_MIN_BACKOFF_MS = 60_000;
const MISSED_THREAD_RECOVERY_RATE_LIMIT_MAX_BACKOFF_MS = 10 * 60_000;

export class SlackConversationService {
  readonly #config: AppConfig;
  readonly #sessions: SessionManager;
  readonly #agentRuntime: AgentRuntime;
  readonly #traceRecorder: AgentTraceRecorder;
  readonly #slackApi: SlackApi;
  readonly #selfMessageFilter: SlackSelfMessageFilter;
  readonly #coauthors: {
    readonly noteIncomingSlackInput: (
      session: SlackSessionRecord,
      item: SlackInputMessage
    ) => Promise<SlackSessionRecord>;
  };
  readonly #githubPrIdentity: GitHubPrIdentityService | undefined;
  readonly #runtimeSessions = new Map<string, RuntimeSessionState>();
  readonly #statusControllers = new Map<string, SlackAssistantStatusController>();
  readonly #inboundStore: SlackInboundStore;
  readonly #turnRunner: SlackTurnRunner;
  readonly #turnReconciler: SlackTurnReconciler;
  readonly #agentRuntimeEventHandler: (event: AgentRuntimeEvent) => void;
  readonly #sessionPageLinkPosts = new Map<string, Promise<SlackSessionRecord>>();
  #botUserId = "";
  #activeTurnReconcileTimer: NodeJS.Timeout | undefined;
  #activeTurnReconcilePromise: Promise<void> | undefined;
  #startupRecoveryPromise: Promise<void> | undefined;
  #stopped = true;
  #catchUpPromise: Promise<void> | undefined;
  #lastMissedThreadRecoveryAtMs = 0;
  #missedThreadRecoveryRateLimitBackoffMs = 0;
  #missedThreadRecoveryRateLimitUntilMs = 0;

  constructor(options: {
    readonly config: AppConfig;
    readonly sessions: SessionManager;
    readonly agentRuntime: AgentRuntime;
    readonly slackApi: SlackApi;
    readonly selfMessageFilter: SlackSelfMessageFilter;
    readonly coauthors?: SlackCoauthorService | undefined;
    readonly githubPrIdentity?: GitHubPrIdentityService | undefined;
  }) {
    this.#config = options.config;
    this.#sessions = options.sessions;
    this.#agentRuntime = options.agentRuntime;
    this.#slackApi = options.slackApi;
    this.#selfMessageFilter = options.selfMessageFilter;
    this.#coauthors = options.coauthors ?? {
      noteIncomingSlackInput: async (session) => session
    };
    this.#githubPrIdentity = options.githubPrIdentity;
    this.#inboundStore = new SlackInboundStore({
      sessions: this.#sessions,
      slackApi: this.#slackApi
    });
    this.#traceRecorder = new AgentTraceRecorder({
      sessions: this.#sessions
    });
    this.#turnRunner = new SlackTurnRunner({
      agentRuntime: this.#agentRuntime,
      slackApi: this.#slackApi,
      sessions: this.#sessions,
      inboundStore: this.#inboundStore
    });
    this.#turnReconciler = new SlackTurnReconciler({
      sessions: this.#sessions,
      turnRunner: this.#turnRunner,
      inboundStore: this.#inboundStore,
      staleActiveTurnAfterMs: this.#config.slackStaleActiveTurnAfterMs
    });
    this.#agentRuntimeEventHandler = (event) => {
      this.#handleAgentRuntimeEvent(event);
    };
    this.#agentRuntime.on("event", this.#agentRuntimeEventHandler);
  }

  setBotUserId(botUserId: string): void {
    this.#botUserId = botUserId;
  }

  async start(): Promise<void> {
    this.#stopped = false;
    this.#startupRecoveryPromise = this.#runStartupRecovery();
    void this.#startupRecoveryPromise;
  }

  async stop(): Promise<void> {
    this.#stopped = true;
    this.#stopActiveTurnReconciler();
    this.#agentRuntime.off("event", this.#agentRuntimeEventHandler);
    for (const runtime of this.#runtimeSessions.values()) {
      if (!runtime.autoResumeTimer) {
        continue;
      }
      clearTimeout(runtime.autoResumeTimer);
      runtime.autoResumeTimer = undefined;
    }
    const stopPromises = [...this.#statusControllers.values()].map((controller) => controller.stop());
    this.#statusControllers.clear();
    await Promise.all(stopPromises);
  }

  async #runStartupRecovery(): Promise<void> {
    try {
      await this.#reconcilePersistedActiveTurns();
      await this.recoverMissedThreadMessages("socket_ready");
      await this.#recoverPendingSessionsOnBoot();
      await this.#recoverPendingSyntheticMessages();
    } catch (error) {
      logger.error("Failed to finish Slack startup recovery", {
        error: error instanceof Error ? error.message : String(error)
      });
    } finally {
      this.#startupRecoveryPromise = undefined;
      if (!this.#stopped) {
        this.#startActiveTurnReconciler();
      }
    }
  }

  isAlreadyHandled(session: SlackSessionRecord, messageTs?: string | undefined): boolean {
    return this.#inboundStore.isAlreadyHandled(session, messageTs);
  }

  async ensureAgentSession(session: SlackSessionRecord): Promise<SlackSessionRecord> {
    return await this.#turnRunner.ensureAgentSession(session);
  }

  async readThreadHistory(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly beforeMessageTs?: string | undefined;
    readonly limit?: number | undefined;
    readonly channelType?: string | undefined;
  }): Promise<{
    readonly messages: ResolvedSlackThreadMessage[];
    readonly formattedText?: string | undefined;
    readonly hasMore: boolean;
  }> {
    const effectiveLimit = clampHistoryLimit(
      options.limit,
      this.#config.slackInitialThreadHistoryCount,
      this.#config.slackHistoryApiMaxLimit
    );

    if (effectiveLimit === 0) {
      return {
        messages: [],
        formattedText: undefined,
        hasMore: false
      };
    }

    const threadMessages = await this.#slackApi.listThreadMessages({
      channelId: options.channelId,
      channelType: options.channelType,
      rootThreadTs: options.rootThreadTs
    });
    const filteredMessages = threadMessages
      .filter((message) => !this.#selfMessageFilter.shouldIgnoreThreadMessage(message))
      .filter((message) => isBeforeSlackTs(message.messageTs, options.beforeMessageTs));
    const boundedMessages = filteredMessages.slice(-effectiveLimit);
    const resolvedMessages = await Promise.all(
      boundedMessages.map(async (message) => {
        const metadata = parseSlackTextMetadata(message.text);
        return {
          ...message,
          text: metadata.text,
          mentionedUserIds: metadata.mentionedUserIds,
          mentionedUsers: await Promise.all(
            metadata.mentionedUserIds.map((userId) => this.#slackApi.getUserIdentity(userId))
          ).then((users) => users.filter((user): user is NonNullable<typeof user> => user !== null)),
          sender: message.senderKind === "user"
            ? await this.#slackApi.getUserIdentity(message.userId)
            : null
        };
      })
    );

    return {
      messages: resolvedMessages,
      formattedText: formatSlackHistoryContextForAgent(resolvedMessages),
      hasMore: filteredMessages.length > boundedMessages.length
    };
  }

  async replayThreadMessage(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly messageTs: string;
  }): Promise<SlackInputMessage | null> {
    const session = this.#sessions.getSession(options.channelId, options.rootThreadTs);
    if (!session) {
      return null;
    }

    const threadMessages = await this.#slackApi.listThreadMessages({
      channelId: options.channelId,
      rootThreadTs: options.rootThreadTs
    });
    const message = threadMessages.find((entry) => entry.messageTs === options.messageTs);

    if (!message || this.#selfMessageFilter.shouldIgnoreThreadMessage(message)) {
      return null;
    }

    if (this.isAlreadyHandled(session, message.messageTs)) {
      return null;
    }

    const input = createSlackInputFromThreadMessage("thread_reply", message);
    if (isSlackMessageEffectivelyEmpty(input.text, input.images, input.slackMessage)) {
      return null;
    }

    await this.acceptInboundMessage(session, input);
    return input;
  }

  async resumePendingSession(sessionKey: string): Promise<number> {
    const session = this.#findSessionByKey(sessionKey);
    if (session.authBlockedAt) {
      throw new Error(`Session auth is still blocked: ${sessionKey}`);
    }

    return await this.#resumePendingDispatch(sessionKey, {
      forceReset: true
    });
  }

  async resetSession(sessionKey: string): Promise<{
    readonly clearedInboundCount: number;
    readonly resetMessageTs: string;
    readonly resumedCount: number;
    readonly interruptedActiveTurn: boolean;
    readonly previousAgentSessionId: string | null;
    readonly previousActiveTurnId: string | null;
    readonly historyMessageCount: number;
    readonly authBlocked: boolean;
  }> {
    const session = this.#findSessionByKey(sessionKey);
    const history = await this.readThreadHistory({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      channelType: session.channelType,
      limit: this.#config.slackHistoryApiMaxLimit
    });
    const previousAgentSessionId = session.agentSessionId ?? null;
    const previousActiveTurnId = session.activeTurnId ?? null;
    let interruptedActiveTurn = false;

    this.#resetRuntimeForManualSessionReset(session.key);

    if (session.activeTurnId && session.agentSessionId) {
      try {
        await this.#turnRunner.interrupt(session);
        interruptedActiveTurn = true;
      } catch (error) {
        logger.warn("Failed to interrupt active turn during manual session reset", {
          sessionKey: session.key,
          agentSessionId: session.agentSessionId,
          turnId: session.activeTurnId,
          error: error instanceof Error ? error.message : String(error)
        });
      }
    }

    const openMessages = this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: ["pending", "inflight"]
    });
    if (openMessages.length > 0) {
      await this.#sessions.updateInboundMessagesForBatch(
        session.channelId,
        session.rootThreadTs,
        openMessages.map((message) => message.messageTs),
        {
          status: "done",
          batchId: undefined
        }
      );
    }

    let resetSession = await this.#sessions.resetSessionRuntimeState(session.key);
    this.#clearAssistantStatus(resetSession.channelId, resetSession.rootThreadTs);

    const resetMessageTs = createSyntheticMessageTs();
    await this.#inboundStore.recordInboundMessage(resetSession, {
      source: "admin_session_reset",
      channelId: resetSession.channelId,
      channelType: resetSession.channelType,
      rootThreadTs: resetSession.rootThreadTs,
      messageTs: resetMessageTs,
      userId: this.#botUserId || "BROKER_ADMIN",
      senderKind: "app",
      text: "管理员已重置这个 session，丢弃旧 agent history 并从当前 Slack thread 重新开始。",
      contextText: history.formattedText
    });
    await this.#appendSessionResetTrace(resetSession, {
      previousAgentSessionId,
      previousActiveTurnId,
      clearedInboundCount: openMessages.length,
      resetMessageTs,
      historyMessageCount: history.messages.length
    });

    resetSession = this.#findSessionByKey(resetSession.key);
    const authBlocked = Boolean(resetSession.authBlockedAt);
    const resumedCount = authBlocked
      ? 0
      : await this.#resumePendingDispatch(resetSession.key, {
          forceReset: true
        });

    return {
      clearedInboundCount: openMessages.length,
      resetMessageTs,
      resumedCount,
      interruptedActiveTurn,
      previousAgentSessionId,
      previousActiveTurnId,
      historyMessageCount: history.messages.length,
      authBlocked
    };
  }

  async deleteSession(sessionKey: string): Promise<{
    readonly deleted: boolean;
    readonly interruptedActiveTurn: boolean;
    readonly previousAgentSessionId: string | null;
    readonly previousActiveTurnId: string | null;
    readonly clearedInboundCount: number;
    readonly interruptError?: string | undefined;
  }> {
    const session = this.#findSessionByKey(sessionKey);
    const previousAgentSessionId = session.agentSessionId ?? null;
    const previousActiveTurnId = session.activeTurnId ?? null;
    let interruptedActiveTurn = false;
    let interruptError: string | undefined;

    this.#resetRuntimeForManualSessionReset(session.key);

    if (session.activeTurnId && session.agentSessionId) {
      try {
        await this.#turnRunner.interrupt(session);
        interruptedActiveTurn = true;
      } catch (error) {
        interruptError = error instanceof Error ? error.message : String(error);
        logger.warn("Failed to interrupt active turn during session delete", {
          sessionKey: session.key,
          agentSessionId: session.agentSessionId,
          turnId: session.activeTurnId,
          error: interruptError
        });
      }
    }

    const openMessages = this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: ["pending", "inflight"]
    });
    if (openMessages.length > 0) {
      await this.#sessions.updateInboundMessagesForBatch(
        session.channelId,
        session.rootThreadTs,
        openMessages.map((message) => message.messageTs),
        {
          status: "done",
          batchId: undefined
        }
      );
    }

    this.#clearAssistantStatus(session.channelId, session.rootThreadTs);
    await this.#statusControllers.get(session.key)?.stop();
    this.#statusControllers.delete(session.key);
    this.#runtimeSessions.delete(session.key);

    const deleted = await this.#sessions.deleteSessionByKey(session.key);
    return {
      deleted,
      interruptedActiveTurn,
      previousAgentSessionId,
      previousActiveTurnId,
      clearedInboundCount: openMessages.length,
      interruptError
    };
  }

  async acceptBackgroundJobEvent(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly payload: BackgroundJobEventPayload;
  }): Promise<void> {
    const session = this.#sessions.getSession(options.channelId, options.rootThreadTs);
    if (!session) {
      throw new Error(`Unknown session for background job event: ${options.channelId}:${options.rootThreadTs}`);
    }

    if (
      session.lastTurnSignalKind === "final" &&
      session.lastTurnSignalAt &&
      options.payload.jobId
    ) {
      const job = this.#sessions.getBackgroundJob(options.payload.jobId);
      if (job && compareIsoTimestamp(job.createdAt, session.lastTurnSignalAt) <= 0) {
        logger.info("Ignoring stale background job event after session was finalized", {
          sessionKey: session.key,
          jobId: job.id,
          eventKind: options.payload.eventKind,
          summary: options.payload.summary
        });
        return;
      }
    }

    await this.acceptInboundMessage(session, {
      source: "background_job_event",
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      messageTs: `${Date.now()}.${Math.floor(Math.random() * 1_000_000).toString().padStart(6, "0")}`,
      userId: this.#botUserId || "BACKGROUND_JOB",
      text: options.payload.summary,
      backgroundJob: options.payload
    });
  }

  async acceptUnexpectedTurnStop(options: {
    readonly session: SlackSessionRecord;
    readonly previousTurnId: string;
    readonly reason: string;
  }): Promise<void> {
    await this.acceptInboundMessage(options.session, {
      source: "unexpected_turn_stop",
      channelId: options.session.channelId,
      rootThreadTs: options.session.rootThreadTs,
      messageTs: `${Date.now()}.${Math.floor(Math.random() * 1_000_000).toString().padStart(6, "0")}`,
      userId: this.#botUserId || "BROKER",
      text: options.reason,
      unexpectedTurnStop: {
        turnId: options.previousTurnId,
        reason: options.reason
      }
    });
  }

  async recoverMissedThreadMessages(reason: "socket_ready" | "periodic"): Promise<void> {
    if (this.#catchUpPromise) {
      await this.#catchUpPromise;
      return;
    }

    this.#catchUpPromise = this.#runMissedThreadRecovery(reason)
      .catch((error) => {
        logger.error("Failed to recover missed Slack thread messages", {
          reason,
          error: error instanceof Error ? error.message : String(error)
        });
      })
      .finally(() => {
        this.#catchUpPromise = undefined;
      });

    await this.#catchUpPromise;
  }

  async postSlackMessage(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly text: string;
    readonly kind?: SlackTurnSignalKind | undefined;
    readonly reason?: string | undefined;
  }): Promise<void> {
    const formattedText = markdownishToMrkdwn(options.text);
    const chunks = chunkSlackMessage(formattedText);
    for (const [index, chunk] of chunks.entries()) {
      await this.#postBotThreadMessage(options.channelId, options.rootThreadTs, chunk, {
        alreadyFormatted: true,
        turnSignal:
          index === 0 && options.kind
            ? {
                kind: options.kind,
                reason: options.reason
              }
            : undefined
      });
    }
  }

  async postSlackState(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly kind: "wait" | "block" | "final";
    readonly reason?: string | undefined;
  }): Promise<void> {
    const session = this.#sessions.getSession(options.channelId, options.rootThreadTs);
    if (!session) {
      throw new Error(`Unknown session for Slack state update: ${options.channelId}:${options.rootThreadTs}`);
    }

    await this.#recordStopSignal(session, {
      kind: options.kind,
      reason: options.reason,
      occurredAt: new Date().toISOString()
    });
  }

  async postSlackFile(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly filePath?: string | undefined;
    readonly contentBase64?: string | undefined;
    readonly filename?: string | undefined;
    readonly title?: string | undefined;
    readonly initialComment?: string | undefined;
    readonly altText?: string | undefined;
    readonly snippetType?: string | undefined;
    readonly contentType?: string | undefined;
  }): Promise<SlackUploadedFile> {
    const hasFilePath = Boolean(options.filePath?.trim());
    const hasInlineContent = Boolean(options.contentBase64?.trim());

    if (hasFilePath === hasInlineContent) {
      throw new Error("Provide exactly one of file_path or content_base64");
    }

    let filename = options.filename?.trim() || undefined;
    let bytes: Uint8Array;

    if (hasFilePath) {
      const filePath = options.filePath!.trim();
      bytes = await readFile(filePath);
      filename ??= path.basename(filePath);
    } else {
      const decoded = Buffer.from(options.contentBase64!.trim(), "base64");
      if (decoded.byteLength === 0) {
        throw new Error("Decoded content_base64 was empty");
      }
      if (!filename) {
        throw new Error("filename is required when using content_base64");
      }
      bytes = decoded;
    }

    if (!filename) {
      throw new Error("Unable to determine filename for Slack upload");
    }

    this.#clearAssistantStatus(options.channelId, options.rootThreadTs);
    const uploaded = await this.#slackApi.uploadThreadFile({
      channelId: options.channelId,
      threadTs: options.rootThreadTs,
      filename,
      bytes,
      title: options.title?.trim() || undefined,
      initialComment: options.initialComment
        ? markdownishToMrkdwn(options.initialComment.trim())
        : undefined,
      altText: options.altText?.trim() || undefined,
      snippetType: options.snippetType?.trim() || undefined,
      contentType: options.contentType?.trim() || undefined
    });
    await this.#sessions.setLastSlackReplyAt(options.channelId, options.rootThreadTs, new Date().toISOString());
    return uploaded;
  }

  async #handleCompletedTurnDisposition(
    session: SlackSessionRecord,
    turnId: string,
    dispatchMessages: readonly PersistedInboundMessage[],
    options: {
      readonly aborted: boolean;
    }
  ): Promise<SlackSessionRecord> {
    if (options.aborted) {
      return session;
    }

    const latestSession = this.#findSessionByKey(session.key);
    const disposition = planCompletedTurnDisposition({
      latestSession,
      turnId,
      dispatchMessages,
      aborted: false,
      hasRunningBackgroundJob: this.#hasRunningBackgroundJob(latestSession),
      hasPendingUnexpectedStopNudge: this.#hasPendingUnexpectedStopNudge(latestSession, turnId)
    });
    if (disposition.kind === "none") {
      return latestSession;
    }

    await this.acceptUnexpectedTurnStop({
      session: latestSession,
      previousTurnId: turnId,
      reason: disposition.reason
    });

    return this.#findSessionByKey(latestSession.key);
  }

  #hasRunningBackgroundJob(session: SlackSessionRecord): boolean {
    return this.#sessions.listBackgroundJobs({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs
    }).some((job) => job.status === "registered" || job.status === "running");
  }

  #hasPendingUnexpectedStopNudge(session: SlackSessionRecord, turnId: string): boolean {
    return this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      source: "unexpected_turn_stop",
      status: ["pending", "inflight", "done"]
    }).some((message) => message.unexpectedTurnStop?.turnId === turnId);
  }

  #resolveTurnIdForSignal(session: SlackSessionRecord): string | undefined {
    if (session.activeTurnId) {
      return session.activeTurnId;
    }

    const inflightBatchIds = new Set(
      this.#sessions.listInboundMessages({
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        status: "inflight"
      })
        .map((message) => message.batchId)
        .filter((batchId): batchId is string => Boolean(batchId))
    );

    if (inflightBatchIds.size === 1) {
      return [...inflightBatchIds][0];
    }

    return undefined;
  }

  async stopActiveTurn(session: SlackSessionRecord): Promise<boolean> {
    const runtime = this.#getRuntimeSession(session.key);
    runtime.queue.length = 0;

    if (!session.activeTurnId || !session.agentSessionId) {
      return false;
    }

    await this.#turnRunner.ensureAgentSession(session);
    await this.#turnRunner.interrupt(session);
    await this.#inboundStore.markTurnBatchDone(session, session.activeTurnId);
    await this.#sessions.setActiveTurnId(session.channelId, session.rootThreadTs, undefined);
    this.#clearAssistantStatus(session.channelId, session.rootThreadTs);
    return true;
  }

  async acceptInboundMessage(session: SlackSessionRecord, item: SlackInputMessage): Promise<void> {
    if (!item.messageTs) {
      logger.warn("Skipping Slack inbound message without message ts", {
        sessionKey: session.key,
        source: item.source,
        userId: item.userId
      });
      return;
    }

    this.#clearDispatchFailureBlock(session.key);
    const coauthoredSession = await this.#coauthors.noteIncomingSlackInput(session, item);
    const recordedSession = await this.#inboundStore.recordInboundMessage(coauthoredSession, item);
    this.#setAssistantThinking(recordedSession);
    await this.#dispatchPersistedMessage(recordedSession, item.messageTs);
  }

  async #reconcilePersistedActiveTurns(): Promise<void> {
    const sessions = this.#sessions
      .listSessions()
      .filter((session) => session.activeTurnId)
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    if (sessions.length === 0) {
      return;
    }

    logger.info("Reconciling persisted active Slack sessions", {
      candidateSessionCount: sessions.length
    });

    let clearedCount = 0;
    let retainedCount = 0;

    for (const session of sessions) {
      const outcome = await this.#reconcileSingleActiveTurn(session, {
        treatMissingAsStale: true,
        resumePending: false
      });
      if (outcome === "retained") {
        retainedCount += 1;
      } else {
        clearedCount += 1;
      }
    }

    logger.info("Finished persisted active session reconciliation", {
      clearedCount,
      retainedCount
    });
  }

  #startActiveTurnReconciler(): void {
    this.#stopActiveTurnReconciler();
    this.#activeTurnReconcileTimer = setInterval(() => {
      this.#runLiveActiveTurnReconcileOnce();
    }, this.#config.slackActiveTurnReconcileIntervalMs);
  }

  #stopActiveTurnReconciler(): void {
    if (!this.#activeTurnReconcileTimer) {
      return;
    }

    clearInterval(this.#activeTurnReconcileTimer);
    this.#activeTurnReconcileTimer = undefined;
  }

  #runLiveActiveTurnReconcileOnce(): void {
    if (this.#activeTurnReconcilePromise) {
      logger.debug("Skipping duplicate active-turn reconcile tick while previous pass is still running");
      return;
    }

    this.#activeTurnReconcilePromise = this.#reconcileLiveActiveTurns()
      .catch((error: unknown) => {
        logger.warn("Failed to finish live active-turn reconciliation pass", {
          error: error instanceof Error ? error.message : String(error)
        });
      })
      .finally(() => {
        this.#activeTurnReconcilePromise = undefined;
      });
  }

  async #reconcileLiveActiveTurns(): Promise<void> {
    const sessions = this.#sessions
      .listSessions()
      .filter((session) => session.activeTurnId)
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    for (const session of sessions) {
      try {
        await this.#reconcileSingleActiveTurn(session);
      } catch (error) {
        logger.warn("Failed to reconcile live agent turn state", {
          sessionKey: session.key,
          turnId: session.activeTurnId ?? null,
          error: error instanceof Error ? error.message : String(error)
        });
      }
    }

    await this.#recoverDormantPendingSessions();

    if (this.#shouldRunPeriodicMissedThreadRecovery()) {
      await this.recoverMissedThreadMessages("periodic");
    }
  }

  async #reconcileSingleActiveTurn(
    session: SlackSessionRecord,
    options?: {
      readonly treatMissingAsStale?: boolean | undefined;
      readonly resumePending?: boolean | undefined;
    }
  ): Promise<"cleared" | "retained"> {
    const outcome = await this.#turnReconciler.reconcileSingleActiveTurn(session, options);

    if (outcome === "cleared") {
      this.#resetRuntimeProcessing(session.key);
      const latestSession = this.#findSessionByKey(session.key);
      this.#clearAssistantStatus(latestSession.channelId, latestSession.rootThreadTs);
      if (options?.resumePending ?? true) {
        await this.#resumePendingDispatch(session.key);
      }
    }

    return outcome;
  }

  async #runMissedThreadRecovery(reason: "socket_ready" | "periodic"): Promise<void> {
    const now = Date.now();
    this.#lastMissedThreadRecoveryAtMs = now;
    if (now < this.#missedThreadRecoveryRateLimitUntilMs) {
      logger.info("Skipping Slack missed-message recovery during rate-limit backoff", {
        reason,
        retryInMs: this.#missedThreadRecoveryRateLimitUntilMs - now
      });
      return;
    }

    const sessions = this.#sessions
      .listSessions()
      .filter((session) => shouldAutoRecoverSession(session, now))
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    if (sessions.length === 0) {
      return;
    }

    logger.info("Checking Slack threads for missed messages", {
      reason,
      candidateSessionCount: sessions.length
    });

    let recoveredBatchCount = 0;
    let recoveredMessageCount = 0;
    let skippedByRateLimit = false;

    for (let session of sessions) {
      let messages: SlackThreadMessage[];
      try {
        messages = await this.#slackApi.listThreadMessages({
          channelId: session.channelId,
          rootThreadTs: session.rootThreadTs
        });
      } catch (error) {
        if (isSlackRateLimitError(error)) {
          const retryInMs = this.#deferMissedThreadRecoveryAfterRateLimit(error.retryAfterMs);
          skippedByRateLimit = true;
          logger.warn("Paused Slack missed-message recovery after Slack rate limit", {
            reason,
            sessionKey: session.key,
            retryInMs,
            retryAfterMs: error.retryAfterMs ?? null
          });
          break;
        }

        logger.warn("Failed to check Slack thread for missed messages", {
          reason,
          sessionKey: session.key,
          error: error instanceof Error ? error.message : String(error)
        });
        continue;
      }
      const latestPersistedMessageTs =
        this.#sessions.getLatestSlackInboundMessageTs(session.channelId, session.rootThreadTs) ??
        session.lastObservedMessageTs;
      const missedMessages = messages
        .filter((message) => !this.#selfMessageFilter.shouldIgnoreThreadMessage(message))
        .filter((message) => isSlackMessageAfterCursor(message.messageTs, latestPersistedMessageTs));

      if (missedMessages.length > 0) {
        logger.warn("Recovering missed Slack thread messages", {
          reason,
          sessionKey: session.key,
          missedCount: missedMessages.length,
          fromTs: latestPersistedMessageTs,
          toTs: missedMessages.at(-1)?.messageTs ?? null
        });
      }

      for (const message of missedMessages) {
        const input = createSlackInputFromThreadMessage("thread_reply", message);
        if (isSlackMessageEffectivelyEmpty(input.text, input.images, input.slackMessage)) {
          continue;
        }

        session = await this.#inboundStore.recordInboundMessage(session, input);
      }

      const recovered = await this.#dispatchPendingRecoveryBatch(session, "missed_thread_messages");
      if (recovered > 0) {
        recoveredBatchCount += 1;
        recoveredMessageCount += recovered;
      }
    }

    if (!skippedByRateLimit) {
      this.#missedThreadRecoveryRateLimitBackoffMs = 0;
      this.#missedThreadRecoveryRateLimitUntilMs = 0;
    }

    logger.info("Finished Slack missed-message recovery", {
      reason,
      recoveredBatchCount,
      recoveredMessageCount,
      skippedByRateLimit
    });
  }

  async #postBotThreadMessage(
    channelId: string,
    rootThreadTs: string,
    text: string,
    options?: {
      readonly alreadyFormatted?: boolean | undefined;
      readonly turnSignal?: {
        readonly kind: SlackTurnSignalKind;
        readonly reason?: string | undefined;
      } | undefined;
    }
  ): Promise<string | undefined> {
    this.#clearAssistantStatus(channelId, rootThreadTs);
    const formattedText = options?.alreadyFormatted ? text : markdownishToMrkdwn(text);
    const ts = await this.#slackApi.postThreadMessage(channelId, rootThreadTs, formattedText);
    if (ts) {
      this.#selfMessageFilter.rememberPostedMessageTs(ts);
      const occurredAt = new Date().toISOString();
      const session = await this.#sessions.setLastSlackReplyAt(channelId, rootThreadTs, occurredAt);
      if (options?.turnSignal?.kind) {
        await this.#recordStopSignal(session, {
          kind: options.turnSignal.kind,
          reason: options.turnSignal.reason,
          occurredAt
        });
      }
    }
    return ts;
  }

  async #postSessionPageLinkIfNeeded(session: SlackSessionRecord): Promise<SlackSessionRecord> {
    const latestSession = this.#findSessionByKey(session.key);
    if (latestSession.sessionPageLinkPostedAt) {
      return latestSession;
    }

    const inflight = this.#sessionPageLinkPosts.get(session.key);
    if (inflight) {
      return await inflight;
    }

    const posting = this.#postSessionPageLinkOnce(latestSession);
    this.#sessionPageLinkPosts.set(session.key, posting);
    try {
      return await posting;
    } finally {
      if (this.#sessionPageLinkPosts.get(session.key) === posting) {
        this.#sessionPageLinkPosts.delete(session.key);
      }
    }
  }

  async #postSessionPageLinkOnce(session: SlackSessionRecord): Promise<SlackSessionRecord> {
    const latestSession = this.#findSessionByKey(session.key);
    if (latestSession.sessionPageLinkPostedAt) {
      return latestSession;
    }

    const url = buildAdminSessionUrl(this.#config.adminBaseUrl, session.key);
    try {
      await this.#postBotThreadMessage(
        latestSession.channelId,
        latestSession.rootThreadTs,
        this.#formatSessionPageLinkMessage(latestSession, url),
        { alreadyFormatted: true }
      );
      return await this.#sessions.setSessionPageLinkPostedAt(
        latestSession.channelId,
        latestSession.rootThreadTs,
        new Date().toISOString()
      );
    } catch (error) {
      logger.warn("Failed to post admin session link into Slack thread", {
        sessionKey: session.key,
        adminSessionUrl: url,
        error: error instanceof Error ? error.message : String(error)
      });
      return session;
    }
  }

  #formatSessionPageLinkMessage(session: SlackSessionRecord, url: string): string {
    const lines = [`<${url}|查看会话活动时间线>`];
    const identity = this.#githubPrIdentity?.getSessionIdentityStatus(session);
    if (identity?.binding.state === "unbound") {
      const warning = identity.defaultAccount.available
        ? `当前发起人还没有绑定 GitHub 账号。不绑定时，后续创建 PR 会使用默认账号 ${identity.defaultAccount.githubLogin}。`
        : "当前发起人还没有绑定 GitHub 账号。当前没有默认 GitHub PR 账号，创建 PR 前需要先绑定。";
      lines.push(
        "",
        warning,
        `<${url}/github/bind|绑定 GitHub>`
      );
    }
    return lines.join("\n");
  }

  async #recordStopSignal(
    session: SlackSessionRecord,
    signal: {
      readonly kind: SlackTurnSignalKind;
      readonly reason?: string | undefined;
      readonly occurredAt: string;
    }
  ): Promise<SlackSessionRecord> {
    const turnId = this.#resolveTurnIdForSignal(session);
    let latestSession = await this.#sessions.recordTurnSignal(session.channelId, session.rootThreadTs, {
      turnId,
      kind: signal.kind,
      reason: signal.reason,
      occurredAt: signal.occurredAt
    });

    if (turnId) {
      latestSession = await this.#inboundStore.markTurnBatchDone(latestSession, turnId);
    }

    this.#clearAssistantStatus(session.channelId, session.rootThreadTs);
    return latestSession;
  }

  async #dispatchPersistedMessage(session: SlackSessionRecord, messageTs: string): Promise<void> {
    let latestSession = this.#findSessionByKey(session.key);
    const pendingMessage = this.#sessions.getInboundMessage(
      latestSession.channelId,
      latestSession.rootThreadTs,
      messageTs
    );

    if (!pendingMessage || pendingMessage.status !== "pending") {
      return;
    }

    latestSession = await this.#postSessionPageLinkIfNeeded(latestSession);
    this.#setAssistantThinking(latestSession);
    if (latestSession.activeTurnId) {
      try {
        const input = this.#inboundStore.createSlackInputFromPersistedMessage(pendingMessage);
        const submittedSession = await this.#submitPersistedMessageIntoActiveTurn(latestSession, pendingMessage, input);
        if (submittedSession) {
          latestSession = submittedSession;
          return;
        }
      } catch (error) {
        logger.warn("Failed to deliver persisted Slack message into active agent turn", {
          sessionKey: session.key,
          turnId: latestSession.activeTurnId,
          messageTs,
          error: error instanceof Error ? error.message : String(error)
        });

        if (isMissingActiveTurnInputError(error)) {
          latestSession = await this.#syncActiveTurnFromActiveInputError(latestSession, error, {
            messageTs
          });
        }
      }
    }

    this.#enqueueDispatch(latestSession, {
      kind: "dispatch_pending"
    });
  }

  async #dispatchPendingRecoveryBatch(
    session: SlackSessionRecord,
    recoveryKind: "missed_thread_messages"
  ): Promise<number> {
    let latestSession = this.#findSessionByKey(session.key);
    const pendingMessages = this.#inboundStore.listPendingMessages(latestSession, {
      source: ["app_mention", "direct_message", "thread_reply"]
    });

    if (pendingMessages.length === 0) {
      return 0;
    }

    this.#setAssistantThinking(latestSession);
    if (latestSession.activeTurnId) {
      try {
        const input = await this.#inboundStore.createRecoveredBatchInput(latestSession, pendingMessages, recoveryKind);
        if (!input) {
          return 0;
        }

        const submittedSession = await this.#submitPersistedBatchIntoActiveTurn(
          latestSession,
          pendingMessages,
          input
        );
        if (submittedSession) {
          latestSession = submittedSession;
          return pendingMessages.length;
        }
      } catch (error) {
        logger.warn("Failed to deliver recovered Slack backlog into active agent turn", {
          sessionKey: session.key,
          turnId: latestSession.activeTurnId,
          recoveryKind,
          error: error instanceof Error ? error.message : String(error)
        });

        if (isMissingActiveTurnInputError(error)) {
          latestSession = await this.#syncActiveTurnFromActiveInputError(latestSession, error);
        }
      }
    }

    this.#enqueueDispatch(latestSession, {
      kind: "dispatch_pending",
      recoveryKind
    });
    return pendingMessages.length;
  }

  #enqueueDispatch(session: SlackSessionRecord, request: PendingDispatchRequest): void {
    const runtime = this.#getRuntimeSession(session.key);
    runtime.blockedUntilMs = undefined;
    runtime.blockedFailureFingerprint = undefined;
    const existing = runtime.queue.find((entry) => entry.kind === "dispatch_pending");

    if (existing) {
      if (!existing.recoveryKind && request.recoveryKind) {
        runtime.queue.splice(runtime.queue.indexOf(existing), 1, request);
      }
    } else {
      runtime.queue.push(request);
    }

    logger.debug("Queued pending Slack dispatch", {
      sessionKey: session.key,
      recoveryKind: request.recoveryKind ?? null,
      queueLength: runtime.queue.length
    });

    if (!runtime.processing) {
      void this.#drainQueue(session.key);
    }
  }

  async #submitPersistedMessageIntoActiveTurn(
    session: SlackSessionRecord,
    pendingMessage: PersistedInboundMessage,
    input: SlackInputMessage
  ): Promise<SlackSessionRecord | null> {
    let latestSession = session;

    for (let attempt = 0; attempt < 2; attempt += 1) {
      if (!latestSession.activeTurnId) {
        return null;
      }

      try {
        await this.#turnRunner.submitAdditionalInput(latestSession, input);
        await this.#inboundStore.markMessagesInflight(
          latestSession,
          [pendingMessage],
          latestSession.activeTurnId
        );
        logger.debug("Delivered persisted Slack message into active agent turn", {
          sessionKey: session.key,
          turnId: latestSession.activeTurnId,
          source: input.source,
          userId: input.userId
        });
        return latestSession;
      } catch (error) {
        const syncedSession = isMissingActiveTurnInputError(error)
          ? await this.#syncActiveTurnFromActiveInputError(latestSession, error, {
              messageTs: pendingMessage.messageTs
            })
          : latestSession;
        if (syncedSession.activeTurnId && syncedSession.activeTurnId !== latestSession.activeTurnId) {
          latestSession = syncedSession;
          continue;
        }
        if (!syncedSession.activeTurnId && latestSession.activeTurnId) {
          return null;
        }
        throw error;
      }
    }

    return null;
  }

  async #submitPersistedBatchIntoActiveTurn(
    session: SlackSessionRecord,
    pendingMessages: readonly PersistedInboundMessage[],
    input: SlackInputMessage
  ): Promise<SlackSessionRecord | null> {
    let latestSession = session;

    for (let attempt = 0; attempt < 2; attempt += 1) {
      if (!latestSession.activeTurnId) {
        return null;
      }

      try {
        await this.#turnRunner.submitAdditionalInput(latestSession, input);
        await this.#inboundStore.markMessagesInflight(
          latestSession,
          pendingMessages,
          latestSession.activeTurnId
        );
        return latestSession;
      } catch (error) {
        const syncedSession = isMissingActiveTurnInputError(error)
          ? await this.#syncActiveTurnFromActiveInputError(latestSession, error)
          : latestSession;
        if (syncedSession.activeTurnId && syncedSession.activeTurnId !== latestSession.activeTurnId) {
          latestSession = syncedSession;
          continue;
        }
        if (!syncedSession.activeTurnId && latestSession.activeTurnId) {
          return null;
        }
        throw error;
      }
    }

    return null;
  }

  async #syncActiveTurnFromActiveInputError(
    session: SlackSessionRecord,
    error: unknown,
    options?: {
      readonly messageTs?: string | undefined;
    }
  ): Promise<SlackSessionRecord> {
    const mismatch = parseActiveTurnMismatch(error);
    if (mismatch && mismatch.actualTurnId !== session.activeTurnId) {
      const inflightBatchIds = this.#sessions.listInboundMessages({
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        status: "inflight"
      }).map((message) => message.batchId);

      if (shouldResetConflictingActiveTurnMismatch(inflightBatchIds, mismatch.actualTurnId)) {
        logger.warn("Detected conflicting inflight Slack batches during active-turn resync; resetting broker runtime state", {
          sessionKey: session.key,
          previousTurnId: session.activeTurnId,
          actualTurnId: mismatch.actualTurnId,
          inflightBatchIds: [...new Set(inflightBatchIds.filter((batchId): batchId is string => Boolean(batchId)))],
          messageTs: options?.messageTs ?? null
        });
        await this.#sessions.resetInflightMessages(
          session.channelId,
          session.rootThreadTs
        );
        const latestSession = await this.#sessions.setActiveTurnId(
          session.channelId,
          session.rootThreadTs,
          undefined
        );
        this.#resetRuntimeProcessing(session.key);
        return latestSession;
      }

      logger.warn("Synchronizing broker active turn id to agent-runtime-reported active turn", {
        sessionKey: session.key,
        previousTurnId: session.activeTurnId,
        actualTurnId: mismatch.actualTurnId,
        messageTs: options?.messageTs ?? null
      });
      return await this.#sessions.setActiveTurnId(
        session.channelId,
        session.rootThreadTs,
        mismatch.actualTurnId
      );
    }

    logger.warn("Detected stale active agent turn; resetting broker runtime state", {
      sessionKey: session.key,
      turnId: session.activeTurnId,
      messageTs: options?.messageTs ?? null
    });
    await this.#sessions.resetInflightMessages(
      session.channelId,
      session.rootThreadTs,
      session.activeTurnId
    );
    const latestSession = await this.#sessions.setActiveTurnId(
      session.channelId,
      session.rootThreadTs,
      undefined
    );
    this.#resetRuntimeProcessing(session.key);
    return latestSession;
  }

  async #drainQueue(sessionKey: string): Promise<void> {
    const runtime = this.#getRuntimeSession(sessionKey);
    const generation = runtime.generation;
    runtime.processing = true;

    while (runtime.queue.length > 0) {
      if (runtime.generation !== generation) {
        return;
      }

      const next = runtime.queue.shift();
      if (!next) {
        continue;
      }

      let session = this.#findSessionByKey(sessionKey);

      try {
        if (session.activeTurnId) {
          runtime.queue.unshift(next);
          break;
        }

        this.#setAssistantThinking(session);
        session = await this.#turnRunner.ensureAgentSession(session);
        const pendingMessages = this.#inboundStore.listPendingMessages(session);

        if (pendingMessages.length === 0) {
          continue;
        }

        session = await this.#postSessionPageLinkIfNeeded(session);
        const dispatchMessages = next.recoveryKind ? pendingMessages : [pendingMessages[0]!];
        const slackInput = next.recoveryKind
          ? await this.#inboundStore.createRecoveredBatchInput(session, dispatchMessages, next.recoveryKind)
          : this.#inboundStore.createSlackInputFromPersistedMessage(dispatchMessages[0]!);

        if (!slackInput) {
          continue;
        }

        const input = await this.#turnRunner.buildTurnInput(slackInput);
        const turnOutcome = await this.#turnRunner.submitInputWithRecovery({
          session,
          sessionKey,
          senderUserId: slackInput.userId,
          input,
          messageTsList: dispatchMessages.map((message) => message.messageTs)
        });

        if (runtime.generation !== generation) {
          return;
        }

        session = turnOutcome.session;
        const result = turnOutcome.result;
        logger.debug("agent turn finished without broker-managed Slack reply forwarding", {
          sessionKey,
          turnId: result.turnId,
          aborted: result.aborted,
          hadFinalMessage: Boolean(result.finalMessage)
        });
        session = await this.#handleCompletedTurnDisposition(session, result.turnId, dispatchMessages, {
          aborted: result.aborted
        });
        this.#maybeClearAssistantStatusIfIdle(session);
      } catch (error) {
        if (runtime.generation !== generation) {
          return;
        }

        if (isAuthProfileUnavailableError(error)) {
          await this.#handleAuthProfileUnavailable(session, error, runtime);
          break;
        }

        logger.error("Slack conversation turn dispatch failed", {
          sessionKey,
          channelId: session.channelId,
          rootThreadTs: session.rootThreadTs,
          error: error instanceof Error ? error.message : String(error)
        });
        const nowMs = Date.now();
        if (shouldNotifySlackFailure({
          previousFingerprint: runtime.lastFailureNotificationFingerprint,
          previousNotifiedAtMs: runtime.lastFailureNotificationAtMs,
          error,
          nowMs
        })) {
          if (shouldPostSlackRunFailure(error)) {
            await this.#postBotThreadMessage(
              session.channelId,
              session.rootThreadTs,
              formatSlackRunFailureMessage(error)
            );
            runtime.lastFailureNotificationFingerprint = createSlackFailureFingerprint(error);
            runtime.lastFailureNotificationAtMs = nowMs;
          } else {
            logger.info("Suppressing recoverable Slack reconnect notification", {
              sessionKey,
              error: error instanceof Error ? error.message : String(error)
            });
          }
        } else {
          logger.warn("Suppressing duplicate Slack run failure notification", {
            sessionKey,
            error: error instanceof Error ? error.message : String(error)
          });
        }
        await this.#sessions.setActiveTurnId(session.channelId, session.rootThreadTs, undefined);
        if (
          isRecoverableAgentTurnFailure(error) ||
          isMissingActiveTurnInputError(error) ||
          isMissingAgentSessionError(error)
        ) {
          this.#scheduleAutoResume(session.key);
        } else {
          runtime.blockedUntilMs = nowMs + NONRECOVERABLE_DISPATCH_RETRY_COOLDOWN_MS;
          runtime.blockedFailureFingerprint = createSlackFailureFingerprint(error);
          logger.warn("Pausing automatic retries for a session after non-recoverable dispatch failure", {
            sessionKey,
            blockedUntilMs: runtime.blockedUntilMs,
            error: error instanceof Error ? error.message : String(error)
          });
        }
        this.#clearAssistantStatus(session.channelId, session.rootThreadTs);
        break;
      }
    }

    if (runtime.generation === generation) {
      runtime.processing = false;
    }
  }

  #getRuntimeSession(sessionKey: string): RuntimeSessionState {
    let runtime = this.#runtimeSessions.get(sessionKey);

    if (!runtime) {
      runtime = {
        queue: [],
        processing: false,
        generation: 0
      };
      this.#runtimeSessions.set(sessionKey, runtime);
    }

    return runtime;
  }

  #resetRuntimeForManualSessionReset(sessionKey: string): void {
    const runtime = this.#getRuntimeSession(sessionKey);
    if (runtime.autoResumeTimer) {
      clearTimeout(runtime.autoResumeTimer);
      runtime.autoResumeTimer = undefined;
    }
    runtime.queue.length = 0;
    runtime.processing = false;
    runtime.generation += 1;
    runtime.blockedUntilMs = undefined;
    runtime.blockedFailureFingerprint = undefined;
    runtime.lastFailureNotificationFingerprint = undefined;
    runtime.lastFailureNotificationAtMs = undefined;
  }

  #findSessionByKey(sessionKey: string): SlackSessionRecord {
    const session = this.#sessions.listSessions().find((entry) => entry.key === sessionKey);
    if (!session) {
      throw new Error(`Unknown session runtime key: ${sessionKey}`);
    }

    return session;
  }

  #resetRuntimeProcessing(sessionKey: string): void {
    const runtime = this.#getRuntimeSession(sessionKey);
    runtime.generation += 1;
    runtime.processing = false;
  }

  #clearDispatchFailureBlock(sessionKey: string): void {
    const runtime = this.#getRuntimeSession(sessionKey);
    runtime.blockedUntilMs = undefined;
    runtime.blockedFailureFingerprint = undefined;
  }

  async #appendSessionResetTrace(
    session: SlackSessionRecord,
    detail: {
      readonly previousAgentSessionId: string | null;
      readonly previousActiveTurnId: string | null;
      readonly clearedInboundCount: number;
      readonly resetMessageTs: string;
      readonly historyMessageCount: number;
    }
  ): Promise<void> {
    const now = new Date().toISOString();
    const sequence = this.#sessions.listAgentTraceEvents(session.key, 10_000).length + 1;
    await this.#sessions.upsertAgentTraceEvent({
      id: randomUUID(),
      sessionKey: session.key,
      source: "broker",
      type: "agent_session_reset",
      at: now,
      sequence,
      title: "Session 已重置",
      summary: "已清空 agent history 并重新唤起 bot",
      detail: JSON.stringify(detail, null, 2),
      status: "completed",
      role: "system",
      metadata: detail,
      createdAt: now,
      updatedAt: now
    });
  }

  #scheduleAutoResume(sessionKey: string): void {
    const runtime = this.#getRuntimeSession(sessionKey);
    if (runtime.autoResumeTimer) {
      return;
    }

    runtime.autoResumeTimer = setTimeout(() => {
      runtime.autoResumeTimer = undefined;
      void this.#resumePendingDispatch(sessionKey, {
        forceReset: true
      }).catch((error) => {
        logger.warn("Failed to auto-resume pending Slack dispatch after recoverable turn failure", {
          sessionKey,
          error: error instanceof Error ? error.message : String(error)
        });
      });
    }, AUTO_RESUME_AFTER_FAILURE_MS);

    logger.warn("Scheduled automatic retry for pending Slack dispatch after recoverable turn failure", {
      sessionKey,
      delayMs: AUTO_RESUME_AFTER_FAILURE_MS
    });
  }

  async #resumePendingDispatch(sessionKey: string, options?: {
    readonly forceReset?: boolean | undefined;
  }): Promise<number> {
    const session = this.#sessions.listSessions().find((entry) => entry.key === sessionKey);
    if (!session) {
      return 0;
    }

    const pendingMessages = this.#inboundStore.listPendingMessages(session);
    if (pendingMessages.length === 0) {
      return 0;
    }

    if (!session.activeTurnId && options?.forceReset) {
      logger.warn("Force-resetting broker runtime state before resuming pending Slack dispatch", {
        sessionKey,
        pendingCount: pendingMessages.length
      });
      this.#resetRuntimeProcessing(sessionKey);
    }

    this.#setAssistantThinking(session);
    this.#enqueueDispatch(session, {
      kind: "dispatch_pending"
    });

    return pendingMessages.length;
  }

  async #recoverPendingSessionsOnBoot(): Promise<void> {
    const sessions = this.#sessions
      .listSessions()
      .filter((session) => !session.activeTurnId)
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    let resumedSessionCount = 0;
    let resumedMessageCount = 0;
    let orphanedInflightDoneCount = 0;
    let orphanedInflightResetCount = 0;

    for (const session of sessions) {
      const latestSession = this.#findSessionByKey(session.key);
      if (latestSession.activeTurnId) {
        continue;
      }

      const reconciled = await this.#inboundStore.reconcileOrphanedInflightMessages(latestSession);
      orphanedInflightDoneCount += reconciled.markedDoneCount;
      orphanedInflightResetCount += reconciled.resetToPendingCount;

      const runtime = this.#getRuntimeSession(session.key);
      if (runtime.processing || runtime.queue.some((entry) => entry.kind === "dispatch_pending")) {
        continue;
      }

      const refreshedSession = this.#findSessionByKey(session.key);
      if (refreshedSession.activeTurnId) {
        continue;
      }
      const resumedCount = await this.#resumePendingDispatch(refreshedSession.key);

      if (resumedCount === 0) {
        continue;
      }

      resumedSessionCount += 1;
      resumedMessageCount += resumedCount;
    }

    if (resumedSessionCount > 0) {
      logger.warn("Recovered pending Slack dispatch backlog during broker startup", {
        resumedSessionCount,
        resumedMessageCount,
        orphanedInflightDoneCount,
        orphanedInflightResetCount
      });
    } else if (orphanedInflightDoneCount > 0 || orphanedInflightResetCount > 0) {
      logger.warn("Reconciled orphaned inflight Slack messages during broker startup", {
        orphanedInflightDoneCount,
        orphanedInflightResetCount
      });
    }
  }

  async #recoverDormantPendingSessions(): Promise<void> {
    const nowMs = Date.now();
    const sessions = this.#sessions
      .listSessions()
      .filter((session) => !session.activeTurnId)
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    for (const session of sessions) {
      const runtime = this.#getRuntimeSession(session.key);
      const openMessages = this.#sessions.listInboundMessages({
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        status: ["pending", "inflight"]
      });

      const latestOpenMessageUpdatedAt = openMessages
        .map((message) => message.updatedAt)
        .filter(Boolean)
        .sort(compareIsoTimestamp)
        .at(-1);

      if (shouldForceResetStaleIdleRuntime({
        activeTurnId: session.activeTurnId,
        runtimeProcessing: runtime.processing,
        latestOpenMessageUpdatedAt,
        nowMs,
        staleAfterMs: this.#config.slackActiveTurnReconcileIntervalMs
      })) {
        logger.warn("Force-resetting stale idle Slack runtime state", {
          sessionKey: session.key,
          latestOpenMessageUpdatedAt,
          openMessageCount: openMessages.length
        });
        this.#resetRuntimeProcessing(session.key);
      }

      if (runtime.processing) {
        continue;
      }
      const authBlocked = Boolean(session.authBlockedAt);

      const reconciled = await this.#inboundStore.reconcileOrphanedInflightMessages(session);
      if (reconciled.markedDoneCount > 0 || reconciled.resetToPendingCount > 0) {
        logger.warn("Reconciled orphaned inflight Slack messages for idle session", {
          sessionKey: session.key,
          markedDoneCount: reconciled.markedDoneCount,
          resetToPendingCount: reconciled.resetToPendingCount
        });
      }

      if (!authBlocked && runtime.blockedUntilMs && runtime.blockedUntilMs > nowMs) {
        continue;
      }

      await this.#resumePendingDispatch(session.key);
    }
  }

  async #recoverPendingSyntheticMessages(): Promise<void> {
    const sessions = this.#sessions
      .listSessions()
      .sort((left, right) => compareIsoTimestamp(right.updatedAt, left.updatedAt));

    for (const session of sessions) {
      const pendingSyntheticMessages = this.#sessions.listInboundMessages({
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        status: "pending",
        source: "background_job_event"
      });

      for (const message of pendingSyntheticMessages) {
        await this.#dispatchPersistedMessage(session, message.messageTs);
      }
    }
  }

  #shouldRunPeriodicMissedThreadRecovery(): boolean {
    const intervalMs = Math.max(
      this.#config.slackMissedThreadRecoveryIntervalMs,
      this.#config.slackActiveTurnReconcileIntervalMs
    );
    return Date.now() - this.#lastMissedThreadRecoveryAtMs >= intervalMs;
  }

  #deferMissedThreadRecoveryAfterRateLimit(retryAfterMs: number | undefined): number {
    const nextExponentialBackoffMs = this.#missedThreadRecoveryRateLimitBackoffMs > 0
      ? Math.min(
        this.#missedThreadRecoveryRateLimitBackoffMs * 2,
        MISSED_THREAD_RECOVERY_RATE_LIMIT_MAX_BACKOFF_MS
      )
      : MISSED_THREAD_RECOVERY_RATE_LIMIT_MIN_BACKOFF_MS;
    const requestedBackoffMs = retryAfterMs ?? 0;
    const nextBackoffMs = Math.min(
      Math.max(nextExponentialBackoffMs, requestedBackoffMs, MISSED_THREAD_RECOVERY_RATE_LIMIT_MIN_BACKOFF_MS),
      MISSED_THREAD_RECOVERY_RATE_LIMIT_MAX_BACKOFF_MS
    );
    this.#missedThreadRecoveryRateLimitBackoffMs = nextBackoffMs;
    this.#missedThreadRecoveryRateLimitUntilMs = Date.now() + nextBackoffMs;
    return nextBackoffMs;
  }

  #setAssistantThinking(session: SlackSessionRecord): void {
    this.#getStatusController(session.channelId, session.rootThreadTs).setThinking();
  }

  #clearAssistantStatus(channelId: string, rootThreadTs: string): void {
    const sessionKey = SessionManager.createKey(channelId, rootThreadTs);
    this.#statusControllers.get(sessionKey)?.clear();
  }

  #maybeClearAssistantStatusIfIdle(session: SlackSessionRecord): void {
    if (session.activeTurnId) {
      return;
    }

    const hasPendingOrInflightMessages = this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: ["pending", "inflight"]
    }).length > 0;

    if (hasPendingOrInflightMessages) {
      return;
    }

    this.#clearAssistantStatus(session.channelId, session.rootThreadTs);
  }

  #getStatusController(channelId: string, rootThreadTs: string): SlackAssistantStatusController {
    const sessionKey = SessionManager.createKey(channelId, rootThreadTs);
    let controller = this.#statusControllers.get(sessionKey);

    if (!controller) {
      controller = new SlackAssistantStatusController({
        slackApi: this.#slackApi,
        channelId,
        threadTs: rootThreadTs
      });
      this.#statusControllers.set(sessionKey, controller);
    }

    return controller;
  }

  #handleAgentRuntimeEvent(event: AgentRuntimeEvent): void {
    void this.#traceRecorder.record(event).catch((error: unknown) => {
      logger.warn("Failed to persist agent runtime trace event", {
        type: event.type,
        error: error instanceof Error ? error.message : String(error)
      });
    });

    const sessionKey = "brokerSessionKey" in event ? event.brokerSessionKey : undefined;
    const session = sessionKey ? this.#sessions.getSessionByKey(sessionKey) : undefined;
    if (!session) {
      return;
    }

    const controller = this.#getStatusController(session.channelId, session.rootThreadTs);
    switch (event.type) {
      case "agent.tool.started":
        controller.handleToolStart({
          name: event.name,
          callId: event.callId,
          turnId: event.turnId
        });
        return;
      case "agent.tool.completed":
        controller.handleToolEnd({
          name: event.name,
          callId: event.callId,
          turnId: event.turnId,
          status: event.status
        });
        return;
      case "agent.message.delta":
      case "agent.message.completed":
      case "agent.turn.completed":
      case "agent.error":
        controller.clear();
        return;
      default:
        return;
    }
  }

  async #handleAuthProfileUnavailable(
    session: SlackSessionRecord,
    error: AuthProfileUnavailableError,
    runtime: RuntimeSessionState
  ): Promise<void> {
    if (isAuthProfileProbeFailureReason(error.reason)) {
      await this.#handleAuthProfileProbeFailure(session, error, runtime);
      return;
    }

    logger.warn("Pausing Slack session until a human switches auth profile", {
      sessionKey: session.key,
      profileName: error.profileName ?? null,
      reason: error.reason
    });

    runtime.blockedUntilMs = Number.MAX_SAFE_INTEGER;
    runtime.blockedFailureFingerprint = `auth:${error.profileName ?? "none"}:${error.reason}`;
    const alreadyBlocked = Boolean(session.authBlockedAt);
    const blockedAt = new Date().toISOString();
    let blockedSession = await this.#sessions.markSessionAuthBlocked(session.key, {
      reason: error.reason,
      blockedAt
    });
    blockedSession = await this.#sessions.setActiveTurnId(
      blockedSession.channelId,
      blockedSession.rootThreadTs,
      undefined
    );
    if (!alreadyBlocked) {
      await this.#recordAuthBlockedTrace(blockedSession, error, blockedAt);
    }

    if (!blockedSession.authBlockedNoticePostedAt) {
      const url = buildAdminSessionUrl(this.#config.adminBaseUrl, blockedSession.key);
      const postedAt = new Date().toISOString();
      await this.#postBotThreadMessage(
        blockedSession.channelId,
        blockedSession.rootThreadTs,
        [
          `当前会话绑定的账号额度不可用：${error.userMessage}`,
          `<${url}|打开 Session 页面手动切换账号并继续处理>`
        ].join("\n"),
        {
          alreadyFormatted: true,
          turnSignal: {
            kind: "block",
            reason: error.reason
          }
        }
      );
      await this.#sessions.setSessionAuthBlockedNoticePostedAt(blockedSession.key, postedAt);
    }

    this.#clearAssistantStatus(blockedSession.channelId, blockedSession.rootThreadTs);
  }

  async #handleAuthProfileProbeFailure(
    session: SlackSessionRecord,
    error: AuthProfileUnavailableError,
    runtime: RuntimeSessionState
  ): Promise<void> {
    logger.warn("Deferring Slack session until auth profile status can be read", {
      sessionKey: session.key,
      profileName: error.profileName ?? null,
      reason: error.reason
    });

    runtime.blockedUntilMs = undefined;
    runtime.blockedFailureFingerprint = `auth-probe:${error.profileName ?? "none"}:${error.reason}`;
    const latestSession = session.authBlockedAt && isAuthProfileProbeFailureReason(session.authBlockReason)
      ? await this.#sessions.clearSessionAuthBlock(session.key)
      : session;
    await this.#sessions.setActiveTurnId(
      latestSession.channelId,
      latestSession.rootThreadTs,
      undefined
    );
    this.#scheduleAutoResume(latestSession.key);
    this.#clearAssistantStatus(latestSession.channelId, latestSession.rootThreadTs);
  }

  async #recordAuthBlockedTrace(
    session: SlackSessionRecord,
    error: AuthProfileUnavailableError,
    at: string
  ): Promise<void> {
    const existingCount = this.#sessions.listAgentTraceEvents(session.key, 10_000).length;
    await this.#sessions.upsertAgentTraceEvent({
      id: randomUUID(),
      sessionKey: session.key,
      source: "broker",
      type: "agent_runtime_error",
      at,
      sequence: existingCount + 1,
      title: "Auth 不可用",
      summary: `等待人工切换账号：${error.userMessage}`,
      detail: JSON.stringify({
        profileName: error.profileName ?? null,
        reason: error.reason
      }, null, 2),
      status: "blocked",
      metadata: {
        profileName: error.profileName ?? null,
        reason: error.reason
      },
      createdAt: at,
      updatedAt: at
    });
  }
}
