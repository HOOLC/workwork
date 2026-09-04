import http from "node:http";
import { once } from "node:events";

import { WebSocketServer, type WebSocket } from "ws";

export interface PostedMessage {
  readonly channel: string;
  readonly threadTs: string;
  readonly text: string;
  readonly ts: string;
}

export interface AssistantStatusUpdate {
  readonly channel: string;
  readonly threadTs: string;
  readonly status: string;
  readonly atMs: number;
  readonly loadingMessages?: string | undefined;
}

export interface ReactionOperation {
  readonly action: "add" | "remove";
  readonly channel: string;
  readonly timestamp: string;
  readonly name: string;
}

export interface UploadedThreadFile {
  readonly fileId: string;
  readonly channelId: string;
  readonly threadTs: string;
  readonly filename: string;
  readonly title?: string | undefined;
  readonly initialComment?: string | undefined;
  readonly bytes: Buffer;
}

export interface EphemeralPost {
  readonly channel: string;
  readonly user: string;
  readonly threadTs?: string | undefined;
  readonly text: string;
  readonly blocks?: string | undefined;
}

export interface OpenedView {
  readonly triggerId: string;
  readonly view: Record<string, unknown>;
}

export interface MockThreadMessage {
  readonly channel: string;
  readonly threadTs: string;
  readonly ts: string;
  readonly text: string;
  readonly user?: string | undefined;
  readonly subtype?: string | undefined;
  readonly bot_id?: string | undefined;
  readonly app_id?: string | undefined;
  readonly username?: string | undefined;
  readonly files?: readonly Record<string, unknown>[] | undefined;
  readonly blocks?: readonly Record<string, unknown>[] | undefined;
  readonly attachments?: readonly Record<string, unknown>[] | undefined;
}

export interface MockConversation {
  readonly id: string;
  readonly name?: string | undefined;
  readonly is_im?: boolean | undefined;
  readonly is_mpim?: boolean | undefined;
  readonly is_group?: boolean | undefined;
  readonly is_channel?: boolean | undefined;
}

export class MockSlackServer {
  readonly #server: http.Server;
  readonly #wsServer: WebSocketServer;
  #socket: WebSocket | undefined;
  #nextTs = 9_000_000_000;
  readonly postedMessages: PostedMessage[] = [];
  readonly assistantStatusUpdates: AssistantStatusUpdate[] = [];
  readonly reactionOperations: ReactionOperation[] = [];
  readonly #activeReactions = new Set<string>();
  readonly #channels = new Map<string, MockConversation>();
  readonly #users = new Map<
    string,
    {
      readonly id: string;
      readonly name: string;
      readonly real_name: string;
      readonly profile: {
        readonly display_name: string;
        readonly real_name: string;
        readonly email?: string | undefined;
      };
    }
  >([
    [
      "U123",
      {
        id: "U123",
        name: "mock-user-123",
        real_name: "Mock User 123",
        profile: {
          display_name: "Mock Display 123",
          real_name: "Mock User 123",
          email: "alice@example.com",
        },
      },
    ],
    [
      "U234",
      {
        id: "U234",
        name: "mock-user-234",
        real_name: "Mock User 234",
        profile: {
          display_name: "Mock Display 234",
          real_name: "Mock User 234",
          email: "bob@example.com",
        },
      },
    ],
    [
      "UBOT",
      {
        id: "UBOT",
        name: "mock-bot",
        real_name: "Mock Bot",
        profile: {
          display_name: "Mock Bot",
          real_name: "Mock Bot",
        },
      },
    ],
  ]);
  readonly #threadMessages = new Map<string, MockThreadMessage[]>();
  readonly #downloadableFiles = new Map<string, { readonly bytes: Buffer; readonly contentType: string }>();
  readonly #pendingUploads = new Map<string, { readonly filename: string; bytes?: Buffer }>();
  #port = 0;
  #nextFileSeq = 1;
  conversationsRepliesCalls = 0;
  conversationsRepliesFailure:
    | {
        readonly status: number;
        readonly retryAfterSec?: number | undefined;
      }
    | undefined;
  readonly uploadedFiles: UploadedThreadFile[] = [];
  readonly ephemeralPosts: EphemeralPost[] = [];
  readonly openedViews: OpenedView[] = [];
  readonly acknowledgedEnvelopeIds: string[] = [];
  socketConnectionCount = 0;

  constructor(
    private readonly botUserId: string,
    private readonly options?: {
      readonly botId?: string;
      readonly appId?: string;
      readonly assistantStatusError?: string;
      readonly channels?: readonly MockConversation[] | undefined;
    },
  ) {
    for (const channel of options?.channels ?? []) {
      this.#channels.set(channel.id, channel);
    }

    this.#server = http.createServer((request, response) => {
      void this.#handleHttp(request, response);
    });
    this.#wsServer = new WebSocketServer({ noServer: true });

    this.#server.on("upgrade", (request, socket, head) => {
      if (request.url !== "/socket") {
        socket.destroy();
        return;
      }

      this.#wsServer.handleUpgrade(request, socket, head, (websocket) => {
        this.socketConnectionCount += 1;
        this.#socket = websocket;
        websocket.on("message", (data) => {
          const text = data.toString();
          process.stdout.write(`[mock-slack] client->server ${text}\n`);
          try {
            const payload = JSON.parse(text) as { envelope_id?: unknown };
            if (typeof payload.envelope_id === "string") {
              this.acknowledgedEnvelopeIds.push(payload.envelope_id);
            }
          } catch {
            // Non-JSON client frames are irrelevant to Socket Mode acknowledgements.
          }
        });
        websocket.on("close", () => {
          if (this.#socket === websocket) {
            this.#socket = undefined;
          }
        });
        process.stdout.write("[mock-slack] websocket connected\n");
        this.#wsServer.emit("connection", websocket, request);
        websocket.send(JSON.stringify({ type: "hello" }));
      });
    });
  }

  async start(): Promise<number> {
    this.#server.listen(0, "127.0.0.1");
    await once(this.#server, "listening");
    const address = this.#server.address();
    if (!address || typeof address === "string") {
      throw new Error("Mock Slack server did not bind to a TCP port");
    }

    this.#port = address.port;
    return address.port;
  }

  get port(): number {
    return this.#port;
  }

  async stop(): Promise<void> {
    this.#socket?.close();
    this.#wsServer.close();
    await new Promise<void>((resolve, reject) => {
      this.#server.close((error) => {
        if (error) {
          reject(error);
          return;
        }

        resolve();
      });
    });
  }

  async waitForSocket(): Promise<void> {
    if (this.#socket && this.#socket.readyState === 1) {
      return;
    }

    await once(this.#wsServer, "connection");
  }

  async sendEvent(eventId: string, event: Record<string, unknown>): Promise<void> {
    this.#recordThreadEvent(event);
    await this.waitForSocket();
    const envelope = {
      envelope_id: `env-${eventId}`,
      type: "events_api",
      payload: {
        event_id: eventId,
        event,
      },
    };

    process.stdout.write(`[mock-slack] server->client event ${eventId}\n`);
    await this.#sendSocketJson(envelope);
  }

  addDownloadableFile(fileId: string, bytes: Buffer, contentType = "application/octet-stream"): string {
    this.#downloadableFiles.set(fileId, { bytes, contentType });
    return `http://127.0.0.1:${this.#port}/files/${encodeURIComponent(fileId)}`;
  }

  async waitForPostedMessage(predicate: (message: PostedMessage) => boolean, timeoutMs = 30_000): Promise<PostedMessage> {
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
      const match = this.postedMessages.find(predicate);
      if (match) {
        return match;
      }

      await new Promise((resolve) => setTimeout(resolve, 200));
    }

    throw new Error("Timed out waiting for Slack message");
  }

  recordThreadMessage(message: MockThreadMessage): void {
    const key = getThreadKey(message.channel, message.threadTs);
    const messages = this.#threadMessages.get(key) ?? [];
    messages.push(message);
    messages.sort((left, right) => Number(left.ts) - Number(right.ts));
    this.#threadMessages.set(key, messages);
  }

  async #sendSocketJson(payload: Record<string, unknown>): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.#socket?.send(JSON.stringify(payload), (error) => {
        if (error) {
          reject(error);
          return;
        }

        resolve();
      });
    });
  }

  async #handleHttp(request: http.IncomingMessage, response: http.ServerResponse): Promise<void> {
    const url = request.url ?? "";

    if (request.method === "GET" && url.startsWith("/files/")) {
      const fileId = decodeURIComponent(url.slice("/files/".length).split("?")[0] ?? "");
      const file = this.#downloadableFiles.get(fileId);
      if (!file) {
        response.writeHead(404).end();
        return;
      }

      response.writeHead(200, { "content-type": file.contentType });
      response.end(file.bytes);
      return;
    }

    if (request.method === "POST" && url.startsWith("/upload/")) {
      const fileId = decodeURIComponent(url.slice("/upload/".length).split("?")[0] ?? "");
      const bytes = await readRawBody(request);
      const pending = this.#pendingUploads.get(fileId);
      if (pending) {
        this.#pendingUploads.set(fileId, { ...pending, bytes });
      }
      response.writeHead(200).end("ok");
      return;
    }

    if (request.method !== "POST") {
      response.writeHead(405).end();
      return;
    }

    const body = await readRequestBody(request);

    if (request.url === "/api/apps.connections.open") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          url: `ws://127.0.0.1:${(this.#server.address() as { port: number }).port}/socket`,
        }),
      );
      return;
    }

    if (request.url === "/api/auth.test") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          user_id: this.botUserId,
          bot_id: this.options?.botId,
          app_id: this.options?.appId,
        }),
      );
      return;
    }

    if (request.url === "/api/chat.postMessage") {
      const ts = `${this.#nextTs++}.000000`;
      this.postedMessages.push({
        channel: String(body.channel),
        threadTs: String(body.thread_ts),
        text: String(body.text),
        ts,
      });
      this.recordThreadMessage({
        channel: String(body.channel),
        threadTs: String(body.thread_ts),
        ts,
        text: String(body.text),
        user: this.botUserId,
        bot_id: this.options?.botId,
        app_id: this.options?.appId,
      });
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          ts,
        }),
      );
      return;
    }

    if (request.url === "/api/assistant.threads.setStatus") {
      if (this.options?.assistantStatusError) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            ok: false,
            error: this.options.assistantStatusError,
          }),
        );
        return;
      }

      this.assistantStatusUpdates.push({
        channel: String(body.channel_id),
        threadTs: String(body.thread_ts),
        status: String(body.status ?? ""),
        atMs: Date.now(),
        loadingMessages: typeof body.loading_messages === "string" && body.loading_messages.length > 0 ? body.loading_messages : undefined,
      });

      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }

    if (request.url === "/api/reactions.add") {
      const reactionKey = getReactionKey(String(body.channel), String(body.timestamp), String(body.name));
      this.reactionOperations.push({
        action: "add",
        channel: String(body.channel),
        timestamp: String(body.timestamp),
        name: String(body.name),
      });
      this.#activeReactions.add(reactionKey);

      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }

    if (request.url === "/api/reactions.remove") {
      const reactionKey = getReactionKey(String(body.channel), String(body.timestamp), String(body.name));
      this.reactionOperations.push({
        action: "remove",
        channel: String(body.channel),
        timestamp: String(body.timestamp),
        name: String(body.name),
      });
      this.#activeReactions.delete(reactionKey);

      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }

    if (request.url === "/api/users.info") {
      const user = this.#users.get(String(body.user));

      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          user,
        }),
      );
      return;
    }

    if (request.url === "/api/conversations.info") {
      const channelId = String(body.channel);
      const channel = this.#channels.get(channelId) ?? inferMockConversation(channelId);

      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          channel,
        }),
      );
      return;
    }

    if (request.url === "/api/conversations.replies") {
      this.conversationsRepliesCalls += 1;
      if (this.conversationsRepliesFailure) {
        const failure = this.conversationsRepliesFailure;
        const headers: Record<string, string> = { "content-type": "application/json" };
        if (failure.retryAfterSec != null) {
          headers["retry-after"] = String(failure.retryAfterSec);
        }
        response.writeHead(failure.status, headers);
        response.end(JSON.stringify({ ok: false, error: "ratelimited" }));
        return;
      }

      const channel = String(body.channel);
      const threadTs = String(body.ts);
      const messages = this.#threadMessages.get(getThreadKey(channel, threadTs)) ?? [];

      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          messages,
        }),
      );
      return;
    }

    if (request.url === "/api/files.getUploadURLExternal") {
      const fileId = `FUPLOAD${this.#nextFileSeq++}`;
      const filename = String(body.filename ?? "upload.bin");
      this.#pendingUploads.set(fileId, { filename });
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          upload_url: `http://127.0.0.1:${this.#port}/upload/${encodeURIComponent(fileId)}`,
          file_id: fileId,
        }),
      );
      return;
    }

    if (request.url === "/api/files.completeUploadExternal") {
      const filesJson = typeof body.files === "string" ? body.files : "[]";
      const parsedFiles = JSON.parse(filesJson) as Array<{ id?: string; title?: string }>;
      const first = parsedFiles[0];
      const fileId = String(first?.id ?? "");
      const pending = this.#pendingUploads.get(fileId);
      const uploaded: UploadedThreadFile = {
        fileId,
        channelId: String(body.channel_id ?? ""),
        threadTs: String(body.thread_ts ?? ""),
        filename: pending?.filename ?? "upload.bin",
        title: first?.title,
        initialComment: typeof body.initial_comment === "string" ? body.initial_comment : undefined,
        bytes: pending?.bytes ?? Buffer.alloc(0),
      };
      this.uploadedFiles.push(uploaded);
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          files: [
            {
              id: fileId,
              title: uploaded.title ?? uploaded.filename,
              name: uploaded.filename,
              mimetype: "application/octet-stream",
              permalink: `https://slack.test/files/${fileId}`,
              url_private: `https://slack.test/private/${fileId}`,
              url_private_download: `https://slack.test/private/${fileId}/download`,
              size: uploaded.bytes.byteLength,
            },
          ],
        }),
      );
      return;
    }

    if (request.url === "/api/chat.postEphemeral") {
      this.ephemeralPosts.push({
        channel: String(body.channel ?? ""),
        user: String(body.user ?? ""),
        threadTs: typeof body.thread_ts === "string" ? body.thread_ts : undefined,
        text: String(body.text ?? ""),
        blocks: typeof body.blocks === "string" ? body.blocks : undefined,
      });
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true, message_ts: `${this.#nextTs++}.000000` }));
      return;
    }

    if (request.url === "/api/views.open") {
      const rawView = typeof body.view === "string" ? body.view : "{}";
      this.openedViews.push({
        triggerId: String(body.trigger_id ?? ""),
        view: JSON.parse(rawView) as Record<string, unknown>,
      });
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }

    if (request.url === "/api/chat.getPermalink") {
      const channel = String(body.channel ?? "");
      const messageTs = String(body.message_ts ?? "");
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ok: true,
          permalink: `https://workspace.slack.com/archives/${channel}/p${messageTs.replace(".", "")}`,
        }),
      );
      return;
    }

    response.writeHead(404).end();
  }

  #recordThreadEvent(event: Record<string, unknown>): void {
    if (event.type !== "message" && event.type !== "app_mention") {
      return;
    }

    const channel = typeof event.channel === "string" ? event.channel : undefined;
    const ts = typeof event.ts === "string" ? event.ts : undefined;

    if (!channel || !ts) {
      return;
    }

    const threadTs = typeof event.thread_ts === "string" ? event.thread_ts : ts;
    this.recordThreadMessage({
      channel,
      threadTs,
      ts,
      text: String(event.text ?? ""),
      user: typeof event.user === "string" ? event.user : undefined,
      subtype: typeof event.subtype === "string" ? event.subtype : undefined,
      bot_id: typeof event.bot_id === "string" ? event.bot_id : undefined,
      app_id: typeof event.app_id === "string" ? event.app_id : undefined,
      username: typeof event.username === "string" ? event.username : undefined,
      files: Array.isArray(event.files) ? (event.files as readonly Record<string, unknown>[]) : undefined,
      blocks: Array.isArray(event.blocks) ? (event.blocks as readonly Record<string, unknown>[]) : undefined,
      attachments: Array.isArray(event.attachments) ? (event.attachments as readonly Record<string, unknown>[]) : undefined,
    });
  }
}

async function readRawBody(request: http.IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}

async function readRequestBody(request: http.IncomingMessage): Promise<Record<string, unknown>> {
  const rawBuffer = await readRawBody(request);
  if (rawBuffer.byteLength === 0) {
    return {};
  }

  const rawBody = rawBuffer.toString("utf8");
  const contentType = request.headers["content-type"] ?? "";

  if (contentType.includes("application/json")) {
    return JSON.parse(rawBody) as Record<string, unknown>;
  }

  const params = new URLSearchParams(rawBody);
  const body: Record<string, unknown> = {};

  for (const [key, value] of params.entries()) {
    body[key] = value;
  }

  return body;
}

function getThreadKey(channel: string, threadTs: string): string {
  return `${channel}:${threadTs}`;
}

function getReactionKey(channel: string, timestamp: string, name: string): string {
  return `${channel}:${timestamp}:${name}`;
}

function inferMockConversation(channelId: string): MockConversation {
  if (channelId.startsWith("D")) {
    return {
      id: channelId,
      is_im: true,
    };
  }

  if (channelId.startsWith("G")) {
    return {
      id: channelId,
      name: channelId.toLowerCase(),
      is_group: true,
    };
  }

  return {
    id: channelId,
    name: channelId.toLowerCase(),
    is_channel: true,
  };
}
