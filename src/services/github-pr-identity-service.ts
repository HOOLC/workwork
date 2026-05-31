import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import path from "node:path";

import type { SlackSessionRecord } from "../types.js";
import { ensureDir } from "../utils/fs.js";
import { withoutGlobalGitHubTokenEnv } from "../utils/github-env.js";

export interface GitHubPrBindingRecord {
  readonly slackUserId: string;
  readonly githubLogin: string;
  readonly githubUserId: number;
  readonly githubEmail?: string | undefined;
  readonly githubName?: string | undefined;
  readonly token: string;
  readonly scopes: readonly string[];
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly lastValidatedAt?: string | undefined;
  readonly revokedAt?: string | undefined;
}

export type GitHubPrTokenResolution =
  | {
      readonly ok: true;
      readonly mode: "initiator";
      readonly slackUserId: string;
      readonly githubLogin: string;
      readonly token: string;
    }
  | {
      readonly ok: true;
      readonly mode: "default";
      readonly defaultSource: "bound" | "env";
      readonly githubLogin: string;
      readonly token: string;
      readonly reason: "missing_initiator" | "initiator_unbound";
      readonly slackUserId?: string | undefined;
    }
  | {
      readonly ok: false;
      readonly mode: "blocked";
      readonly reason: "session_not_found" | "missing_initiator" | "initiator_unbound" | "initiator_token_invalid" | "default_account_unavailable";
      readonly message: string;
      readonly slackUserId?: string | undefined;
      readonly githubLogin?: string | undefined;
    };

export interface GitHubPrIdentityStatus {
  readonly initiatorUserId?: string | undefined;
  readonly binding:
    | {
        readonly state: "bound";
        readonly githubLogin: string;
        readonly githubUserId: number;
        readonly githubEmail?: string | undefined;
        readonly githubName?: string | undefined;
        readonly scopes: readonly string[];
        readonly updatedAt: string;
        readonly lastValidatedAt?: string | undefined;
      }
    | {
        readonly state: "revoked";
        readonly githubLogin: string;
        readonly githubUserId: number;
        readonly revokedAt: string;
      }
    | {
        readonly state: "unbound";
      }
    | {
        readonly state: "missing_initiator";
      };
  readonly defaultAccount:
    | {
        readonly available: true;
        readonly source: "bound";
        readonly slackUserId: string;
        readonly githubLogin: string;
        readonly githubUserId: number;
        readonly githubEmail?: string | undefined;
      }
    | {
        readonly available: true;
        readonly source: "env";
        readonly githubLogin: string;
      }
    | {
        readonly available: false;
        readonly selectedSlackUserId?: string | undefined;
        readonly githubLogin?: string | undefined;
        readonly reason?: "not_configured" | "selected_default_missing" | "selected_default_revoked" | undefined;
      };
}

interface StoredDeviceAuthorization {
  readonly id: string;
  readonly slackUserId: string;
  readonly ghConfigDir: string;
  readonly child: ReturnType<typeof spawn>;
  userCode: string;
  verificationUri: string;
  readonly expiresAt: number;
  intervalSeconds: number;
  output: string;
  processError?: string | undefined;
}

interface StoredSettings {
  readonly defaultSlackUserId?: string | undefined;
  readonly updatedAt?: string | undefined;
}

export class GitHubPrIdentityService {
  readonly #rootDir: string;
  readonly #bindingsDir: string;
  readonly #pendingDir: string;
  readonly #settingsPath: string;
  readonly #defaultGitHubLogin: string | undefined;
  readonly #defaultGitHubToken: string | undefined;
  readonly #githubApiBaseUrl: string;
  readonly #githubHostname: string;
  readonly #githubOAuthScopes: readonly string[];
  readonly #ghPath: string;
  readonly #bindings = new Map<string, GitHubPrBindingRecord>();
  readonly #deviceAuthorizations = new Map<string, StoredDeviceAuthorization>();
  #settings: StoredSettings = {};

  constructor(options: { readonly stateDir: string; readonly defaultGitHubLogin?: string | undefined; readonly defaultGitHubToken?: string | undefined; readonly githubApiBaseUrl?: string | undefined; readonly githubOAuthScopes?: readonly string[] | undefined; readonly ghPath?: string | undefined }) {
    this.#rootDir = path.join(options.stateDir, "github-pr-identities");
    this.#bindingsDir = path.join(this.#rootDir, "bindings");
    this.#pendingDir = path.join(this.#rootDir, "pending");
    this.#settingsPath = path.join(this.#rootDir, "settings.json");
    this.#defaultGitHubLogin = normalizeString(options.defaultGitHubLogin);
    this.#defaultGitHubToken = normalizeString(options.defaultGitHubToken);
    this.#githubApiBaseUrl = normalizeString(options.githubApiBaseUrl) ?? "https://api.github.com";
    this.#githubHostname = resolveGitHubHostname(this.#githubApiBaseUrl);
    this.#githubOAuthScopes = options.githubOAuthScopes?.length ? options.githubOAuthScopes : ["repo", "read:user", "user:email"];
    this.#ghPath = normalizeString(options.ghPath) ?? "gh";
  }

  async load(): Promise<void> {
    await ensureDir(this.#rootDir);
    await ensureDir(this.#bindingsDir);
    this.#bindings.clear();
    this.#settings = await this.#readSettings();

    const entries = await fs.readdir(this.#bindingsDir, { withFileTypes: true }).catch((error: unknown) => {
      if (isNodeErrno(error, "ENOENT")) {
        return [];
      }
      throw error;
    });

    for (const entry of entries) {
      if (!entry.isFile() || !entry.name.endsWith(".json")) {
        continue;
      }

      const filePath = path.join(this.#bindingsDir, entry.name);
      const raw = JSON.parse(await fs.readFile(filePath, "utf8")) as unknown;
      const record = normalizeBinding(raw);
      if (record) {
        this.#bindings.set(record.slackUserId, record);
      }
    }
  }

  async upsertBinding(options: {
    readonly slackUserId: string;
    readonly githubLogin: string;
    readonly githubUserId: number;
    readonly githubEmail?: string | undefined;
    readonly githubName?: string | undefined;
    readonly token: string;
    readonly scopes: readonly string[];
    readonly lastValidatedAt?: string | undefined;
    readonly revokedAt?: string | undefined;
  }): Promise<GitHubPrBindingRecord> {
    const slackUserId = requiredString(options.slackUserId, "slackUserId");
    const githubLogin = requiredString(options.githubLogin, "githubLogin");
    const token = requiredString(options.token, "token");
    if (!Number.isInteger(options.githubUserId) || options.githubUserId <= 0) {
      throw new Error("githubUserId must be a positive integer.");
    }

    const now = new Date().toISOString();
    const existing = this.#bindings.get(slackUserId);
    const record: GitHubPrBindingRecord = {
      slackUserId,
      githubLogin,
      githubUserId: options.githubUserId,
      ...(normalizeString(options.githubEmail) ? { githubEmail: normalizeString(options.githubEmail) } : {}),
      ...(normalizeString(options.githubName) ? { githubName: normalizeString(options.githubName) } : {}),
      token,
      scopes: [...new Set(options.scopes.map((scope) => scope.trim()).filter(Boolean))],
      createdAt: existing?.createdAt ?? now,
      updatedAt: now,
      ...(options.lastValidatedAt ? { lastValidatedAt: options.lastValidatedAt } : {}),
      ...(options.revokedAt ? { revokedAt: options.revokedAt } : {}),
    };

    await ensureDir(this.#bindingsDir);
    await fs.writeFile(this.#bindingPath(slackUserId), `${JSON.stringify(record, null, 2)}\n`, {
      mode: 0o600,
    });
    this.#bindings.set(slackUserId, record);
    return record;
  }

  async getBinding(slackUserId: string): Promise<GitHubPrBindingRecord | undefined> {
    await this.load();
    return this.#bindings.get(slackUserId);
  }

  listBindings(): readonly GitHubPrBindingRecord[] {
    return [...this.#bindings.values()].sort((left, right) => left.slackUserId.localeCompare(right.slackUserId));
  }

  getDefaultAccountStatus(): GitHubPrIdentityStatus["defaultAccount"] {
    return this.#defaultAccountStatus();
  }

  async setDefaultBinding(slackUserId: string): Promise<GitHubPrIdentityStatus["defaultAccount"]> {
    await this.load();
    const normalizedSlackUserId = requiredString(slackUserId, "slackUserId");
    const binding = this.#bindings.get(normalizedSlackUserId);
    if (!binding) {
      throw new Error("Cannot set default GitHub PR account to an unbound Slack user.");
    }
    if (binding.revokedAt) {
      throw new Error("Cannot set default GitHub PR account to a revoked binding.");
    }

    this.#settings = {
      defaultSlackUserId: normalizedSlackUserId,
      updatedAt: new Date().toISOString(),
    };
    await fs.writeFile(this.#settingsPath, `${JSON.stringify(this.#settings, null, 2)}\n`, {
      mode: 0o600,
    });
    return this.#defaultAccountStatus();
  }

  getSessionIdentityStatus(session: SlackSessionRecord): GitHubPrIdentityStatus {
    const initiatorUserId = normalizeString(session.initiatorUserId);
    const defaultAccount = this.#defaultAccountStatus();

    if (!initiatorUserId) {
      return {
        defaultAccount,
        binding: {
          state: "missing_initiator",
        },
      };
    }

    const binding = this.#bindings.get(initiatorUserId);
    if (!binding) {
      return {
        initiatorUserId,
        defaultAccount,
        binding: {
          state: "unbound",
        },
      };
    }

    if (binding.revokedAt) {
      return {
        initiatorUserId,
        defaultAccount,
        binding: {
          state: "revoked",
          githubLogin: binding.githubLogin,
          githubUserId: binding.githubUserId,
          revokedAt: binding.revokedAt,
        },
      };
    }

    return {
      initiatorUserId,
      defaultAccount,
      binding: {
        state: "bound",
        githubLogin: binding.githubLogin,
        githubUserId: binding.githubUserId,
        ...(binding.githubEmail ? { githubEmail: binding.githubEmail } : {}),
        ...(binding.githubName ? { githubName: binding.githubName } : {}),
        scopes: binding.scopes,
        updatedAt: binding.updatedAt,
        ...(binding.lastValidatedAt ? { lastValidatedAt: binding.lastValidatedAt } : {}),
      },
    };
  }

  async resolveTokenForSession(options: { readonly session: SlackSessionRecord; readonly command: readonly string[] }): Promise<GitHubPrTokenResolution> {
    await this.load();
    const initiatorUserId = normalizeString(options.session.initiatorUserId);
    if (!initiatorUserId) {
      return this.#resolveDefault("missing_initiator");
    }

    const binding = this.#bindings.get(initiatorUserId);
    if (!binding) {
      return this.#resolveDefault("initiator_unbound", initiatorUserId);
    }

    if (binding.revokedAt) {
      return {
        ok: false,
        mode: "blocked",
        reason: "initiator_token_invalid",
        slackUserId: initiatorUserId,
        githubLogin: binding.githubLogin,
        message: `GitHub token for ${binding.githubLogin} is invalid. Open the session page and rebind GitHub before running gh ${options.command.join(" ")}.`,
      };
    }

    return {
      ok: true,
      mode: "initiator",
      slackUserId: initiatorUserId,
      githubLogin: binding.githubLogin,
      token: binding.token,
    };
  }

  async startDeviceAuthorization(options: { readonly slackUserId: string }): Promise<{
    readonly id: string;
    readonly slackUserId: string;
    readonly userCode: string;
    readonly verificationUri: string;
    readonly verificationUriComplete?: string | undefined;
    readonly expiresAt: string;
    readonly intervalSeconds: number;
  }> {
    const slackUserId = requiredString(options.slackUserId, "slackUserId");
    const id = randomUUID();
    const ghConfigDir = path.join(this.#pendingDir, encodePathPart(id));
    await ensureDir(ghConfigDir);
    const expiresAt = Date.now() + 15 * 60 * 1000;
    const child = spawn(this.#ghPath, ["auth", "login", "--hostname", this.#githubHostname, "--git-protocol", "https", "--web", "--skip-ssh-key", "--insecure-storage", "--scopes", this.#githubOAuthScopes.join(",")], {
      env: {
        ...withoutGlobalGitHubTokenEnv(process.env),
        GH_BROWSER: "echo",
        GH_CONFIG_DIR: ghConfigDir,
        NO_COLOR: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const pending: StoredDeviceAuthorization = {
      id,
      slackUserId,
      ghConfigDir,
      child,
      userCode: "",
      verificationUri: "",
      expiresAt,
      intervalSeconds: 2,
      output: "",
    };
    this.#deviceAuthorizations.set(id, pending);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      pending.output += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      pending.output += chunk;
    });
    child.on("error", (error: Error) => {
      pending.processError = error.message;
    });

    let prompt: {
      readonly userCode: string;
      readonly verificationUri: string;
    };
    try {
      prompt = await waitForGhDevicePrompt(pending);
    } catch (error) {
      stopPendingGhLogin(pending);
      this.#deviceAuthorizations.delete(id);
      await fs.rm(ghConfigDir, { recursive: true, force: true });
      throw error;
    }
    pending.userCode = prompt.userCode;
    pending.verificationUri = prompt.verificationUri;

    return {
      id,
      slackUserId,
      userCode: prompt.userCode,
      verificationUri: prompt.verificationUri,
      expiresAt: new Date(expiresAt).toISOString(),
      intervalSeconds: pending.intervalSeconds,
    };
  }

  async pollDeviceAuthorization(id: string): Promise<
    | {
        readonly status: "pending";
        readonly retryAfterSeconds: number;
      }
    | {
        readonly status: "expired";
      }
    | {
        readonly status: "failed";
        readonly error: string;
      }
    | {
        readonly status: "completed";
        readonly binding: GitHubPrBindingRecord;
      }
  > {
    const pending = this.#deviceAuthorizations.get(id);
    if (!pending) {
      return {
        status: "failed",
        error: "unknown_device_authorization",
      };
    }
    if (Date.now() >= pending.expiresAt) {
      stopPendingGhLogin(pending);
      this.#deviceAuthorizations.delete(id);
      await fs.rm(pending.ghConfigDir, { recursive: true, force: true });
      return {
        status: "expired",
      };
    }

    if (pending.processError) {
      this.#deviceAuthorizations.delete(id);
      await fs.rm(pending.ghConfigDir, { recursive: true, force: true });
      return {
        status: "failed",
        error: pending.processError,
      };
    }

    if (pending.child.exitCode === null && pending.child.signalCode === null) {
      return {
        status: "pending",
        retryAfterSeconds: pending.intervalSeconds,
      };
    }

    if (pending.child.exitCode !== 0) {
      this.#deviceAuthorizations.delete(id);
      await fs.rm(pending.ghConfigDir, { recursive: true, force: true });
      return {
        status: "failed",
        error: summarizeGhOutput(pending.output) || `gh auth login exited with status ${pending.child.exitCode ?? pending.child.signalCode ?? "unknown"}`,
      };
    }

    const account = await this.#readGhAccount(pending.ghConfigDir);
    const token = await this.#readGhToken(pending.ghConfigDir, account.login);
    if (!token) {
      this.#deviceAuthorizations.delete(id);
      await fs.rm(pending.ghConfigDir, { recursive: true, force: true });
      return {
        status: "failed",
        error: "missing_access_token",
      };
    }

    const user = await this.#fetchGitHubUser(token);
    const githubEmail = await this.#fetchGitHubPrimaryEmail(token, user.email);
    const validatedAt = new Date().toISOString();
    const binding = await this.upsertBinding({
      slackUserId: pending.slackUserId,
      githubLogin: user.login,
      githubUserId: user.id,
      ...(githubEmail ? { githubEmail } : {}),
      ...(user.name ? { githubName: user.name } : {}),
      token,
      scopes: account.scopes,
      lastValidatedAt: validatedAt,
    });
    this.#deviceAuthorizations.delete(id);
    await fs.rm(pending.ghConfigDir, { recursive: true, force: true });
    return {
      status: "completed",
      binding,
    };
  }

  async #fetchGitHubUser(token: string): Promise<{
    readonly id: number;
    readonly login: string;
    readonly name?: string | undefined;
    readonly email?: string | undefined;
  }> {
    const response = await fetch(joinUrl(this.#githubApiBaseUrl, "/user"), {
      headers: {
        accept: "application/vnd.github+json",
        authorization: `Bearer ${token}`,
        "user-agent": "workwork",
      },
    });
    const raw = await readJsonResponse(response);
    if (!isRecord(raw) || typeof raw.login !== "string" || typeof raw.id !== "number" || !Number.isInteger(raw.id)) {
      throw new Error("GitHub user response did not include login and id.");
    }
    return {
      login: raw.login,
      id: raw.id,
      ...(normalizeString(raw.name) ? { name: normalizeString(raw.name) } : {}),
      ...(normalizeString(raw.email) ? { email: normalizeString(raw.email) } : {}),
    };
  }

  async #fetchGitHubPrimaryEmail(token: string, fallbackEmail?: string | undefined): Promise<string | undefined> {
    const raw = await readJsonResponse(
      await fetch(joinUrl(this.#githubApiBaseUrl, "/user/emails"), {
        headers: {
          accept: "application/vnd.github+json",
          authorization: `Bearer ${token}`,
          "user-agent": "workwork",
        },
      }),
    );
    if (!Array.isArray(raw)) {
      return normalizeString(fallbackEmail);
    }

    const emails = raw
      .filter((entry): entry is Record<string, unknown> => isRecord(entry))
      .map((entry) => ({
        email: normalizeString(entry.email),
        primary: entry.primary === true,
        verified: entry.verified === true,
      }))
      .filter((entry): entry is { readonly email: string; readonly primary: boolean; readonly verified: boolean } => Boolean(entry.email));
    return emails.find((entry) => entry.primary && entry.verified)?.email ?? emails.find((entry) => entry.verified)?.email ?? normalizeString(fallbackEmail);
  }

  #resolveDefault(reason: "missing_initiator" | "initiator_unbound", slackUserId?: string | undefined): GitHubPrTokenResolution {
    const selectedSlackUserId = normalizeString(this.#settings.defaultSlackUserId);
    if (selectedSlackUserId) {
      const selected = this.#bindings.get(selectedSlackUserId);
      if (selected && !selected.revokedAt) {
        return {
          ok: true,
          mode: "default",
          defaultSource: "bound",
          slackUserId: selectedSlackUserId,
          githubLogin: selected.githubLogin,
          token: selected.token,
          reason,
        };
      }

      return {
        ok: false,
        mode: "blocked",
        reason: "default_account_unavailable",
        slackUserId: selectedSlackUserId,
        ...(selected?.githubLogin ? { githubLogin: selected.githubLogin } : {}),
        message: selected?.revokedAt ? `The selected default GitHub PR account ${selected.githubLogin} is revoked. Open admin and choose another bound GitHub account.` : "The selected default GitHub PR account is missing. Open admin and choose another bound GitHub account.",
      };
    }

    if (this.#defaultGitHubLogin && this.#defaultGitHubToken) {
      return {
        ok: true,
        mode: "default",
        defaultSource: "env",
        githubLogin: this.#defaultGitHubLogin,
        token: this.#defaultGitHubToken,
        reason,
      };
    }

    return {
      ok: false,
      mode: "blocked",
      reason: this.#defaultGitHubLogin || this.#defaultGitHubToken ? reason : "default_account_unavailable",
      ...(slackUserId ? { slackUserId } : {}),
      message: "No GitHub account is bound to this Slack session initiator, and no default GitHub account is configured.",
    };
  }

  #defaultAccountStatus(): GitHubPrIdentityStatus["defaultAccount"] {
    const selectedSlackUserId = normalizeString(this.#settings.defaultSlackUserId);
    if (selectedSlackUserId) {
      const selected = this.#bindings.get(selectedSlackUserId);
      if (selected && !selected.revokedAt) {
        return {
          available: true,
          source: "bound",
          slackUserId: selectedSlackUserId,
          githubLogin: selected.githubLogin,
          githubUserId: selected.githubUserId,
          ...(selected.githubEmail ? { githubEmail: selected.githubEmail } : {}),
        };
      }
      return {
        available: false,
        selectedSlackUserId,
        ...(selected?.githubLogin ? { githubLogin: selected.githubLogin } : {}),
        reason: selected?.revokedAt ? "selected_default_revoked" : "selected_default_missing",
      };
    }

    if (this.#defaultGitHubLogin && this.#defaultGitHubToken) {
      return {
        available: true,
        source: "env",
        githubLogin: this.#defaultGitHubLogin,
      };
    }

    return {
      available: false,
      reason: "not_configured",
    };
  }

  async #readSettings(): Promise<StoredSettings> {
    try {
      const raw = JSON.parse(await fs.readFile(this.#settingsPath, "utf8")) as unknown;
      if (!isRecord(raw)) {
        return {};
      }
      const defaultSlackUserId = normalizeString(raw.defaultSlackUserId);
      return {
        ...(defaultSlackUserId ? { defaultSlackUserId } : {}),
        ...(normalizeString(raw.updatedAt) ? { updatedAt: normalizeString(raw.updatedAt) } : {}),
      };
    } catch (error) {
      if (isNodeErrno(error, "ENOENT")) {
        return {};
      }
      throw error;
    }
  }

  async #readGhAccount(ghConfigDir: string): Promise<{
    readonly login: string;
    readonly scopes: readonly string[];
  }> {
    const output = await this.#runGh(["auth", "status", "--hostname", this.#githubHostname, "--active", "--json", "hosts"], ghConfigDir);
    const parsed = JSON.parse(output) as unknown;
    return parseGhActiveAccount(parsed, this.#githubHostname);
  }

  async #readGhToken(ghConfigDir: string, login: string): Promise<string> {
    return (await this.#runGh(["auth", "token", "--hostname", this.#githubHostname, "--user", login], ghConfigDir)).trim();
  }

  async #runGh(args: readonly string[], ghConfigDir: string): Promise<string> {
    return await new Promise<string>((resolve, reject) => {
      const child = spawn(this.#ghPath, [...args], {
        env: {
          ...withoutGlobalGitHubTokenEnv(process.env),
          GH_CONFIG_DIR: ghConfigDir,
          GH_PROMPT_DISABLED: "1",
          NO_COLOR: "1",
        },
        stdio: ["ignore", "pipe", "pipe"],
      });
      let stdout = "";
      let stderr = "";
      let timeout: NodeJS.Timeout | undefined = setTimeout(() => {
        timeout = undefined;
        child.kill("SIGTERM");
        reject(new Error(`gh ${args.join(" ")} timed out.`));
      }, 15_000);
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk: string) => {
        stdout += chunk;
      });
      child.stderr.on("data", (chunk: string) => {
        stderr += chunk;
      });
      child.on("error", reject);
      child.on("close", (code) => {
        if (timeout) {
          clearTimeout(timeout);
          timeout = undefined;
        }
        if (code === 0) {
          resolve(stdout);
          return;
        }
        reject(new Error(summarizeGhOutput(stderr || stdout) || `gh ${args.join(" ")} exited with status ${code}.`));
      });
    });
  }

  #bindingPath(slackUserId: string): string {
    return path.join(this.#bindingsDir, `${encodePathPart(slackUserId)}.json`);
  }
}

async function waitForGhDevicePrompt(pending: StoredDeviceAuthorization): Promise<{
  readonly userCode: string;
  readonly verificationUri: string;
}> {
  const startedAt = Date.now();
  while (Date.now() - startedAt < 10_000) {
    if (pending.processError) {
      throw new Error(pending.processError);
    }

    const prompt = parseGhDevicePrompt(pending.output);
    if (prompt) {
      return prompt;
    }

    if (pending.child.exitCode !== null || pending.child.signalCode !== null) {
      throw new Error(summarizeGhOutput(pending.output) || "gh auth login exited before printing a device code.");
    }

    await sleep(100);
  }

  stopPendingGhLogin(pending);
  throw new Error("Timed out waiting for gh auth login to print a GitHub device code.");
}

function parseGhDevicePrompt(output: string): {
  readonly userCode: string;
  readonly verificationUri: string;
} | null {
  const userCode = output.match(/one-time code:\s*([A-Z0-9-]+)/i)?.[1]?.trim();
  const verificationUri = output.match(/https:\/\/[^\s]+\/login\/device\b/i)?.[0]?.trim() ?? output.match(/https:\/\/github\.com\/login\/device\b/i)?.[0]?.trim();
  if (!userCode || !verificationUri) {
    return null;
  }
  return {
    userCode,
    verificationUri,
  };
}

function stopPendingGhLogin(pending: StoredDeviceAuthorization): void {
  if (pending.child.exitCode === null && pending.child.signalCode === null) {
    pending.child.kill("SIGTERM");
  }
}

function parseGhActiveAccount(
  raw: unknown,
  hostname: string,
): {
  readonly login: string;
  readonly scopes: readonly string[];
} {
  if (!isRecord(raw) || !isRecord(raw.hosts)) {
    throw new Error("gh auth status did not return hosts.");
  }

  const hostEntries = raw.hosts[hostname];
  const accounts = Array.isArray(hostEntries) ? hostEntries : Object.values(raw.hosts).find((value) => Array.isArray(value));
  if (!Array.isArray(accounts)) {
    throw new Error(`gh auth status did not include ${hostname}.`);
  }

  const active = accounts.find((entry) => isRecord(entry) && entry.active === true && entry.state === "success" && typeof entry.login === "string" && entry.login.trim());
  if (!isRecord(active) || typeof active.login !== "string") {
    throw new Error(`gh auth status did not include an active account for ${hostname}.`);
  }

  return {
    login: active.login.trim(),
    scopes: typeof active.scopes === "string" ? parseScopes(active.scopes) : [],
  };
}

function summarizeGhOutput(output: string): string {
  return output
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .slice(-4)
    .join("\n");
}

function resolveGitHubHostname(apiBaseUrl: string): string {
  try {
    const hostname = new URL(apiBaseUrl).hostname;
    return hostname === "api.github.com" ? "github.com" : hostname.replace(/^api\./, "");
  } catch {
    return "github.com";
  }
}

async function sleep(ms: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

async function readJsonResponse(response: Response): Promise<unknown> {
  const text = await response.text();
  const parsed = text ? JSON.parse(text) : null;
  if (!response.ok) {
    const error = isRecord(parsed) && typeof parsed.error === "string" ? parsed.error : response.statusText;
    throw new Error(`GitHub request failed (${response.status}): ${error}`);
  }
  return parsed;
}

function normalizeBinding(raw: unknown): GitHubPrBindingRecord | null {
  if (!isRecord(raw)) {
    return null;
  }

  const slackUserId = normalizeString(raw.slackUserId);
  const githubLogin = normalizeString(raw.githubLogin);
  const githubUserId = typeof raw.githubUserId === "number" && Number.isInteger(raw.githubUserId) ? raw.githubUserId : 0;
  const githubEmail = normalizeString(raw.githubEmail);
  const githubName = normalizeString(raw.githubName);
  const token = normalizeString(raw.token);
  const createdAt = normalizeString(raw.createdAt);
  const updatedAt = normalizeString(raw.updatedAt);
  if (!slackUserId || !githubLogin || githubUserId <= 0 || !token || !createdAt || !updatedAt) {
    return null;
  }

  const scopes = Array.isArray(raw.scopes) ? raw.scopes.map((entry) => normalizeString(entry)).filter((entry): entry is string => Boolean(entry)) : [];
  const lastValidatedAt = normalizeString(raw.lastValidatedAt);
  const revokedAt = normalizeString(raw.revokedAt);
  return {
    slackUserId,
    githubLogin,
    githubUserId,
    ...(githubEmail ? { githubEmail } : {}),
    ...(githubName ? { githubName } : {}),
    token,
    scopes,
    createdAt,
    updatedAt,
    ...(lastValidatedAt ? { lastValidatedAt } : {}),
    ...(revokedAt ? { revokedAt } : {}),
  };
}

function joinUrl(baseUrl: string, pathName: string): string {
  return new URL(pathName.replace(/^\/+/, ""), `${baseUrl.replace(/\/+$/, "")}/`).toString();
}

function parseScopes(value: string): string[] {
  return value
    .split(",")
    .map((entry) => entry.trim())
    .filter(Boolean);
}

function requiredString(value: unknown, name: string): string {
  const normalized = normalizeString(value);
  if (!normalized) {
    throw new Error(`${name} is required.`);
  }
  return normalized;
}

function normalizeString(value: unknown): string | undefined {
  if (typeof value !== "string") {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed ? trimmed : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function isNodeErrno(error: unknown, code: string): boolean {
  return Boolean(error && typeof error === "object" && "code" in error && error.code === code);
}

function encodePathPart(value: string): string {
  return encodeURIComponent(value).replace(/%/g, "_");
}
