from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from zork_deepswe.agents.pi_qwen import (
    QWEN_MODEL,
    QWEN_PI_PROVIDER,
    QWEN_THINKING,
    PiQwenDeepSweAgent,
    build_pi_qwen_auth_document,
    build_pi_qwen_models_document,
    build_pi_qwen_settings_document,
    build_qwen_wire_audit_source,
    load_pi_qwen_profile,
)


def write_profile(path: Path, *, secret: str = "profile-secret") -> None:
    path.write_text(
        json.dumps(
            {
                "provider": "openai-compatible",
                "billing": "usage",
                "base_url": "https://qwen.example.test/v1",
                "headers": {"x-benchmark-route": "qwen"},
                "auth": {"type": "api_key", "key": secret},
                "models": [
                    {
                        "id": QWEN_MODEL,
                        "api": "openai-responses",
                        "streaming": True,
                        "parallel_tool_calls": False,
                        "thinking": ["low", "medium", "xhigh"],
                        "default_thinking": "xhigh",
                        "capabilities": {"input": ["text"]},
                        "limits": {
                            "context_window_tokens": 256_000,
                            "max_output_tokens": 56_000,
                        },
                        "default": True,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )


class PiQwenConfigurationTest(unittest.TestCase):
    def test_maps_one_profile_to_native_pi_auth_model_and_settings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "qwen38.json"
            write_profile(path)
            profile = load_pi_qwen_profile(
                path, model_name=QWEN_MODEL, thinking=QWEN_THINKING
            )

        self.assertEqual(
            build_pi_qwen_auth_document(profile),
            {QWEN_PI_PROVIDER: {"type": "api_key", "key": "profile-secret"}},
        )
        self.assertEqual(
            build_pi_qwen_models_document(profile),
            {
                "providers": {
                    QWEN_PI_PROVIDER: {
                        "baseUrl": "https://qwen.example.test/v1",
                        "api": "openai-responses",
                        "headers": {"x-benchmark-route": "qwen"},
                        "models": [
                            {
                                "id": QWEN_MODEL,
                                "name": QWEN_MODEL,
                                "reasoning": True,
                                "thinkingLevelMap": {
                                    "off": None,
                                    "minimal": None,
                                    "low": "low",
                                    "medium": "medium",
                                    "high": None,
                                    "xhigh": "xhigh",
                                    "max": None,
                                },
                                "input": ["text"],
                                "contextWindow": 256_000,
                                "maxTokens": 56_000,
                                "samplingParams": {"parallel_tool_calls": False},
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
            },
        )
        settings = build_pi_qwen_settings_document(profile)
        self.assertEqual(settings["transport"], "sse")
        self.assertEqual(settings["httpIdleTimeoutMs"], 0)
        self.assertEqual(
            settings["compaction"],
            {"enabled": True, "reserveTokens": 56_000, "keepRecentTokens": 20_000},
        )
        self.assertEqual(
            settings["retry"],
            {
                "enabled": False,
                "maxRetries": 0,
                "provider": {"maxRetries": 0, "maxRetryDelayMs": 60_000},
            },
        )

    def test_rejects_a_model_limit_override_instead_of_splitting_model_truth(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "qwen38.json"
            write_profile(path)
            profile = json.loads(path.read_text(encoding="utf-8"))
            profile["models"][0]["limits"]["max_output_tokens"] = 32_000
            path.write_text(json.dumps(profile), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "56000"):
                load_pi_qwen_profile(
                    path, model_name=QWEN_MODEL, thinking=QWEN_THINKING
                )

    def test_wire_audit_is_read_only_and_contains_no_credential(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "qwen38.json"
            write_profile(path)
            profile = load_pi_qwen_profile(
                path, model_name=QWEN_MODEL, thinking=QWEN_THINKING
            )
            source = build_qwen_wire_audit_source(profile)

        self.assertIn("before_provider_request", source)
        self.assertIn("max_output_tokens", source)
        self.assertIn("parallel_tool_calls", source)
        self.assertNotIn("return event.payload", source)
        self.assertNotIn("profile-secret", source)


class PiQwenAgentTest(unittest.TestCase):
    def test_constructor_uses_only_profile_domain_and_public_state_has_no_secret(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "pi-linux-amd64.tar.gz"
            bundle.write_bytes(b"bundle")
            profile_path = root / "qwen38.json"
            write_profile(profile_path)
            logs = root / "logs"
            logs.mkdir()

            agent = PiQwenDeepSweAgent(
                logs_dir=logs,
                model_name=QWEN_MODEL,
                pi_bundle=str(bundle),
                profile_file=str(profile_path),
                thinking=QWEN_THINKING,
            )

            self.assertEqual(
                agent.network_allowlist().model_dump()["domains"],
                ["qwen.example.test"],
            )
            manifest = json.dumps(agent._public_manifest_document())
            self.assertNotIn("profile-secret", manifest)
            self.assertNotIn("profile-secret", repr(agent))
            self.assertEqual(agent._profile.context_window_tokens, 256_000)
            self.assertEqual(agent._profile.max_output_tokens, 56_000)


if __name__ == "__main__":
    unittest.main()
