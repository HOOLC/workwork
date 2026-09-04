from __future__ import annotations

import json
import shlex
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from pier.agents.base import BaseAgent
from pier.environments.base import BaseEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.network import NetworkAllowlist

from zork_deepswe.agents.pi import (
    DATASET_COMMIT,
    PI_APPEND_SYSTEM_PROMPT,
    PI_VERSION,
    _file_sha256,
    _write_json_private,
    aggregate_pi_session_entries,
    iter_pi_session_entries,
)

QWEN_PI_PROVIDER = "qwen38"
QWEN_PROFILE_PROVIDER = "openai-compatible"
QWEN_BILLING = "usage"
QWEN_MODEL = "/models/GT-NVFP4-5090"
QWEN_THINKING = "xhigh"
QWEN_API = "openai-responses"
QWEN_CONTEXT_WINDOW_TOKENS = 256_000
QWEN_MAX_OUTPUT_TOKENS = 56_000
THINKING_LEVELS = ("off", "minimal", "low", "medium", "high", "xhigh", "max")

REMOTE_ROOT = "/tmp/pi-qwen-deepswe"
REMOTE_ARCHIVE = f"{REMOTE_ROOT}/pi-linux-amd64.tar.gz"
REMOTE_BUNDLE = f"{REMOTE_ROOT}/bundle"
REMOTE_CONFIG = f"{REMOTE_ROOT}/config"
REMOTE_SESSIONS = f"{REMOTE_ROOT}/sessions"
REMOTE_INSTRUCTION = f"{REMOTE_ROOT}/instruction.txt"
REMOTE_APPEND_PROMPT = f"{REMOTE_ROOT}/append-system-prompt.md"
REMOTE_WIRE_AUDIT = f"{REMOTE_ROOT}/qwen-wire-audit.mjs"
REMOTE_PI_ENTRYPOINT = (
    f"{REMOTE_BUNDLE}/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
)
REMOTE_EVENTS = "/logs/agent/pi-events.jsonl"
REMOTE_STDERR = "/logs/agent/pi.stderr.log"
REMOTE_WIRE = "/logs/agent/pi-wire.jsonl"


@dataclass(frozen=True)
class PiQwenProfile:
    profile_id: str
    provider: str
    billing: str
    base_url: str
    network_domain: str
    model: str
    api: str
    thinking: str
    thinking_levels: tuple[str, ...]
    default_thinking: str
    input_types: tuple[str, ...]
    context_window_tokens: int
    max_output_tokens: int
    streaming: bool
    parallel_tool_calls: bool
    source_path: Path = field(repr=False)
    auth_key: str = field(repr=False)
    headers: dict[str, str] = field(repr=False)


def _required_string(document: dict[str, Any], key: str, source: Path) -> str:
    value = document.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{key} is missing from {source}")
    return value


def _required_positive_integer(document: dict[str, Any], key: str, source: Path) -> int:
    value = document.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise ValueError(f"{key} must be a positive integer in {source}")
    return value


def load_pi_qwen_profile(
    profile_path: Path, *, model_name: str, thinking: str
) -> PiQwenProfile:
    resolved = profile_path.expanduser().resolve()
    document = json.loads(resolved.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise TypeError(f"profile must be a JSON object: {resolved}")

    profile_id = resolved.stem
    if not profile_id or profile_id == "auto":
        raise ValueError(f"profile file has an invalid profile id: {resolved}")
    provider = _required_string(document, "provider", resolved)
    billing = _required_string(document, "billing", resolved)
    base_url = _required_string(document, "base_url", resolved)
    if provider != QWEN_PROFILE_PROVIDER:
        raise ValueError(f"profile provider must be {QWEN_PROFILE_PROVIDER}")
    if billing != QWEN_BILLING:
        raise ValueError(f"profile billing must be {QWEN_BILLING}")
    parsed_url = urlparse(base_url)
    if parsed_url.scheme != "https" or not parsed_url.hostname:
        raise ValueError(f"profile base_url must be an HTTPS URL in {resolved}")

    raw_headers = document.get("headers", {})
    if not isinstance(raw_headers, dict) or not all(
        isinstance(key, str) and isinstance(value, str)
        for key, value in raw_headers.items()
    ):
        raise ValueError(f"profile headers must contain only strings in {resolved}")

    auth = document.get("auth")
    if not isinstance(auth, dict) or auth.get("type") != "api_key":
        raise ValueError(f"profile auth must be api_key in {resolved}")
    auth_key = _required_string(auth, "key", resolved)

    models = document.get("models")
    if not isinstance(models, list):
        raise TypeError(f"profile models are missing from {resolved}")
    normalized_model = (
        model_name.removeprefix(f"{QWEN_PI_PROVIDER}/")
        if model_name.startswith(f"{QWEN_PI_PROVIDER}/")
        else model_name
    )
    selected = [
        item
        for item in models
        if isinstance(item, dict) and item.get("id") == normalized_model
    ]
    if len(selected) != 1 or normalized_model != QWEN_MODEL:
        raise ValueError(f"profile must declare exactly model {QWEN_MODEL}")
    model = selected[0]

    api = _required_string(model, "api", resolved)
    if api != QWEN_API:
        raise ValueError(f"profile model API must be {QWEN_API}")
    streaming = model.get("streaming", True)
    if streaming is not True:
        raise ValueError("Pi openai-responses control requires profile streaming=true")
    parallel_tool_calls = model.get("parallel_tool_calls", False)
    if not isinstance(parallel_tool_calls, bool):
        raise TypeError("profile parallel_tool_calls must be boolean")

    raw_thinking_levels = model.get("thinking")
    if not isinstance(raw_thinking_levels, list) or not all(
        isinstance(level, str) and level in THINKING_LEVELS
        for level in raw_thinking_levels
    ):
        raise ValueError("profile model thinking levels are invalid")
    thinking_levels = tuple(raw_thinking_levels)
    if thinking != QWEN_THINKING or thinking not in thinking_levels:
        raise ValueError(f"profile model must support thinking {QWEN_THINKING}")
    default_thinking = _required_string(model, "default_thinking", resolved)
    if default_thinking not in thinking_levels:
        raise ValueError("profile default_thinking is not a supported level")

    capabilities = model.get("capabilities")
    if not isinstance(capabilities, dict):
        raise TypeError("profile model capabilities are missing")
    input_types_value = capabilities.get("input")
    if input_types_value != ["text"]:
        raise ValueError("Qwen DeepSWE control requires text-only input capability")
    input_types = tuple(input_types_value)

    limits = model.get("limits")
    if not isinstance(limits, dict):
        raise TypeError("profile model limits are missing")
    context_window_tokens = _required_positive_integer(
        limits, "context_window_tokens", resolved
    )
    max_output_tokens = _required_positive_integer(
        limits, "max_output_tokens", resolved
    )
    if context_window_tokens != QWEN_CONTEXT_WINDOW_TOKENS:
        raise ValueError(
            f"profile context_window_tokens must be {QWEN_CONTEXT_WINDOW_TOKENS}"
        )
    if max_output_tokens != QWEN_MAX_OUTPUT_TOKENS:
        raise ValueError(f"profile max_output_tokens must be {QWEN_MAX_OUTPUT_TOKENS}")

    return PiQwenProfile(
        profile_id=profile_id,
        provider=provider,
        billing=billing,
        base_url=base_url,
        network_domain=parsed_url.hostname,
        model=normalized_model,
        api=api,
        thinking=thinking,
        thinking_levels=thinking_levels,
        default_thinking=default_thinking,
        input_types=input_types,
        context_window_tokens=context_window_tokens,
        max_output_tokens=max_output_tokens,
        streaming=streaming,
        parallel_tool_calls=parallel_tool_calls,
        source_path=resolved,
        auth_key=auth_key,
        headers=dict(raw_headers),
    )


def build_pi_qwen_auth_document(profile: PiQwenProfile) -> dict[str, Any]:
    return {
        QWEN_PI_PROVIDER: {
            "type": "api_key",
            "key": profile.auth_key,
        }
    }


def build_pi_qwen_models_document(profile: PiQwenProfile) -> dict[str, Any]:
    thinking_map = {
        level: level if level in profile.thinking_levels else None
        for level in THINKING_LEVELS
    }
    return {
        "providers": {
            QWEN_PI_PROVIDER: {
                "baseUrl": profile.base_url,
                "api": profile.api,
                "headers": dict(profile.headers),
                "models": [
                    {
                        "id": profile.model,
                        "name": profile.model,
                        "reasoning": True,
                        "thinkingLevelMap": thinking_map,
                        "input": list(profile.input_types),
                        "contextWindow": profile.context_window_tokens,
                        "maxTokens": profile.max_output_tokens,
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


def build_pi_qwen_settings_document(profile: PiQwenProfile) -> dict[str, Any]:
    return {
        "transport": "sse",
        "httpIdleTimeoutMs": 0,
        "defaultProjectTrust": "never",
        "compaction": {
            "enabled": True,
            "reserveTokens": profile.max_output_tokens,
            "keepRecentTokens": 20_000,
        },
        "retry": {
            "enabled": False,
            "maxRetries": 0,
            "provider": {
                "maxRetries": 0,
                "maxRetryDelayMs": 60_000,
            },
        },
        "quietStartup": True,
        "enableInstallTelemetry": False,
        "enableAnalytics": False,
    }


def build_qwen_wire_audit_source(profile: PiQwenProfile) -> str:
    return f"""import {{ appendFileSync }} from "node:fs";

let requestIndex = 0;

function requireWire(condition, message) {{
  if (!condition) throw new Error(`Qwen benchmark wire mismatch: ${{message}}`);
}}

export default function registerQwenWireAudit(pi) {{
  pi.on("before_provider_request", (event, ctx) => {{
    requestIndex += 1;
    const payload = event.payload;
    requireWire(ctx.model?.provider === {json.dumps(QWEN_PI_PROVIDER)}, "provider");
    requireWire(ctx.model?.id === {json.dumps(profile.model)}, "model");
    requireWire(payload.model === {json.dumps(profile.model)}, "payload model");
    requireWire(payload.reasoning?.effort === {json.dumps(profile.thinking)}, "thinking");
    requireWire(payload.store === false, "store");
    requireWire(payload.stream === true, "stream");
    requireWire(payload.parallel_tool_calls === {json.dumps(profile.parallel_tool_calls)}, "parallel_tool_calls");
    requireWire(Number.isInteger(payload.max_output_tokens) && payload.max_output_tokens > 0, "max_output_tokens type");
    requireWire(payload.max_output_tokens <= {profile.max_output_tokens}, "max_output_tokens limit");
    if (requestIndex === 1) {{
      requireWire(payload.max_output_tokens === {profile.max_output_tokens}, "first max_output_tokens");
    }}
    appendFileSync(
      {json.dumps(REMOTE_WIRE)},
      JSON.stringify({{
        timestamp: new Date().toISOString(),
        request_index: requestIndex,
        provider: ctx.model.provider,
        model: ctx.model.id,
        reasoning: payload.reasoning?.effort ?? null,
        store: payload.store ?? null,
        stream: payload.stream ?? null,
        parallel_tool_calls: payload.parallel_tool_calls ?? null,
        max_output_tokens: payload.max_output_tokens ?? null,
        prompt_cache_key_present: typeof payload.prompt_cache_key === "string" && payload.prompt_cache_key.length > 0,
        input_items: Array.isArray(payload.input) ? payload.input.length : null,
        tools: Array.isArray(payload.tools) ? payload.tools.length : 0,
      }}) + "\\n",
      "utf8",
    );
  }});
}}
"""


class PiQwenDeepSweAgent(BaseAgent):
    """Pier adapter for Pi using the benchmark's custom Qwen Responses profile."""

    SUPPORTS_ATIF = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = None,
        pi_bundle: str | None = None,
        profile_file: str | None = None,
        thinking: str = QWEN_THINKING,
        pi_version: str = PI_VERSION,
        **kwargs: Any,
    ) -> None:
        super().__init__(logs_dir=logs_dir, model_name=model_name, **kwargs)
        if not model_name:
            raise ValueError(f"model_name must be {QWEN_MODEL}")
        if not pi_bundle:
            raise ValueError("pi_bundle is required")
        if not profile_file:
            raise ValueError("profile_file is required")

        self._bundle = Path(pi_bundle).expanduser().resolve()
        if not self._bundle.is_file():
            raise FileNotFoundError(f"Pi bundle not found: {self._bundle}")
        self._profile = load_pi_qwen_profile(
            Path(profile_file), model_name=model_name, thinking=thinking
        )
        self._pi_version = pi_version
        self._bundle_sha256 = _file_sha256(self._bundle)

    @staticmethod
    def name() -> str:
        return "pi-qwen"

    def version(self) -> str:
        return f"{self._pi_version}+sha256:{self._bundle_sha256}"

    def network_allowlist(self) -> NetworkAllowlist:
        return NetworkAllowlist(domains=[self._profile.network_domain])

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
        self._require_success(created, "create Pi Qwen benchmark directories")
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

        with tempfile.TemporaryDirectory(prefix="pi-qwen-deepswe-config-") as directory:
            root = Path(directory)
            auth_path = root / "auth.json"
            models_path = root / "models.json"
            settings_path = root / "settings.json"
            prompt_path = root / "append-system-prompt.md"
            extension_path = root / "qwen-wire-audit.mjs"
            _write_json_private(auth_path, build_pi_qwen_auth_document(self._profile))
            _write_json_private(
                models_path, build_pi_qwen_models_document(self._profile)
            )
            _write_json_private(
                settings_path, build_pi_qwen_settings_document(self._profile)
            )
            prompt_path.write_text(PI_APPEND_SYSTEM_PROMPT + "\n", encoding="utf-8")
            extension_path.write_text(
                build_qwen_wire_audit_source(self._profile), encoding="utf-8"
            )
            await environment.upload_file(auth_path, f"{REMOTE_CONFIG}/auth.json")
            await environment.upload_file(models_path, f"{REMOTE_CONFIG}/models.json")
            await environment.upload_file(
                settings_path, f"{REMOTE_CONFIG}/settings.json"
            )
            await environment.upload_file(prompt_path, REMOTE_APPEND_PROMPT)
            await environment.upload_file(extension_path, REMOTE_WIRE_AUDIT)

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
                    f"chmod 644 {shlex.quote(REMOTE_APPEND_PROMPT)} {shlex.quote(REMOTE_WIRE_AUDIT)}",
                ]
            ),
            user="root",
        )
        self._require_success(permissions, "set Pi Qwen benchmark ownership")

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
            f"--provider {shlex.quote(QWEN_PI_PROVIDER)} --no-refresh",
            cwd="/tmp",
            env=environment_variables,
        )
        self._require_success(auth_check, "validate Pi Qwen auth")
        if (auth_check.stdout or "").strip() != "ready":
            raise RuntimeError("Pi Qwen auth is not ready")

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
        self._require_success(setup_sizes, "measure Pi Qwen setup disk")
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "pi-setup-sizes.txt").write_text(
            setup_sizes.stdout or "", encoding="utf-8"
        )
        (self.logs_dir / "pi-manifest.json").write_text(
            json.dumps(self._public_manifest_document(), indent=2, sort_keys=True)
            + "\n",
            encoding="utf-8",
        )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="pi-qwen-deepswe-instruction-"
        ) as directory:
            instruction_path = Path(directory) / "instruction.txt"
            instruction_path.write_text(instruction, encoding="utf-8")
            await environment.upload_file(instruction_path, REMOTE_INSTRUCTION)

        command = " ".join(
            [
                "node",
                shlex.quote(REMOTE_PI_ENTRYPOINT),
                "--mode json",
                "--provider",
                shlex.quote(QWEN_PI_PROVIDER),
                "--model",
                shlex.quote(self._profile.model),
                "--thinking",
                shlex.quote(self._profile.thinking),
                "--session-dir",
                shlex.quote(REMOTE_SESSIONS),
                "--append-system-prompt",
                shlex.quote(REMOTE_APPEND_PROMPT),
                "--extension",
                shlex.quote(REMOTE_WIRE_AUDIT),
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
        if metrics["providers"] != [QWEN_PI_PROVIDER] or metrics["models"] != [
            self._profile.model
        ]:
            raise RuntimeError("Pi session did not use only the configured Qwen model")

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
                "profile_id": self._profile.profile_id,
                "profile_provider": self._profile.provider,
                "billing": self._profile.billing,
                "provider": QWEN_PI_PROVIDER,
                "model": self._profile.model,
                "thinking": self._profile.thinking,
                "context_window_tokens": self._profile.context_window_tokens,
                "max_output_tokens": self._profile.max_output_tokens,
                "transport": "sse",
                "streaming": True,
                "parallel_tool_calls": self._profile.parallel_tool_calls,
                "agent_retry_enabled": False,
                "provider_max_retries": 0,
            }
        )
        self._validate_wire_audit(metrics)
        (self.logs_dir / "pi-metrics.json").write_text(
            json.dumps(metrics, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return metrics

    def _validate_wire_audit(self, metrics: dict[str, Any]) -> None:
        path = self.logs_dir / "pi-wire.jsonl"
        if not path.is_file():
            raise RuntimeError("Pi Qwen wire audit is missing")
        records = [
            json.loads(line)
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        if len(records) != metrics["provider_requests"]:
            raise RuntimeError(
                "Pi Qwen wire request count does not match durable provider usage"
            )
        if (
            not records
            or records[0].get("max_output_tokens") != self._profile.max_output_tokens
        ):
            raise RuntimeError(
                "Pi Qwen first request did not use the profile output limit"
            )
        metrics["wire_requests"] = len(records)
        metrics["wire_prompt_cache_key_requests"] = sum(
            record.get("prompt_cache_key_present") is True for record in records
        )
        metrics["wire_input_items_max"] = max(
            (
                record["input_items"]
                for record in records
                if isinstance(record.get("input_items"), int)
            ),
            default=0,
        )

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

    def _public_manifest_document(self) -> dict[str, Any]:
        return {
            "agent": "pi-qwen",
            "pi_version": self._pi_version,
            "bundle_sha256": self._bundle_sha256,
            "dataset_commit": DATASET_COMMIT,
            "profile_id": self._profile.profile_id,
            "profile_provider": self._profile.provider,
            "billing": self._profile.billing,
            "base_url": self._profile.base_url,
            "configured_header_names": sorted(self._profile.headers),
            "provider": QWEN_PI_PROVIDER,
            "model": self._profile.model,
            "api": self._profile.api,
            "thinking": self._profile.thinking,
            "context_window_tokens": self._profile.context_window_tokens,
            "max_output_tokens": self._profile.max_output_tokens,
            "transport": "sse",
            "streaming": True,
            "parallel_tool_calls": self._profile.parallel_tool_calls,
            "agent_retry_enabled": False,
            "provider_max_retries": 0,
        }

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
