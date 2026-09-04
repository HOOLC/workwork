from __future__ import annotations

import base64
import hashlib
import json
import os
import shlex
import tempfile
from collections import Counter
from collections.abc import Iterable, Iterator
from pathlib import Path
from typing import Any

from pier.agents.base import BaseAgent
from pier.environments.base import BaseEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.network import NetworkAllowlist

from zork_deepswe.agents.zork import load_benchmark_profile

PI_PROVIDER = "openai-codex"
PI_MODEL = "gpt-5.6-luna"
PI_THINKING = "max"
PI_SERVICE_TIER = "priority"
PI_VERSION = "0.84.1"
DATASET_COMMIT = "435ee89ec2f2e2289f33b0da4f992f0b7b7266b9"

REMOTE_ROOT = "/tmp/pi-deepswe"
REMOTE_ARCHIVE = f"{REMOTE_ROOT}/pi-linux-amd64.tar.gz"
REMOTE_BUNDLE = f"{REMOTE_ROOT}/bundle"
REMOTE_CONFIG = f"{REMOTE_ROOT}/config"
REMOTE_SESSIONS = f"{REMOTE_ROOT}/sessions"
REMOTE_INSTRUCTION = f"{REMOTE_ROOT}/instruction.txt"
REMOTE_APPEND_PROMPT = f"{REMOTE_ROOT}/append-system-prompt.md"
REMOTE_FAST_EXTENSION = f"{REMOTE_ROOT}/fast-service-tier.mjs"
REMOTE_PI_ENTRYPOINT = (
    f"{REMOTE_BUNDLE}/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
)
REMOTE_EVENTS = "/logs/agent/pi-events.jsonl"
REMOTE_STDERR = "/logs/agent/pi.stderr.log"
REMOTE_WIRE = "/logs/agent/pi-wire.jsonl"

PI_APPEND_SYSTEM_PROMPT = """This is a non-interactive coding benchmark. Solve the supplied task by changing the repository and verifying the result.

Recommended workflow:
1. Analyze the codebase by finding and reading the relevant files.
2. Reproduce the issue or establish a failing check when practical.
3. Edit the source code to implement the required behavior.
4. Verify the fix by running the relevant checks again.
5. Test edge cases that follow from the task.

Do not merely describe a solution: make the changes. There is no interactive user, so resolve the task from the supplied instruction and repository evidence. When the implementation is complete and verified, give the normal final response and stop."""


def _required_string(document: dict[str, Any], key: str, source: Path) -> str:
    value = document.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{key} is missing from {source}")
    return value


def _jwt_expiration_ms(token: str) -> int:
    parts = token.split(".")
    if len(parts) != 3:
        raise ValueError("ChatGPT access token is not a JWT")
    payload = parts[1] + "=" * (-len(parts[1]) % 4)
    try:
        document = json.loads(base64.urlsafe_b64decode(payload))
    except (ValueError, json.JSONDecodeError) as error:
        raise ValueError("ChatGPT access token has an invalid JWT payload") from error
    expires = document.get("exp") if isinstance(document, dict) else None
    if not isinstance(expires, int) or expires <= 0:
        raise ValueError("ChatGPT access token has no expiration")
    return expires * 1000


def build_pi_auth_document(auth_path: Path) -> dict[str, Any]:
    resolved = auth_path.expanduser().resolve()
    source = json.loads(resolved.read_text(encoding="utf-8"))
    if not isinstance(source, dict) or source.get("auth_mode") != "chatgpt":
        raise ValueError(f"{resolved} does not contain ChatGPT subscription auth")
    tokens = source.get("tokens")
    if not isinstance(tokens, dict):
        raise TypeError(f"tokens are missing from {resolved}")
    access = _required_string(tokens, "access_token", resolved)
    return {
        PI_PROVIDER: {
            "type": "oauth",
            "access": access,
            "refresh": _required_string(tokens, "refresh_token", resolved),
            "expires": _jwt_expiration_ms(access),
            "accountId": _required_string(tokens, "account_id", resolved),
        }
    }


def build_pi_models_document(
    context_window_tokens: int, max_output_tokens: int
) -> dict[str, Any]:
    if context_window_tokens <= 0:
        raise ValueError("context window must be positive")
    if max_output_tokens <= 0 or max_output_tokens >= context_window_tokens:
        raise ValueError("max output tokens must be positive and less than context")
    return {
        "providers": {
            PI_PROVIDER: {
                "modelOverrides": {
                    PI_MODEL: {
                        "contextWindow": context_window_tokens,
                        "maxTokens": max_output_tokens,
                    }
                }
            }
        }
    }


def build_pi_settings_document(
    max_output_tokens: int, *, transport: str = "websocket-cached"
) -> dict[str, Any]:
    if max_output_tokens <= 0:
        raise ValueError("max output tokens must be positive")
    return {
        "transport": transport,
        "httpIdleTimeoutMs": 0,
        "defaultProjectTrust": "never",
        "compaction": {
            "enabled": True,
            "reserveTokens": max_output_tokens,
            "keepRecentTokens": 20_000,
        },
        "quietStartup": True,
        "enableInstallTelemetry": False,
        "enableAnalytics": False,
    }


def build_fast_extension_source(
    provider: str = PI_PROVIDER,
    model: str = PI_MODEL,
    service_tier: str | None = PI_SERVICE_TIER,
) -> str:
    summary_path = json.dumps(REMOTE_WIRE)
    payload = (
        "event.payload"
        if service_tier is None
        else f"{{ ...event.payload, service_tier: {json.dumps(service_tier)} }}"
    )
    return f"""import {{ appendFileSync }} from "node:fs";

export default function registerFastServiceTier(pi) {{
  pi.on("before_provider_request", (event, ctx) => {{
    if (ctx.model?.provider !== {json.dumps(provider)} || ctx.model?.id !== {json.dumps(model)}) {{
      throw new Error(`unexpected benchmark model: ${{ctx.model?.provider}}/${{ctx.model?.id}}`);
    }}
    const payload = {payload};
    appendFileSync(
      {summary_path},
      JSON.stringify({{
        timestamp: new Date().toISOString(),
        provider: ctx.model.provider,
        model: ctx.model.id,
        reasoning: payload.reasoning?.effort ?? null,
        service_tier: payload.service_tier ?? null,
        store: payload.store ?? null,
        stream: payload.stream ?? null,
        parallel_tool_calls: payload.parallel_tool_calls ?? null,
        max_output_tokens: payload.max_output_tokens ?? null,
        context_window_tokens: ctx.model.contextWindow,
        input_items: Array.isArray(payload.input) ? payload.input.length : null,
        tools: Array.isArray(payload.tools) ? payload.tools.length : 0,
      }}) + "\\n",
      "utf8",
    );
    return payload;
  }});
}}
"""


def _non_negative_integer(value: Any, field: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"Pi usage {field} must be a non-negative integer")
    return value


def _usage_values(usage: Any) -> tuple[int, int, int, int, int, float]:
    if not isinstance(usage, dict):
        raise TypeError("Pi provider record is missing usage")
    uncached = _non_negative_integer(usage.get("input"), "input")
    cache_read = _non_negative_integer(usage.get("cacheRead"), "cacheRead")
    cache_write = _non_negative_integer(usage.get("cacheWrite"), "cacheWrite")
    output = _non_negative_integer(usage.get("output"), "output")
    reasoning_value = usage.get("reasoning", 0)
    reasoning = _non_negative_integer(reasoning_value, "reasoning")
    cost_document = usage.get("cost")
    cost = 0.0
    if isinstance(cost_document, dict):
        raw_cost = cost_document.get("total", 0)
        if isinstance(raw_cost, (int, float)) and not isinstance(raw_cost, bool):
            cost = float(raw_cost)
    return uncached, cache_read, cache_write, output, reasoning, cost


def aggregate_pi_session_entries(entries: Iterable[dict[str, Any]]) -> dict[str, Any]:
    input_tokens = 0
    cached_input_tokens = 0
    uncached_input_tokens = 0
    output_tokens = 0
    output_reasoning_tokens = 0
    peak_context_tokens = 0
    provider_requests = 0
    provider_errors = 0
    agent_steps = 0
    compactions = 0
    tool_calls = 0
    tool_errors = 0
    multi_tool_call_rounds = 0
    max_tool_calls_per_round = 0
    tool_counts: Counter[str] = Counter()
    providers: set[str] = set()
    models: set[str] = set()
    apis: set[str] = set()
    total_cost_usd = 0.0
    final_assistant_content = ""
    final_stop_reason: str | None = None

    def apply_usage(usage: Any) -> None:
        nonlocal input_tokens
        nonlocal cached_input_tokens
        nonlocal uncached_input_tokens
        nonlocal output_tokens
        nonlocal output_reasoning_tokens
        nonlocal peak_context_tokens
        nonlocal provider_requests
        nonlocal total_cost_usd
        uncached, cache_read, cache_write, output, reasoning, cost = _usage_values(
            usage
        )
        current_input = uncached + cache_read + cache_write
        provider_requests += 1
        input_tokens += current_input
        cached_input_tokens += cache_read
        uncached_input_tokens += uncached + cache_write
        output_tokens += output
        output_reasoning_tokens += reasoning
        peak_context_tokens = max(peak_context_tokens, current_input)
        total_cost_usd += cost

    for entry in entries:
        entry_type = entry.get("type")
        if entry_type == "compaction":
            compactions += 1
            apply_usage(entry.get("usage"))
            continue
        if entry_type != "message":
            continue
        message = entry.get("message")
        if not isinstance(message, dict):
            continue
        role = message.get("role")
        if role == "toolResult":
            tool_errors += int(message.get("isError") is True)
            continue
        if role != "assistant":
            continue

        agent_steps += 1
        apply_usage(message.get("usage"))
        provider = message.get("provider")
        model = message.get("model")
        api = message.get("api")
        if isinstance(provider, str):
            providers.add(provider)
        if isinstance(model, str):
            models.add(model)
        if isinstance(api, str):
            apis.add(api)
        stop_reason = message.get("stopReason")
        if stop_reason in ("error", "aborted"):
            provider_errors += 1
        final_stop_reason = stop_reason if isinstance(stop_reason, str) else None

        content = message.get("content")
        text_blocks: list[str] = []
        current_tool_calls = 0
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "text" and isinstance(block.get("text"), str):
                    text_blocks.append(block["text"])
                elif block.get("type") == "toolCall":
                    current_tool_calls += 1
                    name = block.get("name")
                    if isinstance(name, str):
                        tool_counts[name] += 1
        final_assistant_content = "".join(text_blocks)
        tool_calls += current_tool_calls
        multi_tool_call_rounds += int(current_tool_calls > 1)
        max_tool_calls_per_round = max(max_tool_calls_per_round, current_tool_calls)

    return {
        "input_tokens": input_tokens,
        "cached_input_tokens": cached_input_tokens,
        "uncached_input_tokens": uncached_input_tokens,
        "output_tokens": output_tokens,
        "output_reasoning_tokens": output_reasoning_tokens,
        "output_text_tokens": output_tokens - output_reasoning_tokens,
        "total_tokens": input_tokens + output_tokens,
        "peak_context_tokens": peak_context_tokens,
        "agent_steps": agent_steps,
        "provider_requests": provider_requests,
        "provider_errors": provider_errors,
        "compactions": compactions,
        "tool_calls": tool_calls,
        "tool_errors": tool_errors,
        "multi_tool_call_rounds": multi_tool_call_rounds,
        "max_tool_calls_per_round": max_tool_calls_per_round,
        "tool_counts": dict(sorted(tool_counts.items())),
        "providers": sorted(providers),
        "models": sorted(models),
        "apis": sorted(apis),
        "cost_usd": total_cost_usd,
        "final_assistant_content_bytes": len(final_assistant_content.encode("utf-8")),
        "final_stop_reason": final_stop_reason,
        "completion_submitted": final_stop_reason == "stop",
    }


def iter_pi_session_entries(session_dir: Path) -> Iterator[dict[str, Any]]:
    for path in sorted(session_dir.rglob("*.jsonl"), key=lambda item: str(item)):
        with path.open("r", encoding="utf-8") as stream:
            for line in stream:
                if not line.strip():
                    continue
                entry = json.loads(line)
                if isinstance(entry, dict):
                    yield entry


def _positive_integer_argument(value: int | str, name: str) -> int:
    if isinstance(value, bool):
        raise TypeError(f"{name} must be a positive integer")
    try:
        parsed = int(value)
    except (TypeError, ValueError) as error:
        raise ValueError(f"{name} must be a positive integer") from error
    if parsed <= 0:
        raise ValueError(f"{name} must be a positive integer")
    return parsed


class PiDeepSweAgent(BaseAgent):
    """Pier adapter for native Pi with subscription auth or an API-key profile."""

    SUPPORTS_ATIF = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = None,
        pi_bundle: str | None = None,
        codex_auth_file: str | None = None,
        profile_file: str | None = None,
        thinking: str = PI_THINKING,
        context_window_tokens: int | str | None = None,
        max_output_tokens: int | str | None = None,
        service_tier: str = PI_SERVICE_TIER,
        pi_version: str = PI_VERSION,
        **kwargs: Any,
    ) -> None:
        super().__init__(logs_dir=logs_dir, model_name=model_name, **kwargs)
        if not pi_bundle:
            raise ValueError("pi_bundle is required")
        self._bundle = Path(pi_bundle).expanduser().resolve()
        if not self._bundle.is_file():
            raise FileNotFoundError(f"Pi bundle not found: {self._bundle}")
        if profile_file:
            if codex_auth_file:
                raise ValueError("use either profile_file or codex_auth_file")
            profile = load_benchmark_profile(
                Path(profile_file), model_name=model_name or "", thinking=thinking
            )
            selected = next(
                m for m in profile.document["models"] if m["id"] == profile.model
            )
            auth = profile.document.get("auth", {})
            if auth.get("type") != "api_key":
                raise ValueError("Pi profile requires API-key auth")
            if selected.get("api") != "openai-responses" or not profile.streaming:
                raise ValueError("Pi profile requires streaming openai-responses")
            limits = selected["limits"]
            for name, override in (
                ("context_window_tokens", context_window_tokens),
                ("max_output_tokens", max_output_tokens),
            ):
                if override is not None and int(override) != limits[name]:
                    raise ValueError(f"{name} must match the profile")
            self._provider, self._model = profile.provider, profile.model
            self._context_window_tokens = limits["context_window_tokens"]
            self._max_output_tokens = limits["max_output_tokens"]
            self._transport = "sse"
            self._service_tier = None
            self._network_domains = [profile.network_domain]
            self._auth_document = {
                self._provider: {
                    "type": "api_key",
                    "key": _required_string(auth, "key", profile.source_path),
                }
            }
            self._models_document = {
                "providers": {
                    self._provider: {
                        "baseUrl": profile.base_url,
                        "api": selected["api"],
                        "headers": profile.document.get("headers", {}),
                        "models": [
                            {
                                "id": self._model,
                                "name": self._model,
                                "reasoning": True,
                                "thinkingLevelMap": {
                                    level: level
                                    if level in selected["thinking"]
                                    else None
                                    for level in (
                                        "off",
                                        "minimal",
                                        "low",
                                        "medium",
                                        "high",
                                        "xhigh",
                                        "max",
                                    )
                                },
                                "input": selected["capabilities"]["input"],
                                "contextWindow": self._context_window_tokens,
                                "maxTokens": self._max_output_tokens,
                                "samplingParams": {
                                    "parallel_tool_calls": profile.parallel_tool_calls
                                },
                                "cost": {
                                    "input": 0,
                                    "output": 0,
                                    "cacheRead": 0,
                                    "cacheWrite": 0,
                                },
                            }
                        ],
                    }
                }
            }
        else:
            normalized_model = (
                model_name.removeprefix(f"{PI_PROVIDER}/") if model_name else None
            )
            if normalized_model != PI_MODEL:
                raise ValueError(f"model_name must be {PI_PROVIDER}/{PI_MODEL}")
            if service_tier != PI_SERVICE_TIER:
                raise ValueError(f"service_tier must be {PI_SERVICE_TIER}")
            if not codex_auth_file:
                raise ValueError("codex_auth_file is required")
            self._provider, self._model = PI_PROVIDER, PI_MODEL
            self._context_window_tokens = (
                256_000 if context_window_tokens is None else context_window_tokens
            )
            self._max_output_tokens = (
                32_000 if max_output_tokens is None else max_output_tokens
            )
            self._transport = "websocket-cached"
            self._service_tier = service_tier
            self._network_domains = ["auth.openai.com", "chatgpt.com"]
            self._auth_document = build_pi_auth_document(Path(codex_auth_file))
            self._models_document = build_pi_models_document(
                int(self._context_window_tokens), int(self._max_output_tokens)
            )
        self._context_window_tokens = _positive_integer_argument(
            self._context_window_tokens, "context_window_tokens"
        )
        self._max_output_tokens = _positive_integer_argument(
            self._max_output_tokens, "max_output_tokens"
        )
        if self._max_output_tokens >= self._context_window_tokens:
            raise ValueError("max output tokens must be less than context")
        self._thinking = thinking
        self._pi_version = pi_version
        self._bundle_sha256 = _file_sha256(self._bundle)

    @staticmethod
    def name() -> str:
        return "pi"

    def version(self) -> str:
        return f"{self._pi_version}+sha256:{self._bundle_sha256}"

    def network_allowlist(self) -> NetworkAllowlist:
        return NetworkAllowlist(domains=self._network_domains)

    async def setup(self, environment: BaseEnvironment) -> None:
        created = await environment.exec(
            " && ".join(
                [
                    f"mkdir -p {shlex.quote(REMOTE_ROOT)}",
                    f"mkdir -p {shlex.quote(REMOTE_CONFIG)}",
                    f"mkdir -p {shlex.quote(REMOTE_SESSIONS)}",
                    "mkdir -p /logs/agent",
                ]
            ),
            user="root",
        )
        self._require_success(created, "create Pi benchmark directories")
        await environment.upload_file(self._bundle, REMOTE_ARCHIVE)
        uploaded_hash = await environment.exec(
            f"sha256sum {shlex.quote(REMOTE_ARCHIVE)}", user="root"
        )
        self._require_success(uploaded_hash, "hash uploaded Pi bundle")
        remote_sha256 = (uploaded_hash.stdout or "").strip().split(maxsplit=1)[0]
        if remote_sha256 != self._bundle_sha256:
            raise RuntimeError("uploaded Pi bundle hash does not match source artifact")

        extracted = await environment.exec(
            f"mkdir -p {shlex.quote(REMOTE_BUNDLE)} && "
            f"tar -xzf {shlex.quote(REMOTE_ARCHIVE)} -C {shlex.quote(REMOTE_BUNDLE)}",
            user="root",
            timeout_sec=300,
        )
        self._require_success(extracted, "extract Pi bundle")

        with tempfile.TemporaryDirectory(prefix="pi-deepswe-config-") as directory:
            root = Path(directory)
            auth_path = root / "auth.json"
            models_path = root / "models.json"
            settings_path = root / "settings.json"
            prompt_path = root / "append-system-prompt.md"
            extension_path = root / "fast-service-tier.mjs"
            _write_json_private(auth_path, self._auth_document)
            _write_json_private(models_path, self._models_document)
            _write_json_private(
                settings_path,
                build_pi_settings_document(
                    self._max_output_tokens, transport=self._transport
                ),
            )
            prompt_path.write_text(PI_APPEND_SYSTEM_PROMPT + "\n", encoding="utf-8")
            extension_path.write_text(
                build_fast_extension_source(
                    self._provider, self._model, self._service_tier
                ),
                encoding="utf-8",
            )
            await environment.upload_file(auth_path, f"{REMOTE_CONFIG}/auth.json")
            await environment.upload_file(models_path, f"{REMOTE_CONFIG}/models.json")
            await environment.upload_file(
                settings_path, f"{REMOTE_CONFIG}/settings.json"
            )
            await environment.upload_file(prompt_path, REMOTE_APPEND_PROMPT)
            await environment.upload_file(extension_path, REMOTE_FAST_EXTENSION)

        owner = await environment.exec("id -u", user=environment.default_user)
        self._require_success(owner, "resolve Pi agent user")
        uid = (owner.stdout or "").strip()
        if not uid.isdigit():
            raise RuntimeError("could not resolve numeric Pi agent uid")
        permissions = await environment.exec(
            " && ".join(
                [
                    f"chown -R {uid} {shlex.quote(REMOTE_ROOT)} /logs/agent",
                    f"chmod 700 {shlex.quote(REMOTE_CONFIG)} {shlex.quote(REMOTE_SESSIONS)}",
                    f"chmod 600 {shlex.quote(REMOTE_CONFIG + '/auth.json')}",
                    f"chmod 600 {shlex.quote(REMOTE_CONFIG + '/models.json')}",
                    f"chmod 600 {shlex.quote(REMOTE_CONFIG + '/settings.json')}",
                    f"chmod 644 {shlex.quote(REMOTE_APPEND_PROMPT)} {shlex.quote(REMOTE_FAST_EXTENSION)}",
                ]
            ),
            user="root",
        )
        self._require_success(permissions, "set Pi benchmark ownership")

        environment_variables = self._pi_environment(environment)
        version_result = await environment.exec(
            f"node {shlex.quote(REMOTE_PI_ENTRYPOINT)} --version",
            cwd="/tmp",
            env=environment_variables,
        )
        self._require_success(version_result, "run uploaded Pi")
        observed_version = (version_result.stdout or "").strip()
        if observed_version != self._pi_version:
            raise RuntimeError(
                f"uploaded Pi version is {observed_version!r}, expected {self._pi_version!r}"
            )
        auth_check = await environment.exec(
            f"node {shlex.quote(REMOTE_PI_ENTRYPOINT)} auth check "
            f"--provider {shlex.quote(self._provider)} --no-refresh",
            cwd="/tmp",
            env=environment_variables,
        )
        self._require_success(auth_check, "validate Pi OpenAI Codex auth")
        if (auth_check.stdout or "").strip() != "ready":
            raise RuntimeError("Pi OpenAI Codex auth is not ready")

        setup_sizes = await environment.exec(
            " ".join(
                [
                    "du -sk",
                    shlex.quote(REMOTE_ARCHIVE),
                    shlex.quote(REMOTE_BUNDLE),
                    shlex.quote(REMOTE_CONFIG),
                ]
            ),
            cwd="/tmp",
        )
        self._require_success(setup_sizes, "measure Pi setup disk")
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "pi-setup-sizes.txt").write_text(
            setup_sizes.stdout or "", encoding="utf-8"
        )
        self._write_public_manifest()

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="pi-deepswe-instruction-") as directory:
            instruction_path = Path(directory) / "instruction.txt"
            instruction_path.write_text(instruction, encoding="utf-8")
            await environment.upload_file(instruction_path, REMOTE_INSTRUCTION)

        command = " ".join(
            [
                "node",
                shlex.quote(REMOTE_PI_ENTRYPOINT),
                "--mode json",
                "--provider",
                shlex.quote(self._provider),
                "--model",
                shlex.quote(self._model),
                "--thinking",
                shlex.quote(self._thinking),
                "--session-dir",
                shlex.quote(REMOTE_SESSIONS),
                "--append-system-prompt",
                shlex.quote(REMOTE_APPEND_PROMPT),
                "--extension",
                shlex.quote(REMOTE_FAST_EXTENSION),
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "--no-approve",
                "--offline",
                f"< {shlex.quote(REMOTE_INSTRUCTION)}",
                f"> {shlex.quote(REMOTE_EVENTS)}",
                f"2> {shlex.quote(REMOTE_STDERR)}",
            ]
        )
        result = await environment.exec(
            command,
            cwd="/app",
            env=self._pi_environment(environment),
        )

        metrics: dict[str, Any] | None = None
        collection_error: BaseException | None = None
        try:
            metrics = await self._collect_artifacts(environment)
            self._populate_context(context, metrics)
        except Exception as error:  # noqa: BLE001 - preserve the primary Pi exit
            collection_error = error

        if result.return_code != 0:
            if collection_error is not None:
                raise RuntimeError(
                    f"Pi exited with code {result.return_code}; diagnostics also failed: {collection_error!r}"
                ) from collection_error
            stderr_tail = await self._read_stderr_tail(environment)
            raise RuntimeError(
                f"Pi exited with code {result.return_code}: {stderr_tail or 'no stderr'}"
            )
        if collection_error is not None:
            raise collection_error
        if metrics is None:
            raise RuntimeError("Pi did not produce diagnosable session metrics")
        if not metrics["completion_submitted"]:
            raise RuntimeError(
                f"Pi stopped without its native stop completion: {metrics['final_stop_reason']!r}"
            )
        if metrics["providers"] != [self._provider] or metrics["models"] != [
            self._model
        ]:
            raise RuntimeError(
                "Pi session did not use only the configured provider/model"
            )

    async def _collect_artifacts(self, environment: BaseEnvironment) -> dict[str, Any]:
        session_logs = self.logs_dir / "pi-session"
        await environment.download_dir(
            source_dir=REMOTE_SESSIONS, target_dir=session_logs
        )
        for remote_path, local_name in (
            (REMOTE_EVENTS, "pi-events.jsonl"),
            (REMOTE_STDERR, "pi.stderr.log"),
            (REMOTE_WIRE, "pi-wire.jsonl"),
        ):
            if await environment.is_file(remote_path):
                await environment.download_file(
                    source_path=remote_path,
                    target_path=self.logs_dir / local_name,
                )
        metrics = aggregate_pi_session_entries(iter_pi_session_entries(session_logs))
        metrics.update(
            {
                "agent_api": "pi-json-session/v3",
                "pi_version": self._pi_version,
                "bundle_sha256": self._bundle_sha256,
                "dataset_commit": DATASET_COMMIT,
                "provider": self._provider,
                "model": self._model,
                "thinking": self._thinking,
                "service_tier": self._service_tier,
                "context_window_tokens": self._context_window_tokens,
                "max_output_tokens": self._max_output_tokens,
                "transport": self._transport,
                "streaming": True,
            }
        )
        (self.logs_dir / "pi-metrics.json").write_text(
            json.dumps(metrics, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return metrics

    def _pi_environment(self, environment: BaseEnvironment) -> dict[str, str] | None:
        return environment.agent_process_env(
            {
                "PI_CODING_AGENT_DIR": REMOTE_CONFIG,
                "PI_CODING_AGENT_SESSION_DIR": REMOTE_SESSIONS,
                "PI_OFFLINE": "1",
                "PI_SKIP_VERSION_CHECK": "1",
                "PI_TELEMETRY": "0",
                "NO_COLOR": "1",
            }
        )

    async def _read_stderr_tail(self, environment: BaseEnvironment) -> str:
        if not await environment.is_file(REMOTE_STDERR):
            return ""
        result = await environment.exec(
            f"tail -c 4000 {shlex.quote(REMOTE_STDERR)}", cwd="/tmp"
        )
        return (result.stdout or result.stderr or "").strip()

    def _write_public_manifest(self) -> None:
        manifest = {
            "agent": "pi",
            "pi_version": self._pi_version,
            "bundle_sha256": self._bundle_sha256,
            "dataset_commit": DATASET_COMMIT,
            "provider": self._provider,
            "model": self._model,
            "thinking": self._thinking,
            "service_tier": self._service_tier,
            "context_window_tokens": self._context_window_tokens,
            "max_output_tokens": self._max_output_tokens,
            "transport": self._transport,
        }
        (self.logs_dir / "pi-manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    @staticmethod
    def _populate_context(context: AgentContext, metrics: dict[str, Any]) -> None:
        context.n_input_tokens = metrics["input_tokens"]
        context.n_cache_tokens = metrics["cached_input_tokens"]
        context.n_output_tokens = metrics["output_tokens"]
        context.cost_usd = metrics["cost_usd"]
        context.peak_context_tokens = metrics["peak_context_tokens"]
        context.summarization_count = metrics["compactions"]
        context.n_agent_steps = metrics["agent_steps"]
        context.metadata = metrics

    @staticmethod
    def _require_success(result: Any, action: str) -> None:
        if result.return_code != 0:
            output = result.stderr or result.stdout or "no output"
            raise RuntimeError(f"failed to {action}: {output}")


def _write_json_private(path: Path, document: dict[str, Any]) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(document, stream, separators=(",", ":"))
        stream.write("\n")
    os.chmod(path, 0o600)


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()
