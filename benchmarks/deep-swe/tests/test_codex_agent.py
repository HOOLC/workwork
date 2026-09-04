import tempfile
import unittest
from pathlib import Path

import tomllib
from zork_deepswe.agents.codex import (
    CODEX_MODEL,
    CODEX_REASONING_EFFORT,
    CODEX_SERVICE_TIER,
    CODEX_VERSION,
    CodexSubscriptionDeepSweAgent,
    build_codex_benchmark_config,
)


class CodexDeepSweAgentTest(unittest.TestCase):
    def test_config_freezes_luna_fast_and_the_256k_boundary(self) -> None:
        config = tomllib.loads(build_codex_benchmark_config(256_000, 32_000))

        self.assertEqual(CODEX_MODEL, "gpt-5.6-luna")
        self.assertEqual(CODEX_REASONING_EFFORT, "max")
        self.assertEqual(CODEX_SERVICE_TIER, "priority")
        self.assertEqual(CODEX_VERSION, "0.148.0")
        self.assertEqual(config["service_tier"], "priority")
        self.assertEqual(config["model_context_window"], 256_000)
        self.assertEqual(config["model_auto_compact_token_limit"], 224_000)
        self.assertIs(config["features"]["fast_mode"], True)

    def test_subscription_network_boundary_includes_native_codex_hosts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = CodexSubscriptionDeepSweAgent(
                logs_dir=Path(directory),
                model_name=CODEX_MODEL,
                version=CODEX_VERSION,
                reasoning_effort="medium",
                context_window_tokens=256_000,
                max_output_tokens=32_000,
            )

        self.assertEqual(agent._resolved_flags["reasoning_effort"], "medium")
        self.assertEqual(
            agent.network_allowlist().model_dump()["domains"],
            ["api.openai.com", "auth.openai.com", "chatgpt.com"],
        )

    def test_rejects_an_output_reserve_that_cannot_fit_the_context(self) -> None:
        with self.assertRaisesRegex(ValueError, "less than context"):
            build_codex_benchmark_config(32_000, 32_000)


if __name__ == "__main__":
    unittest.main()
