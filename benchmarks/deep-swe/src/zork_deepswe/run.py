from __future__ import annotations

import argparse
import asyncio
import os
import subprocess
import tempfile
from contextlib import ExitStack
from dataclasses import dataclass
from importlib.resources import as_file, files
from pathlib import Path

from zork_deepswe.build import SUPPORTED_PLATFORMS, build_zork_agent, native_linux_platform
from zork_deepswe.prepare import (
    collect_unique_images,
    pull_images_serially,
    select_task_paths,
)
from zork_deepswe.profile import build_profile, write_profile

DATASET_URL = "https://github.com/datacurve-ai/deep-swe.git"
DATASET_COMMIT = "435ee89ec2f2e2289f33b0da4f992f0b7b7266b9"
AGENT_IMPORT_PATH = "zork_deepswe.agents.zork:ZorkDeepSweAgent"


@dataclass(frozen=True)
class ProfilePreset:
    auth_file: Path
    model: str
    thinking: str
    auth_domains: str


def _profile_presets() -> dict[str, ProfilePreset]:
    return {
        "opencode-go": ProfilePreset(
            auth_file=Path.home() / ".pi/agent/auth.json",
            model="muse-spark-1.2-contributor",
            thinking="xhigh",
            auth_domains="",
        ),
        "openai-subscription": ProfilePreset(
            auth_file=Path.home() / ".codex/auth.json",
            model="gpt-5.6-luna",
            thinking="max",
            auth_domains="auth.openai.com",
        ),
    }


def find_repo_root(start: Path) -> Path:
    resolved = start.resolve()
    for candidate in (resolved, *resolved.parents):
        if (candidate / "Cargo.toml").is_file() and (
            candidate / "pnpm-workspace.yaml"
        ).is_file():
            return candidate
    raise ValueError(f"could not find the Zork repository above {start}")


def fixed_task_ids() -> tuple[str, ...]:
    resource = files("zork_deepswe").joinpath("resources/deepswe-seed0-task-ids.txt")
    with as_file(resource) as path:
        return tuple(
            line.strip() for line in path.read_text().splitlines() if line.strip()
        )


def checkout_dataset(
    dataset_root: Path,
    *,
    commit: str = DATASET_COMMIT,
    command_runner=subprocess.run,
) -> None:
    if not (dataset_root / ".git").is_dir():
        if dataset_root.exists() and any(dataset_root.iterdir()):
            raise ValueError(
                f"dataset path exists but is not a git checkout: {dataset_root}"
            )
        dataset_root.parent.mkdir(parents=True, exist_ok=True)
        command_runner(["git", "clone", DATASET_URL, str(dataset_root)], check=True)
    command_runner(
        ["git", "-C", str(dataset_root), "fetch", "origin", commit], check=True
    )
    command_runner(
        ["git", "-C", str(dataset_root), "checkout", "--detach", commit], check=True
    )


def pier_command(
    *,
    tasks_path: Path,
    task_name: str,
    jobs_root: Path,
    model: str,
    binary: Path,
    profile_file: Path,
    thinking: str,
    auth_domains: str,
    no_streaming: bool,
    attempts: int,
    concurrency: int,
    responses_probe_script: Path | None = None,
) -> list[str]:
    command = [
        "pier",
        "run",
        "--path",
        str(tasks_path),
        "--include-task-name",
        task_name,
        "--agent-import-path",
        AGENT_IMPORT_PATH,
        "--environment-import-path",
        "zork_deepswe.environment:RetainedDockerEnvironment",
        "--model",
        model,
        "--agent-kwarg",
        f"zork_binary={binary}",
        "--agent-kwarg",
        f"profile_file={profile_file}",
        "--agent-kwarg",
        f"thinking={thinking}",
        "--agent-kwarg",
        f"auth_domains={auth_domains}",
        "--agent-kwarg",
        f"no_streaming={str(no_streaming).lower()}",
    ]
    if responses_probe_script is not None:
        command.extend(
            ["--agent-kwarg", f"responses_probe_script={responses_probe_script}"]
        )
    command.extend(
        [
            "--n-attempts",
            str(attempts),
            "--n-concurrent",
            str(concurrency),
            "--max-retries",
            "0",
            "--jobs-dir",
            str(jobs_root),
            "--yes",
        ]
    )
    return command


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("task_id")
    parser.add_argument("--repo-root", type=Path)
    parser.add_argument("--dataset-root", type=Path)
    parser.add_argument("--jobs-root", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--profile-file", type=Path)
    parser.add_argument(
        "--profile-preset", choices=tuple(_profile_presets()), default="opencode-go"
    )
    parser.add_argument("--auth-file", type=Path)
    parser.add_argument("--model")
    parser.add_argument("--thinking")
    parser.add_argument("--auth-domains", default="")
    parser.add_argument("--no-streaming", action="store_true")
    parser.add_argument(
        "--parallel-tool-calls",
        action="store_true",
    )
    parser.add_argument("--context-window-tokens", type=int)
    parser.add_argument("--max-output-tokens", type=int)
    parser.add_argument("--attempts", type=int, default=4)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--responses-probe", action="store_true")
    parser.add_argument(
        "--platform", choices=SUPPORTED_PLATFORMS, default=native_linux_platform()
    )


def execute(arguments: argparse.Namespace) -> None:
    if arguments.task_id not in fixed_task_ids():
        raise ValueError(
            f"task is not in the fixed DeepSWE seed-0 subset: {arguments.task_id}"
        )
    if arguments.attempts <= 0 or arguments.concurrency <= 0:
        raise ValueError("attempts and concurrency must be positive")

    repo_root = (
        arguments.repo_root.expanduser().resolve()
        if arguments.repo_root
        else find_repo_root(Path.cwd())
    )
    dataset_root = (
        arguments.dataset_root.expanduser().resolve()
        if arguments.dataset_root
        else repo_root / ".data/benchmarks/deep-swe"
    )
    jobs_root = (
        arguments.jobs_root.expanduser().resolve()
        if arguments.jobs_root
        else repo_root / "artifacts/deepswe"
    )

    with ExitStack() as stack:
        directory = stack.enter_context(
            tempfile.TemporaryDirectory(prefix="zork-deepswe-profile-")
        )
        profile_file, model, thinking, auth_domains = _resolve_profile(
            arguments, Path(directory)
        )
        binary = _resolve_binary(arguments.binary, repo_root, arguments.platform)

        subprocess.run(["docker", "compose", "version"], check=True)
        checkout_dataset(dataset_root)
        task_paths = asyncio.run(
            select_task_paths(dataset_root / "tasks", n_tasks=10, sample_seed=0)
        )
        pull_images_serially(collect_unique_images(task_paths), arguments.platform)
        jobs_root.mkdir(parents=True, exist_ok=True)

        responses_probe_script = None
        if arguments.responses_probe:
            resource = files("zork_deepswe").joinpath("responses_probe.py")
            responses_probe_script = stack.enter_context(as_file(resource))
        subprocess.run(
            pier_command(
                tasks_path=dataset_root / "tasks",
                task_name=arguments.task_id,
                jobs_root=jobs_root,
                model=model,
                binary=binary,
                profile_file=profile_file,
                thinking=thinking,
                auth_domains=auth_domains,
                no_streaming=arguments.no_streaming,
                attempts=arguments.attempts,
                concurrency=arguments.concurrency,
                responses_probe_script=responses_probe_script,
            ),
            cwd=repo_root,
            check=True,
        )


def _resolve_binary(explicit: Path | None, repo_root: Path, platform: str) -> Path:
    if explicit is not None:
        binary = explicit.expanduser().resolve()
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError(f"explicit zork-agent binary is not executable: {binary}")
        return binary
    binary = repo_root / f"target/deepswe/zork-agent-{platform.replace('/', '-')}"
    build_zork_agent(repo_root, binary, platform=platform)
    return binary


def _resolve_profile(
    arguments: argparse.Namespace, temporary_directory: Path
) -> tuple[Path, str, str, str]:
    if arguments.profile_file is not None:
        profile_file = arguments.profile_file.expanduser().resolve()
        if not profile_file.is_file():
            raise ValueError(f"DeepSWE profile file does not exist: {profile_file}")
        if not arguments.model or not arguments.thinking:
            raise ValueError("--model and --thinking are required with --profile-file")
        return (
            profile_file,
            arguments.model,
            arguments.thinking,
            arguments.auth_domains,
        )

    preset = _profile_presets()[arguments.profile_preset]
    if arguments.model and arguments.model != preset.model:
        raise ValueError(f"{arguments.profile_preset} requires model {preset.model}")
    if arguments.thinking and arguments.thinking != preset.thinking:
        raise ValueError(
            f"{arguments.profile_preset} requires thinking {preset.thinking}"
        )
    auth_file = (arguments.auth_file or preset.auth_file).expanduser().resolve()
    profile_file = temporary_directory / f"{arguments.profile_preset}.json"
    write_profile(
        profile_file,
        build_profile(
            arguments.profile_preset,
            auth_file,
            streaming=True,
            parallel_tool_calls=arguments.parallel_tool_calls,
            context_window_tokens=arguments.context_window_tokens,
            max_output_tokens=arguments.max_output_tokens,
        ),
    )
    return profile_file, preset.model, preset.thinking, preset.auth_domains


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser()
    add_arguments(parser)
    execute(parser.parse_args(argv))


if __name__ == "__main__":
    main()
