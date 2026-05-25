import { PassThrough, Writable } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import { handleSlackRequest } from "../src/http/slack-routes.js";

function createJsonRequest(body: unknown) {
  const request = new PassThrough() as PassThrough & {
    headers: Record<string, string>;
  };
  request.headers = {
    "content-type": "application/json"
  };
  request.end(JSON.stringify(body));
  return request;
}

function createResponse() {
  const chunks: Buffer[] = [];

  const response = new Writable({
    write(chunk, _encoding, callback) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      callback();
    }
  }) as Writable & {
    statusCode: number;
    writeHead: (code: number) => typeof response;
    end: (chunk?: string | Uint8Array) => typeof response;
    bodyText?: string;
  };

  response.statusCode = 200;
  response.writeHead = (code) => {
    response.statusCode = code;
    return response;
  };
  response.end = (chunk) => {
    if (chunk) {
      chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk));
    }
    response.bodyText = Buffer.concat(chunks).toString("utf8");
    return response;
  };

  return response;
}

describe("handleSlackRequest", () => {
  it("routes internal active-turn repair requests through the bridge", async () => {
    const repairActiveTurn = vi.fn(async () => ({
      repaired: true,
      previousActiveTurnId: "turn-1",
      resetInflightCount: 2,
      resumedCount: 2,
      interruptedActiveTurn: true
    }));
    const response = createResponse();

    const handled = await handleSlackRequest(
      "POST",
      new URL(`http://localhost/slack/sessions/${encodeURIComponent("C123:111.222")}/repair-active-turn`),
      createJsonRequest({}) as never,
      response as never,
      {
        bridge: {
          repairActiveTurn
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(repairActiveTurn).toHaveBeenCalledWith("C123:111.222");
    expect(response.statusCode).toBe(200);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: true,
      sessionKey: "C123:111.222",
      repair: {
        repaired: true,
        previousActiveTurnId: "turn-1",
        resetInflightCount: 2,
        resumedCount: 2,
        interruptedActiveTurn: true
      }
    });
  });

  it("routes internal session delete requests through the bridge", async () => {
    const deleteSession = vi.fn(async () => ({
      deleted: true,
      interruptedActiveTurn: true,
      clearedInboundCount: 2
    }));
    const response = createResponse();

    const handled = await handleSlackRequest(
      "DELETE",
      new URL(`http://localhost/slack/sessions/${encodeURIComponent("C123:111.222")}`),
      createJsonRequest({}) as never,
      response as never,
      {
        bridge: {
          deleteSession
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(deleteSession).toHaveBeenCalledWith("C123:111.222");
    expect(response.statusCode).toBe(200);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: true,
      sessionKey: "C123:111.222",
      delete: {
        deleted: true,
        interruptedActiveTurn: true,
        clearedInboundCount: 2
      }
    });
  });

  it("returns 404 for internal session delete requests when the worker session is unknown", async () => {
    const deleteSession = vi.fn(async () => {
      throw new Error("Unknown session runtime key: C123:missing");
    });
    const response = createResponse();

    const handled = await handleSlackRequest(
      "DELETE",
      new URL(`http://localhost/slack/sessions/${encodeURIComponent("C123:missing")}`),
      createJsonRequest({}) as never,
      response as never,
      {
        bridge: {
          deleteSession
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(response.statusCode).toBe(404);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: false,
      error: "Unknown session runtime key: C123:missing"
    });
  });

  it("treats blank co-author arrays as omitted in configure-session", async () => {
    const configureSessionCoauthors = vi.fn(async () => ({
      sessionKey: "session-key",
      channelId: "C123",
      rootThreadTs: "111.222",
      workspacePath: "/tmp/workspace",
      selectionMode: "default_all_candidates",
      ignoreMissing: false,
      needsUserInput: false,
      canCommitDirectly: true,
      selectedUserIds: [],
      resolvedCoAuthors: [],
      missingSelectedUserIds: [],
      candidates: []
    }));

    const request = createJsonRequest({
      cwd: "/tmp/workspace",
      coauthors: ["   "],
      user_ids: [""]
    });
    const response = createResponse();

    const handled = await handleSlackRequest(
      "POST",
      new URL("http://localhost/slack/git-coauthors/configure-session"),
      request as never,
      response as never,
      {
        bridge: {
          configureSessionCoauthors
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(configureSessionCoauthors).toHaveBeenCalledWith({
      cwd: "/tmp/workspace",
      coauthors: undefined,
      userIds: undefined,
      ignoreMissing: undefined,
      mappings: undefined
    });
    expect(response.statusCode).toBe(200);
  });

  it("rejects legacy co-author GitHub author mappings in configure-session", async () => {
    const configureSessionCoauthors = vi.fn();

    const request = createJsonRequest({
      cwd: "/tmp/workspace",
      mappings: [
        {
          slack_user: "Alice Example",
          github_author: "Alice Example <alice@example.com>"
        }
      ]
    });
    const response = createResponse();

    const handled = await handleSlackRequest(
      "POST",
      new URL("http://localhost/slack/git-coauthors/configure-session"),
      request as never,
      response as never,
      {
        bridge: {
          configureSessionCoauthors
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(configureSessionCoauthors).not.toHaveBeenCalled();
    expect(response.statusCode).toBe(400);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: false,
      error: "Manual co-author mappings are no longer supported. Bind GitHub OAuth for Slack users instead."
    });
  });

  it("resolves GitHub PR tokens through the bridge", async () => {
    const resolveGitHubPrToken = vi.fn(async () => ({
      ok: true,
      mode: "initiator",
      slackUserId: "U_STARTER",
      githubLogin: "alice",
      token: "alice-token"
    }));
    const request = createJsonRequest({
      cwd: "/tmp/session/workspace",
      command: ["pr", "create", "--fill"]
    });
    const response = createResponse();

    const handled = await handleSlackRequest(
      "POST",
      new URL("http://localhost/slack/github-token/resolve"),
      request as never,
      response as never,
      {
        bridge: {
          resolveGitHubPrToken
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(resolveGitHubPrToken).toHaveBeenCalledWith({
      cwd: "/tmp/session/workspace",
      command: ["pr", "create", "--fill"]
    });
    expect(response.statusCode).toBe(200);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: true,
      githubLogin: "alice",
      token: "alice-token"
    });
  });

  it("returns a blocking status when GitHub PR token resolution is blocked", async () => {
    const resolveGitHubPrToken = vi.fn(async () => ({
      ok: false,
      mode: "blocked",
      reason: "initiator_token_invalid",
      message: "GitHub token for alice is invalid."
    }));
    const request = createJsonRequest({
      cwd: "/tmp/session/workspace",
      command: ["pr", "create"]
    });
    const response = createResponse();

    const handled = await handleSlackRequest(
      "POST",
      new URL("http://localhost/slack/github-token/resolve"),
      request as never,
      response as never,
      {
        bridge: {
          resolveGitHubPrToken
        } as never,
        config: {} as never
      }
    );

    expect(handled).toBe(true);
    expect(response.statusCode).toBe(409);
    expect(JSON.parse(response.bodyText || "{}")).toMatchObject({
      ok: false,
      reason: "initiator_token_invalid",
      message: "GitHub token for alice is invalid."
    });
  });
});
