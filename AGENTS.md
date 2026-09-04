# Development host

The user moved all zork development to mini1 on 2026-09-05.

- Canonical host: `3720-Mac-mini-1.local` (SSH user `zuozijian`).
- Canonical checkout: `/Users/zuozijian/Projects/zork` on mini1.
- Make source changes, install dependencies, build, test, and run development services on mini1.
- If this task runs on another host, execute project work over SSH using mini1's login shell. The MacBook Air checkout at `/Users/zuozijian/Projects/HOOLC/open-worker` is a preserved migration backup, not the active checkout.
- Never sync the old local checkout over mini1 after migration; mini1 is the source of truth. Do not delete the local backup without a separate user request.
- Use the repository's pinned pnpm 10.33.0 (`npx --yes pnpm@10.33.0` if the host's default pnpm differs) and frozen lockfiles.
- mini1 had approximately 23 GiB free before migration. Build with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=4` to keep the regenerated build cache bounded, unless the task needs a different build configuration.

Example from a local task:

```sh
ssh 3720-Mac-mini-1.local 'zsh -lc '\''cd /Users/zuozijian/Projects/zork && git status --short --branch'\'''
```

Migration evidence and prior test findings are recorded in `artifacts/migration-mini1/README.md` on mini1. Preserve the existing uncommitted work; it was migrated without creating a commit or resetting Git state.
