import http, { type ServerResponse } from "node:http";

import { waitFor } from "../helpers.js";

export const controlledAgentToken = "controlled-agent-token";

export class PendingMailboxAppend {
  constructor(
    readonly sessionId: string,
    readonly body: { content?: string },
    private readonly response: ServerResponse,
  ) {}

  respond(status = 202): void {
    if (this.responded) throw new Error(`mailbox append for ${this.sessionId} was already answered`);
    this.response.writeHead(status);
    this.response.end();
  }

  get responded(): boolean {
    return this.response.writableEnded;
  }
}

export class ControlledAgent {
  readonly appends: PendingMailboxAppend[] = [];
  readonly creates: Array<Record<string, unknown>> = [];
  readonly selectionUpdates: Array<{ sessionId: string; selection: Record<string, unknown> }> = [];
  readonly profileWrites: Array<{ profileId: string; document: Record<string, unknown> }> = [];
  readonly profileDeletes: string[] = [];
  readonly sessions = new Map<string, Record<string, unknown>>();
  readonly profiles: Array<Record<string, unknown>>;
  createFailure: { readonly status: number; readonly message: string } | null = null;

  constructor(
    profiles: Array<Record<string, unknown>> = [
      {
        profile_id: "fixture",
        provider: "xai",
        billing: "usage",
        auth_configured: true,
        account: {},
        rateLimits: { ok: false, error: "not_probed" },
        models: [
          {
            id: "grok-4.6",
            api: "openai-completions",
            streaming: true,
            parallel_tool_calls: false,
            thinking: ["high", "xhigh"],
            default_thinking: "xhigh",
            capabilities: { input: ["text", "image"] },
            default: true,
          },
        ],
      },
    ],
  ) {
    this.profiles = profiles;
  }

  readonly server = http.createServer(async (request, response) => {
    if (request.headers.authorization !== `Bearer ${controlledAgentToken}`) {
      response.writeHead(401).end();
      return;
    }
    const url = request.url ?? "";
    if (request.method === "GET" && url === "/profiles") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ items: this.profiles }));
      return;
    }
    const profilePath = /^\/profiles\/([^/]+)$/.exec(url);
    if (request.method === "PUT" && profilePath) {
      const profileId = decodeURIComponent(profilePath[1]);
      const document = await readJsonRequest(request);
      if (profileId === "invalid") {
        response.writeHead(422, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { code: "invalid_request", message: "invalid profile" } }));
        return;
      }
      this.profileWrites.push({ profileId, document });
      const view = {
        profile_id: profileId,
        provider: document.provider,
        billing: document.billing,
        auth_configured: Boolean(document.auth && Object.keys(document.auth as object).length),
        account: {},
        rateLimits: { ok: false, error: "not_probed" },
        models: document.models,
      };
      const existing = this.profiles.findIndex((profile) => profile.profile_id === profileId);
      if (existing >= 0) this.profiles.splice(existing, 1, view);
      else this.profiles.push(view);
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(view));
      return;
    }
    if (request.method === "DELETE" && profilePath) {
      const profileId = decodeURIComponent(profilePath[1]);
      this.profileDeletes.push(profileId);
      const existing = this.profiles.findIndex((profile) => profile.profile_id === profileId);
      if (existing >= 0) this.profiles.splice(existing, 1);
      response.writeHead(204).end();
      return;
    }
    if (request.method === "GET" && url === "/sessions") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ items: [...this.sessions.values()] }));
      return;
    }
    if (request.method === "GET" && /^\/sessions\/[^/]+$/.test(url)) {
      const sessionId = decodeURIComponent(url.slice("/sessions/".length));
      const session = this.sessions.get(sessionId);
      response.writeHead(session ? 200 : 404, { "content-type": "application/json" });
      response.end(JSON.stringify(session ?? { error: { message: "session not found" } }));
      return;
    }
    if (request.method === "POST" && url === "/sessions") {
      const body = await readJsonRequest(request);
      this.creates.push(body);
      if (this.createFailure) {
        response.writeHead(this.createFailure.status, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            error: { code: "invalid_request", message: this.createFailure.message },
          }),
        );
        return;
      }
      const sessionId = `01ARZ3NDEKTSV4RRFFQ69G5FA${this.creates.length.toString(32).toUpperCase()}`;
      const session = {
        session_id: sessionId,
        workspace: body.workspace,
        profile_id: body.profile_id,
        model: body.model,
        thinking: body.thinking,
        generation: 1,
        context: body.context ?? { strategy: "compaction", keep_recent_tokens: 20_000 },
        status: "wait",
      };
      this.sessions.set(sessionId, session);
      response.writeHead(201, { "content-type": "application/json" });
      response.end(JSON.stringify(session));
      return;
    }
    const selectionPath = /^\/sessions\/([^/]+)\/selection$/.exec(url);
    const contextPath = /^\/sessions\/([^/]+)\/context$/.exec(url);
    if (request.method === "PUT" && contextPath) {
      const session = this.sessions.get(decodeURIComponent(contextPath[1]));
      if (!session) {
        response.writeHead(404, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { code: "not_found", message: "session not found" } }));
        return;
      }
      session.context = await readJsonRequest(request);
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(session));
      return;
    }
    if (request.method === "PUT" && selectionPath) {
      const sessionId = decodeURIComponent(selectionPath[1]);
      const selection = await readJsonRequest(request);
      const session = this.sessions.get(sessionId);
      if (!session) {
        response.writeHead(404, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { message: "session not found" } }));
        return;
      }
      this.selectionUpdates.push({ sessionId, selection });
      const updated = { ...session, ...selection };
      this.sessions.set(sessionId, updated);
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(updated));
      return;
    }
    const mailbox = /^\/sessions\/([^/]+)\/mailbox$/.exec(url);
    if (request.method === "POST" && mailbox) {
      this.appends.push(new PendingMailboxAppend(decodeURIComponent(mailbox[1]), (await readJsonRequest(request)) as { content?: string }, response));
      return;
    }
    response.writeHead(404).end();
  });

  async start(port: number): Promise<void> {
    await new Promise<void>((resolve) => this.server.listen(port, "127.0.0.1", resolve));
  }

  async waitForAppend(count: number): Promise<PendingMailboxAppend> {
    return await waitFor(
      () => this.appends[count - 1],
      (append): append is PendingMailboxAppend => Boolean(append),
      `Agent mailbox append ${count}`,
    );
  }

  async waitForCreate(count: number): Promise<Record<string, unknown>> {
    return await waitFor(
      () => this.creates[count - 1],
      (create): create is Record<string, unknown> => Boolean(create),
      `Agent session create ${count}`,
    );
  }

  async stop(): Promise<void> {
    for (const append of this.appends) {
      if (!append.responded) append.respond();
    }
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }
}

async function readJsonRequest(request: http.IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
}
