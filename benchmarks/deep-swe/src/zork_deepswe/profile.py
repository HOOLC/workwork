from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
from typing import Any


def _required_string(document: dict[str, Any], key: str, source: Path) -> str:
    value = document.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{key} is missing from {source}")
    return value


def _jwt_expiration_ms(token: str) -> int | None:
    parts = token.split(".")
    if len(parts) != 3:
        return None
    payload = parts[1]
    payload += "=" * (-len(payload) % 4)
    try:
        document = json.loads(base64.urlsafe_b64decode(payload))
    except (ValueError, json.JSONDecodeError):
        return None
    expires = document.get("exp") if isinstance(document, dict) else None
    return expires * 1000 if isinstance(expires, int) and expires > 0 else None


def _openai_subscription_profile(auth_path: Path, *, streaming: bool) -> dict[str, Any]:
    source = json.loads(auth_path.expanduser().read_text())
    if not isinstance(source, dict) or source.get("auth_mode") != "chatgpt":
        raise ValueError(f"{auth_path} does not contain ChatGPT subscription auth")
    tokens = source.get("tokens")
    if not isinstance(tokens, dict):
        raise ValueError(f"tokens are missing from {auth_path}")
    access = _required_string(tokens, "access_token", auth_path)
    refresh = _required_string(tokens, "refresh_token", auth_path)
    account_id = _required_string(tokens, "account_id", auth_path)
    auth: dict[str, Any] = {
        "type": "oauth",
        "access": access,
        "refresh": refresh,
        "accountId": account_id,
    }
    if (expires := _jwt_expiration_ms(access)) is not None:
        auth["expires"] = expires
    return {
        "provider": "openai",
        "billing": "subscription",
        "base_url": "https://chatgpt.com/backend-api/codex",
        "headers": {"originator": "zork"},
        "auth": auth,
        "models": [
            {
                "id": "gpt-5.6-luna",
                "api": "openai-codex-responses",
                "streaming": streaming,
                "parallel_tool_calls": False,
                "service_tier": "priority",
                "thinking": ["low", "medium", "high", "xhigh", "max"],
                "default_thinking": "max",
                "capabilities": {"input": ["text", "image"]},
                "limits": {
                    "context_window_tokens": 872_000,
                    "max_output_tokens": 128_000,
                },
                "default": True,
            }
        ],
    }


def _opencode_go_profile(auth_path: Path, *, streaming: bool) -> dict[str, Any]:
    source = json.loads(auth_path.expanduser().read_text())
    entry = source.get("opencode-go") if isinstance(source, dict) else None
    if not isinstance(entry, dict):
        raise ValueError(f"OpenCode Go credentials are missing from {auth_path}")
    key = _required_string(entry, "key", auth_path)
    return {
        "provider": "opencode-go",
        "billing": "subscription",
        "base_url": "https://opencode.ai/zen/go/v1",
        "headers": {},
        "auth": {"type": "api_key", "key": key},
        "models": [
            {
                "id": "muse-spark-1.2-contributor",
                "api": "openai-responses",
                "streaming": streaming,
                "thinking": ["off", "minimal", "low", "medium", "high", "xhigh"],
                "default_thinking": "xhigh",
                "capabilities": {"input": ["text", "image"]},
                "limits": {
                    "context_window_tokens": 1_048_576,
                    "max_output_tokens": 131_072,
                },
                "default": True,
            }
        ],
    }


def build_profile(
    preset: str,
    auth_path: Path,
    *,
    streaming: bool,
    parallel_tool_calls: bool = False,
    context_window_tokens: int | None = None,
    max_output_tokens: int | None = None,
) -> dict[str, Any]:
    if preset == "openai-subscription":
        if parallel_tool_calls:
            raise ValueError(
                "OpenAI Codex Responses Lite requires parallel_tool_calls=false"
            )
        profile = _openai_subscription_profile(
            auth_path,
            streaming=streaming,
        )
    elif preset == "opencode-go":
        if parallel_tool_calls:
            raise ValueError(
                "opencode-go profile does not implement parallel_tool_calls"
            )
        profile = _opencode_go_profile(auth_path, streaming=streaming)
    else:
        raise ValueError(f"unknown DeepSWE profile preset: {preset}")
    limits = profile["models"][0]["limits"]
    if context_window_tokens is not None:
        profile["models"][0]["limits"]["context_window_tokens"] = context_window_tokens
    if max_output_tokens is not None:
        limits["max_output_tokens"] = max_output_tokens
    if (
        limits["max_output_tokens"] <= 0
        or limits["max_output_tokens"] >= limits["context_window_tokens"]
    ):
        raise ValueError(
            "max_output_tokens must be positive and less than context_window_tokens"
        )
    return profile


def write_profile(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(document, stream, separators=(",", ":"))
        stream.write("\n")
    os.chmod(path, 0o600)


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--preset", required=True, choices=("opencode-go", "openai-subscription")
    )
    parser.add_argument("--auth-file", required=True, type=Path)
    parser.add_argument("--streaming", required=True, choices=("true", "false"))
    parser.add_argument(
        "--parallel-tool-calls", choices=("true", "false"), default="false"
    )
    parser.add_argument("--context-window-tokens", type=int)
    parser.add_argument("--max-output-tokens", type=int)
    parser.add_argument("--output", required=True, type=Path)


def execute(arguments: argparse.Namespace) -> None:
    write_profile(
        arguments.output,
        build_profile(
            arguments.preset,
            arguments.auth_file.expanduser().resolve(),
            streaming=arguments.streaming == "true",
            parallel_tool_calls=arguments.parallel_tool_calls == "true",
            context_window_tokens=arguments.context_window_tokens,
            max_output_tokens=arguments.max_output_tokens,
        ),
    )


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser()
    add_arguments(parser)
    execute(parser.parse_args(argv))


if __name__ == "__main__":
    main()
