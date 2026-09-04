import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vite-plus/test";

const adminRoot = fileURLToPath(new URL("../apps/admin-ui/", import.meta.url));

async function adminSourceFiles(): Promise<string[]> {
  return (await fs.readdir(adminRoot))
    .filter((name) => name.endsWith(".ts") || name.endsWith(".tsx"))
    .map((name) => path.join(adminRoot, name))
    .sort();
}

async function relativeImportGraph(files: readonly string[]): Promise<Map<string, string[]>> {
  const sourceFiles = new Set(files);
  const graph = new Map<string, string[]>();
  for (const file of files) {
    const source = await fs.readFile(file, "utf8");
    const targets: string[] = [];
    for (const match of source.matchAll(/\b(?:from|import)\s*["'](\.[^"']+)["']/g)) {
      const specifier = match[1];
      if (!specifier) continue;
      const unresolved = path.resolve(path.dirname(file), specifier.replace(/\.js$/, ""));
      const target = [unresolved, `${unresolved}.ts`, `${unresolved}.tsx`].find((candidate) => sourceFiles.has(candidate));
      if (target) targets.push(target);
    }
    graph.set(file, targets);
  }
  return graph;
}

function importCycles(graph: ReadonlyMap<string, readonly string[]>): string[][] {
  const cycles: string[][] = [];
  const visited = new Set<string>();
  const active = new Map<string, number>();
  const stack: string[] = [];

  function visit(file: string): void {
    if (visited.has(file)) return;
    const activeIndex = active.get(file);
    if (activeIndex !== undefined) {
      cycles.push([...stack.slice(activeIndex), file]);
      return;
    }
    active.set(file, stack.length);
    stack.push(file);
    for (const target of graph.get(file) || []) visit(target);
    stack.pop();
    active.delete(file);
    visited.add(file);
  }

  for (const file of graph.keys()) visit(file);
  return cycles;
}

describe("Admin UI module boundaries", () => {
  it("keeps relative imports acyclic", async () => {
    const files = await adminSourceFiles();
    const cycles = importCycles(await relativeImportGraph(files)).map((cycle) => cycle.map((file) => path.basename(file)));
    expect(cycles).toEqual([]);
  });
});
