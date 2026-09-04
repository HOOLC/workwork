from __future__ import annotations

import argparse
import asyncio
import subprocess
import time
from pathlib import Path

import tomllib
from pier.models.job.config import DatasetConfig

from zork_deepswe.build import SUPPORTED_PLATFORMS, native_linux_platform

RATE_LIMIT_ATTEMPTS = 6
RATE_LIMIT_DELAY_SECONDS = 30


async def select_task_paths(
    tasks_path: Path, n_tasks: int, sample_seed: int
) -> list[Path]:
    configs = await DatasetConfig(
        path=tasks_path,
        n_tasks=n_tasks,
        sample_seed=sample_seed,
    ).get_task_configs()
    paths = [config.path for config in configs]
    if any(path is None for path in paths):
        raise ValueError("the DeepSWE subset contains a non-local task")
    return [path for path in paths if path is not None]


def collect_unique_images(task_paths: list[Path]) -> list[str]:
    images: list[str] = []
    for task_path in task_paths:
        document = tomllib.loads((task_path / "task.toml").read_text())
        environment = document.get("environment")
        image = (
            environment.get("docker_image") if isinstance(environment, dict) else None
        )
        if not isinstance(image, str) or not image:
            raise ValueError(f"task has no environment.docker_image: {task_path}")
        if image not in images:
            images.append(image)
    return images


def pull_images_serially(images: list[str], platform: str) -> None:
    for index, image in enumerate(images, start=1):
        present = subprocess.run(
            ["docker", "image", "inspect", image],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if present.returncode == 0:
            print(f"[{index}/{len(images)}] cached {image}", flush=True)
            continue

        for attempt in range(1, RATE_LIMIT_ATTEMPTS + 1):
            print(
                f"[{index}/{len(images)}] pulling {image} (attempt {attempt})",
                flush=True,
            )
            pulled = subprocess.run(
                ["docker", "pull", "--platform", platform, image],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            if pulled.returncode == 0:
                print(pulled.stdout.rstrip(), flush=True)
                break
            if "toomanyrequests: Rate exceeded" not in pulled.stdout:
                raise RuntimeError(f"docker pull failed for {image}:\n{pulled.stdout}")
            if attempt == RATE_LIMIT_ATTEMPTS:
                raise RuntimeError(
                    f"Public ECR rate limit did not clear while pulling {image}"
                )
            print(
                f"Public ECR rate limited the pull; retrying in {RATE_LIMIT_DELAY_SECONDS}s",
                flush=True,
            )
            time.sleep(RATE_LIMIT_DELAY_SECONDS)


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--tasks-path", type=Path, required=True)
    parser.add_argument("--n-tasks", type=int, required=True)
    parser.add_argument("--sample-seed", type=int, required=True)
    parser.add_argument(
        "--platform", choices=SUPPORTED_PLATFORMS, default=native_linux_platform()
    )


def execute(args: argparse.Namespace) -> None:
    task_paths = asyncio.run(
        select_task_paths(args.tasks_path, args.n_tasks, args.sample_seed)
    )
    pull_images_serially(collect_unique_images(task_paths), args.platform)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser()
    add_arguments(parser)
    execute(parser.parse_args(argv))


if __name__ == "__main__":
    main()
