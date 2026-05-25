import fs from "node:fs/promises";
import path from "node:path";

import { logger } from "../../logger.js";
import type {
  AgentInputItem,
  AgentRuntime,
  AgentTurnResult
} from "../agent-runtime/types.js";
import type {
  PersistedAgentTurnStatus,
  PersistedAgentTurnUsage,
  SlackBatchInputMessage,
  SlackImageAttachment,
  SlackInputMessage,
  SlackSessionRecord
} from "../../types.js";
import { SessionManager } from "../session-manager.js";
import { SlackApi } from "./slack-api.js";
import { formatSlackMessageForAgent } from "./slack-message-format.js";
import { SlackInboundStore } from "./slack-inbound-store.js";
import {
  isMissingAgentSessionError,
  isRecoverableAgentTurnFailure
} from "./slack-conversation-utils.js";

export class SlackTurnRunner {
  readonly #agentRuntime: AgentRuntime;
  readonly #slackApi: SlackApi;
  readonly #sessions: SessionManager;
  readonly #inboundStore: SlackInboundStore;

  constructor(options: {
    readonly agentRuntime: AgentRuntime;
    readonly slackApi: SlackApi;
    readonly sessions: SessionManager;
    readonly inboundStore: SlackInboundStore;
  }) {
    this.#agentRuntime = options.agentRuntime;
    this.#slackApi = options.slackApi;
    this.#sessions = options.sessions;
    this.#inboundStore = options.inboundStore;
  }

  async submitAdditionalInput(session: SlackSessionRecord, item: SlackInputMessage): Promise<void> {
    const input = await this.#buildImmediateSlackInput(session, item);
    const result = await this.#agentRuntime.submitInput({
      session,
      input,
      inputId: inputIdForSessionInput(session, item.messageTs ?? "active", "additional"),
      source: agentInputSourceForSlackInput(item)
    });
    if (result.receipt.delivery !== "joined_active_turn") {
      throw new Error(`Expected active input delivery for ${session.key}, got ${result.receipt.delivery}`);
    }
  }

  async buildTurnInput(message: SlackInputMessage): Promise<readonly AgentInputItem[]>;
  async buildTurnInput(session: SlackSessionRecord, message: SlackInputMessage): Promise<readonly AgentInputItem[]>;
  async buildTurnInput(
    sessionOrMessage: SlackSessionRecord | SlackInputMessage,
    maybeMessage?: SlackInputMessage
  ): Promise<readonly AgentInputItem[]> {
    const message = maybeMessage ?? sessionOrMessage as SlackInputMessage;
    const session = maybeMessage
      ? sessionOrMessage as SlackSessionRecord
      : this.#sessions.getSession(message.channelId, message.rootThreadTs);
    const enrichedMessage = session
      ? await this.#prepareSlackInput(session, message)
      : await this.#enrichMentionedUsers(message);
    const sender = enrichedMessage.source !== "background_job_event" && enrichedMessage.source !== "recovered_thread_batch" && enrichedMessage.senderKind === "user"
      ? await this.#slackApi.getUserIdentity(enrichedMessage.userId)
      : null;
    const inputText = formatSlackMessageForAgent(enrichedMessage, sender);
    return [createTextInputItem(inputText)];
  }

  async submitInputWithRecovery(options: {
    readonly session: SlackSessionRecord;
    readonly sessionKey: string;
    readonly senderUserId: string;
    readonly input: readonly AgentInputItem[];
    readonly messageTsList: readonly string[];
  }): Promise<{
    readonly session: SlackSessionRecord;
    readonly result: AgentTurnResult;
  }> {
    let session = options.session;

    for (let attempt = 0; attempt < 2; attempt += 1) {
      const submitted = await this.#agentRuntime.submitInput({
        session,
        input: options.input,
        inputId: inputIdForSessionInput(session, options.messageTsList.join(","), `turn-${attempt + 1}`),
        source: "slack_user"
      });
      if (submitted.receipt.delivery !== "started_turn" || !submitted.completion) {
        throw new Error(`Expected new turn delivery for queued input in ${session.key}, got ${submitted.receipt.delivery}`);
      }

      logger.debug("Agent turn started", {
        sessionKey: options.sessionKey,
        turnId: submitted.receipt.turnId,
        senderUserId: options.senderUserId,
        attempt: attempt + 1
      });
      session = await this.#sessions.setActiveTurnId(
        session.channelId,
        session.rootThreadTs,
        submitted.receipt.turnId
      );
      await this.#inboundStore.markMessagesInflightByTs(session, options.messageTsList, submitted.receipt.turnId);

      try {
        const result = await submitted.completion;
        logger.debug("Agent turn completed", {
          sessionKey: options.sessionKey,
          turnId: result.turnId,
          aborted: result.aborted,
          attempt: attempt + 1
        });
        session = await this.#finalizeCompletedTurnIfStillActive(session, submitted.receipt.turnId, result);
        return {
          session,
          result
        };
      } catch (error) {
        const recovered = await this.#recoverTurnResult(session, submitted.receipt.turnId);

        if (recovered) {
          logger.warn("Recovered agent turn result from runtime snapshot after disconnect", {
            sessionKey: options.sessionKey,
            senderUserId: options.senderUserId,
            turnId: submitted.receipt.turnId,
            recoveredStatus: recovered.aborted ? "interrupted" : "completed"
          });
          session = await this.#finalizeCompletedTurnIfStillActive(session, submitted.receipt.turnId, recovered);
          return {
            session,
            result: recovered
          };
        }

        const shouldStop = attempt === 1 || !isRecoverableAgentTurnFailure(error);
        if (shouldStop) {
          await this.#persistMissingTurnUsage(session, submitted.receipt.turnId, "failed");
        }

        await this.#inboundStore.resetTurnBatchToPending(session, submitted.receipt.turnId);
        session = await this.#sessions.clearActiveTurnIdIfMatches(
          session.channelId,
          session.rootThreadTs,
          submitted.receipt.turnId
        );

        if (shouldStop) {
          throw error;
        }

        logger.warn("Agent turn lost during runtime disconnect; retrying once", {
          sessionKey: options.sessionKey,
          senderUserId: options.senderUserId,
          error: error instanceof Error ? error.message : String(error)
        });
        session = await this.#ensureAgentSessionInternal(session);
      }
    }

    throw new Error("Agent turn retry exhausted unexpectedly");
  }

  async readTurnSnapshot(
    session: SlackSessionRecord,
    turnId: string,
    options?: {
      readonly syncActiveTurn?: boolean | undefined;
      readonly treatMissingAsStale?: boolean | undefined;
    }
  ) {
    return await this.#agentRuntime.readTurn(session, turnId, options);
  }

  async #finalizeCompletedTurnIfStillActive(
    session: SlackSessionRecord,
    turnId: string,
    result: AgentTurnResult
  ): Promise<SlackSessionRecord> {
    const latestSession = this.#sessions.getSession(session.channelId, session.rootThreadTs) ?? session;
    await this.#persistTurnUsage(latestSession, result);

    if (latestSession.activeTurnId !== turnId) {
      logger.warn("Skipping stale agent turn completion finalization because broker active turn changed", {
        sessionKey: session.key,
        turnId,
        currentActiveTurnId: latestSession.activeTurnId ?? null
      });
      return latestSession;
    }

    let finalizedSession = await this.#inboundStore.markTurnBatchDone(latestSession, turnId);
    finalizedSession = await this.#sessions.clearActiveTurnIdIfMatches(
      finalizedSession.channelId,
      finalizedSession.rootThreadTs,
      turnId
    );
    return finalizedSession;
  }

  async ensureAgentSession(session: SlackSessionRecord): Promise<SlackSessionRecord> {
    return await this.#ensureAgentSessionInternal(session);
  }

  async interrupt(session: SlackSessionRecord): Promise<void> {
    await this.#agentRuntime.interrupt(session);
  }

  async #ensureAgentSessionInternal(session: SlackSessionRecord): Promise<SlackSessionRecord> {
    if (session.agentSessionId) {
      try {
        await this.#agentRuntime.ensureSession(session);
        return session;
      } catch (error) {
        if (!isMissingAgentSessionError(error)) {
          throw error;
        }

        logger.warn("Stored agent session id no longer exists; resetting broker session runtime state", {
          sessionKey: session.key,
          agentSessionId: session.agentSessionId,
          error: error instanceof Error ? error.message : String(error)
        });

        session = await this.#sessions.setActiveTurnId(session.channelId, session.rootThreadTs, undefined);
        session = await this.#sessions.setAgentSessionId(session.channelId, session.rootThreadTs, undefined);
      }
    }

    const agentSession = await this.#agentRuntime.ensureSession(session);
    return await this.#sessions.setAgentSessionId(session.channelId, session.rootThreadTs, agentSession.id);
  }

  async #buildImmediateSlackInput(session: SlackSessionRecord, message: SlackInputMessage): Promise<readonly AgentInputItem[]> {
    const enrichedItem = await this.#prepareSlackInput(session, message);
    const sender = enrichedItem.source !== "background_job_event" && enrichedItem.source !== "recovered_thread_batch" && enrichedItem.senderKind === "user"
      ? await this.#slackApi.getUserIdentity(enrichedItem.userId)
      : null;
    const formattedMessage = formatSlackMessageForAgent(enrichedItem, sender);
    return [
      createTextInputItem([
        enrichedItem.recoveryKind === "missed_thread_messages"
          ? "The broker detected Slack thread messages that were not previously delivered into the active turn."
          : "A newer Slack message arrived while the current turn is still active.",
        enrichedItem.recoveryKind === "missed_thread_messages"
          ? "Review the recovered batch, merge it into the current context, and decide whether you need to adjust the ongoing work or reply now."
          : "Treat it as the latest instruction and adjust the ongoing work accordingly.",
        "",
        formattedMessage
      ].join("\n"))
    ];
  }

  async #prepareSlackInput(session: SlackSessionRecord, message: SlackInputMessage): Promise<SlackInputMessage> {
    const enrichedMessage = await this.#enrichMentionedUsers(message);
    const batchMessages = enrichedMessage.batchMessages
      ? await Promise.all(
        enrichedMessage.batchMessages.map((entry) => this.#materializeSlackAttachments(session, entry))
      )
      : undefined;
    const messageWithAttachments = await this.#materializeSlackAttachments(session, enrichedMessage);

    return {
      ...messageWithAttachments,
      batchMessages
    };
  }

  async #materializeSlackAttachments<T extends SlackInputMessage | SlackBatchInputMessage>(
    session: SlackSessionRecord,
    message: T
  ): Promise<T> {
    if (!message.images || message.images.length === 0) {
      return message;
    }

    const images = await Promise.all(
      message.images.map((attachment) => this.#downloadSlackAttachment(session, message, attachment))
    );

    return {
      ...message,
      images
    };
  }

  async #downloadSlackAttachment(
    session: SlackSessionRecord,
    message: Pick<SlackInputMessage | SlackBatchInputMessage, "messageTs" | "source">,
    attachment: SlackImageAttachment
  ): Promise<SlackImageAttachment> {
    if (attachment.localPath || attachment.downloadError) {
      return attachment;
    }

    try {
      const downloaded = await this.#slackApi.downloadFileAttachment(attachment);
      const localPath = await writeSlackAttachmentFile({
        workspacePath: session.workspacePath,
        messageTs: message.messageTs,
        attachment,
        bytes: downloaded.bytes
      });

      return {
        ...attachment,
        mimetype: downloaded.contentType || attachment.mimetype,
        localPath
      };
    } catch (error) {
      const downloadError = error instanceof Error ? error.message : String(error);
      logger.warn("Failed to download Slack attachment into session workspace", {
        source: message.source,
        channelId: session.channelId,
        rootThreadTs: session.rootThreadTs,
        messageTs: message.messageTs,
        fileId: attachment.fileId,
        error: downloadError
      });

      return {
        ...attachment,
        downloadError
      };
    }
  }

  async #enrichMentionedUsers(message: SlackInputMessage): Promise<SlackInputMessage> {
    if ((message.mentionedUsers?.length ?? 0) > 0 || !message.mentionedUserIds || message.mentionedUserIds.length === 0) {
      return message;
    }

    const mentionedUsers = (
      await Promise.all(message.mentionedUserIds.map((userId) => this.#slackApi.getUserIdentity(userId)))
    ).filter((user): user is NonNullable<typeof user> => user !== null);

    if (mentionedUsers.length === 0) {
      return message;
    }

    return {
      ...message,
      mentionedUsers
    };
  }

  async #recoverTurnResult(
    session: SlackSessionRecord,
    turnId: string
  ): Promise<AgentTurnResult | null> {
    try {
      const snapshot = await this.#agentRuntime.readTurn(session, turnId, {
        syncActiveTurn: true
      });

      if (!snapshot) {
        return null;
      }

      if (snapshot.status === "completed") {
        return {
          agentSessionId: session.agentSessionId ?? "",
          turnId,
          finalMessage: snapshot.finalMessage,
          aborted: false,
          generatedImages: snapshot.generatedImages,
          usage: snapshot.usage
        };
      }

      if (snapshot.status === "interrupted") {
        return {
          agentSessionId: session.agentSessionId ?? "",
          turnId,
          finalMessage: snapshot.finalMessage,
          aborted: true,
          generatedImages: snapshot.generatedImages,
          usage: snapshot.usage
        };
      }

      if (snapshot.status === "failed") {
        throw new Error(snapshot.errorMessage ?? "Agent turn failed");
      }

      return null;
    } catch (error) {
      logger.warn("Failed to recover agent turn result from runtime snapshot", {
        sessionKey: session.key,
        turnId,
        error: error instanceof Error ? error.message : String(error)
      });
      return null;
    }
  }

  async #persistTurnUsage(session: SlackSessionRecord, result: AgentTurnResult): Promise<void> {
    const status: PersistedAgentTurnStatus = result.aborted ? "interrupted" : "completed";
    const usage = result.usage;
    await this.#upsertTurnUsage({
      turnId: result.turnId,
      sessionKey: session.key,
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      agentSessionId: result.agentSessionId || session.agentSessionId,
      status,
      source: usage?.source ?? "missing",
      model: usage?.model,
      effort: usage?.effort,
      inputTokens: usage?.inputTokens ?? 0,
      cachedInputTokens: usage?.cachedInputTokens ?? 0,
      outputTokens: usage?.outputTokens ?? 0,
      reasoningTokens: usage?.reasoningTokens ?? 0,
      totalTokens: usage?.totalTokens ?? 0,
      rawUsage: usage?.rawUsage,
      startedAt: session.activeTurnId === result.turnId ? session.activeTurnStartedAt : undefined
    });
  }

  async #persistMissingTurnUsage(
    session: SlackSessionRecord,
    turnId: string,
    status: PersistedAgentTurnStatus
  ): Promise<void> {
    await this.#upsertTurnUsage({
      turnId,
      sessionKey: session.key,
      channelId: session.channelId,
      rootThreadTs: session.rootThreadTs,
      agentSessionId: session.agentSessionId,
      status,
      source: "missing",
      inputTokens: 0,
      cachedInputTokens: 0,
      outputTokens: 0,
      reasoningTokens: 0,
      totalTokens: 0,
      startedAt: session.activeTurnId === turnId ? session.activeTurnStartedAt : undefined
    });
  }

  async #upsertTurnUsage(record: Omit<PersistedAgentTurnUsage, "createdAt" | "updatedAt" | "completedAt">): Promise<void> {
    const now = new Date().toISOString();
    await this.#sessions.upsertAgentTurnUsage({
      ...record,
      completedAt: now,
      createdAt: record.startedAt ?? now,
      updatedAt: now
    });
  }
}

function createTextInputItem(text: string): AgentInputItem {
  return {
    type: "text",
    text,
    text_elements: []
  };
}

const SLACK_ATTACHMENTS_DIRECTORY = ".slack-attachments";

async function writeSlackAttachmentFile(options: {
  readonly workspacePath: string;
  readonly messageTs?: string | undefined;
  readonly attachment: SlackImageAttachment;
  readonly bytes: Buffer;
}): Promise<string> {
  const workspaceRoot = path.resolve(options.workspacePath);
  const messageDirectory = sanitizePathSegment(options.messageTs ?? "unknown-message");
  const directoryPath = path.join(workspaceRoot, SLACK_ATTACHMENTS_DIRECTORY, messageDirectory);
  const fileName = buildAttachmentFileName(options.attachment);
  const filePath = path.join(directoryPath, fileName);
  const resolvedFilePath = path.resolve(filePath);

  if (!resolvedFilePath.startsWith(`${workspaceRoot}${path.sep}`)) {
    throw new Error(`Refusing to write Slack attachment outside session workspace: ${resolvedFilePath}`);
  }

  await fs.mkdir(directoryPath, { recursive: true });
  await fs.writeFile(resolvedFilePath, options.bytes);
  return resolvedFilePath;
}

function buildAttachmentFileName(attachment: SlackImageAttachment): string {
  const rawName = attachment.name ?? attachment.title ?? attachment.fileId;
  const safeBaseName = sanitizePathSegment(path.basename(rawName)) || "attachment";
  const safeFileId = sanitizePathSegment(attachment.fileId) || "slack-file";
  const prefixedName = safeBaseName.startsWith(`${safeFileId}-`)
    ? safeBaseName
    : `${safeFileId}-${safeBaseName}`;
  return truncateFileName(prefixedName, 180);
}

function sanitizePathSegment(value: string): string {
  return value
    .replaceAll(/[\\/]/g, "_")
    .replaceAll(/[^A-Za-z0-9._-]+/g, "_")
    .replaceAll(/^\.{1,2}$/g, "_")
    .replaceAll(/^\.+/g, "_")
    .replaceAll(/_+/g, "_")
    .slice(0, 180);
}

function truncateFileName(fileName: string, maxLength: number): string {
  if (fileName.length <= maxLength) {
    return fileName;
  }

  const extension = path.extname(fileName);
  const baseName = path.basename(fileName, extension);
  const allowedBaseLength = Math.max(1, maxLength - extension.length);
  return `${baseName.slice(0, allowedBaseLength)}${extension}`;
}

function inputIdForSessionInput(session: SlackSessionRecord, scope: string, kind: string): string {
  return `${session.key}:${kind}:${scope || Date.now()}`;
}

function agentInputSourceForSlackInput(message: SlackInputMessage): "slack_user" | "broker_recovery" | "background_job" {
  if (message.source === "background_job_event") {
    return "background_job";
  }
  if (message.source === "recovered_thread_batch" || message.source === "admin_session_reset" || message.recoveryKind) {
    return "broker_recovery";
  }
  return "slack_user";
}
