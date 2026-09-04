from __future__ import annotations

import asyncio
import hashlib
import io
import json
import os
import re
import shlex
import tempfile
import time
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import zstandard
from pier.agents.base import BaseAgent
from pier.environments.base import BaseEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.network import NetworkAllowlist

REMOTE_ROOT = "/tmp/zork-deepswe"
REMOTE_DATA = f"{REMOTE_ROOT}/data"
REMOTE_BINARY = f"{REMOTE_ROOT}/bin/zork-agent"
REMOTE_RESPONSES_PROBE = f"{REMOTE_ROOT}/bin/responses-wire-probe.py"
REMOTE_SESSION_REQUEST = f"{REMOTE_ROOT}/session-request.json"
REMOTE_MAILBOX_REQUEST = f"{REMOTE_ROOT}/mailbox-request.json"
REMOTE_STORE = f"{REMOTE_DATA}/sessions"
AGENT_URL = "http://127.0.0.1:3010"
AGENT_API_PREFIX = ""
RESPONSES_PROBE_URL = "http://127.0.0.1:3021"
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
DATASET_COMMIT = "435ee89ec2f2e2289f33b0da4f992f0b7b7266b9"
BENCHMARK_SYSTEM_PROMPT = """You are a coding agent working in the current repository. Solve the user's task by changing the repository and verifying the result.

Recommended workflow:
1. Analyze the codebase by finding and reading the relevant files.
2. Reproduce the issue or establish a failing check when practical.
3. Edit the source code to implement the required behavior.
4. Verify the fix by running the relevant checks again.
5. Test edge cases that follow from the task.

Use file.read to examine files instead of cat or sed. Use shell.run for file discovery such as ls, rg, and find. Use file.edit for precise changes and file.write only for new files or complete rewrites. Consult tool.help for a tool's current arguments when needed. While work remains, every response MUST include at least one tool call. A shell.run call runs in a fresh shell, so shell-local directory and environment changes do not carry into a later call; filesystem changes do.

When and only when the implementation is complete and verified, call end as the only tool call in that model response. An assistant response without end does not submit or finish the task. Never call end alongside any other tool. Do not merely describe a solution: make the changes. This benchmark has no interactive user, so resolve the task from the supplied instruction and repository evidence."""


def _boolean_argument(value: bool | str, name: str) -> bool:
    if isinstance(value, bool):
        return value
    if value == "true":
        return True
    if value == "false":
        return False
    raise ValueError(f"{name} must be true or false")


@dataclass(frozen=True)
class BenchmarkProfile:
    profile_id: str
    provider: str
    billing: str
    base_url: str
    model: str
    thinking: str
    streaming: bool
    parallel_tool_calls: bool
    network_domain: str
    source_path: Path = field(repr=False)
    document: dict[str, Any] = field(repr=False)


def load_benchmark_profile(
    profile_path: Path, *, model_name: str, thinking: str
) -> BenchmarkProfile:
    resolved_path = profile_path.expanduser().resolve()
    document = json.loads(resolved_path.read_text())
    if not isinstance(document, dict):
        raise ValueError(f"profile must be a JSON object: {resolved_path}")
    profile_id = resolved_path.stem
    if not profile_id or profile_id == "auto":
        raise ValueError(f"profile file has an invalid profile id: {resolved_path}")
    provider = document.get("provider")
    billing = document.get("billing")
    base_url = document.get("base_url")
    if not isinstance(provider, str) or not provider.strip():
        raise ValueError(f"profile provider is missing from {resolved_path}")
    if not isinstance(billing, str) or not billing.strip():
        raise ValueError(f"profile billing is missing from {resolved_path}")
    if not isinstance(base_url, str):
        raise ValueError(f"profile base_url is missing from {resolved_path}")
    parsed_url = urlparse(base_url)
    if parsed_url.scheme not in ("http", "https") or not parsed_url.hostname:
        raise ValueError(f"profile base_url is invalid in {resolved_path}")
    models = document.get("models")
    if not isinstance(models, list):
        raise ValueError(f"profile models are missing from {resolved_path}")
    selected_model = model_name
    selected = [
        model
        for model in models
        if isinstance(model, dict) and model.get("id") == selected_model
    ]
    if not selected and model_name.startswith(f"{provider}/"):
        selected_model = model_name.removeprefix(f"{provider}/")
        selected = [
            model
            for model in models
            if isinstance(model, dict) and model.get("id") == selected_model
        ]
    if len(selected) != 1:
        raise ValueError(f"profile does not declare model {model_name}")
    thinking_levels = selected[0].get("thinking")
    if not isinstance(thinking_levels, list) or thinking not in thinking_levels:
        raise ValueError(
            f"profile model {selected_model} does not support thinking {thinking}"
        )
    streaming = selected[0].get("streaming", True)
    if not isinstance(streaming, bool):
        raise ValueError(f"profile model {selected_model} streaming must be boolean")
    parallel_tool_calls = selected[0].get("parallel_tool_calls", False)
    if not isinstance(parallel_tool_calls, bool):
        raise ValueError(
            f"profile model {selected_model} parallel_tool_calls must be boolean"
        )
    return BenchmarkProfile(
        profile_id=profile_id,
        provider=provider,
        billing=billing,
        base_url=base_url,
        model=selected_model,
        thinking=thinking,
        streaming=streaming,
        parallel_tool_calls=parallel_tool_calls,
        network_domain=parsed_url.hostname,
        source_path=resolved_path,
        document=document,
    )


def parse_session_id(response_body: str) -> str:
    document = json.loads(response_body)
    session_id = document.get("session_id") if isinstance(document, dict) else None
    if not isinstance(session_id, str) or ULID.fullmatch(session_id) is None:
        raise ValueError("zork-agent returned an invalid session_id")
    return session_id


def aggregate_event_records(records: Iterable[dict[str, Any]]) -> dict[str, Any]:
    input_tokens = 0
    cached_input_tokens: int | None = 0
    output_tokens = 0
    output_reasoning_tokens: int | None = 0
    output_text_tokens: int | None = 0
    peak_context_tokens = 0
    agent_steps = 0
    provider_requests = 0
    provider_requests_with_usage = 0
    context_handoffs = 0
    context_compactions = 0
    compaction_input_tokens = 0
    compaction_output_tokens = 0
    step_purposes: dict[str, str] = {}
    total_tool_calls = 0
    multi_tool_call_rounds = 0
    max_tool_calls_per_round = 0
    activation_outcome: str | None = None
    final_assistant_content = ""
    final_assistant_tool_call_count = 0
    end_tool_call_ids: set[str] = set()
    completion_end_tool_succeeded = False
    previous_domain_record: dict[str, Any] | None = None

    last_end_result_ok = False

    for record in records:
        event = _record_event(record)
        if event is None:
            continue
        event_type = event.get("type") or event.get("kind")
        if event_type in ("activation_started", "turn_started"):
            activation_outcome = None
            end_tool_call_ids.clear()
            completion_end_tool_succeeded = False
            last_end_result_ok = False
        elif event_type in ("model_attempt_started", "step_started"):
            provider_requests += 1
            if isinstance(event.get("step_id"), str):
                step_purposes[event["step_id"]] = event.get("purpose", "conversation")
        elif event_type in ("model_request_completed", "step_completed", "step_failed"):
            agent_steps += int(event_type != "step_failed")
            purpose = event.get("purpose", step_purposes.get(event.get("step_id"), "conversation"))
            usage = event.get("usage")
            if event_type == "step_failed":
                error = event.get("error")
                usage = error.get("usage") if isinstance(error, dict) else None
            if (
                isinstance(usage, dict)
                and "input_tokens" in usage
                and "output_tokens" in usage
            ):
                provider_requests_with_usage += 1
                current_input = int(usage["input_tokens"])
                current_output = int(usage["output_tokens"])
                input_tokens += current_input
                output_tokens += current_output
                cached_input_tokens = _accumulate_optional_usage(
                    cached_input_tokens, usage.get("cached_input_tokens")
                )
                output_reasoning_tokens = _accumulate_optional_usage(
                    output_reasoning_tokens, usage.get("output_reasoning_tokens")
                )
                output_text_tokens = _accumulate_optional_usage(
                    output_text_tokens, usage.get("output_text_tokens")
                )
                if purpose == "compaction":
                    compaction_input_tokens += current_input
                    compaction_output_tokens += current_output
                else:
                    peak_context_tokens = max(peak_context_tokens, current_input)
            if event_type == "step_completed" and purpose == "conversation":
                text = event.get("assistant_text")
                final_assistant_content = text if isinstance(text, str) else ""
                counted = _count_assistant_tools(
                    event.get("tool_calls", event.get("invocations")),
                    end_tool_call_ids,
                )
                final_assistant_tool_call_count = counted
                total_tool_calls += counted
                multi_tool_call_rounds += int(counted > 1)
                max_tool_calls_per_round = max(max_tool_calls_per_round, counted)
                last_end_result_ok = False
        elif event_type in ("context_handoff_created", "handoff_applied"):
            context_handoffs += 1
        elif event_type == "context_applied":
            if event.get("purpose") == "compaction":
                context_compactions += 1
            else:
                context_handoffs += 1
        elif event_type == "message_appended":
            message = event.get("message")
            if not isinstance(message, dict) or message.get("role") != "assistant":
                previous_domain_record = record
                continue
            content = message.get("content")
            final_assistant_content = content if isinstance(content, str) else ""
            counted = _count_assistant_tools(
                message.get("tool_calls"),
                end_tool_call_ids,
            )
            final_assistant_tool_call_count = counted
            total_tool_calls += counted
            multi_tool_call_rounds += int(counted > 1)
            max_tool_calls_per_round = max(max_tool_calls_per_round, counted)
        elif event_type == "tool_result":
            result = event.get("result")
            result = result if isinstance(result, dict) else event
            if (
                result.get("tool_name", result.get("tool")) == "end"
                and result.get("outcome") == "succeeded"
                and result.get("tool_call_id", result.get("invocation_id"))
                in end_tool_call_ids
            ):
                last_end_result_ok = True
        elif event_type == "activation_finished":
            outcome = event.get("outcome")
            activation_outcome = outcome if isinstance(outcome, str) else None
            completion_end_tool_succeeded = (
                activation_outcome == "finished"
                and _is_atomic_successful_end(
                    previous_domain_record,
                    record,
                    end_tool_call_ids,
                )
            )
        elif event_type == "turn_finished":
            outcome = event.get("outcome")
            activation_outcome = outcome if isinstance(outcome, str) else None
            completion_end_tool_succeeded = (
                activation_outcome == "finished" and last_end_result_ok
            )
        previous_domain_record = record

    completion_submitted = (
        activation_outcome == "finished" and completion_end_tool_succeeded
    )

    return {
        "input_tokens": input_tokens,
        "cached_input_tokens": cached_input_tokens,
        "uncached_input_tokens": (
            input_tokens - cached_input_tokens
            if cached_input_tokens is not None
            else None
        ),
        "output_tokens": output_tokens,
        "output_reasoning_tokens": output_reasoning_tokens,
        "output_text_tokens": output_text_tokens,
        "total_tokens": input_tokens + output_tokens,
        "peak_context_tokens": peak_context_tokens,
        "agent_steps": agent_steps,
        "provider_requests": provider_requests,
        "provider_requests_missing_usage": (
            provider_requests - provider_requests_with_usage
        ),
        "context_handoffs": context_handoffs,
        "context_compactions": context_compactions,
        "compaction_input_tokens": compaction_input_tokens,
        "compaction_output_tokens": compaction_output_tokens,
        "tool_calls": total_tool_calls,
        "multi_tool_call_rounds": multi_tool_call_rounds,
        "max_tool_calls_per_round": max_tool_calls_per_round,
        "activation_outcome": activation_outcome,
        "final_assistant_content_bytes": len(final_assistant_content.encode("utf-8")),
        "final_assistant_tool_call_count": final_assistant_tool_call_count,
        "completion_end_tool_succeeded": completion_end_tool_succeeded,
        "completion_submitted": completion_submitted,
    }


def _session_event_kinds(session_dir: Path) -> set[str]:
    kinds: set[str] = set()
    for record in iter_session_event_records(session_dir):
        event = _record_event(record)
        if event is not None and isinstance(event.get("kind"), str):
            kinds.add(event["kind"])
    return kinds


def _record_event(record: dict[str, Any]) -> dict[str, Any] | None:
    if record.get("record") == "snapshot":
        return None
    event = record.get("event")
    if record.get("record") == "event" and isinstance(event, dict):
        return event
    if record.get("kind") == "domain" and isinstance(event, dict):
        return event
    if isinstance(record.get("event_id"), str) and isinstance(event, dict):
        return event
    return None


def _count_assistant_tools(
    tool_calls: Any,
    end_tool_call_ids: set[str],
) -> int:
    if not isinstance(tool_calls, list):
        return 0
    if len(tool_calls) == 1:
        call = tool_calls[0]
        if isinstance(call, dict) and call.get("tool_name", call.get("tool")) == "end":
            # 唯一的 end 调用即提交，参数可含 acknowledge_outstanding。
            call_id = call.get("tool_call_id", call.get("invocation_id"))
            if isinstance(call_id, str):
                end_tool_call_ids.add(call_id)
    return len(tool_calls)


def _accumulate_optional_usage(total: int | None, value: Any) -> int | None:
    if (
        total is None
        or not isinstance(value, int)
        or isinstance(value, bool)
        or value < 0
    ):
        return None
    return total + value


def _is_atomic_successful_end(
    result_record: dict[str, Any] | None,
    finish_record: dict[str, Any],
    end_tool_call_ids: set[str],
) -> bool:
    batch_size = finish_record.get("batch_size")
    finish_index = finish_record.get("batch_index")
    result_index = result_record.get("batch_index") if result_record else None
    if (
        result_record is None
        or not isinstance(batch_size, int)
        or not isinstance(finish_index, int)
        or not isinstance(result_index, int)
        or result_record.get("batch_size") != batch_size
        or result_index + 1 != finish_index
        or finish_index + 1 != batch_size
    ):
        return False
    event = result_record.get("event")
    result = event.get("result") if isinstance(event, dict) else None
    return (
        isinstance(event, dict)
        and event.get("type") == "tool_execution_result"
        and isinstance(result, dict)
        and result.get("tool_name") == "end"
        and result.get("outcome") == "succeeded"
        and result.get("tool_call_id") in end_tool_call_ids
    )


def iter_session_event_records(session_dir: Path) -> Iterator[dict[str, Any]]:
    segments_dir = session_dir / "segments"
    jsonl_files: list[Path] = []
    if segments_dir.is_dir():
        jsonl_files.extend(sorted(segments_dir.iterdir(), key=lambda path: path.name))
    else:
        jsonl_files.extend(sorted(session_dir.glob("*.jsonl*")))
    for segment in jsonl_files:
        if segment.name.endswith(".jsonl"):
            with segment.open("r", encoding="utf-8") as stream:
                yield from _iter_json_lines(stream)
        elif segment.name.endswith(".jsonl.zst"):
            with (
                segment.open("rb") as compressed,
                zstandard.ZstdDecompressor().stream_reader(compressed) as decoded,
                io.TextIOWrapper(decoded, encoding="utf-8") as stream,
            ):
                yield from _iter_json_lines(stream)


def _iter_json_lines(stream: Iterable[str]) -> Iterator[dict[str, Any]]:
    for line in stream:
        if line.strip():
            record = json.loads(line)
            if isinstance(record, dict):
                yield record


class ZorkDeepSweAgent(BaseAgent):
    """Pier adapter that runs one zork-agent session with a fixed coding prompt."""

    SUPPORTS_ATIF = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = None,
        zork_binary: str | None = None,
        profile_file: str | None = None,
        thinking: str | None = None,
        auth_domains: str = "",
        no_streaming: bool | str = False,
        responses_probe_script: str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(logs_dir=logs_dir, model_name=model_name, **kwargs)
        if not profile_file:
            raise ValueError("profile_file is required")
        if not model_name:
            raise ValueError("model_name is required")
        if not thinking:
            raise ValueError("thinking is required")
        self._profile = load_benchmark_profile(
            Path(profile_file), model_name=model_name, thinking=thinking
        )
        self._no_streaming = _boolean_argument(no_streaming, "no_streaming")
        self._network_domains = [self._profile.network_domain]
        for domain in auth_domains.split(","):
            domain = domain.strip()
            if domain and domain not in self._network_domains:
                self._network_domains.append(domain)
        if not zork_binary:
            raise ValueError("zork_binary is required")
        self._zork_binary = Path(zork_binary).expanduser().resolve()
        if not self._zork_binary.is_file():
            raise FileNotFoundError(f"zork-agent binary not found: {self._zork_binary}")
        self._binary_sha256 = _file_sha256(self._zork_binary)
        self._responses_probe_script = (
            Path(responses_probe_script).expanduser().resolve()
            if responses_probe_script
            else None
        )
        if (
            self._responses_probe_script is not None
            and not self._responses_probe_script.is_file()
        ):
            raise FileNotFoundError(
                f"Responses wire probe not found: {self._responses_probe_script}"
            )
        self._pid: int | None = None
        self._probe_pid: int | None = None

    @staticmethod
    def name() -> str:
        return "zork-agent"

    def version(self) -> str:
        return f"sha256:{self._binary_sha256}"

    def network_allowlist(self) -> NetworkAllowlist:
        return NetworkAllowlist(domains=self._network_domains)

    async def setup(self, environment: BaseEnvironment) -> None:
        created = await environment.exec(
            f"mkdir -p {shlex.quote(REMOTE_ROOT + '/bin')} {shlex.quote(REMOTE_DATA + '/profiles')}",
            user="root",
        )
        self._require_success(created, "create zork benchmark directories")
        await environment.upload_file(self._zork_binary, REMOTE_BINARY)
        if self._responses_probe_script is not None:
            await environment.upload_file(
                self._responses_probe_script, REMOTE_RESPONSES_PROBE
            )
        uploaded_hash = await environment.exec(
            f"sha256sum {shlex.quote(REMOTE_BINARY)}",
            user="root",
        )
        self._require_success(uploaded_hash, "hash uploaded zork-agent binary")
        remote_sha256 = (uploaded_hash.stdout or "").strip().split(maxsplit=1)[0]
        if remote_sha256 != self._binary_sha256:
            raise RuntimeError(
                "uploaded zork-agent binary hash does not match the source artifact"
            )

        profile_document = dict(self._profile.document)
        if self._responses_probe_script is not None:
            profile_document["base_url"] = RESPONSES_PROBE_URL
        with tempfile.TemporaryDirectory(prefix="zork-deepswe-profile-") as directory:
            profile_path = Path(directory) / f"{self._profile.profile_id}.json"
            profile_path.write_text(
                json.dumps(profile_document, separators=(",", ":")),
                encoding="utf-8",
            )
            os.chmod(profile_path, 0o600)
            await environment.upload_file(profile_path, self._remote_profile)

        owner = await environment.exec("id -u", user=environment.default_user)
        self._require_success(owner, "resolve agent user")
        uid = (owner.stdout or "").strip()
        if not uid.isdigit():
            raise RuntimeError("could not resolve numeric agent uid")
        permissions = await environment.exec(
            " && ".join(
                [
                    f"chmod 755 {shlex.quote(REMOTE_BINARY)}",
                    *(
                        [f"chmod 644 {shlex.quote(REMOTE_RESPONSES_PROBE)}"]
                        if self._responses_probe_script is not None
                        else []
                    ),
                    f"chown -R {uid} {shlex.quote(REMOTE_ROOT)}",
                    f"chmod 600 {shlex.quote(self._remote_profile)}",
                ]
            ),
            user="root",
        )
        self._require_success(permissions, "set zork benchmark file ownership")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        session_id: str | None = None
        metrics: dict[str, Any] | None = None
        run_error: BaseException | None = None
        run_traceback = None
        try:
            await self._upload_request_documents(environment, instruction)
            await self._start_agent(environment)
            await self._wait_until_ready(environment)

            create_body, create_status = await self._request(
                environment,
                "POST",
                f"{AGENT_API_PREFIX}/sessions",
                REMOTE_SESSION_REQUEST,
            )
            if create_status != 201:
                raise RuntimeError(
                    f"create session returned HTTP {create_status}: {create_body}"
                )
            session_id = parse_session_id(create_body)

            mailbox_body, mailbox_status = await self._request(
                environment,
                "POST",
                f"{AGENT_API_PREFIX}/sessions/{session_id}/mailbox",
                REMOTE_MAILBOX_REQUEST,
            )
            if mailbox_status != 202:
                raise RuntimeError(
                    f"append benchmark instruction returned HTTP {mailbox_status}: {mailbox_body}"
                )
            await self._wait_for_session_wait(environment, session_id)
        except BaseException as error:
            run_error = error
            run_traceback = error.__traceback__
        finally:
            cleanup_errors: list[BaseException] = []
            try:
                await self._stop_agent(environment)
            except BaseException as error:
                cleanup_errors.append(error)
            if session_id is not None:
                try:
                    metrics = await self._collect_session_artifacts(
                        environment, session_id
                    )
                    self._populate_context(context, metrics)
                except BaseException as error:
                    cleanup_errors.append(error)
            if cleanup_errors:
                if run_error is None:
                    raise cleanup_errors[0]
                for cleanup_error in cleanup_errors:
                    run_error.add_note(
                        f"benchmark cleanup/diagnostics also failed: {cleanup_error!r}"
                    )

        if run_error is not None:
            raise run_error.with_traceback(run_traceback)
        if session_id is None or metrics is None:
            raise RuntimeError("zork-agent did not create a diagnosable session")
        if metrics["activation_outcome"] != "finished":
            raise RuntimeError(
                f"zork-agent activation ended as {metrics['activation_outcome'] or 'unknown'}"
            )
        if not metrics["completion_submitted"]:
            raise RuntimeError(
                "zork-agent activation did not finish through an exclusive successful end tool call"
            )

    async def _collect_session_artifacts(
        self, environment: BaseEnvironment, session_id: str
    ) -> dict[str, Any]:
        session_logs = self.logs_dir / "zork-session"
        copied = await environment.exec(
            " && ".join(
                [
                    "mkdir -p /tmp/zork-session-copy",
                    "rm -rf /tmp/zork-session-copy/*",
                    (
                        f"if [ -f {shlex.quote(f'{REMOTE_STORE}/{session_id}.jsonl')} ]; then "
                        f"cp {shlex.quote(f'{REMOTE_STORE}/{session_id}.jsonl')} /tmp/zork-session-copy/; "
                        f"elif [ -d {shlex.quote(f'{REMOTE_DATA}/sessions/{session_id}')} ]; then "
                        f"cp -R {shlex.quote(f'{REMOTE_DATA}/sessions/{session_id}/.')} /tmp/zork-session-copy/; "
                        "fi"
                    ),
                ]
            ),
            cwd="/tmp",
        )
        self._require_success(copied, "copy session stream")
        await environment.download_dir(
            source_dir="/tmp/zork-session-copy",
            target_dir=session_logs,
        )
        kinds = _session_event_kinds(session_logs)
        if not kinds:
            raise RuntimeError(
                "zork-agent produced no session stream; "
                "the running binary does not expose the session API"
            )
        if "session_created" not in kinds:
            raise RuntimeError(
                "session stream does not look like zork-agent "
                f"(kinds: {sorted(kinds)[:6]}); expected the session API"
            )
        metrics = aggregate_event_records(iter_session_event_records(session_logs))
        metrics.update(
            {
                "agent_api": "zork-agent",
                "binary_sha256": self._binary_sha256,
                "dataset_commit": DATASET_COMMIT,
                "session_id": session_id,
                "profile_id": self._profile.profile_id,
                "provider": self._profile.provider,
                "model": self._profile.model,
                "thinking": self._profile.thinking,
                "profile_streaming": self._profile.streaming,
                "parallel_tool_calls": self._profile.parallel_tool_calls,
                "no_streaming": self._no_streaming,
                "streaming": self._profile.streaming and not self._no_streaming,
            }
        )
        (self.logs_dir / "zork-metrics.json").write_text(
            json.dumps(metrics, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return metrics

    @staticmethod
    def _populate_context(context: AgentContext, metrics: dict[str, Any]) -> None:
        context.n_input_tokens = metrics["input_tokens"]
        context.n_cache_tokens = metrics["cached_input_tokens"]
        context.n_output_tokens = metrics["output_tokens"]
        context.peak_context_tokens = metrics["peak_context_tokens"]
        context.n_agent_steps = metrics["agent_steps"]
        context.summarization_count = metrics["context_handoffs"] + metrics.get("context_compactions", 0)
        context.metadata = metrics

    async def _upload_request_documents(
        self, environment: BaseEnvironment, instruction: str
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="zork-deepswe-request-") as directory:
            root = Path(directory)
            session_request = root / "session-request.json"
            mailbox_request = root / "mailbox-request.json"
            session_request.write_text(
                json.dumps(
                    {
                        "profile_id": self._profile.profile_id,
                        "model": self._profile.model,
                        "thinking": self._profile.thinking,
                        "system_prompt": BENCHMARK_SYSTEM_PROMPT,
                        "workspace": "/app",
                    },
                    separators=(",", ":"),
                ),
                encoding="utf-8",
            )
            mailbox_request.write_text(
                json.dumps({"content": instruction}, separators=(",", ":")),
                encoding="utf-8",
            )
            await environment.upload_file(session_request, REMOTE_SESSION_REQUEST)
            await environment.upload_file(mailbox_request, REMOTE_MAILBOX_REQUEST)

    async def _start_agent(self, environment: BaseEnvironment) -> None:
        if getattr(self, "_responses_probe_script", None) is not None:
            await self._start_responses_probe(environment)
        no_streaming = (
            " --no-streaming" if getattr(self, "_no_streaming", False) else ""
        )
        command = (
            f"nohup {shlex.quote(REMOTE_BINARY)} --data {shlex.quote(REMOTE_DATA)}"
            f"{no_streaming} "
            "> /logs/agent/zork-agent.log 2>&1 < /dev/null & echo $!"
        )
        started = await environment.exec(
            command,
            cwd="/tmp",
            env=environment.agent_process_env(None),
        )
        self._require_success(started, "start zork-agent")
        raw_pid = (started.stdout or "").strip().splitlines()[-1]
        if not raw_pid.isdigit():
            raise RuntimeError("zork-agent did not return a process id")
        self._pid = int(raw_pid)

    async def _start_responses_probe(self, environment: BaseEnvironment) -> None:
        command = (
            f"nohup python3 {shlex.quote(REMOTE_RESPONSES_PROBE)} "
            f"--listen 127.0.0.1:3021 --upstream-base {shlex.quote(self._profile.base_url)} "
            "--log /logs/agent/responses-wire.jsonl "
            "> /logs/agent/responses-wire-probe.log 2>&1 < /dev/null & echo $!"
        )
        started = await environment.exec(
            command,
            cwd="/tmp",
            env=environment.agent_process_env(None),
        )
        self._require_success(started, "start Responses wire probe")
        raw_pid = (started.stdout or "").strip().splitlines()[-1]
        if not raw_pid.isdigit():
            raise RuntimeError("Responses wire probe did not return a process id")
        self._probe_pid = int(raw_pid)
        await self._wait_until_probe_ready(environment)

    async def _stop_agent(self, environment: BaseEnvironment) -> None:
        pid = getattr(self, "_pid", None)
        self._pid = None
        if pid is not None:
            await self._terminate_pid(environment, pid)
        probe_pid = getattr(self, "_probe_pid", None)
        self._probe_pid = None
        if probe_pid is not None:
            await self._terminate_pid(environment, probe_pid)

    async def _terminate_pid(self, environment: BaseEnvironment, pid: int) -> None:
        await environment.exec(
            f"kill -TERM {pid} 2>/dev/null || true; "
            f"for n in 1 2 3 4 5 6 7 8 9 10; do kill -0 {pid} 2>/dev/null || exit 0; sleep 0.2; done; "
            f"kill -KILL {pid} 2>/dev/null || true",
            cwd="/tmp",
        )

    async def _wait_until_probe_ready(self, environment: BaseEnvironment) -> None:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            ready = await environment.exec(
                "curl --silent --show-error --fail "
                f"{shlex.quote(RESPONSES_PROBE_URL + '/healthz')} >/dev/null",
                cwd="/tmp",
            )
            if ready.return_code == 0:
                return
            await asyncio.sleep(0.25)
        raise RuntimeError(
            "Responses wire probe did not become ready within 30 seconds"
        )

    async def _wait_until_ready(self, environment: BaseEnvironment) -> None:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            ready = await environment.exec(
                f"curl --silent --show-error --fail {shlex.quote(AGENT_URL + '/readyz')} >/dev/null",
                cwd="/tmp",
            )
            if ready.return_code == 0:
                return
            await asyncio.sleep(0.25)
        raise RuntimeError("zork-agent did not become ready within 30 seconds")

    async def _wait_for_session_wait(
        self, environment: BaseEnvironment, session_id: str
    ) -> None:
        while True:
            body, status = await self._request(
                environment,
                "GET",
                f"{AGENT_API_PREFIX}/sessions/{session_id}",
                None,
            )
            if status != 200:
                raise RuntimeError(f"get session returned HTTP {status}: {body}")
            document = json.loads(body)
            session_status = (
                document.get("status") if isinstance(document, dict) else None
            )
            if session_status in ("wait", "finished"):
                return
            if session_status not in ("thinking", "waiting", "working", "recovering"):
                raise RuntimeError(
                    f"zork-agent returned invalid session status: {session_status!r}"
                )
            await asyncio.sleep(1)

    async def _request(
        self,
        environment: BaseEnvironment,
        method: str,
        endpoint: str,
        request_path: str | None,
    ) -> tuple[str, int]:
        request_body = (
            [
                "--header 'content-type: application/json'",
                f"--data-binary @{shlex.quote(request_path)}",
            ]
            if request_path is not None
            else []
        )
        result = await environment.exec(
            " ".join(
                [
                    "curl --silent --show-error",
                    f"--request {shlex.quote(method)}",
                    *request_body,
                    "--write-out '\\n%{http_code}'",
                    shlex.quote(f"{AGENT_URL}{endpoint}"),
                ]
            ),
            cwd="/tmp",
        )
        self._require_success(result, f"{method} {endpoint}")
        output = result.stdout or ""
        if "\n" not in output:
            raise RuntimeError(f"{method} {endpoint} returned no HTTP status")
        body, raw_status = output.rsplit("\n", 1)
        try:
            status = int(raw_status.strip())
        except ValueError as error:
            raise RuntimeError(
                f"{method} {endpoint} returned an invalid HTTP status"
            ) from error
        return body, status

    @property
    def _remote_profile(self) -> str:
        return f"{REMOTE_DATA}/profiles/{self._profile.profile_id}.json"

    @staticmethod
    def _require_success(result: Any, action: str) -> None:
        if result.return_code != 0:
            output = result.stderr or result.stdout or "no output"
            raise RuntimeError(f"failed to {action}: {output}")


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()
