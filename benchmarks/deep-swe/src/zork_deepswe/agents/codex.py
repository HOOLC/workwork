from __future__ import annotations

from pathlib import Path
from typing import Any

from pier.agents.installed.codex import Codex
from pier.models.agent.network import NetworkAllowlist

CODEX_MODEL = "gpt-5.6-luna"
CODEX_REASONING_EFFORT = "max"
CODEX_SERVICE_TIER = "priority"
CODEX_VERSION = "0.148.0"


def build_codex_benchmark_config(
    context_window_tokens: int, max_output_tokens: int
) -> str:
    if context_window_tokens <= 0:
        raise ValueError("context window must be positive")
    if max_output_tokens <= 0 or max_output_tokens >= context_window_tokens:
        raise ValueError("max output tokens must be positive and less than context")
    compact_at = context_window_tokens - max_output_tokens
    return (
        f'service_tier = "{CODEX_SERVICE_TIER}"\n'
        f"model_context_window = {context_window_tokens}\n"
        f"model_auto_compact_token_limit = {compact_at}\n"
        "check_for_update_on_startup = false\n"
        "\n"
        "[features]\n"
        "fast_mode = true\n"
    )


class CodexSubscriptionDeepSweAgent(Codex):
    """Native Codex CLI with only the subscription network boundary completed."""

    def __init__(
        self,
        *args: Any,
        logs_dir: Path,
        model_name: str,
        version: str = CODEX_VERSION,
        reasoning_effort: str = CODEX_REASONING_EFFORT,
        context_window_tokens: int = 256_000,
        max_output_tokens: int = 32_000,
        **kwargs: Any,
    ) -> None:
        if model_name != CODEX_MODEL:
            raise ValueError(f"Codex benchmark requires model {CODEX_MODEL}")
        if version != CODEX_VERSION:
            raise ValueError(f"Codex benchmark requires version {CODEX_VERSION}")
        if "config_toml" in kwargs or "config_toml_file" in kwargs:
            raise ValueError("Codex benchmark config is derived from its frozen limits")
        super().__init__(
            *args,
            logs_dir=logs_dir,
            model_name=model_name,
            version=version,
            reasoning_effort=reasoning_effort,
            config_toml=build_codex_benchmark_config(
                context_window_tokens, max_output_tokens
            ),
            **kwargs,
        )

    def network_allowlist(self) -> NetworkAllowlist:
        domains = set(super().network_allowlist().domains)
        domains.update({"chatgpt.com", "auth.openai.com"})
        return NetworkAllowlist(domains=sorted(domains))
