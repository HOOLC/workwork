# workwork

Workwork is a Slack + China Feishu broker that routes chat sessions into Codex app-server sessions with isolated workspaces.

It connects to Slack over Socket Mode and to China Feishu over long connection. Each Slack thread or Feishu topic can start or resume one Codex app-server thread, backed by an isolated workspace directory. The Codex session always starts in that neutral workspace instead of being pinned to a specific repository. If code work is needed, the agent is expected to use a shared `repos/` cache for canonical clones and create any task-specific git worktrees under the current session workspace. Normal thread/topic replies continue the same Codex thread. Sending `-stop` interrupts the current Codex turn.

Slack remains a first-class runtime: on the first `@bot` inside an existing Slack thread, the broker backfills a bounded slice of earlier thread history into Codex. If Codex needs older context than the initial backfill, it can query the broker's local thread-history HTTP API from inside its shell.

Feishu support runs in the same broker process as Slack. Feishu group `@bot ...` messages create or resume a group session, Feishu topic replies act as the Slack-thread equivalent, and private chats are ignored. For production parity, configure `FEISHU_ENABLED=true`, `FEISHU_GROUP_MESSAGE_MODE=all`, `FEISHU_APP_ID`, `FEISHU_APP_SECRET`, and at least one Feishu bot identity. `at_only` is a visible degraded mode; set `FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=true` only after the real non-@ follow-up smoke passes; keep `LOG_RAW_FEISHU_EVENTS=false` unless collecting a focused, redacted fixture.

Feishu rollout:

- run the preflight and smoke scripts against a China Feishu self-built app installed in the target group
- capture a sanitized pre-rollout log snapshot; the snapshot redacts non-structured lines instead of copying raw Docker log text
- verify metadata recursively redacts unsafe string fields while preserving safe posture text such as `FEISHU_APP_SECRET=missing`
- Operator-facing auth status and replacement output summarize filesystem paths instead of echoing full host paths
- Profile command output also summarizes auth/profile paths without full host filesystem paths

Admin and chat APIs are platform-aware:

- platform-aware `Slack/Feishu user -> GitHub author` mappings
- `GET /admin/api/status?platform=slack|feishu`
- `DELETE /admin/api/github-authors/:userId?platform=slack|feishu`
- filters sessions, jobs, and GitHub author mappings to that platform
- allowlisted `recentBrokerLogs` remain cross-platform
- Platform query/body values must be `slack` or `feishu`; invalid values return 400 `invalid_platform` instead of falling back to Slack
- generic platform-aware chat endpoints
- Generic `/chat/*` JSON/query contracts use canonical `conversationId` and `rootMessageId` fields
- also accepts `conversation_id` and `root_message_id` aliases
- Invalid `platform` values return 400 `invalid_platform` with allowed values `slack` and `feishu`
- Generic file uploads use canonical `filePath` or `contentBase64`
- `file_path` and `content_base64` aliases accepted and named in validation errors
- Inline `contentBase64`/`content_base64` uploads require `filename` and must decode to non-empty file content
- Generic file uploads accept `filePath` or non-empty `content_base64` plus `filename`
- `richText`/`rich_text` and `card` can be structured JSON values or JSON strings
- invalid JSON strings return 400 with only the field name, not the raw payload
- request logging redact message text, state reasons, file comments/alt text, rich/card payloads
- `/integrations/*` request logging redacts MCP call `arguments`
- Registered jobs receive `CHAT_PLATFORM`, `CHAT_CONVERSATION_ID`, and `CHAT_ROOT_MESSAGE_ID`
- legacy Slack `channel_id` and `thread_ts` aliases only for Slack compatibility when `platform` is omitted or set to `slack`
- Invalid generic job `platform` values return 400 `invalid_platform` before coordinate validation
- Job callback `detailsJson`/`details_json` fields and `/integrations/mcp-call` `arguments` can be structured JSON values or JSON strings
- `pnpm test:e2e:feishu-mock` covers the Feishu mock e2e gate, fixture replay, and Slack+Feishu same-process readiness
- `pnpm rfc:feishu-audit` and `pnpm rfc:feishu-audit:local` summarize implementation surfaces, test slices, behavior evidence probes, package-script gates, and remaining real-tenant evidence gaps without sending Feishu messages
- its JSON still keeps `ok=false` until real tenant gates pass
- run `pnpm manual:feishu-smoke -- --preflight --env-file .env`; the extra `--` keeps Node's own `--env-file` flag from intercepting the smoke-checker argument
- smoke CLI value flags also accept `--flag=value`; missing values fail before another flag is swallowed
- secret-bearing values only as set/missing
- generic platform-aware chat endpoints include `curl -sS -X POST http://127.0.0.1:3000/chat/post-message` and `curl -sS -X POST http://127.0.0.1:3000/chat/post-file`
- `limit` (optional positive integer, clamped by `SLACK_HISTORY_API_MAX_LIMIT`; invalid values return 400 `invalid_limit`)
- Generic chat history `limit` uses the same positive-integer validation
- Generic chat history `format` uses the same `text|json` validation before broker delegation
- For Feishu, outbound message images up to 10 MB are uploaded as image messages and fall back to file upload when still within the 30 MB file/resource limit

## What It Expects

- A Slack app using Socket Mode
- For Feishu support, a China Feishu self-built app with bot, long-connection event delivery, message send, card callback, and resource permissions
- Codex authentication via either:
  - `OPENAI_API_KEY`
  - a mounted `auth.json` plus `CODEX_AUTH_JSON_PATH`

## Slack App Setup

Create a Slack app with:

- Socket Mode enabled
- Interactivity enabled
- App-level token with `connections:write`
- Bot token scopes:
  - `app_mentions:read`
  - `chat:write`
  - `channels:history`
  - `files:read` if you want Codex to receive image attachments from Slack messages
  - `files:write` if you want Codex to upload images/files back into Slack threads
  - `users:read` if you want Codex to see Slack display names instead of only raw user IDs
  - `users:read.email` if you want the broker to infer GitHub co-author mappings from Slack profile email

Event subscriptions needed for the current broker flow:

- `app_mention`
- `message.channels`
- `message.im` for direct-message sessions

If you want to support private channels or DMs, add the corresponding `groups:history`, `im:history`, or `mpim:history` scopes plus matching message events.

The broker's Slack co-author flow uses Socket Mode interactive envelopes, thread ephemerals, and modals. With Socket Mode enabled, you do not need a separate public interactivity Request URL for this flow.

## Environment

Copy `.env.example` to `.env` and fill in:

- `SLACK_APP_TOKEN`
- `SLACK_BOT_TOKEN`
- optional `SLACK_INITIAL_THREAD_HISTORY_COUNT`
- optional `SLACK_HISTORY_API_MAX_LIMIT`
- optional `SESSIONS_ROOT`
- optional `REPOS_ROOT`
- optional `LOG_DIR`
- optional `LOG_LEVEL`
- optional `LOG_RAW_SLACK_EVENTS`
- optional `LOG_RAW_CODEX_RPC`
- optional `LOG_RAW_HTTP_REQUESTS`
- optional `LOG_RAW_MAX_BYTES`
- optional disk cleanup settings (`DISK_CLEANUP_*`), including safe-by-default
  dry-run mode and session cache TTL. Keep `DISK_CLEANUP_DRY_RUN=true`
  for the first few days after enabling cleanup, review the structured
  candidate logs, and only set it to `false` after confirming the listed
  paths are expected rebuildable artifacts.
- one Codex auth mode
- optional host Codex home mount if you want the container to inherit your global `~/.codex` memory/instructions

## Codex Auth Modes

### 1. API key

Set:

```env
OPENAI_API_KEY=sk-...
```

This is the simplest automation setup.

### 2. Reuse Codex/ChatGPT OAuth

Mount an existing `auth.json` into the container and set:

```env
CODEX_AUTH_JSON_PATH=/auth/auth.json
```

Then add a read-only volume to `docker-compose.yml`:

```yaml
volumes:
  - ~/.codex/auth.json:/auth/auth.json:ro
```

At startup the broker copies that file into its own `CODEX_HOME`/data directory and uses it to authenticate the embedded Codex app-server.

The main Codex runtime disables all built-in MCP servers by default, and starts the Codex app-server with the `apps` feature disabled so Apps/Connectors are not exposed to model turns. Keep tool access outside the main runtime and use broker-managed integrations instead. MCP removal only affects the broker's container-local Codex config. It does not modify your host `~/.codex/config.toml`.

## Shared Codex Team Home

Broker auth profiles are quota/auth boundaries, not memory boundaries. Shared Codex behavior should live in one team-level home:

```env
CODEX_TEAM_HOME=/app/.data/team-codex-home
HOST_AGENTS_PATH_HOST=/Users/you/.agents
HOST_AGENTS_CONTAINER_PATH=/Users/you/.agents
```

Shared entries include:

- `AGENT.md`
- `AGENTS.md`
- `memory.md`
- `config.toml`
- `memories/`
- `skills/`
- `superpowers/`
- `rules/`
- `vendor_imports/`

Runtime behavior:

- `CODEX_TEAM_HOME` defaults to `.data/team-codex-home`.
- Each auth profile still has its own `CODEX_HOME` for `auth.json`, generated images, cache, logs, and runtime state.
- Shared entries in each profile `CODEX_HOME` are symlinks to `CODEX_TEAM_HOME`.
- New Slack sessions inject personal memory from `CODEX_TEAM_HOME/AGENT.md` once at `thread/start`; later turns reuse the existing session context instead of re-sending it.
- The runtime shell path `~/.codex/AGENT.md` is wired back to `CODEX_TEAM_HOME/AGENT.md`, so agent-written memory updates are visible across auth profiles.
- If the team home is missing and there is no existing shared profile/source content, the broker creates empty shared files/directories only.
- If existing profile/source shared content is present while the team home is empty, the broker preserves the legacy local-copy behavior instead of linking profiles to empty team files.
- Historical profile data migration is a one-off operator action and is intentionally not part of the runtime code path.
- `HOST_AGENTS_PATH_HOST` plus `HOST_AGENTS_CONTAINER_PATH` lets relative skill symlinks like `../../.agents/...` resolve correctly if the team home contains those symlinks.
- For docker-side skills that need to call a host-local helper service, either set an explicit container-safe URL such as `TEMPAD_LINK_SERVICE_URL=http://host.docker.internal:4320`, or leave it unset and let the broker probe the common host-local tempad endpoints automatically.

Before enabling this on an existing machine, seed `CODEX_TEAM_HOME` once from the reviewed canonical profile/global Codex files and keep an external backup of replaced profile-local shared entries. Do not move `auth.json` into the team home.

## Run With Docker Compose

```bash
cp .env.example .env
docker compose up --build
```

Operational scripts for the real container:

```bash
pnpm ops:check:real
pnpm ops:rollout:real
pnpm ops:status:real
pnpm ops:auth:real status
pnpm ops:auth:profiles bootstrap
pnpm ops:auth:profiles status
pnpm ops:auth:profiles list
pnpm ops:auth:profiles import-host --name backup-account
pnpm ops:auth:profiles use backup-account
pnpm ops:ui:real
```

`ops:rollout:real` reuses the configured real container's env vars and bind mounts (default legacy container name: `slack-codex-broker-real`), refuses to restart while active turns exist unless you pass `--allow-active`, rebuilds the image, recreates the container, and then runs the fixed post-update checks. Each rollout also writes sanitized metadata plus pre-rollout logs under `.backups/rollouts/`.
`ops:status:real` prints a structured runtime snapshot for the live container, including health, active sessions, open inbound messages, background jobs, and recent broker logs. Use `--open-inbound-limit` and `--log-lines` to tune output volume.
`ops:auth:real status` prints the live container's Codex auth files, runtime account identity, any quota/usage fields exposed by `account/read`, plus the current session state snapshot.
`ops:auth:profiles` manages a local auth-profile directory under the live data root. The host auth is kept as a reference copy, while the docker auth points at a selectable `active` profile. Use `bootstrap` once, then `import-host --name <profile>` or `import --name <profile> --from <path>` to add more docker-side auth profiles, and `use <profile>` to switch the live container.
`ops:ui:real` starts a local-only admin page on `127.0.0.1` so you can inspect sessions/account state and upload a replacement `auth.json` without using CLI flags directly.

## Run On a macOS VM

The preferred macOS deployment model is package-first:

- build and publish/pack the admin and worker npm packages outside the VM
- run the bootstrap script with the package version to install
- upload `auth.json` later through the admin page
- do all later deploy / rollback operations from the admin page by target and
  package version

There is no host-side code sync or production build step in the normal path.

### First bootstrap on the VM

```bash
npm install -g @agent-session-broker/admin@0.1.2
agent-session-broker-macos-bootstrap --service-root ~/services/workwork --package-version 0.1.2 --start-worker
```

The bootstrap script installs `@agent-session-broker/admin@<version>` and
`@agent-session-broker/worker@<version>` into the service root. Admin launchd
runs through `current-admin`; worker launchd runs through `current-worker`.

Before running it, make sure the Slack app credentials are available through one of these sources:

- the current shell environment, for example `SLACK_APP_TOKEN=... SLACK_BOT_TOKEN=... node scripts/ops/macos-bootstrap.mjs --start-worker`
- an existing `config/broker.env` in the service root, which the bootstrap script will reuse for the new admin / worker env files

What it prepares:

- `releases/admin/npm-<version>/` and `releases/worker/npm-<version>/` package installs
- `current-admin`, `previous-admin`, `failed-admin` release links
- `current-worker`, `previous-worker`, `failed-worker` release links
- shared runtime state under `.data/`
- support homes under `runtime-support/`
- launchd agents for:
  - `io.github.hoolc.agent-session-broker` (admin/control plane)
  - `io.github.hoolc.agent-session-broker.worker` (chat/Codex worker)

What it does not do:

- it does not copy `auth.json`; import auth profiles later through `/admin`
- it does not copy historical sessions, logs, jobs, or repo caches from another machine
- it does not run `pnpm install` or `pnpm build` on the VM

### Runtime layout on the VM

The npm packages are the release units. Runtime services execute code through
their target-specific current release links, not from a source checkout.

- `<service-root>/`:
  - release manager and shared runtime root
- `<service-root>/releases/admin/npm-<version>/`:
  - npm install root for one admin package version
- `<service-root>/releases/worker/npm-<version>/`:
  - npm install root for one worker package version
- `<service-root>/current-admin` and `<service-root>/current-worker`:
  - symlinks to the active installed package roots
- `<service-root>/previous-admin` and `<service-root>/previous-worker`:
  - symlinks to the last good release for each target
- `<service-root>/failed-admin` and `<service-root>/failed-worker`:
  - symlinks to the most recent failed cutover for each target
- `<service-root>/.data/`:
  - shared broker state, sessions, jobs, logs, repos, auth profiles, codex home

### Deploy and rollback

The admin service deploys a selected target and npm package version into a new
release directory. Admin and worker are independent release targets: an admin
deploy switches only `current-admin` and schedules only the admin restart; a
worker deploy switches only `current-worker`, restarts the worker immediately,
and waits for worker readiness.

- deploy:
  - read package versions from the selected target's npm registry entry
  - install the selected package under `releases/<target>/npm-<version>`
  - switch that target's current symlink
  - for worker deploys, restart the worker launchd service
  - run worker health + Codex-ready checks with a 90s startup window, because worker startup can spend tens of seconds reconciling Slack thread state before `/readyz` answers
  - for admin deploys, schedule the admin launchd service restart from `current-admin`
  - auto-rollback on failed cutover
- rollback:
  - switch the requested target back to `previous-*`, or to an explicitly selected installed package version
  - restart only that target's launchd service
  - run worker health checks only for worker rollback

Because old releases stay on disk, rollback is a pointer switch. It does not fetch source or build a missing version.

### Admin surface

```text
GET /admin
GET /readyz
GET /admin/api/status
POST /admin/api/auth-profiles
POST /admin/api/auth-profiles/:name/activate
DELETE /admin/api/auth-profiles/:name
POST /admin/api/github-authors
DELETE /admin/api/github-authors/:slackUserId
POST /admin/api/deploy
POST /admin/api/rollback
```

Typical first-run flow:

1. Open `/admin`.
2. Upload one or more `auth.json` files into Auth Profiles.
3. Activate the profile you want the worker to use.
4. Later, deploy a package version from the Deploy panel.
5. Roll back from the same panel when needed.

The same admin page also exposes a `GitHub Authors` panel for manually maintaining `Slack user -> GitHub author` mappings. Manual entries override Slack-inferred mappings.

If `BROKER_ADMIN_TOKEN` is set, `/admin/api/*` requires that token via `x-admin-token` or `Authorization: Bearer ...`. If it is unset, the admin API is still enabled, so only expose the broker port in environments you trust.

The container image:

- uses Node 22.13+ for the built-in SQLite runtime state store and lint/format toolchain
- installs `git`
- installs `gh`
- installs `rg` via `ripgrep`
- installs the Codex CLI globally via `@openai/codex`
- runs the broker with `node dist/src/index.js`

## Runtime Layout

Inside the container:

- broker state lives under `/app/.data`
- Codex state defaults to `/app/.data/codex-home`
- session workspaces default to `/app/.data/sessions/<channel-thread>/workspace`
- shared canonical repositories live under `/app/.data/repos`
- structured logs default to `/app/.data/logs`

In practice, `.data` is the broker's runtime data root. It contains both durable broker-owned identity/config data and disposable runtime state.

Durable broker-owned identity/config data:

- `codex-home/`
- `auth-profiles/`

Disposable runtime state:

- `state/broker.sqlite`
- `sessions/`
- `jobs/`
- `logs/`
- `repos/`

The macOS bare-run deploy path only reuses the durable broker-owned subset that defines behavior and identity. It intentionally leaves the disposable runtime state behind and starts the VM with a clean `state/`, `sessions/`, `jobs/`, `logs/`, and `repos/`.

## Logging

The broker now keeps a layered JSONL log set intended for postmortem debugging.

Default layout under `LOG_DIR`:

- `broker/<yyyy-mm-dd-hh>.jsonl`
  Hourly global structured application logs for every `info` / `warn` / `error` / `debug` event.
- `sessions/<base64url-session-key>/<yyyy-mm-dd-hh>.jsonl`
  Per-session fan-out log. Useful when one Slack thread goes bad and you want only its history.
- `jobs/<base64url-job-id>/<yyyy-mm-dd-hh>.jsonl`
  Per-background-job fan-out log.
- `raw/slack-events/<yyyy-mm-dd-hh>.jsonl`
  Raw Socket Mode envelopes from Slack.
- `raw/codex-rpc/<yyyy-mm-dd-hh>.jsonl`
  Raw Codex app-server RPC requests, responses, and notifications.
- `raw/http-requests/<yyyy-mm-dd-hh>.jsonl`
  Raw local broker HTTP traffic for `/slack/*` and `/jobs/*`.

Supported environment knobs:

- `LOG_LEVEL=debug|info|warn|error`
- `LOG_RAW_SLACK_EVENTS=true|false`
- `LOG_RAW_CODEX_RPC=true|false`
- `LOG_RAW_HTTP_REQUESTS=true|false`
- `LOG_RAW_MAX_BYTES=131072`
- `DISK_CLEANUP_ENABLED=true|false`
- `DISK_CLEANUP_CHECK_INTERVAL_MS=300000`
- `DISK_CLEANUP_MIN_FREE_BYTES=10737418240`
- `DISK_CLEANUP_TARGET_FREE_BYTES=21474836480`
- `DISK_CLEANUP_INACTIVE_SESSION_MS=86400000`
- `DISK_CLEANUP_JOB_PROTECTION_MS=172800000`
- `DISK_CLEANUP_OLD_LOG_MS=86400000`

Notes:

- Raw logs are intentionally verbose and can grow quickly during long sessions. Oversized raw payloads are truncated to `LOG_RAW_MAX_BYTES` before they are written.
- Admin status reads only a bounded tail of recent broker JSONL files; it does not decode entire log files into memory.
- Broker-managed background jobs are automatically cancelled once they exceed five hours of runtime, including restartable jobs that are already over the limit when the worker boots.
- When free space falls below `DISK_CLEANUP_MIN_FREE_BYTES`, the worker removes old hourly log files first. If space is still below `DISK_CLEANUP_TARGET_FREE_BYTES`, it removes sessions inactive for at least `DISK_CLEANUP_INACTIVE_SESSION_MS`, oldest activity first. Active turns, pending inbound work, and running jobs protect sessions only until `DISK_CLEANUP_JOB_PROTECTION_MS`; older sessions can be removed with their jobs.
- `/slack/post-file` request logging redacts inline `content_base64` payloads into a size marker instead of writing the full blob.
- Session and job log files are written independently, so one noisy thread no longer forces the entire broker state or log history into one giant file.

## Current Interaction Model

- First `@bot ...` in a thread: create or resume the session, ensure the session workspace exists, send the message to Codex
- First `@bot ...` inside an already active human thread: also backfill the most recent earlier thread messages before that mention
- Later plain thread replies: continue the same Codex thread
- Direct message root message: create a session keyed by that DM thread and send it to Codex
- `-stop`: interrupt the current Codex turn
- If the task needs code, Codex should use `/app/.data/repos` for canonical clones and create any worktrees or task directories inside the current session workspace

## Slack Thread History API

The broker exposes a local-only helper endpoint on the same port as the health check:

```bash
curl "http://127.0.0.1:3000/slack/thread-history?channel_id=C123&thread_ts=111.222&before_ts=111.223&limit=20&format=text"
```

Query params:

- `channel_id` (required)
- `thread_ts` (required)
- `before_ts` (optional, exclusive upper bound)
- `limit` (optional, clamped by `SLACK_HISTORY_API_MAX_LIMIT`)
- `channel_type` (optional)
- `format=text|json` (default `json`)

This is meant for Codex itself to pull older Slack context when the initial backfill window is not enough.

## Slack Post APIs

The broker exposes two local-only delivery endpoints for Codex:

### Post text

```bash
curl -sS -X POST http://127.0.0.1:3000/slack/post-message \
  -H 'content-type: application/json' \
  -d '{"channel_id":"C123","thread_ts":"111.222","text":"working on it"}'
```

`text` accepts normal Markdown/markdownish input. The broker converts it to Slack `mrkdwn` before posting.

### Upload a local image or file

```bash
curl -sS -X POST http://127.0.0.1:3000/slack/post-file \
  -H 'content-type: application/json' \
  -d '{"channel_id":"C123","thread_ts":"111.222","file_path":"/absolute/path/to/report.png","initial_comment":"latest screenshot"}'
```

`/slack/post-file` accepts either:

- `file_path` pointing at a local file visible to the broker process
- or `content_base64` plus `filename`

Optional fields:

- `title`
- `initial_comment` (or `text` as an alias)
- `alt_text`
- `snippet_type`
- `content_type`

`initial_comment` accepts normal Markdown/markdownish input and is converted to Slack `mrkdwn` before upload completion.

## Notes

- This compose file is intentionally minimal and does not pre-mount or pre-select any single target repository.
- The runtime image already includes `gh`, `git`, and `rg`.
- The broker no longer manages repo selection or git worktree naming. That is now an agent-level responsibility inside the shared `repos/` cache and the current session workspace.

## GitHub Support

If you want Codex to push branches or open PRs with `gh`:

- set `GH_TOKEN` (and optionally `GITHUB_TOKEN`) to a token with `repo` scope
- mount an SSH agent socket if your repo remote uses `git@github.com:...`

Example:

```env
GH_TOKEN=gho_***
SSH_AUTH_SOCK_HOST=/run/host-services/ssh-auth.sock
SSH_AUTH_SOCK_CONTAINER=/ssh-agent
```

The runtime image includes `gh`, exports your GitHub token to the process environment, and configures git to:

- use `gh auth git-credential` as the credential helper
- rewrite `git@github.com:...` remotes to `https://github.com/...`

That means `gh` and ordinary `git push` can both work with a GitHub token, even if the checked-out repo still uses an SSH-style origin URL.

## License

[MIT](LICENSE)
