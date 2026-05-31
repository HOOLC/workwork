import fs from "node:fs/promises";
import path from "node:path";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import type { OAuthClientInformationMixed, OAuthClientMetadata, OAuthTokens } from "@modelcontextprotocol/sdk/shared/auth.js";
import type { OAuthClientProvider } from "@modelcontextprotocol/sdk/client/auth.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";

import type { JsonLike } from "../../types.js";
import { fileExists } from "../../utils/fs.js";

const DEFAULT_MCP_URLS: Record<string, string> = {
  linear: "https://mcp.linear.app/mcp",
  notion: "https://mcp.notion.com/mcp",
};

interface StoredMcpCredential {
  readonly server_name?: string;
  readonly server_url?: string;
  readonly client_id?: string;
  readonly client_secret?: string;
  readonly access_token?: string;
  readonly refresh_token?: string;
  readonly expires_at?: number;
  readonly scopes?: readonly string[];
}

export interface IsolatedMcpToolSummary {
  readonly name: string;
  readonly description?: string | undefined;
  readonly inputSchema?: JsonLike | undefined;
}

export interface IsolatedMcpToolCallResult {
  readonly content?: JsonLike | undefined;
  readonly structuredContent?: JsonLike | undefined;
  readonly isError?: boolean | undefined;
  readonly _meta?: JsonLike | undefined;
}

interface ConnectedIsolatedMcpClient {
  readonly close: () => Promise<void>;
  readonly listTools: () => Promise<readonly IsolatedMcpToolSummary[]>;
  readonly callTool: (name: string, args: Record<string, unknown>) => Promise<IsolatedMcpToolCallResult>;
}

export class IsolatedMcpService {
  constructor(
    private readonly options: {
      readonly codexHome: string;
      readonly isolatedMcpServers: readonly string[];
      readonly createClient?: ((server: string) => Promise<ConnectedIsolatedMcpClient>) | undefined;
    },
  ) {}

  async listTools(server: string): Promise<readonly IsolatedMcpToolSummary[]> {
    const normalizedServer = this.#normalizeServer(server);
    const client = await this.#createClient(normalizedServer);

    try {
      return await client.listTools();
    } finally {
      await client.close();
    }
  }

  async callTool(options: { readonly server: string; readonly name: string; readonly arguments?: Record<string, unknown> | undefined }): Promise<IsolatedMcpToolCallResult> {
    const normalizedServer = this.#normalizeServer(options.server);
    const toolName = options.name.trim();
    if (!toolName) {
      throw new Error("missing_mcp_tool_name");
    }

    const client = await this.#createClient(normalizedServer);

    try {
      return await client.callTool(toolName, options.arguments ?? {});
    } finally {
      await client.close();
    }
  }

  #normalizeServer(server: string): string {
    const normalized = server.trim();
    if (!normalized) {
      throw new Error("missing_mcp_server");
    }

    if (!this.options.isolatedMcpServers.includes(normalized)) {
      throw new Error(`unsupported_isolated_mcp_server:${normalized}`);
    }

    return normalized;
  }

  async #createClient(server: string): Promise<ConnectedIsolatedMcpClient> {
    if (this.options.createClient) {
      return await this.options.createClient(server);
    }

    const serverUrl = await this.#resolveServerUrl(server);
    const credentialsEntry = await this.#readCredentialsEntry(server, serverUrl);
    const authProvider = new StoredOauthProvider({
      codexHome: this.options.codexHome,
      server,
      serverUrl,
      entry: credentialsEntry,
    });

    const transport = new StreamableHTTPClientTransport(new URL(serverUrl), {
      authProvider,
    });
    const client = new Client({
      name: "workwork",
      version: "0.1.0",
    });
    await client.connect(transport as unknown as Transport);

    return {
      close: async () => {
        await client.close();
      },
      listTools: async () => {
        const result = await client.listTools();
        return result.tools.map((tool) => ({
          name: tool.name,
          description: tool.description,
          inputSchema: tool.inputSchema as JsonLike | undefined,
        }));
      },
      callTool: async (name: string, args: Record<string, unknown>) => {
        const result = await client.callTool({
          name,
          arguments: args,
        });
        return {
          content: result.content as JsonLike | undefined,
          structuredContent: result.structuredContent as JsonLike | undefined,
          isError: typeof result.isError === "boolean" ? result.isError : undefined,
          _meta: result._meta as JsonLike | undefined,
        };
      },
    };
  }

  async #resolveServerUrl(server: string): Promise<string> {
    const sourceConfigPath = path.join(this.options.codexHome, "config.toml");
    if (await fileExists(sourceConfigPath)) {
      const raw = await fs.readFile(sourceConfigPath, "utf8");
      const pattern = new RegExp(String.raw`^\[mcp_servers\.${escapeRegExp(server)}\][\s\S]*?^url\s*=\s*"([^"]+)"`, "m");
      const match = raw.match(pattern);
      if (match?.[1]) {
        return match[1];
      }
    }

    const fallback = DEFAULT_MCP_URLS[server];
    if (fallback) {
      return fallback;
    }

    throw new Error(`missing_mcp_server_url:${server}`);
  }

  async #readCredentialsEntry(server: string, serverUrl: string): Promise<StoredMcpCredential> {
    const credentialsPath = path.join(this.options.codexHome, ".credentials.json");
    if (!(await fileExists(credentialsPath))) {
      throw new Error(`missing_mcp_credentials:${server}`);
    }

    const raw = await fs.readFile(credentialsPath, "utf8");
    const parsed = JSON.parse(raw) as Record<string, StoredMcpCredential>;
    const match = Object.values(parsed).find((entry) => entry.server_name === server && entry.server_url === serverUrl);

    if (!match) {
      throw new Error(`missing_mcp_credentials:${server}`);
    }

    return match;
  }
}

class StoredOauthProvider implements OAuthClientProvider {
  #codeVerifier = "broker-static-code-verifier";
  #state = "broker-static-state";
  #entry: StoredMcpCredential;

  constructor(
    private readonly options: {
      readonly codexHome: string;
      readonly server: string;
      readonly serverUrl: string;
      readonly entry: StoredMcpCredential;
    },
  ) {
    this.#entry = options.entry;
  }

  get redirectUrl(): undefined {
    return undefined;
  }

  get clientMetadata(): OAuthClientMetadata {
    return {
      redirect_uris: [],
      client_name: "workwork",
      client_uri: "https://github.com/HOOLC/workwork",
      grant_types: this.#entry.refresh_token ? ["refresh_token"] : ["authorization_code", "refresh_token"],
      response_types: ["code"],
      token_endpoint_auth_method: this.#entry.client_secret ? "client_secret_post" : "none",
    };
  }

  clientInformation(): OAuthClientInformationMixed | undefined {
    if (!this.#entry.client_id) {
      return undefined;
    }

    return {
      client_id: this.#entry.client_id,
      client_secret: this.#entry.client_secret,
    };
  }

  tokens(): OAuthTokens | undefined {
    if (!this.#entry.access_token) {
      return undefined;
    }

    return {
      access_token: this.#entry.access_token,
      token_type: "Bearer",
      refresh_token: this.#entry.refresh_token,
      expires_in: this.#entry.expires_at ? Math.max(0, Math.floor((this.#entry.expires_at - Date.now()) / 1000)) : undefined,
      scope: this.#entry.scopes?.join(" ") || undefined,
    };
  }

  prepareTokenRequest(scope?: string): URLSearchParams | undefined {
    if (!this.#entry.refresh_token) {
      return undefined;
    }

    const params = new URLSearchParams();
    params.set("grant_type", "refresh_token");
    params.set("refresh_token", this.#entry.refresh_token);
    if (scope && scope.trim()) {
      params.set("scope", scope);
    }
    return params;
  }

  async saveTokens(tokens: OAuthTokens): Promise<void> {
    const credentialsPath = path.join(this.options.codexHome, ".credentials.json");
    if (!(await fileExists(credentialsPath))) {
      return;
    }

    const raw = await fs.readFile(credentialsPath, "utf8");
    const parsed = JSON.parse(raw) as Record<string, StoredMcpCredential>;
    const nextEntries = Object.fromEntries(
      Object.entries(parsed).map(([key, value]) => {
        if (!(value.server_name === this.options.server && value.server_url === this.options.serverUrl)) {
          return [key, value];
        }

        return [
          key,
          {
            ...value,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token ?? value.refresh_token,
            expires_at: tokens.expires_in ? Date.now() + tokens.expires_in * 1000 : value.expires_at,
            scopes: tokens.scope ? tokens.scope.split(" ").filter(Boolean) : value.scopes,
          },
        ];
      }),
    );

    const nextRefreshToken = tokens.refresh_token ?? this.#entry.refresh_token;
    const nextExpiresAt = tokens.expires_in ? Date.now() + tokens.expires_in * 1000 : this.#entry.expires_at;
    const nextScopes = tokens.scope ? tokens.scope.split(" ").filter(Boolean) : this.#entry.scopes;
    this.#entry = {
      ...this.#entry,
      access_token: tokens.access_token,
      ...(nextRefreshToken ? { refresh_token: nextRefreshToken } : {}),
      ...(typeof nextExpiresAt === "number" ? { expires_at: nextExpiresAt } : {}),
      ...(nextScopes ? { scopes: nextScopes } : {}),
    };

    await fs.writeFile(credentialsPath, JSON.stringify(nextEntries), "utf8");
  }

  redirectToAuthorization(): void {
    throw new Error(`interactive_mcp_auth_required:${this.options.server}`);
  }

  saveCodeVerifier(codeVerifier: string): void {
    this.#codeVerifier = codeVerifier;
  }

  codeVerifier(): string {
    return this.#codeVerifier;
  }

  saveState(state: string): void {
    this.#state = state;
  }

  state(): string {
    return this.#state;
  }
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
