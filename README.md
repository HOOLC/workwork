# workwork

Workwork is a Slack + China Feishu bridge/broker that routes chat sessions into Codex app-server sessions with isolated workspaces.

It lets a team talk to Codex from chat while keeping the real work in a broker-managed runtime: sessions, workspaces, logs, auth profiles, GitHub identity mapping, and admin visibility live outside any one target repo.

## What It Does

- Accepts chat requests from Slack threads and China Feishu group topics, then maps each conversation to one Codex app-server thread.
- Keeps follow-up replies, `-stop`, bounded history, attachments, files, cards, and status updates attached to the same session.
- Codex starts in an isolated session workspace. If code work is needed, the agent should use the shared `repos/` cache for canonical clones and create task worktrees under the current session workspace.
- The admin UI shows platform-aware session state, auth profiles, GitHub author mappings, rollout status, and recent sanitized broker logs.

## Core Flow

1. A user mentions the bot in chat.
2. Workwork creates or resumes the matching Codex session.
3. Codex works inside that session workspace and can use the shared repo cache.
4. Replies, files, cards, and status updates go back to the originating conversation.
5. Operators inspect sessions, auth, jobs, logs, and deploy state from `/admin`.

## Quick Start

```bash
cp .env.example .env
docker compose up --build
```

Minimum environment:

```env
SLACK_APP_TOKEN=xapp-...
SLACK_BOT_TOKEN=xoxb-...

# one Codex auth mode
OPENAI_API_KEY=sk-...
# or
CODEX_AUTH_JSON_PATH=/auth/auth.json
```

For Feishu:

```env
FEISHU_ENABLED=true
FEISHU_GROUP_MESSAGE_MODE=all
FEISHU_APP_ID=cli_xxx
FEISHU_APP_SECRET=xxx
FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=false

# at least one Feishu bot identity
FEISHU_BOT_OPEN_ID=ou_xxx
# or FEISHU_BOT_USER_ID=...
# or FEISHU_BOT_UNION_ID=...
```

`at_only` is a visible degraded mode. Set `FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=true` only after the real non-@ follow-up smoke passes.

## Slack Setup

Create a Slack app with:

- Socket Mode enabled
- Interactivity enabled
- an app-level token with `connections:write`
- bot scopes: `app_mentions:read`, `chat:write`, `channels:history`

Useful optional scopes:

- `files:read` and `files:write` for attachments
- `users:read` for display names
- `users:read.email` for GitHub co-author inference
- `groups:history`, `im:history`, or `mpim:history` if you support private channels or DMs

Subscribe to `app_mention`, `message.channels`, and `message.im` as needed for the surfaces you enable.

## Feishu Setup

Feishu rollout:

- Use a China Feishu self-built app, not Lark.
- Feishu support runs in the same broker process as Slack. Feishu group `@bot ...`: create or resume a group session; private chats are ignored.
- Enable bot capability and long-connection event delivery.
- Grant message send, card callback, resource upload/download, and group message receive permissions.
- Configure `FEISHU_APP_ID`, `FEISHU_APP_SECRET`, and at least one Feishu bot identity.
- Production parity needs `FEISHU_GROUP_MESSAGE_MODE=all`; otherwise non-@ follow-ups can be missed.
- For normal operation, keep `LOG_RAW_FEISHU_EVENTS=false` unless collecting a focused, redacted fixture.

<details>
<summary>Feishu evidence and redaction details</summary>

- Capture a sanitized pre-rollout log snapshot; it redacts non-structured lines instead of copying raw Docker log text.
- Smoke and rollout evidence must prove metadata recursively redacts unsafe string fields while preserving safe posture text such as `FEISHU_APP_SECRET=missing`.
- Operator-facing auth status and replacement output summarize filesystem paths instead of echoing full host paths.
- Profile command output also summarizes auth/profile paths without full host filesystem paths.
- Preflight and smoke output records secret-bearing values only as set/missing.

</details>

Real-tenant checks:

```bash
pnpm manual:feishu-smoke -- --preflight --env-file .env
pnpm manual:feishu-smoke -- --setup-evidence-file evidence/feishu-smoke/feishu-setup-evidence.json --output-dir evidence/feishu-smoke
```

More detail lives in:

- [Feishu setup](docs/feishu-setup.md)
- [Feishu permission request](docs/feishu-permission-request.md)
- [RFC 0001](docs/rfcs/0001-slack-feishu-dual-platform.md)
- [RFC 0002](docs/rfcs/0002-feishu-ux-parity-from-codex-feishu-bot.md)

## Admin And APIs

Open `/admin` to inspect sessions, auth profiles, GitHub author mappings, deployment state, platform health, and sanitized logs.

The admin UI exposes platform health and platform-aware session state. Generic chat APIs are platform-aware:

- `POST /chat/post-message`
- `POST /chat/post-file`
- chat history helpers
- broker-managed job callbacks that carry chat coordinates

For generic chat and job APIs, invalid `platform` values return 400 `invalid_platform` with allowed values `slack` and `feishu`.

Legacy Slack aliases such as `channel_id` and `thread_ts` remain for Slack compatibility. Generic chat APIs prefer `conversationId` and `rootMessageId`.

If `BROKER_ADMIN_TOKEN` is set, `/admin/api/*` requires that token via `x-admin-token` or `Authorization: Bearer ...`. If it is unset, expose the broker port only in trusted environments.

<details>
<summary>Detailed API compatibility contract</summary>

- Platform-aware admin views show Slack and Feishu session state, platform health, and safe coordinates without implying that every admin endpoint accepts a `platform` filter.
- allowlisted `recentBrokerLogs` remain cross-platform.
- Generic chat/job `platform` query/body values must be `slack` or `feishu`; invalid values return 400 `invalid_platform` instead of falling back to Slack.
- Generic `/chat/*` JSON/query contracts use canonical `conversationId` and `rootMessageId` fields.
- Generic chat requests also accepts `conversation_id` and `root_message_id` aliases.
- Generic file uploads use canonical `filePath` or `contentBase64`.
- `file_path` and `content_base64` aliases accepted and named in validation errors.
- Inline `contentBase64`/`content_base64` uploads require `filename` and must decode to non-empty file content.
- Generic file uploads also accept `filePath` or non-empty `content_base64` plus `filename`.
- `richText`/`rich_text` and `card` can be structured JSON values or JSON strings.
- invalid JSON strings return 400 with only the field name, not the raw payload.
- request logging redact message text, state reasons, file comments/alt text, rich/card payloads.
- `/integrations/*` request logging redacts MCP call `arguments`.
- Registered jobs receive `CHAT_PLATFORM`, `CHAT_CONVERSATION_ID`, and `CHAT_ROOT_MESSAGE_ID`.
- legacy Slack `channel_id` and `thread_ts` aliases only for Slack compatibility when `platform` is omitted or set to `slack`.
- Invalid generic job `platform` values return 400 `invalid_platform` before coordinate validation.
- Job callback `detailsJson`/`details_json` fields and `/integrations/mcp-call` `arguments` can be structured JSON values or JSON strings.
- Generic chat history `limit` uses the same positive-integer validation.
- Generic chat history `format` uses the same `text|json` validation before broker delegation.
- For Feishu, outbound message images up to 10 MB are uploaded as image messages and fall back to file upload when still within the 30 MB file/resource limit.

```bash
curl -sS -X POST http://127.0.0.1:3000/chat/post-message
curl -sS -X POST http://127.0.0.1:3000/chat/post-file
```

`limit` (optional positive integer, clamped by `SLACK_HISTORY_API_MAX_LIMIT`; invalid values return 400 `invalid_limit`)

</details>

## Codex Auth And Memory

Workwork supports two Codex auth modes:

- `OPENAI_API_KEY` for simple automation
- `CODEX_AUTH_JSON_PATH` for a mounted Codex/ChatGPT OAuth `auth.json`

Auth profiles are quota/auth boundaries, not memory boundaries. Shared behavior should live in `CODEX_TEAM_HOME`, which defaults to `.data/team-codex-home`; profile-specific runtime state stays in that profile's own `CODEX_HOME`.

The embedded Codex runtime starts with built-in MCP servers disabled and Apps/Connectors unavailable. Broker-managed integrations should be exposed through Workwork, not through the main model runtime.

## Deploy

Docker compose is the simplest path for local or single-host use:

```bash
docker compose up --build
```

Operational scripts for the real container:

```bash
pnpm ops:check:real
pnpm ops:rollout:real
pnpm ops:status:real
pnpm ops:auth:real status
pnpm ops:ui:real
```

The default real-container name is still the legacy `slack-codex-broker-real`.

For a macOS VM, the preferred deployment model is package-first: install published admin/worker packages into a service root, switch `current-admin` or `current-worker`, and roll back by switching release pointers.

```bash
npm install -g @agent-session-broker/admin@0.1.2
agent-session-broker-macos-bootstrap --service-root ~/services/workwork --package-version 0.1.2 --start-worker
```

## Data And Logs

Default runtime data lives under `.data/`:

- `state/broker.sqlite`
- `sessions/`
- `jobs/`
- `logs/`
- `repos/`
- `auth-profiles/`
- `team-codex-home/`

Structured logs are JSONL and fan out by broker, session, and job. Raw Slack, Feishu, Codex RPC, and HTTP request logging are opt-in and size-limited. Disk cleanup is safe-by-default; keep `DISK_CLEANUP_DRY_RUN=true` until candidate logs match expected rebuildable artifacts.

## Validation

```bash
pnpm lint
pnpm build
pnpm test
pnpm test:e2e:feishu-mock
pnpm rfc:feishu-audit
pnpm rfc:feishu-audit:local
pnpm rfc:feishu-completion-audit -- --json
```

`pnpm rfc:feishu-audit` and `pnpm rfc:feishu-audit:local` summarize implementation surfaces, test slices, behavior evidence probes, package-script gates, and remaining real-tenant evidence gaps without sending Feishu messages.

`pnpm test:e2e:feishu-mock` covers the Feishu mock e2e gate, fixture replay, and Slack+Feishu same-process readiness.

The RFC audit is conservative: its JSON still keeps `ok=false` until real tenant gates pass. The smoke CLI command uses `pnpm manual:feishu-smoke -- --preflight --env-file .env`; the extra `--` keeps Node's own `--env-file` flag from intercepting the smoke-checker argument. Smoke value flags also accept `--flag=value`, and missing values fail before another flag is swallowed.

## GitHub Support

To let Codex push branches or open PRs:

```env
GH_TOKEN=gho_***
SSH_AUTH_SOCK_HOST=/run/host-services/ssh-auth.sock
SSH_AUTH_SOCK_CONTAINER=/ssh-agent
```

The runtime image includes `gh`, `git`, and `rg`; it configures GitHub token auth for both `gh` and ordinary `git push`.

## License

[MIT](LICENSE)
