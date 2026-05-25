import type { AppConfig } from "../../config.js";
import { logger } from "../../logger.js";
import type {
  BackgroundJobEventPayload,
  JsonLike,
  PersistedInboundMessage,
  ResolvedSlackThreadMessage,
  SlackSessionRecord,
  SlackUserIdentity
} from "../../types.js";
import type { AgentRuntime } from "../agent-runtime/types.js";
import type {
  GitHubPrIdentityService,
  GitHubPrTokenResolution
} from "../github-pr-identity-service.js";
import { SessionManager } from "../session-manager.js";
import type { SessionChannelMetadata } from "../session-manager.js";
import {
  type ParsedSlackEvent,
  isSlackMessageEffectivelyEmpty,
  parseSlackEvent
} from "./slack-event-parser.js";
import { SlackApi } from "./slack-api.js";
import { SlackCoauthorService } from "./slack-coauthor-service.js";
import { SlackConversationService } from "./slack-conversation-service.js";
import { SlackSelfMessageFilter } from "./slack-self-filter.js";
import { SlackSocketModeClient } from "./socket-mode-client.js";

export class SlackAgentBridge {
  readonly #config: AppConfig;
  readonly #sessions: SessionManager;
  readonly #agentRuntime: AgentRuntime;
  readonly #slackApi: SlackApi;
  readonly #slackSocket: SlackSocketModeClient;
  readonly #selfMessageFilter = new SlackSelfMessageFilter();
  readonly #coauthors: SlackCoauthorService;
  readonly #githubPrIdentity: GitHubPrIdentityService;
  readonly #conversations: SlackConversationService;
  #botUserId = "";
  #botIdentity: SlackUserIdentity | null = null;
  #slackEventDrainPromise: Promise<void> | undefined;
  #slackEventDrainTimer: NodeJS.Timeout | undefined;
  #slackEventRetryTimer: NodeJS.Timeout | undefined;

  constructor(options: {
    readonly config: AppConfig;
    readonly sessions: SessionManager;
    readonly agentRuntime: AgentRuntime;
    readonly githubPrIdentity: GitHubPrIdentityService;
  }) {
    this.#config = options.config;
    this.#sessions = options.sessions;
    this.#agentRuntime = options.agentRuntime;
    this.#slackApi = new SlackApi({
      baseUrl: this.#config.slackApiBaseUrl,
      appToken: this.#config.slackAppToken,
      botToken: this.#config.slackBotToken
    });
    this.#slackSocket = new SlackSocketModeClient({
      api: this.#slackApi,
      socketOpenPath: this.#config.slackSocketOpenUrl
    });
    this.#coauthors = new SlackCoauthorService({
      sessions: this.#sessions,
      slackApi: this.#slackApi,
      githubPrIdentity: options.githubPrIdentity
    });
    this.#githubPrIdentity = options.githubPrIdentity;
    this.#conversations = new SlackConversationService({
      config: this.#config,
      sessions: this.#sessions,
      agentRuntime: this.#agentRuntime,
      slackApi: this.#slackApi,
      selfMessageFilter: this.#selfMessageFilter,
      coauthors: this.#coauthors,
      githubPrIdentity: this.#githubPrIdentity
    });
  }

  async start(): Promise<void> {
    await this.#agentRuntime.start();

    const auth = await this.#slackApi.authTest();
    this.#botUserId = auth.userId;
    this.#selfMessageFilter.setIdentity(auth);
    this.#conversations.setBotUserId(auth.userId);

    this.#botIdentity = await this.#slackApi.getUserIdentity(this.#botUserId);
    this.#agentRuntime.setSlackBotIdentity(this.#botIdentity);

    await this.#backfillSessionChannelMetadata("startup");
    await this.#backfillInboundMentionedUsers("startup");
    await this.#conversations.start();
    await this.#drainPersistedSlackEvents("startup");

    this.#slackSocket.on("ready", () => {
      void this.#conversations.recoverMissedThreadMessages("socket_ready");
    });
    this.#slackSocket.on("events_api", (payload) =>
      this.#acceptEventsApi(payload as {
        readonly event?: Record<string, any>;
        readonly event_id?: string;
      })
    );
    this.#slackSocket.on("interactive", (payload) =>
      this.#handleInteractive(payload as Record<string, unknown>)
    );

    await this.#slackSocket.start();
  }

  async stop(): Promise<void> {
    this.#clearSlackEventDrainTimer();
    this.#clearSlackEventRetryTimer();
    await this.#slackSocket.stop();
    await this.#conversations.stop();
    await this.#agentRuntime.stop();
  }

  async readThreadHistory(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly beforeMessageTs?: string | undefined;
    readonly limit?: number | undefined;
    readonly channelType?: string | undefined;
  }): Promise<{
    readonly messages: readonly ResolvedSlackThreadMessage[];
    readonly formattedText?: string | undefined;
    readonly hasMore: boolean;
  }> {
    return await this.#conversations.readThreadHistory(options);
  }

  async replayThreadMessage(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly messageTs: string;
  }) {
    return await this.#conversations.replayThreadMessage(options);
  }

  async resumePendingSession(sessionKey: string): Promise<number> {
    return await this.#conversations.resumePendingSession(sessionKey);
  }

  async repairActiveTurn(sessionKey: string) {
    return await this.#conversations.repairActiveTurn(sessionKey);
  }

  async resetSession(sessionKey: string) {
    return await this.#conversations.resetSession(sessionKey);
  }

  async deleteSession(sessionKey: string) {
    return await this.#conversations.deleteSession(sessionKey);
  }

  async acceptBackgroundJobEvent(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly payload: BackgroundJobEventPayload;
  }): Promise<void> {
    await this.#conversations.acceptBackgroundJobEvent(options);
  }

  async postSlackMessage(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly text: string;
    readonly kind?: "progress" | "final" | "block" | "wait" | undefined;
    readonly reason?: string | undefined;
  }): Promise<void> {
    await this.#conversations.postSlackMessage(options);
  }

  async postSlackState(options: {
    readonly channelId: string;
    readonly rootThreadTs: string;
    readonly kind: "wait" | "block" | "final";
    readonly reason?: string | undefined;
  }): Promise<void> {
    await this.#conversations.postSlackState(options);
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
  }) {
    return await this.#conversations.postSlackFile(options);
  }

  async getCommitCoauthorStatus(cwd: string) {
    return await this.#coauthors.getCommitCoauthorStatus(cwd);
  }

  async configureSessionCoauthors(options: {
    readonly cwd: string;
    readonly coauthors?: readonly string[] | undefined;
    readonly userIds?: readonly string[] | undefined;
    readonly ignoreMissing?: boolean | undefined;
    readonly mappings?: ReadonlyArray<unknown> | undefined;
  }) {
    return await this.#coauthors.configureSessionCoauthors(options);
  }

  async resolveCommitCoauthors(options: {
    readonly cwd: string;
    readonly commitMessage: string;
    readonly primaryAuthorEmail?: string | undefined;
  }) {
    return await this.#coauthors.resolveCommitCoauthors(options);
  }

  async resolveGitHubPrToken(options: {
    readonly cwd: string;
    readonly command: readonly string[];
  }): Promise<GitHubPrTokenResolution> {
    const session = this.#sessions.findSessionByWorkspace(options.cwd);
    if (!session) {
      return {
        ok: false,
        mode: "blocked",
        reason: "session_not_found",
        message: `No Slack session is associated with ${options.cwd}.`
      };
    }

    return await this.#githubPrIdentity.resolveTokenForSession({
      session,
      command: options.command
    });
  }

  async #acceptEventsApi(payload: {
    readonly event?: Record<string, any>;
    readonly event_id?: string;
  }): Promise<void> {
    if (!payload.event || !payload.event_id) {
      return;
    }

    if (this.#sessions.hasProcessedEvent(payload.event_id)) {
      return;
    }

    await this.#sessions.enqueueSlackEvent(payload.event_id, payload as JsonLike);
    this.#scheduleSlackEventDrain("socket_event");
  }

  #scheduleSlackEventDrain(reason: "socket_event" | "retry"): void {
    if (this.#slackEventDrainTimer) {
      return;
    }
    this.#slackEventDrainTimer = setTimeout(() => {
      this.#slackEventDrainTimer = undefined;
      void this.#drainPersistedSlackEvents(reason);
    }, 0);
    this.#slackEventDrainTimer.unref();
  }

  #clearSlackEventDrainTimer(): void {
    if (!this.#slackEventDrainTimer) {
      return;
    }
    clearTimeout(this.#slackEventDrainTimer);
    this.#slackEventDrainTimer = undefined;
  }

  #scheduleSlackEventRetry(): void {
    if (this.#slackEventRetryTimer) {
      return;
    }
    this.#slackEventRetryTimer = setTimeout(() => {
      this.#slackEventRetryTimer = undefined;
      this.#scheduleSlackEventDrain("retry");
    }, 5_000);
    this.#slackEventRetryTimer.unref();
  }

  #clearSlackEventRetryTimer(): void {
    if (!this.#slackEventRetryTimer) {
      return;
    }
    clearTimeout(this.#slackEventRetryTimer);
    this.#slackEventRetryTimer = undefined;
  }

  async #drainPersistedSlackEvents(reason: "startup" | "socket_event" | "retry"): Promise<void> {
    if (this.#slackEventDrainPromise) {
      await this.#slackEventDrainPromise;
      return;
    }

    this.#slackEventDrainPromise = this.#runSlackEventDrain(reason)
      .catch((error) => {
        logger.error("Failed to drain persisted Slack event queue", {
          reason,
          error: error instanceof Error ? error.message : String(error)
        });
        this.#scheduleSlackEventRetry();
      })
      .finally(() => {
        this.#slackEventDrainPromise = undefined;
      });

    await this.#slackEventDrainPromise;
  }

  async #runSlackEventDrain(reason: "startup" | "socket_event" | "retry"): Promise<void> {
    let failedCount = 0;
    let processedCount = 0;

    while (true) {
      const pendingEvents = this.#sessions.listPendingSlackEvents();
      if (pendingEvents.length === 0) {
        break;
      }

      let batchFailedCount = 0;
      for (const queuedEvent of pendingEvents) {
        const payload = queuedEvent.payload as {
          readonly event?: Record<string, any>;
          readonly event_id?: string;
        };

        if (!payload.event || payload.event_id !== queuedEvent.eventId) {
          await this.#sessions.markSlackEventProcessed(queuedEvent.eventId);
          continue;
        }

        if (this.#sessions.hasProcessedEvent(queuedEvent.eventId)) {
          await this.#sessions.markSlackEventProcessed(queuedEvent.eventId);
          continue;
        }

        try {
          await this.#routeSlackEvent(payload.event);
          await this.#sessions.markSlackEventProcessed(queuedEvent.eventId);
          processedCount += 1;
        } catch (error) {
          failedCount += 1;
          batchFailedCount += 1;
          logger.error("Failed to process persisted Slack event", {
            reason,
            eventId: queuedEvent.eventId,
            error: error instanceof Error ? error.message : String(error)
          });
        }
      }

      if (batchFailedCount > 0) {
        break;
      }
    }

    if (processedCount > 0 || failedCount > 0) {
      logger.info("Drained persisted Slack event queue", {
        reason,
        processedCount,
        failedCount
      });
    }

    if (failedCount > 0) {
      this.#scheduleSlackEventRetry();
    }
  }

  async #handleInteractive(payload: Record<string, unknown>): Promise<void> {
    try {
      await this.#coauthors.handleInteractivePayload(payload);
    } catch (error) {
      logger.error("Failed to process Slack interactive payload", {
        error: error instanceof Error ? error.message : String(error)
      });
    }
  }

  async #routeSlackEvent(event: Record<string, any>): Promise<void> {
    if (this.#selfMessageFilter.shouldIgnoreEvent(event)) {
      return;
    }

    const parsed = parseSlackEvent(event, this.#botUserId);
    if (!parsed) {
      return;
    }

    const channelMetadata = await this.#resolveChannelMetadata(parsed);

    switch (parsed.route) {
      case "app_mention":
        await this.#handleInteractiveSessionEvent(parsed, {
          createSession: true,
          preloadHistory: parsed.rootThreadTs !== parsed.messageTs,
          channelMetadata
        });
        return;
      case "direct_message":
        if (parsed.controlText === "-stop" && (parsed.input.images?.length ?? 0) === 0) {
          const existing = await this.#getSessionWithChannelMetadata(parsed, channelMetadata);
          if (existing) {
            await this.#handleStop(existing);
          }
          return;
        }

        await this.#handleInteractiveSessionEvent(parsed, {
          createSession: true,
          preloadHistory: false,
          channelMetadata
        });
        return;
      case "thread_reply": {
        const session = await this.#getSessionWithChannelMetadata(parsed, channelMetadata);
        if (!session) {
          return;
        }

        if (this.#conversations.isAlreadyHandled(session, parsed.messageTs)) {
          return;
        }

        if (parsed.controlText === "-stop" && (parsed.input.images?.length ?? 0) === 0) {
          await this.#handleStop(session);
          return;
        }

        if (isSlackMessageEffectivelyEmpty(parsed.input.text, parsed.input.images, parsed.input.slackMessage)) {
          return;
        }

        await this.#conversations.acceptInboundMessage(session, parsed.input);
        return;
      }
      default:
        return;
    }
  }

  async #getSessionWithChannelMetadata(
    parsed: ParsedSlackEvent,
    metadata: SessionChannelMetadata
  ): Promise<SlackSessionRecord | undefined> {
    const session = this.#sessions.getSession(parsed.channelId, parsed.rootThreadTs);
    if (!session) {
      return undefined;
    }

    return await this.#sessions.setChannelMetadata(parsed.channelId, parsed.rootThreadTs, metadata);
  }

  async #resolveChannelMetadata(parsed: ParsedSlackEvent): Promise<SessionChannelMetadata> {
    const fallback: SessionChannelMetadata = {
      channelType: parsed.channelType
    };
    const info = await this.#slackApi.getConversationInfo(parsed.channelId);
    if (!info) {
      return fallback;
    }

    return {
      channelName: info.name,
      channelType: parsed.channelType ?? info.channelType
    };
  }

  async #backfillSessionChannelMetadata(reason: string): Promise<void> {
    const sessionsByChannel = new Map<string, SlackSessionRecord[]>();
    for (const session of this.#sessions.listSessions()) {
      if (session.channelName && session.channelType) {
        continue;
      }

      const sessions = sessionsByChannel.get(session.channelId) ?? [];
      sessions.push(session);
      sessionsByChannel.set(session.channelId, sessions);
    }

    if (!sessionsByChannel.size) {
      return;
    }

    let updatedCount = 0;
    for (const [channelId, sessions] of sessionsByChannel.entries()) {
      const info = await this.#slackApi.getConversationInfo(channelId);
      if (!info) {
        continue;
      }

      for (const session of sessions) {
        await this.#sessions.setChannelMetadata(session.channelId, session.rootThreadTs, {
          channelName: info.name,
          channelType: info.channelType
        });
        updatedCount += 1;
      }
    }

    if (updatedCount) {
      logger.info("Backfilled Slack session channel metadata", {
        reason,
        updatedCount,
        channelCount: sessionsByChannel.size
      });
    }
  }

  async #backfillInboundMentionedUsers(reason: string): Promise<void> {
    const candidates = this.#sessions.listInboundMessages({
      source: ["app_mention", "direct_message", "thread_reply"],
      needsMentionUserBackfill: true
    });

    if (!candidates.length) {
      return;
    }

    let updatedCount = 0;
    for (const message of candidates) {
      const mentionedUsers = await this.#resolveMentionedUsers(message);
      if (!mentionedUsers.length) {
        continue;
      }

      await this.#sessions.upsertInboundMessage({
        ...message,
        mentionedUsers,
        updatedAt: new Date().toISOString()
      });
      updatedCount += 1;
    }

    if (updatedCount) {
      logger.info("Backfilled Slack inbound mention identities", {
        reason,
        updatedCount
      });
    }
  }

  async #resolveMentionedUsers(message: PersistedInboundMessage): Promise<readonly SlackUserIdentity[]> {
    const mentionedUserIds = message.mentionedUserIds ?? [];
    if (!mentionedUserIds.length) {
      return [];
    }

    const knownUsers = new Map(
      (message.mentionedUsers ?? []).map((user) => [user.userId, user])
    );

    for (const userId of mentionedUserIds) {
      if (knownUsers.has(userId)) {
        continue;
      }

      const identity = await this.#slackApi.getUserIdentity(userId);
      if (identity) {
        knownUsers.set(userId, identity);
      }
    }

    return mentionedUserIds
      .map((userId) => knownUsers.get(userId))
      .filter((user): user is SlackUserIdentity => Boolean(user));
  }

  async #handleInteractiveSessionEvent(
    parsed: ParsedSlackEvent,
    options: {
      readonly createSession: boolean;
      readonly preloadHistory: boolean;
      readonly channelMetadata: SessionChannelMetadata;
    }
  ): Promise<void> {
    const existing = this.#sessions.getSession(parsed.channelId, parsed.rootThreadTs);
    let session = options.createSession
      ? await this.#sessions.ensureSession(parsed.channelId, parsed.rootThreadTs, {
          ...options.channelMetadata,
          ...(parsed.input.senderKind === "user"
            ? {
                initiatorUserId: parsed.input.userId,
                initiatorMessageTs: parsed.messageTs
              }
            : {})
        })
      : existing;

    if (!session) {
      return;
    }

    if (!options.createSession) {
      session = await this.#sessions.setChannelMetadata(parsed.channelId, parsed.rootThreadTs, options.channelMetadata);
    }

    if (this.#conversations.isAlreadyHandled(session, parsed.messageTs)) {
      return;
    }

    session = await this.#conversations.ensureAgentSession(session);

    if (isSlackMessageEffectivelyEmpty(parsed.input.text, parsed.input.images, parsed.input.slackMessage)) {
      return;
    }

    const history = !existing && options.preloadHistory && parsed.messageTs
      ? await this.#conversations.readThreadHistory({
        channelId: parsed.channelId,
        channelType: parsed.channelType,
        rootThreadTs: parsed.rootThreadTs,
        beforeMessageTs: parsed.messageTs,
        limit: this.#config.slackInitialThreadHistoryCount
      })
      : undefined;

    await this.#conversations.acceptInboundMessage(session, {
      ...parsed.input,
      contextText: history?.formattedText
    });
  }

  async #handleStop(session: SlackSessionRecord): Promise<void> {
    const stopped = await this.#conversations.stopActiveTurn(session);
    await this.#conversations.postSlackMessage({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      text: stopped ? "Stopped the current run." : "No active run to stop."
    });
  }
}
