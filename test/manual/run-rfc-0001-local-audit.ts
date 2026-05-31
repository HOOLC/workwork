#!/usr/bin/env node

import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { evaluateFeishuSetupEvidence, evaluateFeishuSmokePreflight, evaluateFeishuSmokeStatusFile, formatFeishuSmokeCliError, loadFeishuSmokeEnv } from "./run-real-feishu-smoke.js";

type AuditStatus = "pass" | "missing";

export interface Rfc0001AuditCheck {
  readonly id: string;
  readonly label: string;
  readonly status: AuditStatus;
  readonly evidence: readonly string[];
  readonly nextAction?: string | undefined;
}

export interface Rfc0001AuditReport {
  readonly ok: boolean;
  readonly checkedAt: string;
  readonly localOk: boolean;
  readonly realTenantOk: boolean;
  readonly localChecks: readonly Rfc0001AuditCheck[];
  readonly realTenantChecks: readonly Rfc0001AuditCheck[];
  readonly nextActions: readonly string[];
}

export interface Rfc0001AuditCliReport extends Rfc0001AuditReport {
  readonly mode: "full" | "local";
  readonly exitOk: boolean;
}

interface AuditOptions {
  readonly cwd?: string | undefined;
  readonly env?: Record<string, string | undefined> | undefined;
  readonly evidenceDir?: string | undefined;
}

interface CliOptions extends AuditOptions {
  readonly json: boolean;
  readonly help: boolean;
  readonly localOnly: boolean;
}

const REQUIRED_PREFLIGHT_CHECK_IDS = [
  "preflight.slack_credentials_present",
  "preflight.feishu_enabled",
  "preflight.feishu_credentials_present",
  "preflight.feishu_bot_identity_present",
  "scope.china_feishu",
  "preflight.feishu_api_base_china",
  "preflight.group_message_mode_all",
  "preflight.startup_required",
  "preflight.raw_feishu_events_disabled",
];

const RFC_DEEP_DIVES = ["architecture.md", "implementation.md", "test-plan.md", "observability.md", "permissions.md", "review-gates.md"];

const REQUIRED_PACKAGE_SCRIPTS: ReadonlyArray<{
  readonly name: string;
  readonly includes?: string | undefined;
}> = [
  { name: "test", includes: "vitest run" },
  { name: "test:e2e:feishu-mock", includes: "test/dual-platform-runtime.test.ts" },
  { name: "manual:feishu-smoke", includes: "run-real-feishu-smoke.ts --" },
  { name: "manual:self-regression", includes: "run-self-regression.ts --" },
  { name: "rfc:feishu-audit", includes: "run-rfc-0001-local-audit.ts" },
  { name: "rfc:feishu-audit:local", includes: "--local-only" },
  { name: "rfc:feishu-completion-audit", includes: "run-rfc-0001-completion-audit.ts" },
  { name: "rfc:feishu-test-plan", includes: "run-rfc-0001-test-plan.ts" },
  { name: "ops:auth:real", includes: "scripts/ops/auth-real.mjs" },
  { name: "ops:auth:profiles", includes: "scripts/ops/auth-profiles.mjs" },
  { name: "ops:ui:real", includes: "scripts/ops/auth-ui-real.mjs" },
  { name: "ops:rollout:real", includes: "scripts/ops/rollout-real.mjs" },
  { name: "ops:check:real", includes: "scripts/ops/check-real.mjs" },
  { name: "ops:status:real", includes: "scripts/ops/status-real.mjs" },
];

export const RFC0001_REQUIRED_LOCAL_IMPLEMENTATION_FILES = [
  ".env.example",
  "src/config.ts",
  "src/index.ts",
  "src/worker-index.ts",
  "src/store/state-store.ts",
  "src/services/chat/chat-platform-adapter.ts",
  "src/services/chat/chat-session-key.ts",
  "src/services/chat/chat-types.ts",
  "src/services/codex/slack-thread-base-instructions.ts",
  "src/services/codex/prompts/slack-thread-base-instructions.md",
  "src/services/session-manager.ts",
  "src/services/feishu/feishu-api.ts",
  "src/services/feishu/feishu-codex-bridge.ts",
  "src/services/feishu/feishu-event-parser.ts",
  "src/services/feishu/feishu-platform-adapter.ts",
  "src/services/github-author-mapping-service.ts",
  "src/services/job-manager.ts",
  "src/services/slack/slack-agent-bridge.ts",
  "src/services/slack/slack-conversation-service.ts",
  "src/services/slack/slack-coauthor-service.ts",
  "src/tools/git-coauthor.ts",
  "src/http/admin-routes.ts",
  "src/http/chat-routes.ts",
  "src/http/common.ts",
  "src/http/integration-routes.ts",
  "src/http/job-routes.ts",
  "src/http/router.ts",
  "src/http/slack-routes.ts",
  "src/http/request-log-redaction.ts",
  "src/services/admin-service.ts",
  "scripts/ops/auth-real.mjs",
  "scripts/ops/auth-real-lib.mjs",
  "scripts/ops/auth-profiles.mjs",
  "scripts/ops/auth-ui-real.mjs",
  "scripts/ops/lib.mjs",
  "scripts/ops/rollout-real.mjs",
  "scripts/ops/check-real.mjs",
  "scripts/ops/status-real.mjs",
];

export const RFC0001_REQUIRED_LOCAL_TEST_FILES = [
  "test/config.test.ts",
  "test/chat-routes.test.ts",
  "test/chat-session-key.test.ts",
  "test/session-manager.test.ts",
  "test/dual-platform-runtime.test.ts",
  "test/e2e-broker.test.ts",
  "test/app-server-client.test.ts",
  "test/feishu-api.test.ts",
  "test/feishu-codex-bridge.test.ts",
  "test/feishu-event-parser.test.ts",
  "test/feishu-fixture-replay.test.ts",
  "test/feishu-platform-adapter.test.ts",
  "test/feishu-real-smoke.test.ts",
  "test/admin-routes.test.ts",
  "test/admin-routes-part2.test.ts",
  "test/admin-service.test.ts",
  "test/admin-service-part2.test.ts",
  "test/github-author-mapping-service.test.ts",
  "test/git-coauthor-helper.test.ts",
  "test/http-request-log-redaction.test.ts",
  "test/integration-routes.test.ts",
  "test/job-manager.test.ts",
  "test/job-routes.test.ts",
  "test/ops-feishu-preflight.test.ts",
  "test/slack-routes.test.ts",
  "test/self-regression-runner.test.ts",
  "test/rfc-0001-completion-audit.test.ts",
  "test/rfc-0001-docs.test.ts",
  "test/rfc-0001-test-plan.test.ts",
];

export const RFC0001_REQUIRED_LOCAL_FIXTURE_FILES = [
  "test/fixtures/feishu/group-at-text.json",
  "test/fixtures/feishu/private-text.json",
  "test/fixtures/feishu/group-app-self-message.json",
  "test/fixtures/feishu/group-followup-text.json",
  "test/fixtures/feishu/group-followup-parent-only.json",
  "test/fixtures/feishu/group-rich-post.json",
  "test/fixtures/feishu/group-interactive-card.json",
  "test/fixtures/feishu/card-action-trigger.json",
  "test/fixtures/feishu/card-action-skip.json",
  "test/fixtures/feishu/duplicate-message.json",
  "test/fixtures/feishu/group-image.json",
  "test/fixtures/feishu/group-file.json",
  "test/fixtures/feishu/history-page.json",
];

export const RFC0001_REQUIRED_LOCAL_EVIDENCE_PATTERNS: ReadonlyArray<{
  readonly id: string;
  readonly file: string;
  readonly snippets: readonly string[];
}> = [
  {
    id: "env.feishu_rollout_flags",
    file: ".env.example",
    snippets: ["FEISHU_ENABLED=false", "FEISHU_DOMAIN=feishu", "FEISHU_API_BASE_URL=https://open.feishu.cn/open-apis", "FEISHU_GROUP_MESSAGE_MODE=all", "FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=false", "FEISHU_STARTUP_REQUIRED=true", "LOG_RAW_FEISHU_EVENTS=false"],
  },
  {
    id: "config.feishu_flags",
    file: "src/config.ts",
    snippets: ["Missing required environment variable: FEISHU_APP_ID", "Missing required environment variable: FEISHU_APP_SECRET", "Invalid FEISHU_DOMAIN: expected feishu", "Invalid FEISHU_API_BASE_URL: expected https://open.feishu.cn", "logRawFeishuEvents"],
  },
  {
    id: "session.legacy_slack_isolation",
    file: "test/feishu-codex-bridge.test.ts",
    snippets: ["sessions.getChatSession({", 'platform: "feishu"', 'codexThreadId: "thread-1"'],
  },
  {
    id: "http.slack_compat_wrappers",
    file: "src/http/slack-routes.ts",
    snippets: ["readThreadHistory", "postChatMessage", "postChatState", "postChatFile", 'platform: "slack"'],
  },
  {
    id: "runtime.slack_e2e_regression",
    file: "src/services/slack/slack-agent-bridge.ts",
    snippets: ["chat.message.accepted", "chat.outbound.posted", 'platform: "slack"'],
  },
  {
    id: "parser.group_mentions",
    file: "test/feishu-event-parser.test.ts",
    snippets: ["parses group mentions as bot mention inputs"],
  },
  {
    id: "parser.private_self_ignore",
    file: "test/feishu-event-parser.test.ts",
    snippets: ["ignores private chats", "ignored_self"],
  },
  {
    id: "bridge.group_session",
    file: "test/feishu-codex-bridge.test.ts",
    snippets: ["starts a Feishu group mention as a persisted Codex session"],
  },
  {
    id: "bridge.non_at_followup",
    file: "test/feishu-codex-bridge.test.ts",
    snippets: ["steers non-mention group follow-ups into an active Feishu session in all mode", "steers rootless all-message group follow-ups into the active Feishu session for that group"],
  },
  {
    id: "bridge.stop_history",
    file: "test/feishu-codex-bridge.test.ts",
    snippets: ["interrupts the active Feishu turn when the same group sends stop", "starts a recovered Feishu turn for recently active sessions after restart", "marks Feishu history recovery degraded when no last observed cursor is persisted"],
  },
  {
    id: "bridge.resources_coauthor",
    file: "test/feishu-codex-bridge.test.ts",
    snippets: [
      "downloads Feishu image attachments into Codex image input",
      "uploads Feishu files through the adapter and logs replay coordinates",
      "confirms Feishu co-authors through a card callback before resolving commit trailers",
      "records ordered Feishu co-author card callback evidence through the platform adapter",
    ],
  },
  {
    id: "adapter.long_connection_content",
    file: "test/feishu-platform-adapter.test.ts",
    snippets: ["starts a long-connection dispatcher and forwards group mention events", "logs retained Feishu rich payloads by reference without copying bodies", "logs Feishu image and file messages as resource msgTypes with retained payload references", "routes Feishu card callbacks through the interactive handler"],
  },
  {
    id: "api.feishu_resource_transfer",
    file: "test/feishu-api.test.ts",
    snippets: ["downloads message resources as data URLs", "rejects message resource downloads whose headers exceed the configured size limit", "rejects message resource downloads with unexpected content types before reading the body", "uploads message images and files before they are sent"],
  },
  {
    id: "http.chat_payload_contract",
    file: "test/chat-routes.test.ts",
    snippets: ["rejects invalid rich/card JSON fields before delegation", "documents canonical chat file source names in missing-source errors", "rejects invalid inline chat file content before delegation"],
  },
  {
    id: "http.integration_mcp_arguments",
    file: "test/integration-routes.test.ts",
    snippets: ["calls an isolated MCP tool through the broker router", "accepts MCP call arguments as a JSON string", "rejects invalid MCP call arguments JSON before delegation"],
  },
  {
    id: "runtime.dual_platform",
    file: "test/dual-platform-runtime.test.ts",
    snippets: ["starts Slack Socket Mode and a real Feishu bridge in one broker runtime", "keeps Slack ready when optional Feishu startup degrades", "reports at_only as degraded after Feishu long connection starts", "fails fast before Slack Socket Mode when required Feishu startup fails"],
  },
  {
    id: "prompt.platform_aware_template",
    file: "src/services/codex/prompts/slack-thread-base-instructions.md",
    snippets: ["{{chat_surface_name}}", "{{thread_coordinates_section}}", "{{post_state_route_label}}", "{{registered_job_env_vars}}"],
  },
  {
    id: "prompt.feishu_platform_runtime_instructions",
    file: "src/services/codex/slack-thread-base-instructions.ts",
    snippets: ['const postStateRoute = isSlack ? "/slack/post-state" : "/chat/post-state"', "SLACK_CHANNEL_ID", "SLACK_THREAD_TS", "CHAT_PLATFORM", "CHAT_CONVERSATION_ID", "CHAT_ROOT_MESSAGE_ID"],
  },
  {
    id: "smoke.final_evidence_gates",
    file: "test/feishu-real-smoke.test.ts",
    snippets: [
      "requires group @bot accepted evidence, not only an existing Feishu session",
      "requires non-@ follow-up evidence to match the steered or resumed message",
      "requires Feishu co-author card confirmation for final RFC smoke readiness",
      "requires rich text, card, image, and file evidence before passing resource smoke",
      "requires Slack event and reply evidence in the shared runtime",
    ],
  },
  {
    id: "smoke.saved_evidence_requires_setup",
    file: "test/feishu-real-smoke.test.ts",
    snippets: ["can evaluate a saved admin status evidence file", "const missingSetupReport = await evaluateFeishuSmokeStatusFile(statusFile", "expect(missingSetupReport.ok).toBe(false)", "setupEvidenceFile", "expect(report.ok).toBe(true)"],
  },
  {
    id: "self_regression.shared_runner",
    file: "test/self-regression-runner.test.ts",
    snippets: ["checks Slack preflight without exposing token values", "replays Slack admin status for roundtrip, status, and file evidence", "uses the Feishu smoke evaluator for Feishu preflight under the shared command"],
  },
  {
    id: "admin.platform_health",
    file: "test/admin-service.test.ts",
    snippets: ["bounds slow runtime status probes so overview can still answer"],
  },
  {
    id: "admin.recent_broker_logs",
    file: "test/admin-service-part2.test.ts",
    snippets: ["reads recent broker logs from a bounded tail instead of decoding whole files"],
  },
  {
    id: "admin.github_author_mappings",
    file: "test/admin-routes-part2.test.ts",
    snippets: ["forwards GitHub author mapping upserts to the admin service", "deleteGitHubAuthorMapping"],
  },
  {
    id: "mapping.platform_aware_authors",
    file: "test/github-author-mapping-service.test.ts",
    snippets: ["keeps Slack and Feishu author mappings separate for the same user id"],
  },
  {
    id: "jobs.route_coordinates",
    file: "test/job-routes.test.ts",
    snippets: ["registers jobs with canonical platform-aware chat coordinates", "invalid_platform"],
  },
  {
    id: "jobs.manager_feishu_coordinates",
    file: "src/services/job-manager.ts",
    snippets: ["CHAT_PLATFORM", "CHAT_CONVERSATION_ID", "CHAT_ROOT_MESSAGE_ID"],
  },
  {
    id: "ops.redaction_rollout",
    file: "test/ops-feishu-preflight.test.ts",
    snippets: ["summarizes platform health from admin status without copying recent logs", "sanitizes rollout preflight docker logs before writing evidence"],
  },
  {
    id: "ops.status_check_real_sanitization",
    file: "test/ops-feishu-preflight.test.ts",
    snippets: [
      "summarizes detailed host state without copying raw inbound bodies, job tokens, or raw broker logs",
      "OPS_STATUS_SECRET_BODY",
      "OPS_STATUS_JOB_TOKEN_SECRET",
      "OPS_STATUS_RAW_LINE_SECRET",
      "readDetailedStateFromHost(dataRoot",
      "workspacePathBasename",
      "cwdBasename",
      "log_parse_error",
      "summarizes ops host paths without exposing full filesystem paths",
      "summarizes rollout evidence paths as safe repo-relative coordinates",
      "summarizeOpsEvidencePath(rolloutPath)",
    ],
  },
  {
    id: "ops.auth_path_redaction",
    file: "test/ops-feishu-preflight.test.ts",
    snippets: ["formats operator-facing paths without exposing full host filesystem paths", "auth.json (path redacted)", "[redacted-path] (path redacted)"],
  },
  {
    id: "ops.auth_real_path_summarization",
    file: "scripts/ops/auth-real-lib.mjs",
    snippets: ["summarizeOpsDisplayPath(filePath)", "codexHome: summarizeOpsDisplayPath(codexHome)", "target: summarizeOpsDisplayPath(entry.target)"],
  },
  {
    id: "ops.auth_profiles_path_summarization",
    file: "scripts/ops/auth-profiles.mjs",
    snippets: ['const sourcePath = requireOption(options.sourcePath, "--from")', "const targetPath = dockerProfilePath(paths, profileName)", "await fs.copyFile(sourcePath, targetPath)"],
  },
  {
    id: "ops.auth_ui_reuses_sanitized_status",
    file: "scripts/ops/auth-ui-real.mjs",
    snippets: ["getAuthRealStatus", "replaceAuthInRealContainer", 'esc(payload.restartAction || "updated")'],
  },
  {
    id: "http.generic_chat_redaction",
    file: "test/chat-routes.test.ts",
    snippets: ["posts Feishu rich/card messages through generic chat coordinates", "uploads Feishu inline files through generic chat coordinates"],
  },
  {
    id: "http.request_log_redaction",
    file: "test/http-request-log-redaction.test.ts",
    snippets: ["redacts body-like fields from generic chat route raw request logs", "redacts MCP call arguments from integration route raw request logs"],
  },
  {
    id: "docs.readme_user_surface",
    file: "README.md",
    snippets: [
      "Slack + China Feishu bridge",
      "FEISHU_ENABLED=true",
      "same broker process",
      "Feishu group `@bot ...`: create or resume a group session; private chats are ignored",
      "FEISHU_GROUP_MESSAGE_MODE=all",
      "`at_only` is a visible degraded mode",
      "Generic chat APIs are platform-aware",
      "For generic chat and job APIs, invalid `platform` values return 400 `invalid_platform` with allowed values `slack` and `feishu`",
      "pnpm test:e2e:feishu-mock",
      "pnpm rfc:feishu-audit",
      "pnpm rfc:feishu-audit:local",
      "remaining real-tenant evidence gaps without sending Feishu messages",
      "pnpm manual:feishu-smoke -- --preflight --env-file .env",
    ],
  },
  {
    id: "docs.feishu_setup_runbook",
    file: "docs/feishu-setup.md",
    snippets: [
      "Target China Feishu Open Platform only.",
      "Treat private chats as unsupported",
      "Request all group message delivery capability for `im:message.group_msg`.",
      "FEISHU_GROUP_MESSAGE_MODE=all",
      "pnpm manual:feishu-smoke -- --preflight --env-file .env --output-dir evidence/feishu-smoke",
      "pnpm rfc:feishu-audit:local",
      "Final smoke and saved `--status-file` verification require `--setup-evidence-file`",
      "The output `admin-status.json` is sanitized by the smoke checker",
      "Broker starts with Slack and Feishu enabled in one process.",
      "Feishu co-author candidate confirmation card is clicked after",
      "Set `FEISHU_ENABLED=false` if Feishu must be disabled while Slack continues.",
    ],
  },
  {
    id: "docs.permission_request_packet",
    file: "docs/feishu-permission-request.md",
    snippets: [
      "China Feishu group support",
      "Private-chat product support is out of scope",
      "All group message delivery, `im:message.group_msg`",
      "FEISHU_GROUP_MESSAGE_MODE=at_only",
      "not production parity",
      "Normal info/warn logs exclude message body text",
      "Raw Feishu event logging is disabled by default",
      "broker starts with Slack and Feishu enabled in one process",
      "pnpm manual:feishu-smoke -- --env-file .env --base-url",
    ],
  },
  {
    id: "docs.progressive_gates",
    file: "test/rfc-0001-docs.test.ts",
    snippets: ["keeps completion evidence progressive after real-tenant signoff", "keeps the README aligned with the Slack + Feishu user-facing surface"],
  },
  {
    id: "docs.test_plan_automation",
    file: "test/rfc-0001-test-plan.test.ts",
    snippets: ["passes against the checked-in RFC 0001 test plan", "fails when the default acceptance command set drifts", "fails when a documented proof file is missing"],
  },
  {
    id: "completion.final_gate",
    file: "test/rfc-0001-completion-audit.test.ts",
    snippets: ["fails closed until Slack drive evidence is present", "passes for sanitized self-regression and coding smoke evidence bundles"],
  },
];

export async function collectRfc0001LocalAudit(options: AuditOptions = {}): Promise<Rfc0001AuditReport> {
  const cwd = options.cwd ?? process.cwd();
  const env = options.env ?? process.env;
  const evidenceDir = options.evidenceDir ?? path.join(cwd, "evidence", "feishu-smoke");
  const localChecks = await collectLocalChecks(cwd);
  const realTenantChecks = await collectRealTenantChecks(cwd, env, evidenceDir);
  const localOk = localChecks.every((check) => check.status === "pass");
  const realTenantOk = realTenantChecks.every((check) => check.status === "pass");
  const allChecks = [...localChecks, ...realTenantChecks];

  return {
    ok: localOk && realTenantOk,
    checkedAt: new Date().toISOString(),
    localOk,
    realTenantOk,
    localChecks,
    realTenantChecks,
    nextActions: allChecks.filter((check) => check.status !== "pass" && check.nextAction).map((check) => `${check.id}: ${check.nextAction}`),
  };
}

export function createRfc0001AuditCliReport(report: Rfc0001AuditReport, options: { readonly localOnly?: boolean | undefined }): Rfc0001AuditCliReport {
  const mode = options.localOnly ? "local" : "full";
  return {
    ...report,
    mode,
    exitOk: mode === "local" ? report.localOk : report.ok,
  };
}

async function collectLocalChecks(cwd: string): Promise<Rfc0001AuditCheck[]> {
  const packageScripts = await readPackageScripts(cwd);
  const checks: Rfc0001AuditCheck[] = [];

  checks.push(await fileCheck(cwd, "local.rfc_entry", "RFC 0001 progressive-disclosure entry exists", "docs/rfcs/0001-slack-feishu-dual-platform.md", "Restore the RFC 0001 entry document before claiming local RFC readiness."));

  const missingDeepDives: string[] = [];
  for (const file of RFC_DEEP_DIVES) {
    if (!(await pathExists(path.join(cwd, "docs", "rfcs", "0001-slack-feishu-dual-platform", file)))) {
      missingDeepDives.push(file);
    }
  }
  checks.push({
    id: "local.rfc_deep_dives",
    label: "RFC 0001 deep-dive files exist",
    status: missingDeepDives.length === 0 ? "pass" : "missing",
    evidence: missingDeepDives.length === 0 ? RFC_DEEP_DIVES.map((file) => `present=${file}`) : missingDeepDives.map((file) => `missing=${file}`),
    nextAction: missingDeepDives.length === 0 ? undefined : "Restore every RFC deep-dive file before claiming local RFC readiness.",
  });

  checks.push(await fileCheck(cwd, "local.feishu_setup_doc", "Feishu setup and real-smoke checklist exists", "docs/feishu-setup.md", "Restore docs/feishu-setup.md before asking reviewers to perform tenant setup."));
  checks.push(await fileCheck(cwd, "local.permission_request_packet", "Feishu permission request packet exists", "docs/feishu-permission-request.md", "Restore docs/feishu-permission-request.md so operators have the approval-request packet needed for im:message.group_msg."));
  checks.push(await setupEvidenceTemplateCheck(cwd));
  checks.push(await fileCheck(cwd, "local.smoke_checker", "Real Feishu smoke checker exists", "test/manual/run-real-feishu-smoke.ts", "Restore the smoke checker before claiming RFC 0001 can be verified."));
  checks.push(
    await filesCheck(
      cwd,
      "local.implementation_surfaces",
      "RFC 0001 implementation surfaces exist",
      RFC0001_REQUIRED_LOCAL_IMPLEMENTATION_FILES,
      "Restore the environment template, config/startup, platform-neutral chat/session, Slack compatibility, Feishu, admin health, co-author, generic HTTP/job/integration, and ops rollout/check/status implementation files before claiming local RFC readiness.",
    ),
  );
  checks.push(
    await filesCheck(
      cwd,
      "local.test_slices",
      "RFC 0001 local test slices exist",
      RFC0001_REQUIRED_LOCAL_TEST_FILES,
      "Restore the config, Slack compatibility/e2e, Feishu parser/API/bridge/adapter/fixture/runtime, admin, co-author, generic chat/job/integration, redaction, ops, smoke, and RFC doc tests before claiming local RFC readiness.",
    ),
  );
  checks.push(await filesCheck(cwd, "local.feishu_fixtures", "RFC 0001 Feishu replay fixtures exist", RFC0001_REQUIRED_LOCAL_FIXTURE_FILES, "Restore every required Feishu replay fixture from the RFC fixture ledger before claiming local RFC readiness."));
  checks.push(
    await contentPatternsCheck(
      cwd,
      "local.behavior_evidence",
      "RFC 0001 local behavior evidence is still covered",
      RFC0001_REQUIRED_LOCAL_EVIDENCE_PATTERNS,
      "Restore the named local evidence probes so Slack compatibility, parser, runtime, resource, co-author, redaction, ops, and real-smoke evaluator coverage still matches the RFC ledger.",
    ),
  );

  const scriptEvidence = REQUIRED_PACKAGE_SCRIPTS.map((script) => {
    const command = packageScripts[script.name];
    const ok = command && (!script.includes || command.includes(script.includes));
    return {
      script,
      command,
      ok: Boolean(ok),
    };
  });
  checks.push({
    id: "local.package_scripts",
    label: "RFC verification package scripts are wired",
    status: scriptEvidence.every((item) => item.ok) ? "pass" : "missing",
    evidence: scriptEvidence.map((item) => (item.ok ? `${item.script.name}=present` : `${item.script.name}=missing_or_unexpected`)),
    nextAction: scriptEvidence.every((item) => item.ok) ? undefined : "Restore package scripts for local tests, Feishu mock e2e, smoke, rollout, check, and status commands.",
  });

  return checks;
}

async function collectRealTenantChecks(cwd: string, env: Record<string, string | undefined>, evidenceDir: string): Promise<Rfc0001AuditCheck[]> {
  const checks: Rfc0001AuditCheck[] = [];
  const envFile = path.join(cwd, ".env");
  const setupEvidenceFile = path.join(evidenceDir, "feishu-setup-evidence.json");
  const statusFile = path.join(evidenceDir, "admin-status.json");
  const preflightReportFile = path.join(evidenceDir, "feishu-preflight-report.json");

  checks.push(await preflightCheck(env, (await pathExists(envFile)) ? envFile : undefined, preflightReportFile));
  checks.push(await setupEvidenceCheck(setupEvidenceFile));
  checks.push(await savedSmokeEvidenceCheck(statusFile, setupEvidenceFile, env));

  return checks;
}

async function preflightCheck(env: Record<string, string | undefined>, envFile: string | undefined, preflightReportFile: string): Promise<Rfc0001AuditCheck> {
  let liveFailure: Rfc0001AuditCheck | undefined;

  try {
    const loadedEnv = await loadFeishuSmokeEnv(env, envFile);
    const report = evaluateFeishuSmokePreflight(loadedEnv);
    const check = {
      id: "real.preflight",
      label: "Rollout environment preflight passes",
      status: report.ok ? "pass" : "missing",
      evidence: ["source=live_env", ...report.checks.map((check) => `${check.id}=${check.status}`)],
      nextAction: report.ok ? undefined : "Provide Slack and Feishu rollout credentials, bot identity, China Feishu mode, all-message mode, strict startup, and raw logging posture, then rerun preflight.",
    } satisfies Rfc0001AuditCheck;
    if (check.status === "pass") {
      return check;
    }
    liveFailure = check;
  } catch (error) {
    liveFailure = {
      id: "real.preflight",
      label: "Rollout environment preflight passes",
      status: "missing",
      evidence: [formatFeishuSmokeCliError(error)],
      nextAction: "Fix the rollout environment or .env file, then rerun preflight.",
    };
  }

  const savedReportCheck = await savedPreflightReportCheck(preflightReportFile);
  if (savedReportCheck.status === "pass") {
    return savedReportCheck;
  }

  return {
    id: "real.preflight",
    label: "Rollout environment preflight passes",
    status: "missing",
    evidence: [...(liveFailure?.evidence ?? []), ...savedReportCheck.evidence],
    nextAction: liveFailure?.nextAction ?? savedReportCheck.nextAction,
  };
}

async function savedPreflightReportCheck(preflightReportFile: string): Promise<Rfc0001AuditCheck> {
  if (!(await pathExists(preflightReportFile))) {
    return {
      id: "real.preflight",
      label: "Rollout environment preflight passes",
      status: "missing",
      evidence: [`missing=${path.basename(preflightReportFile)}`],
      nextAction: "Run pnpm manual:feishu-smoke -- --preflight --env-file .env --output-dir evidence/feishu-smoke --json and commit the sanitized preflight report.",
    };
  }

  try {
    const report = JSON.parse(await fs.readFile(preflightReportFile, "utf8")) as unknown;
    const root = asRecord(report);
    const checks = asArray(root.checks).map(asRecord);
    const missing: string[] = [];
    if (root.ok !== true) missing.push(`ok=${String(root.ok)}`);
    if (reportContainsUnsafeText(report)) missing.push("unsafe_text=present");
    for (const checkId of REQUIRED_PREFLIGHT_CHECK_IDS) {
      const check = checks.find((candidate) => readString(candidate.id) === checkId);
      if (!check) {
        missing.push(`missing_check=${checkId}`);
        continue;
      }
      if (check.required !== true) missing.push(`${checkId}.required=${String(check.required)}`);
      if (check.status !== "pass") missing.push(`${checkId}.status=${String(check.status)}`);
    }

    return {
      id: "real.preflight",
      label: "Rollout environment preflight passes",
      status: missing.length === 0 ? "pass" : "missing",
      evidence: missing.length === 0 ? ["source=saved_preflight_report", `present=${path.basename(preflightReportFile)}`, ...REQUIRED_PREFLIGHT_CHECK_IDS.map((checkId) => `${checkId}=pass`)] : missing,
      nextAction: missing.length === 0 ? undefined : "Refresh evidence/feishu-smoke/feishu-preflight-report.json with a passing, sanitized preflight bundle.",
    };
  } catch (error) {
    return {
      id: "real.preflight",
      label: "Rollout environment preflight passes",
      status: "missing",
      evidence: [formatFeishuSmokeCliError(error)],
      nextAction: "Fix evidence/feishu-smoke/feishu-preflight-report.json, then rerun the audit.",
    };
  }
}

async function setupEvidenceCheck(setupEvidenceFile: string): Promise<Rfc0001AuditCheck> {
  if (!(await pathExists(setupEvidenceFile))) {
    return {
      id: "real.setup_evidence",
      label: "Real tenant setup evidence passes",
      status: "missing",
      evidence: [`missing=${path.basename(setupEvidenceFile)}`],
      nextAction: "Fill evidence/feishu-smoke/feishu-setup-evidence.json with exact real tenant labels and redacted approval/configuration evidence.",
    };
  }

  try {
    const setupEvidence = JSON.parse(await fs.readFile(setupEvidenceFile, "utf8")) as unknown;
    const check = evaluateFeishuSetupEvidence(setupEvidence);
    return {
      id: "real.setup_evidence",
      label: "Real tenant setup evidence passes",
      status: check.status === "pass" ? "pass" : "missing",
      evidence: check.evidence,
      nextAction: check.status === "pass" ? undefined : check.nextAction,
    };
  } catch (error) {
    return {
      id: "real.setup_evidence",
      label: "Real tenant setup evidence passes",
      status: "missing",
      evidence: [formatFeishuSmokeCliError(error)],
      nextAction: "Fix the setup evidence JSON, then rerun the audit.",
    };
  }
}

async function setupEvidenceTemplateCheck(cwd: string): Promise<Rfc0001AuditCheck> {
  const relativePath = "docs/feishu-setup-evidence.example.json";
  const filePath = path.join(cwd, relativePath);
  if (!(await pathExists(filePath))) {
    return {
      id: "local.setup_evidence_template",
      label: "Sanitized setup evidence template is a safe placeholder",
      status: "missing",
      evidence: [`missing=${relativePath}`],
      nextAction: "Restore docs/feishu-setup-evidence.example.json so operators have a safe starting point.",
    };
  }

  try {
    const setupEvidence = JSON.parse(await fs.readFile(filePath, "utf8")) as unknown;
    const check = evaluateFeishuSetupEvidence(setupEvidence);
    const hasSafeEvidence = check.evidence.includes("setup evidence contains no raw secrets, tokens, user emails, or raw bot IDs");
    const hasPlaceholderEvidence = check.evidence.some((entry) => entry.startsWith("placeholder setup evidence:"));
    const hasMissingPostureEvidence = check.evidence.some((entry) => entry.startsWith("missing permission posture:"));
    const expectedEvidence = ["target=china_feishu", "consoleLabels=9/9", "im:message.group_msg.apiName=im:message.group_msg", "im:message.group_msg=pending", "approvalEvidence=set", "send_message=pending", "card.action.trigger=pending", "resource_transfer=pending"];
    const hasExpectedTemplateShape = expectedEvidence.every((entry) => check.evidence.includes(entry));
    const ok = check.status === "fail" && hasSafeEvidence && hasPlaceholderEvidence && hasMissingPostureEvidence && hasExpectedTemplateShape;

    return {
      id: "local.setup_evidence_template",
      label: "Sanitized setup evidence template is a safe placeholder",
      status: ok ? "pass" : "missing",
      evidence: ok ? [`present=${relativePath}`, "template_status=placeholder_rejected", "target=china_feishu", "consoleLabels=9/9", "setup_evidence_safe=true"] : [`present=${relativePath}`, `template_evaluator_status=${check.status}`, ...check.evidence],
      nextAction: ok ? undefined : "Restore the setup evidence example as a China Feishu placeholder that is safe to copy but still rejected until real tenant labels and approval evidence replace placeholders.",
    };
  } catch (error) {
    return {
      id: "local.setup_evidence_template",
      label: "Sanitized setup evidence template is a safe placeholder",
      status: "missing",
      evidence: [formatFeishuSmokeCliError(error)],
      nextAction: "Fix docs/feishu-setup-evidence.example.json so it is valid JSON and matches the safe placeholder template contract.",
    };
  }
}

async function savedSmokeEvidenceCheck(statusFile: string, setupEvidenceFile: string, env: Record<string, string | undefined>): Promise<Rfc0001AuditCheck> {
  if (!(await pathExists(statusFile))) {
    return {
      id: "real.saved_smoke",
      label: "Saved real Feishu smoke evidence passes",
      status: "missing",
      evidence: [`missing=${path.basename(statusFile)}`],
      nextAction: "Run the real Feishu smoke against the rollout runtime and save admin-status.json plus the setup evidence bundle.",
    };
  }
  if (!(await pathExists(setupEvidenceFile))) {
    return {
      id: "real.saved_smoke",
      label: "Saved real Feishu smoke evidence passes",
      status: "missing",
      evidence: [`missing=${path.basename(setupEvidenceFile)}`],
      nextAction: "Save setup evidence with the admin status snapshot before re-verifying saved smoke evidence.",
    };
  }

  try {
    const report = await evaluateFeishuSmokeStatusFile(statusFile, env, {
      setupEvidenceFile,
    });
    return {
      id: "real.saved_smoke",
      label: "Saved real Feishu smoke evidence passes",
      status: report.ok ? "pass" : "missing",
      evidence: report.checks.map((check) => `${check.id}=${check.status}`),
      nextAction: report.ok ? undefined : "Collect or replay evidence until pnpm manual:feishu-smoke passes with setup evidence and accepted/ignored/deduped/degraded/failed/recovered coverage.",
    };
  } catch (error) {
    return {
      id: "real.saved_smoke",
      label: "Saved real Feishu smoke evidence passes",
      status: "missing",
      evidence: [formatFeishuSmokeCliError(error)],
      nextAction: "Fix the saved smoke evidence files, then rerun the audit.",
    };
  }
}

async function fileCheck(cwd: string, id: string, label: string, relativePath: string, nextAction: string): Promise<Rfc0001AuditCheck> {
  const exists = await pathExists(path.join(cwd, relativePath));
  return {
    id,
    label,
    status: exists ? "pass" : "missing",
    evidence: [`${exists ? "present" : "missing"}=${relativePath}`],
    nextAction: exists ? undefined : nextAction,
  };
}

async function filesCheck(cwd: string, id: string, label: string, relativePaths: readonly string[], nextAction: string): Promise<Rfc0001AuditCheck> {
  const missing: string[] = [];
  for (const relativePath of relativePaths) {
    if (!(await pathExists(path.join(cwd, relativePath)))) {
      missing.push(relativePath);
    }
  }

  return {
    id,
    label,
    status: missing.length === 0 ? "pass" : "missing",
    evidence: missing.length === 0 ? [`present_count=${relativePaths.length}`, ...relativePaths.map((relativePath) => `present=${relativePath}`)] : missing.map((relativePath) => `missing=${relativePath}`),
    nextAction: missing.length === 0 ? undefined : nextAction,
  };
}

async function contentPatternsCheck(
  cwd: string,
  id: string,
  label: string,
  probes: readonly {
    readonly id: string;
    readonly file: string;
    readonly snippets: readonly string[];
  }[],
  nextAction: string,
): Promise<Rfc0001AuditCheck> {
  const missing: string[] = [];
  const fileContent = new Map<string, string | undefined>();
  for (const probe of probes) {
    let content = fileContent.get(probe.file);
    if (content === undefined && !fileContent.has(probe.file)) {
      content = await readOptionalText(path.join(cwd, probe.file));
      fileContent.set(probe.file, content);
    }
    if (content === undefined) {
      missing.push(`${probe.id}:${probe.file}:missing_file`);
      continue;
    }
    for (const snippet of probe.snippets) {
      if (!content.includes(snippet)) {
        missing.push(`${probe.id}:${probe.file}:missing_snippet=${snippet}`);
      }
    }
  }

  return {
    id,
    label,
    status: missing.length === 0 ? "pass" : "missing",
    evidence: missing.length === 0 ? [`probe_count=${probes.length}`, ...probes.map((probe) => `present=${probe.id}:${probe.file}`)] : missing,
    nextAction: missing.length === 0 ? undefined : nextAction,
  };
}

async function readOptionalText(filePath: string): Promise<string | undefined> {
  try {
    return await fs.readFile(filePath, "utf8");
  } catch {
    return undefined;
  }
}

async function readPackageScripts(cwd: string): Promise<Record<string, string>> {
  try {
    const packageJson = JSON.parse(await fs.readFile(path.join(cwd, "package.json"), "utf8")) as {
      readonly scripts?: Record<string, string>;
    };
    return packageJson.scripts ?? {};
  } catch {
    return {};
  }
}

async function pathExists(filePath: string): Promise<boolean> {
  try {
    await fs.stat(filePath);
    return true;
  } catch {
    return false;
  }
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function asArray(value: unknown): readonly unknown[] {
  return Array.isArray(value) ? value : [];
}

function readString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function reportContainsUnsafeText(value: unknown): boolean {
  return /\b(?:xox[abprs]-|xapp-|Bearer\s+\S+|[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,})\b|-----BEGIN [A-Z ]*PRIVATE KEY-----/iu.test(JSON.stringify(value));
}

export function parseRfc0001AuditArgs(argv: readonly string[]): CliOptions {
  let json = false;
  let help = false;
  let localOnly = false;
  let evidenceDir: string | undefined;

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]!;
    const next = argv[index + 1];
    if (arg === "--") {
      continue;
    } else if (arg === "--json") {
      json = true;
    } else if (arg === "--help" || arg === "-h") {
      help = true;
    } else if (arg === "--local-only") {
      localOnly = true;
    } else if (arg === "--evidence-dir") {
      if (!next || next.startsWith("--")) {
        throw new Error("Missing value for --evidence-dir");
      }
      evidenceDir = next;
      index += 1;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  return {
    json,
    help,
    localOnly,
    evidenceDir,
  };
}

function renderHumanReport(report: Rfc0001AuditCliReport): string {
  const lines = [
    `RFC 0001 audit checked_at=${report.checkedAt}`,
    `mode: ${report.mode}`,
    `status: ${report.exitOk ? "PASS" : "MISSING"}`,
    `completion: ${report.ok ? "COMPLETE" : "INCOMPLETE"}`,
    `local: ${report.localOk ? "PASS" : "MISSING"}`,
    `real_tenant: ${report.realTenantOk ? "PASS" : "MISSING"}`,
    "",
    "Local checks:",
  ];

  for (const check of report.localChecks) {
    lines.push(renderCheck(check));
  }
  lines.push("", "Real tenant checks:");
  for (const check of report.realTenantChecks) {
    lines.push(renderCheck(check));
  }
  if (report.nextActions.length > 0) {
    lines.push("", "Next actions:");
    for (const nextAction of report.nextActions) {
      lines.push(`- ${nextAction}`);
    }
  }

  return `${lines.join("\n")}\n`;
}

function renderCheck(check: Rfc0001AuditCheck): string {
  const lines = [`- [${check.status === "pass" ? "PASS" : "MISSING"}] ${check.id}: ${check.label}`];
  for (const evidence of check.evidence.slice(0, 4)) {
    lines.push(`  evidence: ${evidence}`);
  }
  if (check.status !== "pass" && check.nextAction) {
    lines.push(`  next: ${check.nextAction}`);
  }
  return lines.join("\n");
}

function printUsage(): void {
  console.log(
    [
      "usage: pnpm rfc:feishu-audit [--] [--json] [--local-only] [--evidence-dir evidence/feishu-smoke]",
      "",
      "Checks local RFC 0001 verification assets and reports whether real tenant evidence is still missing.",
      "--local-only makes the CLI exit on localOk while preserving ok=false until real tenant gates pass.",
      "This audit does not send Feishu messages and cannot replace pnpm manual:feishu-smoke.",
    ].join("\n"),
  );
}

async function main(): Promise<void> {
  const options = parseRfc0001AuditArgs(process.argv.slice(2));
  if (options.help) {
    printUsage();
    return;
  }

  const report = await collectRfc0001LocalAudit(options);
  const cliReport = createRfc0001AuditCliReport(report, options);
  if (options.json) {
    console.log(JSON.stringify(cliReport, null, 2));
  } else {
    process.stdout.write(renderHumanReport(cliReport));
  }
  if (!cliReport.exitOk) {
    process.exitCode = 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  void main().catch((error) => {
    console.error(formatFeishuSmokeCliError(error));
    process.exitCode = 1;
  });
}
