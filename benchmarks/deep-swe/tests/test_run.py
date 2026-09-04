import argparse
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zork_deepswe.run import (
    _resolve_binary,
    add_arguments,
    execute,
    fixed_task_ids,
    pier_command,
)


class DeepSweRunTest(unittest.TestCase):
    def test_pier_command_runs_four_attempts_of_one_named_task(self) -> None:
        command = pier_command(
            tasks_path=Path("/dataset/tasks"),
            task_name="task-a",
            jobs_root=Path("/jobs"),
            model="model-a",
            binary=Path("/bin/zork-agent"),
            profile_file=Path("/profile.json"),
            thinking="high",
            auth_domains="auth.example.com",
            no_streaming=False,
            attempts=4,
            concurrency=4,
        )

        self.assertEqual(command[:2], ["pier", "run"])
        self.assertEqual(command[command.index("--include-task-name") + 1], "task-a")
        self.assertEqual(command[command.index("--n-attempts") + 1], "4")
        self.assertEqual(command[command.index("--n-concurrent") + 1], "4")
        self.assertEqual(
            command[command.index("--environment-import-path") + 1],
            "zork_deepswe.environment:RetainedDockerEnvironment",
        )
        self.assertNotIn("--n-tasks", command)
        self.assertNotIn("--sample-seed", command)

    def test_non_streaming_is_an_agent_startup_option(self) -> None:
        command = pier_command(
            tasks_path=Path("/dataset/tasks"),
            task_name="task-a",
            jobs_root=Path("/jobs"),
            model="model-a",
            binary=Path("/bin/zork-agent"),
            profile_file=Path("/profile.json"),
            thinking="high",
            auth_domains="",
            no_streaming=True,
            attempts=1,
            concurrency=1,
        )

        self.assertIn("no_streaming=true", command)

    def test_default_binary_is_always_rebuilt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo_root = Path(directory)
            output = repo_root / "target/deepswe/zork-agent-linux-arm64"
            output.parent.mkdir(parents=True)
            output.write_text("stale")

            with patch("zork_deepswe.run.build_zork_agent") as build:
                resolved = _resolve_binary(None, repo_root, "linux/arm64")

        self.assertEqual(resolved, output)
        build.assert_called_once_with(repo_root, output, platform="linux/arm64")

    def test_prepares_images_before_starting_pier(self) -> None:
        events: list[str] = []
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = root / "profile.json"
            profile.write_text("{}")
            binary = root / "zork-agent"
            binary.write_text("binary")
            binary.chmod(0o755)
            parser = argparse.ArgumentParser()
            add_arguments(parser)
            arguments = parser.parse_args(
                [
                    fixed_task_ids()[0],
                    "--repo-root",
                    str(root),
                    "--profile-file",
                    str(profile),
                    "--model",
                    "model-a",
                    "--thinking",
                    "high",
                    "--binary",
                    str(binary),
                ]
            )

            async def select(*_args, **_kwargs):
                events.append("select")
                return []

            def run_command(command, **_kwargs):
                events.append(command[0])

            with (
                patch(
                    "zork_deepswe.run.checkout_dataset",
                    side_effect=lambda *_args, **_kwargs: events.append("checkout"),
                ),
                patch("zork_deepswe.run.select_task_paths", side_effect=select),
                patch("zork_deepswe.run.collect_unique_images", return_value=[]),
                patch(
                    "zork_deepswe.run.pull_images_serially",
                    side_effect=lambda _images, _platform: events.append("pull"),
                ),
                patch("zork_deepswe.run.subprocess.run", side_effect=run_command),
            ):
                execute(arguments)

        self.assertLess(events.index("pull"), events.index("pier"))


if __name__ == "__main__":
    unittest.main()
