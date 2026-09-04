import json
import tempfile
import unittest
from pathlib import Path

from zork_deepswe.agents.pi import (
    PI_APPEND_SYSTEM_PROMPT,
    PI_MODEL,
    PI_PROVIDER,
    PI_SERVICE_TIER,
    PiDeepSweAgent,
    aggregate_pi_session_entries,
    build_fast_extension_source,
    build_pi_auth_document,
    build_pi_models_document,
    build_pi_settings_document,
    iter_pi_session_entries,
)


class PiDeepSweConfigurationTest(unittest.TestCase):
    def test_builds_isolated_pi_oauth_from_codex_subscription_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "header.eyJleHAiOjE5MDAwMDAwMDB9.signature",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )

            document = build_pi_auth_document(auth_path)

        self.assertEqual(
            document,
            {
                "openai-codex": {
                    "type": "oauth",
                    "access": "header.eyJleHAiOjE5MDAwMDAwMDB9.signature",
                    "refresh": "refresh-secret",
                    "expires": 1_900_000_000_000,
                    "accountId": "account-secret",
                }
            },
        )

    def test_rejects_non_subscription_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(json.dumps({"auth_mode": "apikey"}))
            with self.assertRaisesRegex(ValueError, "ChatGPT subscription"):
                build_pi_auth_document(auth_path)

    def test_model_and_settings_share_the_256k_32k_boundary(self) -> None:
        self.assertEqual(
            build_pi_models_document(256_000, 32_000),
            {
                "providers": {
                    "openai-codex": {
                        "modelOverrides": {
                            "gpt-5.6-luna": {
                                "contextWindow": 256_000,
                                "maxTokens": 32_000,
                            }
                        }
                    }
                }
            },
        )
        settings = build_pi_settings_document(32_000)
        self.assertEqual(settings["transport"], "websocket-cached")
        self.assertEqual(settings["httpIdleTimeoutMs"], 0)
        self.assertEqual(settings["compaction"]["reserveTokens"], 32_000)
        self.assertIs(settings["compaction"]["enabled"], True)
        self.assertEqual(settings["defaultProjectTrust"], "never")

    def test_rejects_an_output_limit_that_cannot_fit_the_context(self) -> None:
        with self.assertRaisesRegex(ValueError, "less than context"):
            build_pi_models_document(32_000, 32_000)

    def test_common_prompt_does_not_add_zork_end_semantics_to_pi(self) -> None:
        self.assertIn("changing the repository", PI_APPEND_SYSTEM_PROMPT)
        self.assertNotIn("call end", PI_APPEND_SYSTEM_PROMPT.lower())
        self.assertNotIn("end tool", PI_APPEND_SYSTEM_PROMPT.lower())


class PiSessionAggregationTest(unittest.TestCase):
    def test_aggregates_assistant_and_compaction_provider_usage(self) -> None:
        entries = [
            {"type": "session", "version": 3, "id": "session-id"},
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "reason"},
                        {
                            "type": "toolCall",
                            "id": "call-1",
                            "name": "bash",
                            "arguments": {"command": "false"},
                        },
                    ],
                    "provider": PI_PROVIDER,
                    "model": PI_MODEL,
                    "usage": {
                        "input": 100,
                        "cacheRead": 80,
                        "cacheWrite": 5,
                        "output": 20,
                        "reasoning": 12,
                        "totalTokens": 205,
                    },
                    "stopReason": "toolUse",
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "toolResult",
                    "toolCallId": "call-1",
                    "toolName": "bash",
                    "content": [{"type": "text", "text": "failed"}],
                    "isError": True,
                },
            },
            {
                "type": "compaction",
                "summary": "state",
                "tokensBefore": 224_000,
                "usage": {
                    "input": 10,
                    "cacheRead": 190,
                    "cacheWrite": 0,
                    "output": 5,
                    "totalTokens": 205,
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "provider": PI_PROVIDER,
                    "model": PI_MODEL,
                    "usage": {
                        "input": 20,
                        "cacheRead": 180,
                        "cacheWrite": 0,
                        "output": 10,
                        "reasoning": 5,
                        "totalTokens": 210,
                    },
                    "stopReason": "stop",
                },
            },
        ]

        metrics = aggregate_pi_session_entries(entries)

        self.assertEqual(metrics["input_tokens"], 585)
        self.assertEqual(metrics["cached_input_tokens"], 450)
        self.assertEqual(metrics["uncached_input_tokens"], 135)
        self.assertEqual(metrics["output_tokens"], 35)
        self.assertEqual(metrics["output_reasoning_tokens"], 17)
        self.assertEqual(metrics["total_tokens"], 620)
        self.assertEqual(metrics["peak_context_tokens"], 200)
        self.assertEqual(metrics["agent_steps"], 2)
        self.assertEqual(metrics["provider_requests"], 3)
        self.assertEqual(metrics["compactions"], 1)
        self.assertEqual(metrics["tool_calls"], 1)
        self.assertEqual(metrics["tool_errors"], 1)
        self.assertEqual(metrics["tool_counts"], {"bash": 1})
        self.assertEqual(metrics["final_assistant_content_bytes"], 2)
        self.assertEqual(metrics["final_stop_reason"], "stop")
        self.assertIs(metrics["completion_submitted"], True)

    def test_does_not_reclassify_an_empty_stop_as_an_error_or_retry(self) -> None:
        metrics = aggregate_pi_session_entries(
            [
                {
                    "type": "message",
                    "message": {
                        "role": "assistant",
                        "content": [],
                        "provider": PI_PROVIDER,
                        "model": PI_MODEL,
                        "usage": {
                            "input": 1,
                            "cacheRead": 0,
                            "cacheWrite": 0,
                            "output": 0,
                            "totalTokens": 1,
                        },
                        "stopReason": "stop",
                    },
                }
            ]
        )
        self.assertEqual(metrics["final_assistant_content_bytes"], 0)
        self.assertIs(metrics["completion_submitted"], True)
        self.assertEqual(metrics["provider_requests"], 1)

    def test_reads_every_jsonl_file_in_lexical_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "b.jsonl").write_text('{"type":"message","id":"b"}\n')
            (root / "a.jsonl").write_text(
                '\n{"type":"session","id":"a"}\n', encoding="utf-8"
            )
            self.assertEqual(
                [entry["id"] for entry in iter_pi_session_entries(root)],
                ["a", "b"],
            )


class PiDeepSweAgentTest(unittest.TestCase):
    def test_api_key_profile_is_the_single_source_of_model_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "pi.tar.gz"
            bundle.write_bytes(b"bundle")
            profile = root / "muse13.json"
            profile.write_text(
                json.dumps(
                    {
                        "provider": "opencode-go",
                        "billing": "subscription",
                        "base_url": "https://opencode.ai/zen/go/v1",
                        "auth": {"type": "api_key", "key": "private-muse-key"},
                        "models": [
                            {
                                "id": "muse-spark-1.3-contributor",
                                "api": "openai-responses",
                                "streaming": True,
                                "parallel_tool_calls": False,
                                "thinking": ["off", "high", "xhigh"],
                                "capabilities": {"input": ["text"]},
                                "limits": {
                                    "context_window_tokens": 256_000,
                                    "max_output_tokens": 131_072,
                                },
                            }
                        ],
                    }
                )
            )
            args = dict(
                logs_dir=root,
                model_name="muse-spark-1.3-contributor",
                pi_bundle=str(bundle),
                profile_file=str(profile),
                thinking="xhigh",
            )
            agent = PiDeepSweAgent(**args)
            model = agent._models_document["providers"]["opencode-go"]["models"][0]
            self.assertEqual(model["contextWindow"], 256_000)
            self.assertEqual(model["maxTokens"], 131_072)
            self.assertEqual(model["thinkingLevelMap"]["xhigh"], "xhigh")
            self.assertEqual(model["samplingParams"], {"parallel_tool_calls": False})
            self.assertEqual(agent._transport, "sse")
            self.assertIsNone(agent._service_tier)
            self.assertEqual(agent.network_allowlist().domains, ["opencode.ai"])
            settings = build_pi_settings_document(
                agent._max_output_tokens, transport=agent._transport
            )
            self.assertEqual(settings["compaction"]["reserveTokens"], 131_072)
            self.assertEqual(settings["compaction"]["keepRecentTokens"], 20_000)
            agent._write_public_manifest()
            manifest = (root / "pi-manifest.json").read_text()
            self.assertNotIn("private-muse-key", manifest)
            self.assertNotIn("private-muse-key", repr(agent))
            self.assertEqual(
                json.loads(manifest)["model"], "muse-spark-1.3-contributor"
            )
            with self.assertRaisesRegex(ValueError, "max_output_tokens must match"):
                PiDeepSweAgent(**args, max_output_tokens=8192)
            with self.assertRaisesRegex(ValueError, "context_window_tokens must match"):
                PiDeepSweAgent(**args, context_window_tokens=512_000)
            with self.assertRaisesRegex(ValueError, "either profile_file"):
                PiDeepSweAgent(**args, codex_auth_file="unused.json")

    def test_api_key_wire_audit_does_not_inject_subscription_service_tier(self) -> None:
        source = build_fast_extension_source(
            "opencode-go", "muse-spark-1.3-contributor", None
        )
        self.assertIn("const payload = event.payload;", source)
        self.assertNotIn('service_tier: "priority"', source)
        self.assertIn('ctx.model?.provider !== "opencode-go"', source)
        self.assertIn("max_output_tokens: payload.max_output_tokens", source)
        self.assertIn("context_window_tokens: ctx.model.contextWindow", source)

    def test_constructor_forwards_thinking_and_freezes_network_boundary(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "pi-linux-amd64.tar.gz"
            bundle.write_bytes(b"bundle")
            auth = root / "codex-auth.json"
            auth.write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "header.eyJleHAiOjE5MDAwMDAwMDB9.signature",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )
            logs = root / "logs"
            logs.mkdir()

            agent = PiDeepSweAgent(
                logs_dir=logs,
                model_name=PI_MODEL,
                pi_bundle=str(bundle),
                codex_auth_file=str(auth),
                thinking="medium",
                context_window_tokens=256_000,
                max_output_tokens=32_000,
            )

            self.assertEqual(
                agent.network_allowlist().model_dump()["domains"],
                [
                    "auth.openai.com",
                    "chatgpt.com",
                ],
            )
            self.assertIn("sha256:", agent.version())
            self.assertNotIn("refresh-secret", repr(agent))
            self.assertEqual(agent._thinking, "medium")
            self.assertEqual(agent._service_tier, PI_SERVICE_TIER)

    def test_rejects_the_opencode_go_luna_alias(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "pi.tar.gz"
            bundle.write_bytes(b"bundle")
            auth = root / "auth.json"
            auth.write_text(json.dumps({"auth_mode": "chatgpt", "tokens": {}}))
            logs = root / "logs"
            logs.mkdir()
            with self.assertRaisesRegex(ValueError, "gpt-5.6-luna"):
                PiDeepSweAgent(
                    logs_dir=logs,
                    model_name="opencode-go/gpt-5.6-luna",
                    pi_bundle=str(bundle),
                    codex_auth_file=str(auth),
                    thinking="max",
                )


if __name__ == "__main__":
    unittest.main()
