from __future__ import annotations

import hashlib
import platform as host_platform
import shutil
import subprocess
import tempfile
from collections.abc import Callable
from importlib.resources import as_file, files
from pathlib import Path

CommandRunner = Callable[..., subprocess.CompletedProcess[str]]
SUPPORTED_PLATFORMS = ("linux/amd64", "linux/arm64")


def native_linux_platform(machine: str | None = None) -> str:
    architecture = (machine or host_platform.machine()).lower()
    if architecture in ("arm64", "aarch64"):
        return "linux/arm64"
    if architecture in ("amd64", "x86_64"):
        return "linux/amd64"
    raise ValueError(f"unsupported host architecture: {architecture}")


def docker_build_command(
    repo_root: Path, dockerfile: Path, export_directory: Path, platform: str
) -> list[str]:
    return [
        "docker",
        "buildx",
        "build",
        "--platform",
        platform,
        "--file",
        str(dockerfile),
        "--target",
        "export",
        "--output",
        f"type=local,dest={export_directory}",
        str(repo_root),
    ]


def build_zork_agent(
    repo_root: Path,
    output: Path,
    *,
    platform: str | None = None,
    command_runner: CommandRunner = subprocess.run,
) -> str:
    repo_root = repo_root.resolve()
    output = output.expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    platform = platform or native_linux_platform()
    if platform not in SUPPORTED_PLATFORMS:
        raise ValueError(f"unsupported build platform: {platform}")
    command_runner(["docker", "buildx", "version"], check=True)

    resource = files("zork_deepswe").joinpath("resources/zork-agent.Dockerfile")
    with (
        as_file(resource) as dockerfile,
        tempfile.TemporaryDirectory(prefix="zork-deepswe-build-") as directory,
    ):
        export_directory = Path(directory)
        command_runner(
            docker_build_command(repo_root, dockerfile, export_directory, platform),
            check=True,
        )
        built = export_directory / "zork-agent"
        if not built.is_file():
            raise RuntimeError("Docker build did not export zork-agent")
        shutil.copyfile(built, output)

    output.chmod(0o755)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    print(f"built {output} (sha256:{digest})", flush=True)
    return digest
