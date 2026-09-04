import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import fs from "node:fs/promises";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const brokerRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const testConnectionId = "01J00000000000000000000TST";

export function testSlackConnection(slackPort: number, mode: "normal" | "proactive" = "normal"): Record<string, unknown> {
  return {
    id: testConnectionId,
    name: "Test Slack",
    provider: "slack",
    enabled: true,
    mode,
    app_token: "xapp-test",
    bot_token: "xoxb-test",
    api_base_url: `http://127.0.0.1:${slackPort}/api`,
  };
}

export function testSessionKey(channelId: string, rootMessageId: string): string {
  return `${testConnectionId}:${channelId}:${rootMessageId}`;
}

export function testNormalWorkspace(dataRoot: string, channelId: string, rootMessageId: string): string {
  return path.join(dataRoot, "workspaces", "im", testConnectionId, "normal", channelId, rootMessageId);
}

export async function getFreePort(): Promise<number> {
  const server = http.createServer();
  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("failed to allocate free port");
  }
  const port = address.port;
  await new Promise<void>((resolve) => {
    server.close(() => resolve());
  });
  return port;
}

export async function removeTempRoot(tempRoot: string): Promise<void> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      await fs.rm(tempRoot, { force: true, recursive: true });
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 100 * (attempt + 1)));
    }
  }
  throw lastError instanceof Error ? lastError : new Error(String(lastError));
}

export async function writeConfig(dataRoot: string, config: Record<string, unknown>): Promise<void> {
  await fs.mkdir(dataRoot, { recursive: true });
  await fs.writeFile(path.join(dataRoot, "config.json"), `${JSON.stringify(config, null, 2)}\n`);
}

export function spawnAgent(dataRoot: string, fakeAgent = true, env?: Record<string, string>, agentToken?: string, extraArgs: readonly string[] = []): ChildProcess {
  const args = ["--data", dataRoot];
  if (fakeAgent) {
    args.push("--fake-agent");
  }
  if (agentToken !== undefined) {
    args.push("--agent-token", agentToken);
  }
  args.push(...extraArgs);
  return spawnBinary("zork-agent", { cwd: brokerRoot, args, env });
}

export function spawnBinary(
  name: string,
  options: {
    readonly cwd: string;
    readonly args?: readonly string[];
    readonly env?: Record<string, string>;
  },
): ChildProcess {
  const binary = path.join(options.cwd, "target/debug", name);
  if (!existsSync(binary)) {
    throw new Error(`${name} debug binary is missing; run cargo build -p ${name}`);
  }
  const child = spawn(binary, [...(options.args ?? [])], {
    cwd: options.cwd,
    env: {
      ...process.env,
      ...options.env,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (process.env.ZORK_TEST_CHILD_LOG === "1") {
    child.stdout?.pipe(process.stdout);
    child.stderr?.pipe(process.stderr);
  }
  return child;
}

export async function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode != null || child.signalCode != null) {
    return;
  }
  child.kill("SIGTERM");
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      resolve();
    }, 2_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

export async function waitForReady(url: string, label = "readyz"): Promise<void> {
  const deadline = Date.now() + 20_000;
  let lastError = "not ready";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1_000) });
      if (response.ok) {
        return;
      }
      lastError = `status ${response.status}`;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`${label} failed: ${lastError}`);
}

export async function waitFor<T>(read: () => T | Promise<T>, predicate: (value: T) => boolean, label: string): Promise<T> {
  const deadline = Date.now() + 15_000;
  let last: T | undefined;
  while (Date.now() < deadline) {
    last = await read();
    if (predicate(last)) {
      return last;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${label}`);
}
