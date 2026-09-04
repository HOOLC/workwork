import { spawnSync } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { afterEach, describe, expect, it } from "vite-plus/test";

import { brokerRoot, getFreePort, removeTempRoot, spawnBinary, stopChild, waitForReady, writeConfig } from "./helpers.js";

const agentToken = "supervisor-agent-token";

describe.sequential("zork update", () => {
  const cleanups: Array<() => Promise<void>> = [];

  afterEach(async () => {
    while (cleanups.length > 0) {
      await cleanups.pop()?.();
    }
  });

  it("restarts Gateway and Agent without restarting the supervisor", async () => {
    const tempRoot = await fs.mkdtemp(path.join(os.tmpdir(), "zork-update-e2e-"));
    cleanups.push(async () => removeTempRoot(tempRoot));
    const dataRoot = path.join(tempRoot, "data");
    const [gatewayPort, runtimePort, controlPort, agentPort] = await Promise.all([getFreePort(), getFreePort(), getFreePort(), getFreePort()]);
    await writeConfig(dataRoot, {
      bind: {
        gateway: `127.0.0.1:${gatewayPort}`,
        runtime: `127.0.0.1:${runtimePort}`,
        control: `127.0.0.1:${controlPort}`,
        agent: `127.0.0.1:${agentPort}`,
      },
    });

    const supervisor = spawnBinary("zork", {
      cwd: brokerRoot,
      args: ["start", "--data", dataRoot, "--fake-agent", "--agent-token", agentToken],
      env: { RUST_LOG: "info" },
    });
    supervisor.stdout?.resume();
    supervisor.stderr?.resume();
    cleanups.push(() => stopChild(supervisor));

    await Promise.all([
      // One Gateway process serves all three public/control listeners.
      waitForReady(`http://127.0.0.1:${gatewayPort}/readyz`),
      waitForReady(`http://127.0.0.1:${runtimePort}/readyz`),
      waitForReady(`http://127.0.0.1:${controlPort}/readyz`),
      waitForReady(`http://127.0.0.1:${agentPort}/readyz`),
    ]);

    const before = await readPids({ gatewayPort, runtimePort, controlPort, agentPort });
    await expectAgentToken(agentPort);
    expect(before.runtime).toBeGreaterThan(0);
    expect(before.control).toBeGreaterThan(0);
    expect(before.agent).toBeGreaterThan(0);
    expect(supervisor.exitCode).toBeNull();

    const updated = await runZorkUpdate(dataRoot);
    expect(updated.status, `${updated.stdout}\n${updated.stderr}`).toBe(0);
    expect(updated.stdout).toContain("updated");

    const after = await readPids({ gatewayPort, runtimePort, controlPort, agentPort });
    await expectAgentToken(agentPort);
    expect(after.runtime).not.toBe(before.runtime);
    expect(after.control).not.toBe(before.control);
    expect(after.agent).not.toBe(before.agent);
    expect(supervisor.exitCode).toBeNull();

    const [gateway, runtime, control, agent] = await Promise.all([readReady(`http://127.0.0.1:${gatewayPort}/readyz`), readReady(`http://127.0.0.1:${runtimePort}/readyz`), readReady(`http://127.0.0.1:${controlPort}/readyz`), readReady(`http://127.0.0.1:${agentPort}/readyz`)]);
    expect(gateway).toMatchObject({ ok: true, service: "zork-gateway", pid: after.runtime });
    expect(runtime).toMatchObject({ ok: true, service: "zork-gateway", pid: after.runtime });
    expect(control).toMatchObject({ ok: true, service: "zork-gateway", pid: after.control });
    expect(agent).toMatchObject({ ok: true, service: "zork-agent", pid: after.agent });
  }, 90_000);
});

async function readPids(ports: { readonly gatewayPort: number; readonly runtimePort: number; readonly controlPort: number; readonly agentPort: number }): Promise<{ runtime: number; control: number; agent: number }> {
  const [runtime, control, agent] = await Promise.all([readPid(`http://127.0.0.1:${ports.runtimePort}/readyz`), readPid(`http://127.0.0.1:${ports.controlPort}/readyz`), readPid(`http://127.0.0.1:${ports.agentPort}/readyz`)]);
  return { runtime, control, agent };
}

async function readPid(url: string): Promise<number> {
  const body = await readReady(url);
  return Number((body as { pid?: number }).pid ?? 0);
}

async function expectAgentToken(agentPort: number): Promise<void> {
  const url = `http://127.0.0.1:${agentPort}/sessions`;
  expect((await fetch(url)).status).toBe(401);
  expect((await fetch(url, { headers: { authorization: `Bearer ${agentToken}` } })).status).toBe(200);
}

async function readReady(url: string): Promise<Record<string, unknown>> {
  // Stop/drain/start creates a brief unavailable interval, so retry until the
  // replacement listener is reachable.
  const deadline = Date.now() + 15_000;
  let lastError = "not ready";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);
      return (await response.json()) as Record<string, unknown>;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`readReady(${url}) failed: ${lastError}`);
}

async function runZorkUpdate(dataRoot: string): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const result = spawnSync(path.join(brokerRoot, "target/debug/zork"), ["update", "--data", dataRoot], { encoding: "utf8", timeout: 60_000 });
  return { status: result.status, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}
