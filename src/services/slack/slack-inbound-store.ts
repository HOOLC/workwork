import type {
  PersistedInboundMessage,
  PersistedInboundSource,
  ResolvedSlackThreadMessage,
  SlackBatchInputMessage,
  SlackInputMessage,
  SlackSessionRecord,
  SlackThreadMessage
} from "../../types.js";
import { SessionManager } from "../session-manager.js";
import { SlackApi, normalizeSlackJson } from "./slack-api.js";
import {
  isSlackMessageEffectivelyEmpty,
  parseSlackTextMetadata
} from "./slack-event-parser.js";
import {
  compareSlackTs,
  createInboundMessageKey,
  isSlackInboundSource
} from "./slack-conversation-utils.js";

export class SlackInboundStore {
  readonly #sessions: SessionManager;
  readonly #slackApi: SlackApi;

  constructor(options: {
    readonly sessions: SessionManager;
    readonly slackApi: SlackApi;
  }) {
    this.#sessions = options.sessions;
    this.#slackApi = options.slackApi;
  }

  isAlreadyHandled(session: SlackSessionRecord, messageTs?: string | undefined): boolean {
    if (!messageTs) {
      return false;
    }

    return Boolean(
      this.#sessions.getInboundMessage(session.channelId, session.rootThreadTs, messageTs)
    );
  }

  listPendingMessages(session: SlackSessionRecord, options?: {
    readonly source?: PersistedInboundSource | readonly PersistedInboundSource[] | undefined;
  }): PersistedInboundMessage[] {
    return this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: "pending",
      source: options?.source
    });
  }

  async recordInboundMessage(
    session: SlackSessionRecord,
    item: SlackInputMessage
  ): Promise<SlackSessionRecord> {
    if (!item.messageTs) {
      return session;
    }

    if (item.source === "recovered_thread_batch") {
      throw new Error("Synthetic recovered Slack batches must not be persisted as inbound messages");
    }

    const existing = this.#sessions.getInboundMessage(session.channelId, session.rootThreadTs, item.messageTs);
    if (!existing) {
      const enrichedItem = await this.#enrichMentionedUsers(item);
      const now = new Date().toISOString();
      await this.#sessions.upsertInboundMessage({
        key: createInboundMessageKey(session.key, item.messageTs),
        sessionKey: session.key,
        channelId: session.channelId,
        channelType: enrichedItem.channelType,
        rootThreadTs: session.rootThreadTs,
        messageTs: item.messageTs,
        source: item.source,
        userId: enrichedItem.userId,
        text: enrichedItem.text,
        senderKind: enrichedItem.senderKind,
        botId: enrichedItem.botId,
        appId: enrichedItem.appId,
        senderUsername: enrichedItem.senderUsername,
        mentionedUserIds: enrichedItem.mentionedUserIds ?? [],
        mentionedUsers: enrichedItem.mentionedUsers ?? [],
        contextText: enrichedItem.contextText,
        images: enrichedItem.images ?? [],
        slackMessage: enrichedItem.slackMessage,
        backgroundJob: enrichedItem.backgroundJob,
        unexpectedTurnStop: enrichedItem.unexpectedTurnStop,
        status: "pending",
        createdAt: now,
        updatedAt: now
      });
    }

    if (
      isSlackInboundSource(item.source) &&
      (!session.lastObservedMessageTs || compareSlackTs(item.messageTs, session.lastObservedMessageTs) > 0)
    ) {
      return await this.#sessions.setLastObservedMessageTs(
        session.channelId,
        session.rootThreadTs,
        item.messageTs
      );
    }

    return session;
  }

  createSlackInputFromPersistedMessage(message: PersistedInboundMessage): SlackInputMessage {
    return {
      source: message.source,
      channelId: message.channelId,
      channelType: message.channelType,
      rootThreadTs: message.rootThreadTs,
      messageTs: message.messageTs,
      userId: message.userId,
      text: message.text,
      senderKind: message.senderKind,
      botId: message.botId,
      appId: message.appId,
      senderUsername: message.senderUsername,
      mentionedUserIds: message.mentionedUserIds,
      mentionedUsers: message.mentionedUsers,
      contextText: message.contextText,
      images: message.images,
      slackMessage: message.slackMessage,
      backgroundJob: message.backgroundJob,
      unexpectedTurnStop: message.unexpectedTurnStop
    };
  }

  async #enrichMentionedUsers(item: SlackInputMessage): Promise<SlackInputMessage> {
    if ((item.mentionedUsers?.length ?? 0) > 0 || !item.mentionedUserIds || item.mentionedUserIds.length === 0) {
      return item;
    }

    const mentionedUsers = (
      await Promise.all(item.mentionedUserIds.map((userId) => this.#slackApi.getUserIdentity(userId)))
    ).filter((user): user is NonNullable<typeof user> => user !== null);

    if (mentionedUsers.length === 0) {
      return item;
    }

    return {
      ...item,
      mentionedUsers
    };
  }

  async markMessagesInflight(
    session: SlackSessionRecord,
    messages: readonly PersistedInboundMessage[],
    turnId: string
  ): Promise<void> {
    await this.markMessagesInflightByTs(
      session,
      messages.map((message) => message.messageTs),
      turnId
    );
  }

  async markMessagesInflightByTs(
    session: SlackSessionRecord,
    messageTsList: readonly string[],
    turnId: string
  ): Promise<void> {
    if (messageTsList.length === 0) {
      return;
    }

    await this.#sessions.updateInboundMessagesForBatch(
      session.channelId,
      session.rootThreadTs,
      messageTsList,
      {
        status: "inflight",
        batchId: turnId
      }
    );
  }

  async markTurnBatchDone(session: SlackSessionRecord, turnId: string): Promise<SlackSessionRecord> {
    const inflightMessages = this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: "inflight",
      batchId: turnId
    });

    if (inflightMessages.length === 0) {
      return session;
    }

    await this.#sessions.updateInboundMessagesForBatch(
      session.channelId,
      session.rootThreadTs,
      inflightMessages.map((message) => message.messageTs),
      {
        status: "done",
        batchId: undefined
      }
    );

    const deliveredSlackMessages = inflightMessages.filter((message) => isSlackInboundSource(message.source));
    const lastDeliveredMessageTs = deliveredSlackMessages.at(-1)?.messageTs;
    if (!lastDeliveredMessageTs) {
      return session;
    }

    if (
      !session.lastDeliveredMessageTs ||
      compareSlackTs(lastDeliveredMessageTs, session.lastDeliveredMessageTs) > 0
    ) {
      return await this.#sessions.setLastDeliveredMessageTs(
        session.channelId,
        session.rootThreadTs,
        lastDeliveredMessageTs
      );
    }

    return session;
  }

  async resetTurnBatchToPending(session: SlackSessionRecord, turnId: string): Promise<number> {
    const resetMessages = await this.#sessions.resetInflightMessages(session.channelId, session.rootThreadTs, turnId);
    return resetMessages.length;
  }

  async reconcileOrphanedInflightMessages(session: SlackSessionRecord): Promise<{
    readonly markedDoneCount: number;
    readonly resetToPendingCount: number;
  }> {
    if (session.activeTurnId) {
      return {
        markedDoneCount: 0,
        resetToPendingCount: 0
      };
    }

    const inflightMessages = this.#sessions.listInboundMessages({
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      status: "inflight"
    });

    if (inflightMessages.length === 0) {
      return {
        markedDoneCount: 0,
        resetToPendingCount: 0
      };
    }

    const batches = new Map<string, PersistedInboundMessage[]>();
    for (const message of inflightMessages) {
      const key = message.batchId ?? `message:${message.messageTs}`;
      const existing = batches.get(key);
      if (existing) {
        existing.push(message);
        continue;
      }
      batches.set(key, [message]);
    }

    const markDoneTs: string[] = [];
    const resetToPendingTs: string[] = [];

    for (const batchMessages of batches.values()) {
      const latestSlackMessageTs = batchMessages
        .filter((message) => isSlackInboundSource(message.source))
        .map((message) => message.messageTs)
        .sort(compareSlackTs)
        .at(-1);

      const shouldMarkDone = Boolean(
        latestSlackMessageTs &&
        session.lastDeliveredMessageTs &&
        compareSlackTs(latestSlackMessageTs, session.lastDeliveredMessageTs) <= 0
      );

      const target = shouldMarkDone ? markDoneTs : resetToPendingTs;
      target.push(...batchMessages.map((message) => message.messageTs));
    }

    if (markDoneTs.length > 0) {
      await this.#sessions.updateInboundMessagesForBatch(
        session.channelId,
        session.rootThreadTs,
        markDoneTs,
        {
          status: "done",
          batchId: undefined
        }
      );
    }

    if (resetToPendingTs.length > 0) {
      await this.#sessions.updateInboundMessagesForBatch(
        session.channelId,
        session.rootThreadTs,
        resetToPendingTs,
        {
          status: "pending",
          batchId: undefined
        }
      );
    }

    return {
      markedDoneCount: markDoneTs.length,
      resetToPendingCount: resetToPendingTs.length
    };
  }

  async createRecoveredBatchInput(
    session: SlackSessionRecord,
    messages: readonly PersistedInboundMessage[] | readonly ResolvedSlackThreadMessage[] | readonly SlackThreadMessage[],
    recoveryKind: "missed_thread_messages"
  ): Promise<SlackInputMessage | null> {
    const recoveredMessages = (
      await Promise.all(
        messages.map(async (message): Promise<SlackBatchInputMessage | null> => {
          const source = "source" in message ? message.source : "thread_reply";
          const metadata = parseSlackTextMetadata(message.text);
          const images = message.images ?? [];
          const slackMessage = "slackMessage" in message ? message.slackMessage : normalizeSlackJson(message);

          if (isSlackMessageEffectivelyEmpty(metadata.text, images, slackMessage)) {
            return null;
          }

          return {
            messageTs: message.messageTs,
            source,
            userId: message.userId,
            text: metadata.text,
            senderKind: "senderKind" in message ? message.senderKind : undefined,
            botId: "botId" in message ? message.botId : undefined,
            appId: "appId" in message ? message.appId : undefined,
            senderUsername: "senderUsername" in message ? message.senderUsername : undefined,
            mentionedUserIds: metadata.mentionedUserIds,
            mentionedUsers: (
              await Promise.all(
                metadata.mentionedUserIds.map((userId) => this.#slackApi.getUserIdentity(userId))
              )
            ).filter((user): user is NonNullable<typeof user> => user !== null),
            images,
            slackMessage,
            backgroundJob: "backgroundJob" in message ? message.backgroundJob : undefined,
            sender: "senderKind" in message && message.senderKind !== "user"
              ? null
              : await this.#slackApi.getUserIdentity(message.userId)
          };
        })
      )
    ).filter((message): message is NonNullable<typeof message> => message !== null);

    if (recoveredMessages.length === 0) {
      return null;
    }

    return {
      source: "recovered_thread_batch",
      channelId: session.channelId,
      channelType: messages.at(-1)?.channelType,
      rootThreadTs: session.rootThreadTs,
      messageTs: recoveredMessages.at(-1)?.messageTs,
      userId: recoveredMessages.at(-1)?.userId ?? "",
      text: "",
      recoveryKind,
      batchMessages: recoveredMessages
    };
  }
}
