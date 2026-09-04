import { defineConfig } from "vite-plus";

export default defineConfig({
  lint: {
    plugins: ["import", "node", "promise", "typescript", "unicorn", "oxc", "vitest"],
    rules: {
      eqeqeq: ["error", "always", { null: "ignore" }],
      "max-lines": ["error", { max: 1200, skipBlankLines: true, skipComments: true }],
      "no-debugger": "error",
      "no-empty": ["error", { allowEmptyCatch: true }],
      "no-throw-literal": "error",
      "no-unused-vars": "off",
      "no-var": "error",
      "prefer-const": "off",
      "unicorn/no-useless-fallback-in-spread": "off",
      "unicorn/no-useless-spread": "off",
      "import/no-cycle": "off",
      "import/no-duplicates": "error",
      "promise/no-return-wrap": "error",
      "oxc/erasing-op": "off",
      "typescript/no-unsafe-declaration-merging": "off",
      "vitest/no-disabled-tests": "error",
      "vitest/no-conditional-expect": "off",
      "vitest/expect-expect": "off",
      "vitest/no-focused-tests": "error",
      "vitest/no-identical-title": "error",
      "vitest/require-mock-type-parameters": "off",
      "vitest/require-to-throw-message": "off",
      "vitest/valid-title": "off",
    },
    env: {
      builtin: true,
      node: true,
    },
    ignorePatterns: ["dist/**", "target/**", "target-linux/**", "crates/**", "artifacts/**", "node_modules/**", ".data/**", "apps/*/dist/**", "apps/*/node_modules/**"],
    options: {
      denyWarnings: true,
    },
  },
  fmt: {
    printWidth: 320,
    ignorePatterns: ["dist/**", "target/**", "target-linux/**", "crates/**", "artifacts/**", "docs/**", "evidence/**", "pnpm-lock.yaml", "Cargo.lock"],
  },
  run: {
    cache: true,
  },
  test: {
    include: ["test/**/*.test.ts", "apps/admin-ui/test/**/*.test.{ts,tsx}"],
    environment: "node",
  },
  staged: {
    "*.{js,ts,tsx,mjs}": "vp check --fix",
  },
});
