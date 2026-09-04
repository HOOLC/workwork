import argparse
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from zork_deepswe.compare import (
    OFFICIAL_CONFIG,
    add_arguments,
    build_comparison,
    load_zork_trials,
    render_markdown,
)


def zork_result(
    task: str,
    attempt: int,
    *,
    reward: int,
    input_tokens: int | None,
    errored: bool = False,
    provider_requests: int | None = None,
    provider_requests_missing_usage: int | None = 0,
) -> dict:
    steps = None if input_tokens is None else 2 + attempt
    return {
        "task_name": task,
        "trial_name": f"{task}__{attempt}",
        "agent_info": {
            "name": "zork-agent",
            "model_info": {
                "name": "muse-spark-1.2-contributor",
                "provider": "opencode-go",
            },
        },
        "agent_result": {
            "n_input_tokens": input_tokens,
            "n_output_tokens": None if input_tokens is None else 10 + attempt,
            "n_agent_steps": steps,
            "metadata": {
                "provider_requests": provider_requests
                if provider_requests is not None
                else steps,
                "provider_requests_missing_usage": provider_requests_missing_usage,
            },
        },
        "verifier_result": {"rewards": {"reward": reward}},
        "exception_info": {"exception_type": "RuntimeError"} if errored else None,
    }


def official_result(task: str, attempt: int, *, passed: bool) -> dict:
    return {
        "task_name": task,
        "trial_name": f"official-{task}-{attempt}",
        "config": OFFICIAL_CONFIG,
        "included_in_score": True,
        "passed": passed,
        "errored": False,
        "n_input_tokens": 200 + attempt,
        "n_output_tokens": 20 + attempt,
        "n_agent_steps": 3 + attempt,
    }


class DeepSweComparisonTest(unittest.TestCase):
    def test_loads_only_trial_results_from_one_job(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            job = Path(directory)
            (job / "result.json").write_text('{"stats": {}}')
            trial = job / "task-a__1"
            trial.mkdir()
            (trial / "result.json").write_text(
                json.dumps(zork_result("task-a", 1, reward=1, input_tokens=100))
            )

            loaded = load_zork_trials(job)

        self.assertEqual([row["trial_name"] for row in loaded], ["task-a__1"])

    def test_merges_nested_trial_results_from_multiple_job_directories(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            jobs = []
            for index, task in enumerate(("task-a", "task-b"), start=1):
                job = root / f"job-{index}"
                trial = job / "nested" / f"{task}__1"
                trial.mkdir(parents=True)
                (trial / "result.json").write_text(
                    json.dumps(zork_result(task, 1, reward=1, input_tokens=100))
                )
                jobs.append(job)

            loaded = load_zork_trials(jobs)

        self.assertEqual(
            sorted(row["task_name"] for row in loaded), ["task-a", "task-b"]
        )

    def test_rejects_duplicate_task_trial_across_job_directories(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            jobs = []
            for index in (1, 2):
                job = root / f"job-{index}"
                trial = job / "task-a__1"
                trial.mkdir(parents=True)
                (trial / "result.json").write_text(
                    json.dumps(zork_result("task-a", 1, reward=1, input_tokens=100))
                )
                jobs.append(job)

            with self.assertRaisesRegex(ValueError, "duplicate zork trial"):
                load_zork_trials(jobs)

    def test_compares_equal_task_and_attempt_sets_without_inventing_missing_tokens(
        self,
    ) -> None:
        zork = [
            zork_result("task-a", 1, reward=1, input_tokens=100),
            zork_result("task-a", 2, reward=0, input_tokens=120),
            zork_result("task-b", 1, reward=1, input_tokens=80),
            zork_result("task-b", 2, reward=0, input_tokens=None),
        ]
        official = {
            "rows": [
                official_result("task-a", 1, passed=True),
                official_result("task-a", 2, passed=True),
                official_result("task-b", 1, passed=False),
                official_result("task-b", 2, passed=True),
                {
                    **official_result("task-a", 9, passed=False),
                    "config": "another-config",
                },
            ]
        }

        report = build_comparison(zork, official, expected_attempts=2)

        self.assertEqual(report["tasks"], ["task-a", "task-b"])
        self.assertEqual(report["zork"]["trials"], 4)
        self.assertEqual(report["zork"]["passes"], 2)
        self.assertEqual(report["zork"]["success_rate"], 0.5)
        self.assertEqual(report["official"]["passes"], 3)
        self.assertEqual(report["official"]["success_rate"], 0.75)
        self.assertEqual(report["zork"]["tokens"]["input"]["available"], 3)
        self.assertEqual(report["zork"]["tokens"]["input"]["missing"], 1)
        self.assertEqual(report["comparison"]["success_rate_delta"], -0.25)

    def test_matches_pier_display_names_to_official_task_ids(self) -> None:
        zork = [zork_result("datacurve/task-a", 1, reward=1, input_tokens=100)]
        official = {"rows": [official_result("task-a", 1, passed=True)]}

        report = build_comparison(zork, official, expected_attempts=1)

        self.assertEqual(report["tasks"], ["task-a"])
        self.assertEqual(report["official"]["trials"], 1)

    def test_reports_errors_and_provider_attempts_without_usage(self) -> None:
        zork = [
            zork_result(
                "task-a",
                1,
                reward=0,
                input_tokens=100,
                errored=True,
                provider_requests=5,
                provider_requests_missing_usage=1,
            )
        ]
        official = {"rows": [official_result("task-a", 1, passed=False)]}

        report = build_comparison(zork, official, expected_attempts=1)

        self.assertEqual(report["zork"]["errors"], 1)
        self.assertEqual(report["zork"]["provider_usage"]["attempts"], 5)
        self.assertEqual(report["zork"]["provider_usage"]["attempts_without_usage"], 1)
        self.assertIn("| Errored trials | 1/1 | 0/1 |", render_markdown(report))

    def test_reads_provider_request_missing_usage_instead_of_completed_step_gap(
        self,
    ) -> None:
        trial = zork_result(
            "task-a",
            1,
            reward=1,
            input_tokens=100,
            provider_requests=3,
        )
        metadata = trial["agent_result"]["metadata"]
        metadata["provider_requests_missing_usage"] = 2
        official = {"rows": [official_result("task-a", 1, passed=True)]}

        report = build_comparison([trial], official, expected_attempts=1)

        self.assertEqual(report["zork"]["provider_usage"]["attempts_without_usage"], 2)

    def test_rejects_a_nonmatching_four_run_subset(self) -> None:
        zork = [zork_result("task-a", 1, reward=1, input_tokens=100)]
        official = {"rows": [official_result("task-a", 1, passed=True)]}

        with self.assertRaisesRegex(ValueError, "expected 4 zork trials"):
            build_comparison(zork, official, expected_attempts=4)

    def test_cli_requires_an_explicit_expected_task_count(self) -> None:
        parser = argparse.ArgumentParser()
        add_arguments(parser)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parser.parse_args(
                [
                    "--job-dir",
                    "jobs",
                    "--official-trials",
                    "official.json",
                    "--output-json",
                    "comparison.json",
                    "--output-markdown",
                    "comparison.md",
                ]
            )


if __name__ == "__main__":
    unittest.main()
