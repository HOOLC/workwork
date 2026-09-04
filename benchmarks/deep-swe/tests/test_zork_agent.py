import asyncio
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from zork_deepswe.agents.zork import (
    BENCHMARK_SYSTEM_PROMPT,
    ZorkDeepSweAgent,
    _session_event_kinds,
    aggregate_event_records,
    load_benchmark_profile,
    parse_session_id,
)


class _ExecResult:
    return_code = 0
    stdout = "4242\n"
    stderr = ""


class _AgentEnvironment:
    def __init__(self) -> None:
        self.exec_calls: list[dict[str, object]] = []
        self.agent_process_env_calls: list[dict[str, str] | None] = []

    def agent_process_env(self, env: dict[str, str] | None) -> dict[str, str] | None:
        self.agent_process_env_calls.append(env)
        return {"HTTPS_PROXY": "http://pier-egress-proxy:8080"}

    async def exec(self, command: str, **kwargs: object) -> _ExecResult:
        self.exec_calls.append({"command": command, **kwargs})
        return _ExecResult()


class _UploadEnvironment:
    def __init__(self) -> None:
        self.documents: dict[str, dict[str, object]] = {}

    async def upload_file(self, source: Path, target: str) -> None:
        self.documents[target] = json.loads(source.read_text())


class ZorkDeepSweAgentTest(unittest.TestCase):
    def test_loads_selection_and_network_boundary_from_an_explicit_profile(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile_path = Path(directory) / "openai-subscription.json"
            profile_path.write_text(
                json.dumps(
                    {
                        "provider": "openai",
                        "billing": "subscription",
                        "base_url": "https://chatgpt.com/backend-api/codex",
                        "headers": {"originator": "zork"},
                        "auth": {
                            "type": "oauth",
                            "access": "access-secret",
                            "refresh": "refresh-secret",
                            "accountId": "account-secret",
                            "expires": 1_900_000_000_000,
                        },
                        "models": [
                            {
                                "id": "gpt-5.6-luna",
                                "api": "openai-codex-responses",
                                "parallel_tool_calls": True,
                                "thinking": ["low", "max"],
                                "default_thinking": "max",
                                "capabilities": {"input": ["text", "image"]},
                                "limits": {
                                    "context_window_tokens": 272_000,
                                    "max_output_tokens": 128_000,
                                },
                                "default": True,
                            }
                        ],
                    }
                )
            )

            profile = load_benchmark_profile(
                profile_path, model_name="openai/gpt-5.6-luna", thinking="max"
            )

        self.assertEqual(profile.profile_id, "openai-subscription")
        self.assertEqual(profile.provider, "openai")
        self.assertEqual(profile.model, "gpt-5.6-luna")
        self.assertEqual(profile.thinking, "max")
        self.assertIs(profile.streaming, True)
        self.assertIs(profile.parallel_tool_calls, True)
        self.assertEqual(profile.network_domain, "chatgpt.com")
        self.assertNotIn("access-secret", repr(profile))
        self.assertNotIn("refresh-secret", repr(profile))

    def test_agent_allowlist_includes_explicit_oauth_refresh_domain(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile_path = root / "openai-subscription.json"
            profile_path.write_text(
                json.dumps(
                    {
                        "provider": "openai",
                        "billing": "subscription",
                        "base_url": "https://chatgpt.com/backend-api/codex",
                        "auth": {},
                        "models": [
                            {
                                "id": "gpt-5.6-luna",
                                "streaming": True,
                                "thinking": ["max"],
                            }
                        ],
                    }
                )
            )
            binary = root / "zork-agent"
            binary.write_bytes(b"")
            binary.chmod(0o755)
            logs = root / "logs"
            logs.mkdir()

            agent = ZorkDeepSweAgent(
                logs_dir=logs,
                model_name="gpt-5.6-luna",
                zork_binary=str(binary),
                profile_file=str(profile_path),
                thinking="max",
                auth_domains="auth.openai.com",
            )

        self.assertEqual(
            agent.network_allowlist().model_dump()["domains"],
            ["auth.openai.com", "chatgpt.com"],
        )

    def test_rejects_a_non_ulid_session_id(self) -> None:
        self.assertEqual(
            parse_session_id('{"session_id":"01K3ABCDEF0123456789ABCDEF"}'),
            "01K3ABCDEF0123456789ABCDEF",
        )
        with self.assertRaises(ValueError):
            parse_session_id('{"session_id":"../../app"}')

    def test_aggregates_only_durable_provider_usage(self) -> None:
        records = [
            {"kind": "domain", "event": {"type": "model_attempt_started"}},
            {
                "kind": "domain",
                "event": {
                    "type": "model_request_completed",
                    "usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 80,
                        "output_tokens": 20,
                        "output_reasoning_tokens": 12,
                        "output_text_tokens": 8,
                    },
                },
            },
            {"kind": "snapshot", "event": {"type": "model_request_completed"}},
            {"kind": "domain", "event": {"type": "model_attempt_started"}},
            {"kind": "domain", "event": {"type": "model_request_completed"}},
            {
                "kind": "domain",
                "event": {
                    "type": "message_appended",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [],
                    },
                },
            },
            {
                "kind": "domain",
                "event": {"type": "activation_finished", "outcome": "finished"},
            },
        ]
        self.assertEqual(
            aggregate_event_records(records),
            {
                "input_tokens": 100,
                "cached_input_tokens": 80,
                "uncached_input_tokens": 20,
                "output_tokens": 20,
                "output_reasoning_tokens": 12,
                "output_text_tokens": 8,
                "total_tokens": 120,
                "peak_context_tokens": 100,
                "agent_steps": 2,
                "provider_requests": 2,
                "provider_requests_missing_usage": 1,
                "context_handoffs": 0,
                "context_compactions": 0,
                "compaction_input_tokens": 0,
                "compaction_output_tokens": 0,
                "tool_calls": 0,
                "multi_tool_call_rounds": 0,
                "max_tool_calls_per_round": 0,
                "activation_outcome": "finished",
                "final_assistant_content_bytes": 0,
                "final_assistant_tool_call_count": 0,
                "completion_end_tool_succeeded": False,
                "completion_submitted": False,
            },
        )

    def test_compaction_usage_is_counted_without_becoming_the_final_answer(self) -> None:
        records = [
            {"event": {"kind": "step_started", "step_id": "normal"}},
            {"event": {"kind": "step_completed", "step_id": "normal", "assistant_text": "working", "invocations": [],
                       "usage": {"input_tokens": 100, "output_tokens": 10}}},
            {"event": {"kind": "step_started", "step_id": "bad-summary", "purpose": "compaction"}},
            {"event": {"kind": "step_failed", "step_id": "bad-summary",
                       "error": {"usage": {"input_tokens": 500, "output_tokens": 20}}}},
            {"event": {"kind": "step_started", "step_id": "summary", "purpose": "compaction"}},
            {"event": {"kind": "step_completed", "step_id": "summary", "purpose": "compaction",
                       "assistant_text": "internal summary", "invocations": [],
                       "usage": {"input_tokens": 400, "output_tokens": 30}}},
            {"event": {"kind": "context_applied", "purpose": "compaction", "document": "internal summary"}},
            {"event": {"kind": "context_applied", "purpose": "handoff", "document": "handoff"}},
        ]
        records = [{"event_id": str(index), **record} for index, record in enumerate(records)]
        metrics = aggregate_event_records(records)
        self.assertEqual(metrics["total_tokens"], 1060)
        self.assertEqual(metrics["compaction_input_tokens"], 900)
        self.assertEqual(metrics["compaction_output_tokens"], 50)
        self.assertEqual(metrics["peak_context_tokens"], 100)
        self.assertEqual(metrics["provider_requests_missing_usage"], 0)
        self.assertEqual(metrics["context_compactions"], 1)
        self.assertEqual(metrics["context_handoffs"], 1)
        self.assertEqual(metrics["final_assistant_content_bytes"], len("working"))

    def test_aggregates_stream_end_turn(self) -> None:
        records = [
            {
                "event_id": "01",
                "event": {"kind": "turn_started", "turn_id": "t1"},
            },
            {
                "event_id": "02",
                "event": {"kind": "step_started", "step_id": "s1"},
            },
            {
                "event_id": "03",
                "event": {
                    "kind": "step_completed",
                    "assistant_text": "",
                    "invocations": [
                        {
                            "invocation_id": "e1",
                            "tool": "end",
                            "arguments": {},
                        }
                    ],
                    "usage": {
                        "input_tokens": 50,
                        "cached_input_tokens": 10,
                        "output_tokens": 5,
                        "output_text_tokens": 5,
                    },
                },
            },
            {
                "event_id": "04",
                "event": {
                    "kind": "tool_result",
                    "result": {
                        "invocation_id": "e1",
                        "tool": "end",
                        "outcome": "succeeded",
                    },
                },
            },
            {
                "event_id": "05",
                "event": {"kind": "turn_finished", "outcome": "finished"},
            },
        ]
        metrics = aggregate_event_records(records)
        self.assertEqual(metrics["input_tokens"], 50)
        self.assertEqual(metrics["cached_input_tokens"], 10)
        self.assertEqual(metrics["agent_steps"], 1)
        self.assertEqual(metrics["provider_requests"], 1)
        self.assertEqual(metrics["activation_outcome"], "finished")
        self.assertTrue(metrics["completion_end_tool_succeeded"])
        self.assertTrue(metrics["completion_submitted"])

    def test_counts_a_failed_provider_attempt_without_usage_before_retry_success(
        self,
    ) -> None:
        records = [
            {"kind": "domain", "event": {"type": "model_attempt_started"}},
            {
                "kind": "domain",
                "event": {
                    "type": "model_attempt_failed_fact",
                    "error_class": "provider_failed",
                },
            },
            {"kind": "domain", "event": {"type": "model_attempt_started"}},
            {
                "kind": "domain",
                "event": {
                    "type": "model_request_completed",
                    "usage": {"input_tokens": 100, "output_tokens": 20},
                },
            },
        ]

        metrics = aggregate_event_records(records)

        self.assertEqual(metrics["provider_requests"], 2)
        self.assertEqual(metrics["agent_steps"], 1)
        self.assertEqual(metrics["provider_requests_missing_usage"], 1)
        self.assertNotIn("steps_missing_usage", metrics)

    def test_counts_native_multi_tool_call_model_responses(self) -> None:
        records = [
            {
                "kind": "domain",
                "event": {
                    "type": "message_appended",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [
                            {
                                "tool_call_id": "read-a",
                                "tool_name": "read",
                                "arguments": {"path": "a"},
                            },
                            {
                                "tool_call_id": "read-b",
                                "tool_name": "read",
                                "arguments": {"path": "b"},
                            },
                        ],
                    },
                },
            },
            {
                "kind": "domain",
                "event": {
                    "type": "message_appended",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [
                            {
                                "tool_call_id": "bash-c",
                                "tool_name": "bash",
                                "arguments": {"command": "true"},
                            }
                        ],
                    },
                },
            },
        ]

        metrics = aggregate_event_records(records)

        self.assertEqual(metrics["tool_calls"], 3)
        self.assertEqual(metrics["multi_tool_call_rounds"], 1)
        self.assertEqual(metrics["max_tool_calls_per_round"], 2)

    def test_accepts_only_an_atomic_successful_end_as_submission(self) -> None:
        records = [
            {
                "kind": "domain",
                "batch_index": 0,
                "batch_size": 5,
                "event": {
                    "type": "model_request_completed",
                    "usage": {"input_tokens": 100, "output_tokens": 20},
                },
            },
            {
                "kind": "domain",
                "batch_index": 1,
                "batch_size": 5,
                "event": {
                    "type": "message_appended",
                    "message": {
                        "role": "assistant",
                        "content": "Implemented the parser feature and verified the tests.",
                        "tool_calls": [
                            {
                                "tool_call_id": "end-call",
                                "tool_name": "end",
                                "arguments": {},
                            }
                        ],
                    },
                },
            },
            {
                "kind": "domain",
                "batch_index": 2,
                "batch_size": 5,
                "event": {
                    "type": "tool_execution_wait",
                    "wait": {
                        "wait_id": "end-wait",
                        "tool_call_ids": ["end-call"],
                    },
                },
            },
            {
                "kind": "domain",
                "batch_index": 3,
                "batch_size": 5,
                "event": {
                    "type": "tool_execution_result",
                    "result": {
                        "result_id": "end-result",
                        "tool_call_id": "end-call",
                        "tool_name": "end",
                        "outcome": "succeeded",
                        "content": "end accepted",
                    },
                },
            },
            {
                "kind": "domain",
                "batch_index": 4,
                "batch_size": 5,
                "event": {"type": "activation_finished", "outcome": "finished"},
            },
        ]

        metrics = aggregate_event_records(records)

        self.assertEqual(metrics["final_assistant_tool_call_count"], 1)
        self.assertTrue(metrics["completion_end_tool_succeeded"])
        self.assertTrue(metrics["completion_submitted"])

    def test_rejects_finished_without_the_atomic_end_batch(self) -> None:
        end_call = {
            "tool_call_id": "end-call",
            "tool_name": "end",
            "arguments": {},
        }

        def records_for(
            *,
            tool_calls: list[dict[str, object]],
            result_call_id: str = "end-call",
            result_outcome: str = "succeeded",
            result_batch: tuple[int, int] = (3, 5),
            finish_batch: tuple[int, int] = (4, 5),
        ) -> list[dict[str, object]]:
            return [
                {
                    "kind": "domain",
                    "event": {
                        "type": "message_appended",
                        "message": {
                            "role": "assistant",
                            "content": "done",
                            "tool_calls": tool_calls,
                        },
                    },
                },
                {
                    "kind": "domain",
                    "batch_index": result_batch[0],
                    "batch_size": result_batch[1],
                    "event": {
                        "type": "tool_execution_result",
                        "result": {
                            "result_id": "end-result",
                            "tool_call_id": result_call_id,
                            "tool_name": "end",
                            "outcome": result_outcome,
                            "content": "end accepted",
                        },
                    },
                },
                {
                    "kind": "domain",
                    "batch_index": finish_batch[0],
                    "batch_size": finish_batch[1],
                    "event": {
                        "type": "activation_finished",
                        "outcome": "finished",
                    },
                },
            ]

        cases = {
            "failed result": records_for(
                tool_calls=[end_call], result_outcome="failed"
            ),
            "wrong call id": records_for(
                tool_calls=[end_call], result_call_id="another-call"
            ),
            "different batch": records_for(
                tool_calls=[end_call], result_batch=(0, 1), finish_batch=(0, 1)
            ),
            "nonexclusive end": records_for(
                tool_calls=[
                    end_call,
                    {
                        "tool_call_id": "read-call",
                        "tool_name": "read",
                        "arguments": {"path": "README.md"},
                    },
                ]
            ),
        }
        for name, records in cases.items():
            with self.subTest(name=name):
                metrics = aggregate_event_records(records)
                self.assertFalse(metrics["completion_end_tool_succeeded"])
                self.assertFalse(metrics["completion_submitted"])

    def test_prompt_preserves_official_workflow_with_zork_completion_semantics(
        self,
    ) -> None:
        for requirement in (
            "Analyze the codebase",
            "Reproduce the issue",
            "Edit the source code",
            "Verify the fix",
            "Test edge cases",
            "every response MUST include at least one tool call",
            "call end as the only tool call",
            "assistant response without end does not submit",
            "Use file.read to examine files instead of cat or sed.",
            "Use shell.run for file discovery such as ls, rg, and find.",
            "Use file.edit for precise changes and file.write only for new files or complete rewrites.",
            "Consult tool.help for a tool's current arguments when needed.",
        ):
            self.assertIn(requirement, BENCHMARK_SYSTEM_PROMPT)
        self.assertNotIn(
            "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT", BENCHMARK_SYSTEM_PROMPT
        )
        for obsolete in ("oldText", "newText", "Use read ", "Use bash ", "Use edit ", "Use write "):
            self.assertNotIn(obsolete, BENCHMARK_SYSTEM_PROMPT)


class ZorkDeepSweAgentProcessEnvironmentTest(unittest.IsolatedAsyncioTestCase):
    async def test_starts_zork_with_piers_agent_process_environment(self) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._pid = None
        environment = _AgentEnvironment()

        await agent._start_agent(environment)

        self.assertEqual(environment.agent_process_env_calls, [None])
        self.assertEqual(len(environment.exec_calls), 1)
        self.assertEqual(
            environment.exec_calls[0]["env"],
            {"HTTPS_PROXY": "http://pier-egress-proxy:8080"},
        )
        self.assertEqual(agent._pid, 4242)

    async def test_process_wide_non_streaming_is_passed_as_a_zork_startup_flag(
        self,
    ) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._pid = None
        agent._no_streaming = True
        environment = _AgentEnvironment()

        await agent._start_agent(environment)

        self.assertIn(" --no-streaming ", environment.exec_calls[0]["command"])

    async def test_probe_starts_before_zork_and_uses_the_same_pier_egress_env(
        self,
    ) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._pid = None
        agent._probe_pid = None
        agent._responses_probe_script = Path("/host/responses_wire_probe.py")
        agent._profile = SimpleNamespace(
            base_url="https://chatgpt.com/backend-api/codex"
        )
        environment = _AgentEnvironment()

        await agent._start_agent(environment)

        self.assertEqual(environment.agent_process_env_calls, [None, None])
        process_calls = [call for call in environment.exec_calls if "env" in call]
        self.assertEqual(len(process_calls), 2)
        self.assertIn("responses-wire-probe.py", process_calls[0]["command"])
        self.assertIn("zork-agent", process_calls[1]["command"])
        self.assertEqual(agent._probe_pid, 4242)
        self.assertEqual(agent._pid, 4242)

    async def test_session_create_carries_workspace_selection_and_coding_prompt(
        self,
    ) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._profile = SimpleNamespace(
            profile_id="openai-subscription",
            model="gpt-5.6-luna",
            thinking="max",
        )
        environment = _UploadEnvironment()

        await agent._upload_request_documents(environment, "fix the task")

        session = environment.documents["/tmp/zork-deepswe/session-request.json"]
        self.assertEqual(session["profile_id"], "openai-subscription")
        self.assertEqual(session["model"], "gpt-5.6-luna")
        self.assertEqual(session["thinking"], "max")
        self.assertEqual(session["system_prompt"], BENCHMARK_SYSTEM_PROMPT)
        self.assertEqual(session["workspace"], "/app")

    async def test_wait_uses_public_session_status_without_own_task_deadline(
        self,
    ) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._request = AsyncMock(
            side_effect=[
                ('{"status":"thinking"}', 200),
                ('{"status":"waiting"}', 200),
                ('{"status":"working"}', 200),
                ('{"status":"recovering"}', 200),
                ('{"status":"finished"}', 200),
            ]
        )

        await agent._wait_for_session_wait(object(), "01K3ABCDEF0123456789ABCDEF")

        self.assertEqual(agent._request.await_count, 5)
        for call in agent._request.await_args_list:
            self.assertEqual(call.args[1], "GET")
            self.assertEqual(call.args[2], "/sessions/01K3ABCDEF0123456789ABCDEF")
            self.assertIsNone(call.args[3])

    async def test_wait_rejects_failed_session_status(self) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent._request = AsyncMock(return_value=('{"status":"failed"}', 200))

        with self.assertRaisesRegex(RuntimeError, "'failed'"):
            await agent._wait_for_session_wait(object(), "01K3ABCDEF0123456789ABCDEF")

    def test_segmented_session_identity_uses_all_event_segments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            segments = Path(directory) / "segments"
            segments.mkdir()
            (segments / "01.jsonl").write_text(
                '{"event_id":"01","event":{"kind":"session_created"}}\n',
                encoding="utf-8",
            )
            (segments / "02.jsonl").write_text(
                '{"event_id":"02","event":{"kind":"turn_started"}}\n',
                encoding="utf-8",
            )

            self.assertEqual(
                _session_event_kinds(Path(directory)),
                {"session_created", "turn_started"},
            )

    async def test_run_collects_partial_session_metrics_before_reraising(self) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent.logs_dir = Path(tempfile.mkdtemp(prefix="zork-adapter-test-"))
        agent._upload_request_documents = AsyncMock()
        agent._start_agent = AsyncMock()
        agent._wait_until_ready = AsyncMock()
        agent._request = AsyncMock(
            side_effect=[
                ('{"session_id":"01K3ABCDEF0123456789ABCDEF"}', 201),
                ("", 202),
            ]
        )
        failure = RuntimeError("provider stream failed")
        agent._wait_for_session_wait = AsyncMock(side_effect=failure)
        agent._stop_agent = AsyncMock()
        metrics = {
            "input_tokens": 123,
            "cached_input_tokens": 100,
            "uncached_input_tokens": 23,
            "output_tokens": 45,
            "output_reasoning_tokens": 30,
            "output_text_tokens": 15,
            "total_tokens": 168,
            "peak_context_tokens": 100,
            "agent_steps": 2,
            "provider_requests": 3,
            "provider_requests_missing_usage": 1,
            "context_handoffs": 0,
            "activation_outcome": None,
            "profile_id": "open-code-go",
            "provider": "opencode-go",
            "model": "muse-spark-1.2-contributor",
            "thinking": "xhigh",
            "session_id": "01K3ABCDEF0123456789ABCDEF",
            "binary_sha256": "abc",
        }
        agent._collect_session_artifacts = AsyncMock(return_value=metrics)
        environment = _AgentEnvironment()
        context = SimpleNamespace(
            n_input_tokens=None,
            n_output_tokens=None,
            peak_context_tokens=None,
            n_agent_steps=None,
            summarization_count=None,
            metadata=None,
        )

        with self.assertRaises(RuntimeError) as raised:
            await agent.run("fix it", environment, context)

        self.assertIs(raised.exception, failure)
        agent._stop_agent.assert_awaited_once()
        agent._collect_session_artifacts.assert_awaited_once()
        self.assertEqual(context.n_input_tokens, 123)
        self.assertEqual(context.n_output_tokens, 45)
        self.assertEqual(context.metadata, metrics)
        self.assertFalse(
            any("mv /app" in str(call["command"]) for call in environment.exec_calls)
        )

    async def test_run_rejects_finished_without_a_valid_end_commit(self) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent.logs_dir = Path(tempfile.mkdtemp(prefix="zork-adapter-empty-final-test-"))
        agent._upload_request_documents = AsyncMock()
        agent._start_agent = AsyncMock()
        agent._wait_until_ready = AsyncMock()
        agent._request = AsyncMock(
            side_effect=[
                ('{"session_id":"01K3ABCDEF0123456789ABCDEF"}', 201),
                ("", 202),
            ]
        )
        agent._wait_for_session_wait = AsyncMock()
        agent._stop_agent = AsyncMock()
        metrics = {
            "input_tokens": 123,
            "cached_input_tokens": 100,
            "uncached_input_tokens": 23,
            "output_tokens": 45,
            "output_reasoning_tokens": 30,
            "output_text_tokens": 15,
            "total_tokens": 168,
            "peak_context_tokens": 100,
            "agent_steps": 2,
            "provider_requests": 2,
            "provider_requests_missing_usage": 1,
            "context_handoffs": 0,
            "activation_outcome": "finished",
            "final_assistant_content_bytes": 0,
            "final_assistant_tool_call_count": 0,
            "completion_end_tool_succeeded": False,
            "completion_submitted": False,
        }
        agent._collect_session_artifacts = AsyncMock(return_value=metrics)
        environment = _AgentEnvironment()
        context = SimpleNamespace(
            n_input_tokens=None,
            n_output_tokens=None,
            peak_context_tokens=None,
            n_agent_steps=None,
            summarization_count=None,
            metadata=None,
        )

        with self.assertRaisesRegex(RuntimeError, "successful end tool call"):
            await agent.run("fix it", environment, context)

        agent._wait_for_session_wait.assert_awaited_once()
        agent._stop_agent.assert_awaited_once()
        agent._collect_session_artifacts.assert_awaited_once()
        self.assertEqual(agent._request.await_count, 2)
        self.assertEqual(context.metadata, metrics)

    async def test_cancellation_still_restores_and_collects_partial_metrics(
        self,
    ) -> None:
        agent = object.__new__(ZorkDeepSweAgent)
        agent.logs_dir = Path(tempfile.mkdtemp(prefix="zork-adapter-cancel-test-"))
        agent._upload_request_documents = AsyncMock()
        agent._start_agent = AsyncMock()
        agent._wait_until_ready = AsyncMock()
        agent._request = AsyncMock(
            side_effect=[
                ('{"session_id":"01K3ABCDEF0123456789ABCDEF"}', 201),
                ("", 202),
            ]
        )
        cancellation = asyncio.CancelledError()
        agent._wait_for_session_wait = AsyncMock(side_effect=cancellation)
        agent._stop_agent = AsyncMock()
        metrics = {
            "input_tokens": 10,
            "cached_input_tokens": 8,
            "uncached_input_tokens": 2,
            "output_tokens": 2,
            "output_reasoning_tokens": 1,
            "output_text_tokens": 1,
            "total_tokens": 12,
            "peak_context_tokens": 10,
            "agent_steps": 1,
            "provider_requests": 1,
            "provider_requests_missing_usage": 0,
            "context_handoffs": 0,
            "activation_outcome": None,
        }
        agent._collect_session_artifacts = AsyncMock(return_value=metrics)
        environment = _AgentEnvironment()
        context = SimpleNamespace(
            n_input_tokens=None,
            n_output_tokens=None,
            peak_context_tokens=None,
            n_agent_steps=None,
            summarization_count=None,
            metadata=None,
        )

        with self.assertRaises(asyncio.CancelledError) as raised:
            await agent.run("fix it", environment, context)

        self.assertIs(raised.exception, cancellation)
        agent._stop_agent.assert_awaited_once()
        agent._collect_session_artifacts.assert_awaited_once()
        self.assertEqual(context.metadata, metrics)


if __name__ == "__main__":
    unittest.main()
