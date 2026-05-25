import http from "node:http";
import { URL } from "node:url";

import type { AppConfig } from "../config.js";
import { logger } from "../logger.js";
import type { SlackAgentBridge } from "../services/slack/slack-agent-bridge.js";
import {
  readBoolean,
  readJsonBody,
  readFormBody,
  readString,
  respondJson
} from "./common.js";

const LEGACY_COAUTHOR_MAPPING_ERROR = "Manual co-author mappings are no longer supported. Bind GitHub OAuth for Slack users instead.";

export async function handleSlackRequest(
  method: string,
  url: URL,
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
    readonly config: AppConfig;
  }
): Promise<boolean> {
  if (method === "GET" && url.pathname === "/slack/thread-history") {
    await handleSlackThreadHistoryRequest(url, response, options);
    return true;
  }

  if (method === "GET" && url.pathname === "/slack/replay-thread-message") {
    await handleSlackReplayThreadMessageRequest(url, response, options);
    return true;
  }

  const matchedResumeSession = matchResumeSessionPath(url.pathname);
  if (method === "POST" && matchedResumeSession) {
    await handleSlackResumePendingSessionRequest(response, options, matchedResumeSession.sessionKey);
    return true;
  }

  const matchedRepairActiveTurn = matchRepairActiveTurnPath(url.pathname);
  if (method === "POST" && matchedRepairActiveTurn) {
    await handleSlackRepairActiveTurnRequest(response, options, matchedRepairActiveTurn.sessionKey);
    return true;
  }

  const matchedResetSession = matchResetSessionPath(url.pathname);
  if (method === "POST" && matchedResetSession) {
    await handleSlackResetSessionRequest(response, options, matchedResetSession.sessionKey);
    return true;
  }

  const matchedDeleteSession = matchDeleteSessionPath(url.pathname);
  if (method === "DELETE" && matchedDeleteSession) {
    await handleSlackDeleteSessionRequest(response, options, matchedDeleteSession.sessionKey);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/post-message") {
    await handleSlackPostMessageRequest(request, response, options);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/post-state") {
    await handleSlackPostStateRequest(request, response, options);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/post-file") {
    await handleSlackPostFileRequest(request, response, options);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/git-coauthors/resolve-commit-message") {
    await handleResolveCommitCoauthorsRequest(request, response, options);
    return true;
  }

  if (method === "GET" && url.pathname === "/slack/git-coauthors/session-status") {
    await handleGetCommitCoauthorStatusRequest(url, response, options);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/git-coauthors/configure-session") {
    await handleConfigureCommitCoauthorsRequest(request, response, options);
    return true;
  }

  if (method === "POST" && url.pathname === "/slack/github-token/resolve") {
    await handleResolveGitHubTokenRequest(request, response, options);
    return true;
  }

  return false;
}

async function handleSlackResumePendingSessionRequest(
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  },
  sessionKey: string
): Promise<void> {
  try {
    const resumedCount = await options.bridge.resumePendingSession(sessionKey);
    respondJson(response, 200, {
      ok: true,
      sessionKey,
      resumedCount
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackRepairActiveTurnRequest(
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  },
  sessionKey: string
): Promise<void> {
  try {
    const repair = await options.bridge.repairActiveTurn(sessionKey);
    respondJson(response, 200, {
      ok: true,
      sessionKey,
      repair
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackResetSessionRequest(
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  },
  sessionKey: string
): Promise<void> {
  try {
    const reset = await options.bridge.resetSession(sessionKey);
    respondJson(response, 200, {
      ok: true,
      sessionKey,
      reset
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackDeleteSessionRequest(
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  },
  sessionKey: string
): Promise<void> {
  try {
    const deleted = await options.bridge.deleteSession(sessionKey);
    respondJson(response, 200, {
      ok: true,
      sessionKey,
      delete: deleted
    });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    respondJson(response, message.includes("Unknown session") ? 404 : 500, {
      ok: false,
      error: message
    });
  }
}

async function handleSlackThreadHistoryRequest(
  url: URL,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
    readonly config: AppConfig;
  }
): Promise<void> {
  const channelId = url.searchParams.get("channel_id");
  const rootThreadTs = url.searchParams.get("thread_ts");

  if (!channelId || !rootThreadTs) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_query",
      required: ["channel_id", "thread_ts"]
    });
    return;
  }

  const limitParam = url.searchParams.get("limit");
  const parsedLimit = limitParam == null ? undefined : Number(limitParam);

  if (limitParam != null && !Number.isFinite(parsedLimit)) {
    respondJson(response, 400, { ok: false, error: "invalid_limit" });
    return;
  }

  try {
    const result = await options.bridge.readThreadHistory({
      channelId,
      rootThreadTs,
      beforeMessageTs: url.searchParams.get("before_ts") ?? undefined,
      channelType: url.searchParams.get("channel_type") ?? undefined,
      limit: parsedLimit
    });
    const responseFormat = url.searchParams.get("format") ?? "json";

    if (responseFormat === "text") {
      response.writeHead(200, { "content-type": "text/plain; charset=utf-8" });
      response.end(result.formattedText ?? "No earlier Slack thread history matched the request.");
      return;
    }

    respondJson(response, 200, {
      ok: true,
      channelId,
      rootThreadTs,
      beforeMessageTs: url.searchParams.get("before_ts") ?? undefined,
      returnedCount: result.messages.length,
      hasMore: result.hasMore,
      maxLimit: options.config.slackHistoryApiMaxLimit,
      formattedText: result.formattedText,
      messages: result.messages
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackReplayThreadMessageRequest(
  url: URL,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  const channelId = url.searchParams.get("channel_id");
  const rootThreadTs = url.searchParams.get("thread_ts");
  const messageTs = url.searchParams.get("message_ts");

  if (!channelId || !rootThreadTs || !messageTs) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_query",
      required: ["channel_id", "thread_ts", "message_ts"]
    });
    return;
  }

  try {
    const replayed = await options.bridge.replayThreadMessage({
      channelId,
      rootThreadTs,
      messageTs
    });

    if (!replayed) {
      respondJson(response, 404, {
        ok: false,
        error: "message_not_replayed"
      });
      return;
    }

    respondJson(response, 200, {
      ok: true,
      replayed
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackPostMessageRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  let body: Record<string, string>;

  try {
    body = await readFormBody(request);
  } catch (error) {
    respondJson(response, 400, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
    return;
  }
  logger.raw("http-requests", {
    method: "POST",
    path: "/slack/post-message",
    body
  }, {
    channelId: body.channel_id,
    rootThreadTs: body.thread_ts
  });

  const channelId = body.channel_id;
  const rootThreadTs = body.thread_ts;
  const text = body.text?.trim();
  const kind = body.kind?.trim();
  const reason = body.reason?.trim() || body.stop_reason?.trim() || undefined;

  if (!channelId || !rootThreadTs || !text) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_body",
      required: ["channel_id", "thread_ts", "text"]
    });
    return;
  }

  if (kind && !["progress", "final", "block", "wait"].includes(kind)) {
    respondJson(response, 400, {
      ok: false,
      error: "invalid_kind",
      allowed: ["progress", "final", "block", "wait"]
    });
    return;
  }

  if ((kind === "block" || kind === "wait") && !reason) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_reason",
      required: ["reason"]
    });
    return;
  }

  try {
    await options.bridge.postSlackMessage({
      channelId,
      rootThreadTs,
      text,
      kind: kind as "progress" | "final" | "block" | "wait" | undefined,
      reason
    });
    respondJson(response, 200, { ok: true });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackPostStateRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  let body: Record<string, string>;

  try {
    body = await readFormBody(request);
  } catch (error) {
    respondJson(response, 400, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
    return;
  }

  logger.raw("http-requests", {
    method: "POST",
    path: "/slack/post-state",
    body
  }, {
    channelId: body.channel_id,
    rootThreadTs: body.thread_ts
  });

  const channelId = body.channel_id;
  const rootThreadTs = body.thread_ts;
  const kind = body.kind?.trim();
  const reason = body.reason?.trim() || body.stop_reason?.trim() || undefined;

  if (!channelId || !rootThreadTs || !kind) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_body",
      required: ["channel_id", "thread_ts", "kind"]
    });
    return;
  }

  if (kind !== "wait" && kind !== "block" && kind !== "final") {
    respondJson(response, 400, {
      ok: false,
      error: "invalid_kind",
      allowed: ["wait", "block", "final"]
    });
    return;
  }

  if ((kind === "wait" || kind === "block") && !reason) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_reason",
      required: ["reason"]
    });
    return;
  }

  try {
    await options.bridge.postSlackState({
      channelId,
      rootThreadTs,
      kind: kind as "wait" | "block" | "final",
      reason
    });
    respondJson(response, 200, { ok: true });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleSlackPostFileRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  let body: Record<string, string>;

  try {
    body = await readFormBody(request);
  } catch (error) {
    respondJson(response, 400, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
    return;
  }
  logger.raw("http-requests", {
    method: "POST",
    path: "/slack/post-file",
    body: {
      ...body,
      content_base64: body.content_base64 ? `[base64:${body.content_base64.length}]` : undefined
    }
  }, {
    channelId: body.channel_id,
    rootThreadTs: body.thread_ts
  });

  const channelId = body.channel_id;
  const rootThreadTs = body.thread_ts;
  const filePath = body.file_path?.trim() || undefined;
  const contentBase64 = body.content_base64?.trim() || undefined;
  const filename = body.filename?.trim() || undefined;
  const initialComment = (body.initial_comment ?? body.text)?.trim() || undefined;

  if (!channelId || !rootThreadTs || (!filePath && !contentBase64)) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_body",
      required: ["channel_id", "thread_ts", "file_path|content_base64"]
    });
    return;
  }

  try {
    const uploaded = await options.bridge.postSlackFile({
      channelId,
      rootThreadTs,
      filePath,
      contentBase64,
      filename,
      title: body.title?.trim() || undefined,
      initialComment,
      altText: body.alt_text?.trim() || undefined,
      snippetType: body.snippet_type?.trim() || undefined,
      contentType: body.content_type?.trim() || undefined
    });
    respondJson(response, 200, {
      ok: true,
      file: uploaded
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleResolveCommitCoauthorsRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  try {
    const body = await readJsonBody(request);
    const cwd = typeof body.cwd === "string" ? body.cwd.trim() : "";
    const commitMessage = typeof body.commit_message === "string" ? body.commit_message : "";
    const primaryAuthorEmail =
      typeof body.primary_author_email === "string" ? body.primary_author_email.trim() : undefined;

    if (!cwd || !commitMessage) {
      respondJson(response, 400, {
        ok: false,
        error: "missing_required_body",
        required: ["cwd", "commit_message"]
      });
      return;
    }

    const result = await options.bridge.resolveCommitCoauthors({
      cwd,
      commitMessage,
      primaryAuthorEmail
    });

    respondJson(response, 200, {
      ok: true,
      ...result
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleResolveGitHubTokenRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  try {
    const body = await readJsonBody(request);
    const cwd = readString(body.cwd);
    const command = normalizeStringArray(body.command) ?? [];
    if (!cwd) {
      respondJson(response, 400, {
        ok: false,
        error: "missing_required_body",
        required: ["cwd"]
      });
      return;
    }

    const result = await options.bridge.resolveGitHubPrToken({
      cwd,
      command
    });
    respondJson(response, result.ok ? 200 : 409, result);
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleGetCommitCoauthorStatusRequest(
  url: URL,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  const cwd = url.searchParams.get("cwd")?.trim();
  if (!cwd) {
    respondJson(response, 400, {
      ok: false,
      error: "missing_required_query",
      required: ["cwd"]
    });
    return;
  }

  try {
    const status = await options.bridge.getCommitCoauthorStatus(cwd);
    if (!status) {
      respondJson(response, 404, {
        ok: false,
        error: "session_not_found"
      });
      return;
    }

    respondJson(response, 200, {
      ok: true,
      status
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

async function handleConfigureCommitCoauthorsRequest(
  request: http.IncomingMessage,
  response: http.ServerResponse,
  options: {
    readonly bridge: SlackAgentBridge;
  }
): Promise<void> {
  try {
    const body = await readJsonBody(request);
    const cwd = readString(body.cwd);
    if (!cwd) {
      respondJson(response, 400, {
        ok: false,
        error: "missing_required_body",
        required: ["cwd"]
      });
      return;
    }

    const coauthors = normalizeStringArray(body.coauthors);
    const userIds = normalizeStringArray(body.user_ids);
    const mappings = normalizeMappings(body.mappings);
    if (mappings) {
      respondJson(response, 400, {
        ok: false,
        error: LEGACY_COAUTHOR_MAPPING_ERROR
      });
      return;
    }
    const ignoreMissing =
      Object.prototype.hasOwnProperty.call(body, "ignore_missing") ||
      Object.prototype.hasOwnProperty.call(body, "ignoreMissing")
        ? readBoolean(body.ignore_missing ?? body.ignoreMissing, false)
        : undefined;
    const status = await options.bridge.configureSessionCoauthors({
      cwd,
      coauthors,
      userIds,
      ignoreMissing,
      mappings
    });

    if (!status) {
      respondJson(response, 404, {
        ok: false,
        error: "session_not_found"
      });
      return;
    }

    respondJson(response, 200, {
      ok: true,
      status
    });
  } catch (error) {
    respondJson(response, 500, {
      ok: false,
      error: error instanceof Error ? error.message : String(error)
    });
  }
}

function normalizeStringArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }

  const normalized = value
    .map((entry) => (typeof entry === "string" ? entry.trim() : ""))
    .filter(Boolean);
  return normalized.length > 0 ? normalized : undefined;
}

function matchResumeSessionPath(pathname: string): { readonly sessionKey: string } | null {
  const prefix = "/slack/sessions/";
  const suffix = "/resume-pending";
  if (!pathname.startsWith(prefix) || !pathname.endsWith(suffix)) {
    return null;
  }

  const encodedKey = pathname.slice(prefix.length, -suffix.length);
  if (!encodedKey) {
    return null;
  }

  return {
    sessionKey: decodeURIComponent(encodedKey)
  };
}

function matchRepairActiveTurnPath(pathname: string): { readonly sessionKey: string } | null {
  const prefix = "/slack/sessions/";
  const suffix = "/repair-active-turn";
  if (!pathname.startsWith(prefix) || !pathname.endsWith(suffix)) {
    return null;
  }

  const encodedKey = pathname.slice(prefix.length, -suffix.length);
  if (!encodedKey) {
    return null;
  }

  return {
    sessionKey: decodeURIComponent(encodedKey)
  };
}

function matchResetSessionPath(pathname: string): { readonly sessionKey: string } | null {
  const prefix = "/slack/sessions/";
  const suffix = "/reset";
  if (!pathname.startsWith(prefix) || !pathname.endsWith(suffix)) {
    return null;
  }

  const encodedKey = pathname.slice(prefix.length, -suffix.length);
  if (!encodedKey) {
    return null;
  }

  return {
    sessionKey: decodeURIComponent(encodedKey)
  };
}

function matchDeleteSessionPath(pathname: string): { readonly sessionKey: string } | null {
  const prefix = "/slack/sessions/";
  if (!pathname.startsWith(prefix)) {
    return null;
  }

  const encodedKey = pathname.slice(prefix.length);
  if (!encodedKey || encodedKey.includes("/")) {
    return null;
  }

  return {
    sessionKey: decodeURIComponent(encodedKey)
  };
}

function normalizeMappings(value: unknown): readonly unknown[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }

  const entries = value.filter((entry) => Boolean(entry));
  return entries.length > 0 ? entries : undefined;
}
