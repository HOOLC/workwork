import fs from "node:fs/promises";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { REQUIRED_FEISHU_LOG_FIELDS } from "./manual/run-real-feishu-smoke.js";

const repoRoot = process.cwd();
const rfcEntry = path.join(repoRoot, "docs/rfcs/0001-slack-feishu-dual-platform.md");
const rfc0002Entry = path.join(repoRoot, "docs/rfcs/0002-feishu-ux-parity-from-codex-feishu-bot.md");
const rfcDir = path.join(repoRoot, "docs/rfcs/0001-slack-feishu-dual-platform");
const docsRoot = path.join(repoRoot, "docs");

describe("RFC 0001 documentation", () => {
  it("keeps the entry point short and progressive-disclosure oriented", async () => {
    const content = await fs.readFile(rfcEntry, "utf8");
    const nonEmptyLines = content.split(/\r?\n/u).filter((line) => line.trim()).length;

    expect(nonEmptyLines).toBeLessThanOrEqual(45);
    expect(content).toContain("This is the 2-minute entry point");
    expect(content).toContain("## Progressive Reading Path");
    expect(content).toContain("## Deep Dives");
    expect(content).toContain("0001-slack-feishu-dual-platform/architecture.md");
    expect(content).toContain("0001-slack-feishu-dual-platform/implementation.md");
    expect(content).toContain("0001-slack-feishu-dual-platform/test-plan.md");
    expect(content).toContain("0001-slack-feishu-dual-platform/observability.md");
    expect(content).toContain("0001-slack-feishu-dual-platform/permissions.md");
    expect(content).toContain("0001-slack-feishu-dual-platform/review-gates.md");
  });

  it("keeps all RFC deep-dive files present", async () => {
    const expectedDeepDives = ["architecture.md", "implementation.md", "test-plan.md", "observability.md", "permissions.md", "review-gates.md"];

    await expect(Promise.all(expectedDeepDives.map((file) => fs.stat(path.join(rfcDir, file))))).resolves.toHaveLength(expectedDeepDives.length);
  });

  it("keeps RFC deep dives progressively disclosed", async () => {
    const expectedDeepDives = ["architecture.md", "implementation.md", "test-plan.md", "observability.md", "permissions.md", "review-gates.md"];

    for (const file of expectedDeepDives) {
      const content = await fs.readFile(path.join(rfcDir, file), "utf8");
      const layerSummaries = content.match(/<summary>Layer \d:/gu) ?? [];

      expect(content).toContain("## One-Screen Summary");
      expect(content).toContain("## Read Layers");
      expect(layerSummaries.length).toBeGreaterThanOrEqual(2);
      expect(content).toContain("</details>");
    }
  });

  it("keeps RFC 0002 usable as a progressive long-running self-regression task", async () => {
    const content = await fs.readFile(rfc0002Entry, "utf8");
    const layerSummaries = content.match(/<summary id="[^"]+">Layer \d:/gu) ?? [];

    expect(content).toContain("## How to read this RFC");
    expect(content).toContain("## Long-running task readiness");
    expect(content).toContain("## Acceptance gates");
    expect(layerSummaries.length).toBeGreaterThanOrEqual(4);
    expect(content).toContain("Layer 5: Automation, evidence, migration, and open questions");
    expect(content).toContain("## Automated self-regression design");
    expect(content).toContain("## Trigger matrix");
    expect(content).toContain("## Self-regression evidence checklist");
    expect(content).toContain("Slack self-regression evidence bundle is present");
    expect(content).toContain("Feishu self-regression evidence bundle is present");
    expect(content).toContain("Self-regression reports are sanitized, replayable, and safe to attach to a");
    expect(content).toContain("Slack auto-drive must create a controlled inbound message");
    expect(content).toContain("Feishu self-regression is not fully unattended with app-only bot credentials");
    expect(content).toContain("message to itself does not prove user inbound");
    expect(content).toContain("pnpm manual:self-regression -- --platform slack --drive");
    expect(content).toContain("pnpm manual:self-regression -- --platform feishu --observe");
    expect(content).toContain("--manual-action");
    expect(content).toContain("FEISHU_SELF_REGRESSION_MANUAL_ACTION");
    expect(content).toContain("The shared `manual:self-regression` entrypoint is wired");
    expect(content).toContain("Feishu drive remains");
    expect(content).toContain("pnpm rfc:feishu-completion-audit -- --json");
    expect(content).toContain("Completion audit remains red until");
    expect(content).toContain("Slack drive, Feishu observe, and real Codex coding smoke bundles are present");
  });

  it("keeps completion evidence progressive after real-tenant signoff", async () => {
    const reviewGates = await fs.readFile(path.join(rfcDir, "review-gates.md"), "utf8");

    expect(reviewGates).toContain("## Completion Evidence Ledger");
    expect(reviewGates).toContain("Local evidence now");
    expect(reviewGates).toContain("Real tenant gate still required");
    expect(reviewGates).toContain("pnpm rfc:feishu-audit");
    expect(reviewGates).toContain("pnpm rfc:feishu-completion-audit -- --json");
    expect(reviewGates).toContain("local gates and saved real-tenant evidence pass");
    expect(reviewGates).toContain("future drift should be treated as incomplete until both sides pass again");
    expect(reviewGates).toContain("pnpm test:e2e:feishu-mock");
    expect(reviewGates).toContain("pnpm manual:feishu-smoke");
    expect(reviewGates).toContain("- [x] Real Feishu smoke passes.");
    expect(reviewGates).toContain("- [x] Feishu mock e2e passes.");
    expect(reviewGates).toContain("callback smoke matches the same group @ session after a broker-posted card");
    expect(reviewGates).toContain("using `messageId` when Feishu supplies one and callback `eventId`/`payloadRef` otherwise");
  });

  it("keeps local documentation links resolvable", async () => {
    const markdownFiles = await collectMarkdownFiles(docsRoot);
    const brokenLinks: string[] = [];

    for (const markdownFile of markdownFiles) {
      const content = await fs.readFile(markdownFile, "utf8");
      for (const link of extractLocalMarkdownLinks(content)) {
        const target = resolveMarkdownLink(markdownFile, link);
        if (!target) {
          continue;
        }

        try {
          await fs.stat(target);
        } catch {
          brokenLinks.push(`${path.relative(repoRoot, markdownFile)} -> ${link}`);
        }
      }
    }

    expect(brokenLinks).toEqual([]);
  });

  it("keeps the RFC log field matrix synchronized with the smoke checker", async () => {
    const observability = await fs.readFile(path.join(rfcDir, "observability.md"), "utf8");
    const documentedMatrix = parseRequiredLogFieldMatrix(observability);
    const documentedFields = parseStructuredLogFields(observability);

    expect([...documentedMatrix.keys()].sort()).toEqual(Object.keys(REQUIRED_FEISHU_LOG_FIELDS).sort());
    for (const [event, requiredFields] of Object.entries(REQUIRED_FEISHU_LOG_FIELDS)) {
      expect(documentedMatrix.get(event)).toEqual([...requiredFields]);
      for (const field of requiredFields) {
        expect(documentedFields).toContain(field);
      }
    }
  });

  it("documents the reusable real-smoke evidence bundle", async () => {
    const setup = await fs.readFile(path.join(repoRoot, "docs/feishu-setup.md"), "utf8");
    const implementation = await fs.readFile(path.join(rfcDir, "implementation.md"), "utf8");
    const reviewGates = await fs.readFile(path.join(rfcDir, "review-gates.md"), "utf8");
    const permissionRequest = await fs.readFile(path.join(repoRoot, "docs/feishu-permission-request.md"), "utf8");

    expect(implementation).toContain("- RED:");
    expect(implementation).toContain("- GREEN:");
    expect(implementation).toContain("- OBSERVE:");
    expect(implementation).toContain("- REGRESSION:");
    expect(setup).toContain("pnpm manual:feishu-smoke -- --base-url");
    expect(setup).toContain("pnpm manual:feishu-smoke -- --preflight");
    expect(setup).toContain("pnpm manual:feishu-smoke -- --status-file admin-status.json");
    expect(setup).not.toContain("pnpm manual:feishu-smoke --base-url");
    expect(setup).not.toContain("pnpm manual:feishu-smoke --status-file");
    expect(setup).toContain("pnpm ops:rollout:real");
    expect(setup).toContain("pnpm ops:check:real");
    expect(setup).toContain("--skip-feishu-preflight");
    expect(setup).toContain("platform-health summary");
    expect(setup).toContain("only posture-safe Slack and Feishu enabled/state/degraded/permission status values");
    expect(setup).toContain("without copying recent broker logs or permission explanation text");
    expect(setup).toContain("docs/feishu-setup-evidence.example.json");
    expect(setup).toContain("--setup-evidence-file evidence/feishu-smoke/feishu-setup-evidence.json");
    expect(setup).toContain("--output-dir evidence/feishu-smoke");
    expect(setup).toContain("The example evidence file intentionally starts with pending/placeholder values");
    expect(setup).toContain("The checker requires `apiName=im:message.group_msg`, `status=approved`, redacted approval evidence");
    expect(setup).toContain("explicit send/reply, card callback, and resource transfer permission posture evidence");
    expect(setup).toContain('It rejects example text such as "replace", "approval ticket", or other placeholders');
    expect(setup).toContain("exact real-tenant console labels plus redacted approval/configuration evidence");
    expect(setup).toContain("admin-status.json");
    expect(setup).toContain("feishu-setup-evidence.json");
    expect(setup).toContain("feishu-preflight-report.json");
    expect(setup).toContain("feishu-preflight-summary.md");
    expect(setup).toContain("Preflight evidence records secret-bearing settings as set/missing");
    expect(setup).toContain("records only known enum/boolean values for environment posture");
    expect(setup).toContain("Value flags accept both `--flag value` and `--flag=value` forms");
    expect(setup).toContain("`FEISHU_API_BASE_URL` evidence omits query/hash values");
    expect(setup).toContain("feishu-smoke-report.json");
    expect(setup).toContain("feishu-smoke-summary.md");
    expect(setup).toContain("forbidden fields are omitted and secret-like strings are redacted");
    expect(setup).toContain("failed evidence bundles do not preserve credentials, message bodies, raw bot IDs, or user emails");
    expect(setup).toContain("machine-readable `admin.status_available` failure report");
    expect(setup).toContain("omits query/hash values and does not echo the response body");
    expect(setup).toContain("The output `admin-status.json` is sanitized by the smoke checker");
    expect(setup).toContain("When the live admin fetch fails, the same file records `adminStatus.available=false`");
    expect(setup).toContain("later `--status-file` checks replay that explicit failure report");
    expect(setup).toContain("Final smoke and saved `--status-file` verification require `--setup-evidence-file`");
    expect(setup).toContain("a saved admin status JSON alone is not enough for RFC signoff");
    expect(setup).toContain("keeps allowlisted platform health");
    expect(setup).toContain("filters `state.sessions` to Feishu session coordinates");
    expect(setup).toContain("keeps only safe scalar session tokens/timestamps");
    expect(setup).toContain("report JSON, markdown summary, and human-readable CLI output also redact unsafe report text fields and evidence text");
    expect(setup).toContain("summary/source URLs omit query/hash values");
    expect(setup).toContain("bundle write notices print sanitized filenames only");
    expect(setup).toContain("early CLI errors redact unsafe text plus full filesystem paths");
    expect(setup).toContain("pending/inflight message previews");
    expect(setup).toContain("redacts inbound message bodies and background job errors in status summaries");
    expect(setup).toContain("summarizes service/auth/session/job/deployment paths without full host filesystem paths");
    expect(setup).toContain("summarizes active sessions without raw co-author candidate IDs");
    expect(setup).toContain("Rollout JSON and metadata report repo-relative backup coordinates instead of full host filesystem paths");
    expect(setup).toContain("live admin status, `ops:rollout:real`, `ops:check:real`, `ops:status:real`, and smoke evidence bundles allowlist `recentBrokerLogs` top-level event tokens plus metadata to RFC-safe scalar fields");
    expect(setup).toContain("malformed broker log lines are reported without echoing their raw text");
    expect(setup).toContain("`ops:rollout:real` recursively redacts unsafe nested metadata string fields");
    expect(setup).toContain("preserving safe posture text such as `FEISHU_APP_SECRET=missing`");
    expect(setup).toContain("pre-rollout Docker log snapshot as sanitized evidence");
    expect(setup).toContain("preserving known startup markers by name, and redacting other non-structured lines");
    expect(setup).toContain("`ops:rollout:real`, `ops:check:real`, and `ops:status:real` summarize backup/data-root coordinates without full host filesystem paths");
    expect(setup).toContain("`ops:check:real` plus `ops:status:real` summarize active sessions, pending/inflight inbound messages, and background jobs without raw message bodies, job tokens, or job scripts");
    expect(setup).toContain("RFC required fields");
    expect(setup).toContain("chat.coauthor.confirmed");
    expect(setup).toContain("after that callback with the same `sessionKey` and `candidateRevision`");
    expect(setup).toContain("`confirmedCount > 0`, and `conversationId` / `rootMessageId` matching admin session state");
    expect(setup).toContain("accepted logs matching the same group @ session in admin state and no same-message ignored log");
    expect(setup).toContain("route=bot_mention");
    expect(setup).toContain("format=text");
    expect(setup).toContain("recoveredCount > 0");
    expect(setup).toContain("recoveredCount > 0` plus session coordinates matching admin session state");
    expect(setup).toContain("chat.history.recovered` alone is not counted as recovered behavior coverage");
    expect(setup).toContain("source=history_recovery");
    expect(setup).toContain("same `messageId` and `conversationId` as the original accepted event");
    expect(setup).toContain("session coordinates matching admin session state");
    expect(setup).toContain("Ordered same-session `chat.outbound.posted format=card -> chat.card.callback.received` evidence");
    expect(setup).toContain("the callback occurs after the outbound card");
    expect(setup).toContain("ordered `chat.message.accepted -> chat.session.resumed -> chat.turn.stopped` chain with matching stop `messageId`");
    expect(setup).toContain("`-stop` in the same group @ session");
    expect(setup).toContain("a non-@ text follow-up reaches the same active group @ session through an ordered `chat.message.accepted route=group_message msgType=text -> chat.turn.steered|chat.session.resumed` transition");
    expect(setup).toContain("Ordered same-session `chat.message.accepted -> chat.session.resumed -> chat.turn.stopped` stop evidence with matching stop `messageId`");
    expect(setup).toContain("ordered `chat.message.accepted route=bot_mention msgType=text -> chat.session.created|resumed` transition");
    expect(setup).toContain("whose `sessionKey`, `conversationId`, and `rootMessageId` match admin session state");
    expect(setup).toContain("with no same-message ignored log");
    expect(setup).toContain("same group @ session's originating Feishu group/root message");
    expect(setup).toContain("emits `chat.outbound.posted` with `format=text` and session coordinates matching admin session state");
    expect(setup).toContain("conversationKind=direct");
    expect(setup).toContain("has no same-message accepted/session/turn dispatch log");
    expect(setup).toContain("no same-message accepted/session/turn dispatch log");
    expect(setup).toContain("duplicate replay evidence has no later same-message accepted/session/turn dispatch after `chat.message.deduped`");
    expect(setup).toContain("ordered Slack `chat.message.accepted -> chat.outbound.posted format=text`");
    expect(setup).toContain("accepted/reply `messageId` values");
    expect(setup).toContain("shares the same `sessionKey`, `conversationId`, and `rootMessageId`");
    expect(setup).toContain("a posted `messageId`, and the same `sessionKey`, `conversationId`, and `rootMessageId` as the accepted Slack event");
    expect(setup).toContain("the same `sessionKey`, `conversationId`, and `rootMessageId` as the accepted Slack event");
    expect(setup).toContain("The same Feishu session emits an ordered `chat.turn.started|steered -> chat.outbound.posted format=text -> chat.turn.completed` chain");
    expect(setup).toContain("with the completion after the text reply and `turnId` / `batchId` matching the non-history-recovery turn start/steer log");
    expect(setup).toContain("coordinate-bearing send/download failures must match admin session state");
    expect(setup).toContain("detached handler failures must name a known Feishu handler (`message` or `interactive`) plus `errorClass`");
    expect(setup).toContain("with session coordinates matching admin session state plus `format`, `errorClass`, `statusCode`, and `attempt`");
    expect(setup).toContain("download failures must include session coordinates matching admin session state plus `messageId`, `attachmentId`, `kind`, and `errorClass`");
    expect(setup).toContain("chat.platform.ready source=socket_mode");
    expect(setup).toContain("connection.mode=socket_mode");
    expect(setup).toContain("Feishu permission posture");
    expect(setup).toContain("current Feishu admin health is `state=ready`");
    expect(setup).toContain("`bot_identity=configured`, `im:message.group_msg=verified`, `im:message:send_as_bot=configured`");
    expect(setup).toContain("`FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=true` is backed by a same-session non-@ `msgType=text` follow-up transition");
    expect(setup).toContain("rather than by the admin flag alone");
    expect(setup).toContain("chat.platform.ready source=long_connection");
    expect(setup).toContain("connection.mode=long_connection");
    expect(setup).toContain("image/file accepted logs include `fileId`");
    expect(setup).toContain("Phase 4 evidence includes same group @ session inbound rich/card/resource accepted logs plus same-session outbound rich text, card, and file/image `chat.outbound.posted` logs with uploaded `fileId` for `format=file|image`");
    expect(setup).toContain("accepted logs matching the same group @ session in admin state");
    expect(setup).toContain("card callbacks occur after and are tied to a same group @ session broker-posted card, matching the card `messageId` when Feishu supplies one");
    expect(setup).toContain("otherwise proving ordered same-session/root coordinates plus callback `eventId`/`payloadRef`");
    expect(setup).toContain("Feishu outbound rich text, interactive card, and file/image paths are exercised in the same group @ session");
    expect(setup).toContain("`format=markdown` or `format=rich_text`, `format=card`, and `format=file` or `format=image`");
    expect(setup).toContain("`format=file|image` logs must include the uploaded `fileId`");
    expect(setup).toContain("raw body or secret-like fields");
    expect(setup).toContain("raw App Secret/access tokens/message bodies/user emails/raw bot IDs");
    expect(setup).toContain("record bot identity only as set/missing posture");
    expect(setup).toContain("send/reply, card callback, and resource transfer permission posture evidence");
    expect(setup).toContain("im:message.group_msg` approval and send/card/resource posture");
    expect(setup).toContain("## Console Label Map");
    expect(setup).toContain("使用长连接接收事件");
    expect(setup).toContain("im.message.receive_v1");
    expect(setup).toContain("card.action.trigger");
    expect(setup).toContain("im:message.group_msg");
    expect(setup).toContain("im:message:send_as_bot");
    expect(setup).toContain("获取与上传图片或文件资源");
    expect(setup).toContain("downloads message image inputs and sends outbound message images up to 10 MB");
    expect(setup).toContain("falls back to file upload for larger outbound images up to 30 MB");
    expect(setup).toContain("caps file/resource transfers at 30 MB");
    expect(implementation).toContain("pnpm manual:feishu-smoke -- --preflight --env-file .env --output-dir evidence/feishu-smoke");
    expect(implementation).toContain("Run `pnpm test:e2e:feishu-mock` for the local Feishu mock e2e gate");
    expect(implementation).toContain("fixture replay");
    expect(implementation).toContain("Slack+Feishu same-process readiness");
    expect(implementation).toContain("it does not prove tenant permissions or real client delivery");
    expect(implementation).toContain("FEISHU_ENABLED=true");
    expect(implementation).toContain(".backups/rollouts/<timestamp>/feishu-preflight/");
    expect(implementation).toContain("pnpm ops:check:real");
    expect(implementation).toContain("platform-health summary");
    expect(implementation).toContain("pnpm manual:feishu-smoke -- --setup-evidence-file evidence/feishu-smoke/feishu-setup-evidence.json --output-dir evidence/feishu-smoke");
    expect(implementation).toContain("pnpm manual:feishu-smoke -- --status-file admin-status.json --setup-evidence-file evidence/feishu-smoke/feishu-setup-evidence.json");
    expect(implementation).not.toContain("pnpm manual:feishu-smoke --setup-evidence-file");
    expect(implementation).toContain("log field coverage");
    expect(implementation).toContain("chat.coauthor.confirmed");
    expect(implementation).toContain("ordered same-session `chat.outbound.posted format=card -> chat.card.callback.received -> chat.coauthor.confirmed` chain");
    expect(implementation).toContain("callback and confirmation share `candidateRevision`, confirmation includes `confirmedCount > 0`, and session coordinates match admin session state");
    expect(implementation).toContain("route=bot_mention");
    expect(implementation).toContain("format=text");
    expect(implementation).toContain("recoveredCount > 0");
    expect(implementation).toContain("`chat.history.recovered recoveredCount > 0` and session coordinates matching admin session state");
    expect(implementation).toContain("Recovered behavior coverage requires `chat.history.recovered recoveredCount > 0` plus a `history_recovery` turn log");
    expect(implementation).toContain("Accepted and deduped behavior coverage require admin-session-matching accepted logs with no same-message ignored log");
    expect(implementation).toContain("duplicate replay evidence with no later same-message accepted/session/turn dispatch after `chat.message.deduped`");
    expect(implementation).toContain("Ignored behavior coverage requires a private-chat ignored log with `conversationKind=direct` and no persisted Feishu session");
    expect(implementation).toContain("Degraded behavior coverage requires a known Feishu `degradedReason`, with `permission` for permission-related degradation");
    expect(implementation).toContain("ordered Slack `chat.message.accepted -> chat.outbound.posted format=text`");
    expect(implementation).toContain("accepted/reply `messageId` values");
    expect(implementation).toContain("shares the same `sessionKey`, `conversationId`, and `rootMessageId`");
    expect(implementation).toContain("Final Feishu turn completion emits an ordered same-session `chat.turn.started|steered -> chat.outbound.posted format=text -> chat.turn.completed` chain");
    expect(implementation).toContain("with completion after the text reply and `turnId` / `batchId` matching the non-history-recovery turn start/steer log");
    expect(implementation).toContain("Non-@ group text follow-up enters the same active group @ session");
    expect(implementation).toContain("`-stop` proof targets the same group @ session and includes an ordered `chat.message.accepted -> chat.session.resumed -> chat.turn.stopped` chain");
    expect(implementation).toContain("with matching stop `messageId`, `hadActiveTurn=true`, an active `turnId`, no same-message ignored log");
    expect(implementation).toContain("same `messageId` and `conversationId` as the original accepted event");
    expect(implementation).toContain("feishu/group-app-self-message.json");
    expect(implementation).toContain("Bot/app/self group sender is ignored before dispatch.");
    expect(implementation).toContain("no same-message accepted/session/turn dispatch log");
    expect(implementation).toContain("Feishu duplicate processing keys are based on `conversationId + messageId`");
    expect(implementation).toContain("`rootMessageId` drift on replay must not bypass dedupe");
    expect(implementation).toContain("coordinate-bearing send/download failures must match admin session state");
    expect(implementation).toContain("detached handler failures must name a known Feishu handler (`message` or `interactive`) plus `errorClass`");
    expect(implementation).toContain("transition session coordinates matching admin session state");
    expect(implementation).toContain("ordered `chat.message.accepted route=bot_mention msgType=text -> chat.session.created|resumed` transition");
    expect(implementation).toContain("starts or resumes a Codex session whose `sessionKey`, `conversationId`, and `rootMessageId` match admin session state");
    expect(implementation).toContain("with no same-message ignored log");
    expect(implementation).toContain("Final text reply posts to the same group @ session with `chat.outbound.posted format=text` and session coordinates matching admin session state");
    expect(implementation).toContain("admin health coverage for independent Slack/Feishu state");
    expect(implementation).toContain("connection modes, and Feishu permission posture");
    expect(implementation).toContain("current Feishu `state=ready`");
    expect(implementation).toContain("`bot_identity=configured`, `im:message.group_msg=verified`, `im:message:send_as_bot=configured`");
    expect(implementation).toContain("requires `FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=true` to be backed by same-session non-@ `msgType=text` follow-up transition evidence");
    expect(implementation).toContain("rather than by the admin flag alone");
    expect(implementation).toContain("same group @ session broker-posted card where the callback log occurs after the outbound card log");
    expect(implementation).toContain("matching card `messageId` when Feishu supplies one and callback `eventId`/`payloadRef` otherwise");
    expect(implementation).toContain("inbound rich/card/resource accepted logs matching the same group @ session");
    expect(implementation).toContain("outbound `format=file|image` logs carrying uploaded `fileId`");
    expect(implementation).toContain("real-smoke accepted payload logs matching the same group @ session");
    expect(implementation).toContain("same-session outbound rich/card/file or image posting evidence with uploaded `fileId` for file/image logs");
    expect(implementation).toContain("real-smoke callback matching the same group @ session after a broker-posted card");
    expect(implementation).toContain("using `messageId` when Feishu supplies one and callback `eventId`/`payloadRef` otherwise");
    expect(implementation).toContain("Slack Socket Mode readiness through `chat.platform.ready source=socket_mode`");
    expect(implementation).toContain("long-connection readiness through `chat.platform.ready source=long_connection`");
    expect(implementation).toContain("only posture-safe enabled/state/degraded/permission status values");
    expect(implementation).toContain("no recent broker logs or permission explanation text");
    expect(implementation).toContain("Preflight evidence records secret-bearing settings as set/missing");
    expect(implementation).toContain("`--env-file` when the rollout settings live in a local env file");
    expect(implementation).toContain("put `--` before smoke-checker arguments so Node's own `--env-file` flag does not intercept it");
    expect(implementation).toContain("Value flags accept both `--flag value` and `--flag=value` forms");
    expect(implementation).toContain("Exported shell variables still take precedence");
    expect(implementation).toContain("send/reply, card callback, and resource transfer permission posture evidence");
    expect(implementation).toContain("records only known enum/boolean values for environment posture");
    expect(implementation).toContain("`FEISHU_API_BASE_URL` evidence omits query/hash values");
    expect(implementation).toContain("setup evidence safety for raw App Secret/access tokens/message bodies/user emails/raw bot IDs");
    expect(implementation).toContain("requires `apiName=im:message.group_msg`, `status=approved`, approval evidence");
    expect(implementation).toContain("rejects the example's pending status and placeholder text");
    expect(implementation).toContain("template evidence cannot satisfy the real-tenant gate");
    expect(implementation).toContain("Final smoke and saved `--status-file` verification require `--setup-evidence-file`");
    expect(implementation).toContain("a saved admin status JSON alone is not enough for RFC signoff");
    expect(implementation).toContain("machine-readable `admin.status_available` failure report");
    expect(implementation).toContain("omits query/hash values, and does not echo the response body");
    expect(implementation).toContain("Output bundles also write `adminStatus.available=false` into `admin-status.json`");
    expect(implementation).toContain("saved `--status-file` rechecks replay that same `admin.status_available` failure");
    expect(implementation).toContain("Its output `admin-status.json` is sanitized");
    expect(implementation).toContain("only allowlisted platform health");
    expect(implementation).toContain("safe scalar session coordinates/timestamps");
    expect(implementation).toContain("smoke report JSON, markdown summaries, and human-readable CLI output redact unsafe report text fields and evidence text before writing or printing");
    expect(implementation).toContain("summary/source URLs omit query/hash values");
    expect(implementation).toContain("bundle write notices print sanitized filenames only");
    expect(implementation).toContain("early CLI errors redact unsafe text plus full filesystem paths");
    expect(implementation).toContain("account/auth/profile state and pending/inflight message previews are omitted");
    expect(implementation).toContain("copied setup evidence omits forbidden fields and redacts secret-like strings");
    expect(implementation).toContain("live admin API redacts inbound message bodies and background job errors in status summaries");
    expect(implementation).toContain("active session summaries omit raw co-author candidate IDs");
    expect(implementation).toContain("`pnpm ops:rollout:real` reports rollout and preflight directories as repo-relative backup coordinates instead of full host filesystem paths");
    expect(implementation).toContain("pre-rollout log snapshot is sanitized to allowlisted structured event/meta fields or redacted non-structured line summaries instead of raw Docker log text");
    expect(implementation).toContain("live admin status, `ops:rollout:real`, `ops:check:real`, `ops:status:real`, and smoke evidence bundles allowlist `recentBrokerLogs` top-level event tokens plus metadata to RFC-safe scalar fields");
    expect(implementation).toContain("malformed broker log lines without echoing their raw text");
    expect(implementation).toContain("recursively redacts unsafe nested metadata string fields");
    expect(implementation).toContain("safe posture text such as `FEISHU_APP_SECRET=missing`");
    expect(implementation).toContain("known startup markers are kept by name");
    expect(implementation).toContain("`ops:rollout:real`, `ops:check:real`, and `ops:status:real` summarize backup/data-root coordinates without full host filesystem paths");
    expect(implementation).toContain("`ops:check:real` and `ops:status:real` additionally summarize active sessions, open inbound messages, and background jobs without raw message bodies, job tokens, or job scripts");
    expect(implementation).toContain("Recovery uses `FEISHU_INITIAL_THREAD_HISTORY_COUNT`");
    expect(implementation).toContain("clamps explicit `/chat/thread-history` limits to `FEISHU_HISTORY_API_MAX_LIMIT`");
    expect(reviewGates).toContain("match admin session state, with no same-message ignored log");
    expect(reviewGates).toContain("bot/app/self sender events are ignored before dispatch");
    expect(reviewGates).toContain("no same-message accepted/session/turn dispatch log");
    expect(reviewGates).toContain("Private chats and bot/app/self sender events are ignored");
    expect(reviewGates).toContain("ordered `chat.message.accepted -> chat.session.resumed -> chat.turn.stopped` chain whose stop `messageId` matches");
    expect(reviewGates).toContain("same active group @ session");
    expect(reviewGates).toContain("ordered `chat.message.accepted route=group_message msgType=text -> chat.turn.steered|chat.session.resumed` transition");
    expect(reviewGates).toContain("matching `messageId`, transition session coordinates matching admin session state, and no same-message ignored log");
    expect(reviewGates).toContain("accepted and deduped behavior evidence must reference admin-session-matching accepted Feishu logs with no same-message ignored log");
    expect(reviewGates).toContain("deduped evidence must have no later same-message accepted/session/turn dispatch");
    expect(reviewGates).toContain("send/card/resource permission posture");
    const architecture = await fs.readFile(path.join(rfcDir, "architecture.md"), "utf8");
    expect(architecture).toContain("`FEISHU_INITIAL_THREAD_HISTORY_COUNT` capped by `FEISHU_HISTORY_API_MAX_LIMIT`");
    expect(architecture).toContain("platform-aware `/jobs/register` coordinates");
    expect(architecture).toContain("Feishu prompts use `conversation_id` and `root_message_id`");
    expect(architecture).toContain("Show Feishu history pagination with `before_cursor`, not Slack `before_message_id`");
    expect(architecture).toContain("HTTP JSON/query handlers accept canonical camelCase fields and snake_case aliases");
    expect(architecture).toContain("Invalid `platform` values return 400 `invalid_platform` with allowed values `slack` and `feishu`");
    expect(architecture).toContain("`filePath`/`file_path` and `contentBase64`/`content_base64`");
    expect(architecture).toContain("Inline `contentBase64` uploads require `filename` and must decode to non-empty file content");
    expect(architecture).toContain("Error responses must name the canonical fields and the accepted aliases");
    expect(architecture).toContain("`/chat/post-message` accepts structured JSON values or JSON strings for `richText`/`rich_text` and `card`");
    expect(architecture).toContain("Invalid rich/card JSON is a 400 client error that identifies the field without echoing the raw payload");
    expect(architecture).toContain("Platform-aware `/jobs/register` coordinates share the same `platform` validation as `/chat/*`");
    expect(architecture).toContain("Legacy Slack `channel_id`/`thread_ts` job aliases apply only when `platform` is omitted or set to `slack`");
    expect(architecture).toContain("Job callback `detailsJson`/`details_json` fields and `/integrations/mcp-call` `arguments` accept structured JSON values or JSON strings");
    expect(architecture).toContain("invalid JSON strings are 400 client errors that identify the field without echoing the raw payload");
    expect(architecture).toContain("Explicit history `limit` values must be positive integers before broker delegation");
    expect(architecture).toContain("History `format` values must be `json` or `text`; invalid values return 400 `invalid_format` before broker delegation");
    const observability = await fs.readFile(path.join(rfcDir, "observability.md"), "utf8");
    expect(observability).toContain("connection.connected=true");
    expect(observability).toContain("only after platform ready evidence");
    expect(observability).toContain("`connection.connected` as a boolean");
    expect(observability).toContain("`lastConnectedAt` whenever `connection.connected=true`");
    expect(observability).toContain("lastDisconnectedAt");
    expect(observability).toContain("connection close or connection failure");
    expect(observability).toContain("attachmentId");
    expect(observability).toContain("`fileId` when `msgType` is `image` or `file`");
    expect(observability).toContain("`fileId` when `format` is `file` or `image`");
    expect(observability).toContain("kind");
    expect(observability).toContain("`messageCursor`, `recoveredCount`, `degradedReason` when partial");
    expect(observability).toContain("`messageId`, `attachmentId`, `kind`, `errorClass`");
    expect(observability).toContain("`handler` (`message` or `interactive`), `errorClass`");
    expect(observability).toContain("recovered coverage also needs a `history_recovery` turn log");
    expect(observability).toContain("tied to a previously accepted `messageId` in the same conversation");
    expect(observability).toContain("Accepted behavior coverage counts only accepted Feishu logs that match admin session state and have no same-message ignored log");
    expect(observability).toContain("Ignored behavior coverage counts only private-chat ignored logs with `conversationKind=direct`");
    expect(observability).toContain("Deduped behavior coverage counts only when the original accepted Feishu log also matches admin session state");
    expect(observability).toContain("Degraded behavior coverage counts only known Feishu `degradedReason` values");
    expect(observability).toContain("`feishu-message:<messageId>`");
    expect(observability).toContain("`feishu-card:<eventId>`");
    expect(observability).toContain("`payloadRef` points to the current Feishu `messageId` or card callback `eventId`");
    expect(observability).toContain("`chat.message.ignored` for `conversationKind=direct` and no session creation");
    expect(observability).not.toContain("conversationKind=private");
    expect(observability).toContain("nested object/array metadata");
    expect(observability).toContain("token/email-like string values");
    expect(observability).toContain("Raw HTTP request logs for local Slack/chat/job/integration helper routes redact");
    expect(observability).toContain("MCP call arguments before writing JSONL");
    expect(observability).toContain("Redact pending/inflight inbound message bodies and background job errors in admin status summaries");
    expect(observability).toContain("safe scalar session coordinates/timestamps");
    expect(observability).toContain("Redact unsafe report text fields, evidence text, source URLs, bundle notices, and early CLI errors");
    expect(observability).toContain("source URL query/hash values");
    expect(observability).toContain("full filesystem paths");
    expect(observability).toContain("Summarize admin active sessions without raw co-author candidate IDs");
    expect(observability).toContain("Summarize live admin API service roots, auth file/profile paths, active session workspaces, background job cwd, and deployment release paths without full host filesystem paths");
    expect(observability).toContain("not account/auth/profile state or message previews");
    expect(observability).toContain("Allowlist `recentBrokerLogs` top-level event tokens plus metadata in admin status, `ops:rollout:real`, `ops:check:real`, `ops:status:real`, and smoke evidence bundles to RFC-safe scalar fields");
    expect(observability).toContain("malformed broker log lines are reported without echoing their raw text");
    expect(observability).toContain("Recursively redact unsafe nested string fields from `ops:rollout:real` metadata");
    expect(observability).toContain("Write `ops:rollout:real` pre-rollout Docker logs as sanitized evidence snapshots");
    expect(observability).toContain("known startup markers are kept by name, and other non-structured lines are represented by redacted summaries");
    expect(observability).toContain("Summarize `ops:rollout:real`, `ops:check:real`, and `ops:status:real` backup/data-root coordinates without full host filesystem paths");
    expect(observability).toContain("`ops:check:real` and `ops:status:real` also summarize active sessions, open inbound messages, and background jobs without raw message bodies, job tokens, or job scripts");
    expect(observability).toContain("Summarize `ops:auth:real`, `ops:auth:profiles`, and `ops:ui:real` auth/profile paths before printing or rendering");
    const permissions = await fs.readFile(path.join(rfcDir, "permissions.md"), "utf8");
    expect(permissions).toContain("Admin health lists `im:message.group_msg=verified`");
    expect(permissions).toContain("Ordered `chat.message.accepted route=bot_mention msgType=text -> chat.session.created|resumed`");
    expect(permissions).toContain("transition coordinates match admin session state and the accepted message has no same-message ignored log");
    expect(permissions).toContain("ordered accepted and steered/resumed logs share the same `messageId`, include `msgType=text`, target the same group @ admin session coordinates, and have no same-message ignored log");
    expect(permissions).toContain("paired by `sessionKey` with a `history_recovery` turn log");
    expect(permissions).toContain("session coordinates matching admin state");
    expect(permissions).toContain("`chat.outbound.posted` includes Feishu reply `messageId`, `format=text`, and session coordinates matching the same group @ session in admin state");
    expect(permissions).toContain("accepted logs match admin session state with no same-message ignored log");
    expect(permissions).toContain("callback emits `chat.card.callback.received` after a same group @ session broker-posted card, matching `messageId` when Feishu supplies one");
    expect(permissions).toContain("Ordered same-session `chat.outbound.posted format=card -> chat.card.callback.received -> chat.coauthor.confirmed`");
    expect(permissions).toContain("callback `messageId` matches the broker-posted card when Feishu supplies one");
    expect(permissions).toContain("otherwise ordered same-session/root coordinates plus callback `eventId`/`payloadRef` prove the tie");
    expect(permissions).toContain("downloaded message image inputs and outbound message images up to 10 MB");
    expect(permissions).toContain("file/resource transfers up to 30 MB");
    expect(permissions).toContain("larger outbound images fall back to file upload");
    expect(permissions).toContain("Limited-pilot checks may omit non-@ follow-up");
    expect(permissionRequest).toContain("chat.turn.completed");
    expect(permissionRequest).toContain("not only history backfill");
    expect(permissionRequest).toContain("chat.history.recovered recoveredCount > 0");
    expect(permissionRequest).toContain("same-session `history_recovery` turn log");
    expect(permissionRequest).toContain("session coordinates matching admin session state");
    expect(permissionRequest).toContain("no same-message ignored log");
    expect(permissionRequest).toContain("same group @ session's originating Feishu group/root message");
    expect(permissionRequest).toContain("same active group @ session");
    expect(permissionRequest).toContain("ordered `chat.message.accepted -> chat.session.resumed -> chat.turn.stopped` chain whose stop `messageId` matches");
    expect(permissionRequest).toContain("ordered accepted and steered/resumed logs sharing the same `messageId`, matching admin session state");
    expect(permissionRequest).toContain("admin-session-matching accepted logs");
    expect(permissionRequest).toContain("same `messageId` and `conversationId` as an admin-session-matching accepted event");
    expect(permissionRequest).toContain("detached handler failures must name a known Feishu handler (`message` or `interactive`) plus `errorClass`");
    expect(permissionRequest).toContain("chat.coauthor.confirmed");
    expect(permissionRequest).toContain("ordered same-session `chat.outbound.posted format=card -> chat.card.callback.received -> chat.coauthor.confirmed` chain");
    expect(permissionRequest).toContain("callback `messageId` matching the broker-posted card when Feishu supplies one");
    expect(permissionRequest).toContain("otherwise ordered same-session/root coordinates plus callback `eventId`/`payloadRef`");
    expect(permissionRequest).toContain("matching `candidateRevision`, `confirmedCount > 0`, and session coordinates matching admin session state");
    expect(permissionRequest).toContain("accepted, ignored, deduped, degraded, failed, and recovered behavior coverage");
    expect(permissionRequest).toContain("no later same-message accepted/session/turn dispatch");
    expect(permissionRequest).toContain("pnpm manual:feishu-smoke -- --env-file .env --base-url");
    expect(permissionRequest).toContain("Use `-- --env-file .env` when the rollout/admin settings are stored in a local env file");
  });

  it("keeps the README aligned with the Slack + Feishu user-facing surface", async () => {
    const readme = await fs.readFile(path.join(repoRoot, "README.md"), "utf8");
    const setup = await fs.readFile(path.join(repoRoot, "docs/feishu-setup.md"), "utf8");
    const implementation = await fs.readFile(path.join(rfcDir, "implementation.md"), "utf8");
    const packageJson = JSON.parse(await fs.readFile(path.join(repoRoot, "package.json"), "utf8")) as {
      readonly scripts?: Record<string, string>;
    };

    expect(readme).toContain("Slack + China Feishu bridge");
    expect(readme).toContain("FEISHU_ENABLED=true");
    expect(readme).toContain("same broker process");
    expect(readme).toContain("Feishu group `@bot ...`: create or resume a group session; private chats are ignored");
    expect(readme).toContain("FEISHU_GROUP_MESSAGE_MODE=all");
    expect(readme).toContain("`at_only` is a visible degraded mode");
    expect(readme).toContain("Feishu rollout:");
    expect(readme).toContain("`FEISHU_APP_ID`");
    expect(readme).toContain("`FEISHU_APP_SECRET`");
    expect(readme).toContain("at least one Feishu bot identity");
    expect(readme).toContain("`FEISHU_ALL_MESSAGE_DELIVERY_VERIFIED=true` only after the real non-@ follow-up smoke passes");
    expect(readme).toContain("keep `LOG_RAW_FEISHU_EVENTS=false` unless collecting a focused, redacted fixture");
    expect(readme).toContain("Operator-facing auth status and replacement output summarize filesystem paths instead of echoing full host paths");
    expect(readme).toContain("Profile command output also summarizes auth/profile paths without full host filesystem paths");
    expect(readme).toContain("metadata recursively redacts unsafe string fields while preserving safe posture text such as `FEISHU_APP_SECRET=missing`");
    expect(readme).toContain("sanitized pre-rollout log snapshot");
    expect(readme).toContain("redacts non-structured lines instead of copying raw Docker log text");
    expect(readme).toContain("admin UI exposes platform health and platform-aware session state");
    expect(readme).toContain("Generic chat APIs are platform-aware");
    expect(readme).toContain("broker-managed job callbacks that carry chat coordinates");
    expect(readme).toContain("without implying that every admin endpoint accepts a `platform` filter");
    expect(readme).toContain("Generic chat/job `platform` query/body values must be `slack` or `feishu`; invalid values return 400 `invalid_platform` instead of falling back to Slack");
    expect(readme).toContain("allowlisted `recentBrokerLogs` remain cross-platform");
    expect(readme).toContain("Generic `/chat/*` JSON/query contracts use canonical `conversationId` and `rootMessageId` fields");
    expect(readme).toContain("also accepts `conversation_id` and `root_message_id` aliases");
    expect(readme).toContain("For generic chat and job APIs, invalid `platform` values return 400 `invalid_platform` with allowed values `slack` and `feishu`");
    expect(readme).toContain("Generic file uploads use canonical `filePath` or `contentBase64`");
    expect(readme).toContain("`file_path` and `content_base64` aliases accepted and named in validation errors");
    expect(readme).toContain("Inline `contentBase64`/`content_base64` uploads require `filename` and must decode to non-empty file content");
    expect(readme).toContain("or non-empty `content_base64` plus `filename`");
    expect(readme).toContain("`richText`/`rich_text` and `card` can be structured JSON values or JSON strings");
    expect(readme).toContain("invalid JSON strings return 400 with only the field name, not the raw payload");
    expect(readme).toContain("request logging redact message text, state reasons, file comments/alt text, rich/card payloads");
    expect(readme).toContain("`/integrations/*` request logging redacts MCP call `arguments`");
    expect(readme).toContain("Registered jobs receive `CHAT_PLATFORM`, `CHAT_CONVERSATION_ID`, and `CHAT_ROOT_MESSAGE_ID`");
    expect(readme).toContain("legacy Slack `channel_id` and `thread_ts` aliases only for Slack compatibility when `platform` is omitted or set to `slack`");
    expect(readme).toContain("Invalid generic job `platform` values return 400 `invalid_platform` before coordinate validation");
    expect(readme).toContain("Job callback `detailsJson`/`details_json` fields and `/integrations/mcp-call` `arguments` can be structured JSON values or JSON strings");
    expect(readme).toContain("pnpm test:e2e:feishu-mock");
    expect(readme).toContain("pnpm rfc:feishu-audit");
    expect(readme).toContain("pnpm rfc:feishu-audit:local");
    expect(readme).toContain("implementation surfaces, test slices, behavior evidence probes, package-script gates");
    expect(readme).toContain("its JSON still keeps `ok=false` until real tenant gates pass");
    expect(readme).toContain("remaining real-tenant evidence gaps without sending Feishu messages");
    expect(readme).toContain("pnpm manual:feishu-smoke -- --preflight --env-file .env");
    expect(readme).toContain("keeps Node's own `--env-file` flag from intercepting the smoke-checker argument");
    expect(readme).toContain("value flags also accept `--flag=value`");
    expect(readme).toContain("missing values fail before another flag is swallowed");
    expect(setup).toContain("pnpm rfc:feishu-audit");
    expect(setup).toContain("pnpm rfc:feishu-audit:local");
    expect(setup).toContain("implementation surfaces, test slices, behavior evidence probes, and package-script gates");
    expect(setup).toContain("its JSON still keeps `ok=false` until real tenant gates pass");
    expect(setup).toContain("does not send Feishu messages and does not replace the real smoke");
    expect(implementation).toContain("pnpm rfc:feishu-audit");
    expect(implementation).toContain("pnpm rfc:feishu-audit:local");
    expect(implementation).toContain("progressive RFC readiness summary");
    expect(implementation).toContain("local implementation surfaces, local test slices, behavior evidence probes");
    expect(implementation).toContain("exits on `localOk` for CI/local readiness while preserving `ok=false`");
    expect(implementation).toContain("cannot replace `pnpm manual:feishu-smoke`");
    expect(setup).toContain("missing values fail before another flag is swallowed");
    expect(implementation).toContain("missing values fail before another flag is swallowed");
    expect(packageJson.scripts?.["manual:feishu-smoke"]).toContain("run-real-feishu-smoke.ts --");
    expect(packageJson.scripts?.["rfc:feishu-audit"]).toContain("run-rfc-0001-local-audit.ts");
    expect(packageJson.scripts?.["rfc:feishu-audit:local"]).toContain("--local-only");
    expect(readme).toContain("secret-bearing values only as set/missing");
    const smokeCliSource = await fs.readFile(path.join(repoRoot, "test/manual/run-real-feishu-smoke.ts"), "utf8");
    expect(smokeCliSource).toContain("Value flags require a following value that is not another --flag.");
    expect(readme).toContain("Feishu mock e2e gate");
    expect(readme).toContain("fixture replay");
    expect(readme).toContain("Slack+Feishu same-process readiness");
    expect(readme).toContain("curl -sS -X POST http://127.0.0.1:3000/chat/post-message");
    expect(readme).toContain("curl -sS -X POST http://127.0.0.1:3000/chat/post-file");
    expect(readme).toContain("`limit` (optional positive integer, clamped by `SLACK_HISTORY_API_MAX_LIMIT`; invalid values return 400 `invalid_limit`)");
    expect(readme).toContain("Generic chat history `limit` uses the same positive-integer validation");
    expect(readme).toContain("Generic chat history `format` uses the same `text|json` validation before broker delegation");
    expect(readme).toContain("For Feishu, outbound message images up to 10 MB are uploaded as image messages");
    expect(readme).toContain("fall back to file upload when still within the 30 MB file/resource limit");
    expect(packageJson.scripts?.["test:e2e:feishu-mock"]).toContain("test/feishu-codex-bridge.test.ts");
    expect(packageJson.scripts?.["test:e2e:feishu-mock"]).toContain("test/feishu-platform-adapter.test.ts");
    expect(packageJson.scripts?.["test:e2e:feishu-mock"]).toContain("test/feishu-fixture-replay.test.ts");
    expect(packageJson.scripts?.["test:e2e:feishu-mock"]).toContain("test/dual-platform-runtime.test.ts");
  });
});

async function collectMarkdownFiles(dir: string): Promise<string[]> {
  const entries = await fs.readdir(dir, {
    withFileTypes: true,
  });
  const files = await Promise.all(
    entries.map(async (entry) => {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        return collectMarkdownFiles(fullPath);
      }

      return entry.isFile() && entry.name.endsWith(".md") ? [fullPath] : [];
    }),
  );

  return files.flat().sort();
}

function extractLocalMarkdownLinks(content: string): string[] {
  const links: string[] = [];
  const markdownLink = /\[[^\]]+\]\(([^)]+)\)/gu;
  for (const match of content.matchAll(markdownLink)) {
    const rawLink = match[1]?.trim();
    if (!rawLink || rawLink.startsWith("#") || isExternalLink(rawLink)) {
      continue;
    }

    links.push(rawLink);
  }

  return links;
}

function parseRequiredLogFieldMatrix(content: string): Map<string, string[]> {
  const matrix = new Map<string, string[]>();
  const section = content.split("## Log Safety and Retention")[0] ?? content;
  for (const line of section.split(/\r?\n/u)) {
    const match = /^\|\s*`([^`]+)`\s*\|\s*(.+?)\s*\|$/u.exec(line.trim());
    if (!match) {
      continue;
    }

    const event = match[1];
    const fields = match[2];
    if (!event || !fields) {
      continue;
    }

    matrix.set(event, parseRequiredLogFieldsCell(fields));
  }

  return matrix;
}

function parseRequiredLogFieldsCell(cell: string): string[] {
  return cell
    .split(",")
    .map((part) => part.trim())
    .flatMap((part) => {
      if (part.includes(" when ") || part === "`messageId` or `fileId`") {
        return [];
      }

      const match = /`([^`]+)`/u.exec(part);
      const field = match?.[1];
      return field ? [field] : [];
    });
}

function parseStructuredLogFields(content: string): string[] {
  const section = content.split("Rules:", 1)[0] ?? content;
  return [...section.matchAll(/^- `([^`]+)`$/gmu)]
    .map((match) => match[1])
    .filter((field): field is string => Boolean(field))
    .sort();
}

function resolveMarkdownLink(markdownFile: string, link: string): string | undefined {
  const withoutAnchor = link.split("#", 1)[0] ?? "";
  if (!withoutAnchor) {
    return undefined;
  }

  const decoded = decodeURIComponent(withoutAnchor);
  return path.resolve(path.dirname(markdownFile), decoded);
}

function isExternalLink(link: string): boolean {
  return /^[a-z][a-z0-9+.-]*:/iu.test(link);
}
