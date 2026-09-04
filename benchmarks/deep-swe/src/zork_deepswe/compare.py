from __future__ import annotations

import argparse
import json
import math
import statistics
from collections import Counter
from collections.abc import Iterable
from pathlib import Path
from typing import Any

OFFICIAL_CONFIG = "mini_swe_agent_muse_spark_1_2_xhigh"
ZORK_MODEL = "muse-spark-1.2-contributor"
ZORK_PROVIDER = "opencode-go"


def load_zork_trials(job_dirs: Path | Iterable[Path]) -> list[dict[str, Any]]:
    roots = [job_dirs] if isinstance(job_dirs, Path) else list(job_dirs)
    if not roots:
        raise ValueError("at least one zork job directory is required")
    trials: list[dict[str, Any]] = []
    origins: dict[tuple[str, str], Path] = {}
    for job_dir in roots:
        if not job_dir.is_dir():
            raise ValueError(f"zork job directory does not exist: {job_dir}")
        for result_path in sorted(job_dir.rglob("result.json")):
            document = json.loads(result_path.read_text())
            if not isinstance(document, dict):
                raise ValueError(f"trial result is not an object: {result_path}")
            if "task_name" not in document and "trial_name" not in document:
                continue
            task_name = _canonical_task_name(
                _required_string(document, "task_name", f"trial {result_path}")
            )
            trial_name = _required_string(
                document, "trial_name", f"trial {result_path}"
            )
            key = (task_name, trial_name)
            if previous := origins.get(key):
                raise ValueError(
                    f"duplicate zork trial {task_name}/{trial_name}: "
                    f"{previous} and {result_path}"
                )
            origins[key] = result_path
            trials.append(document)
    return trials


def build_comparison(
    zork_documents: list[dict[str, Any]],
    official_document: dict[str, Any],
    *,
    expected_attempts: int = 4,
    expected_task_count: int | None = None,
) -> dict[str, Any]:
    if expected_attempts <= 0:
        raise ValueError("expected_attempts must be positive")
    if expected_task_count is not None and expected_task_count <= 0:
        raise ValueError("expected_task_count must be positive")
    zork = [_normalize_zork_trial(document) for document in zork_documents]
    _require_unique_trials(zork, "zork")
    tasks = sorted({trial["task_name"] for trial in zork})
    if expected_task_count is not None and len(tasks) != expected_task_count:
        raise ValueError(f"expected {expected_task_count} tasks, found {len(tasks)}")
    _require_attempts(zork, tasks, expected_attempts, "zork")

    raw_official = official_document.get("rows")
    if not isinstance(raw_official, list):
        raise TypeError("official artifact does not contain a rows array")
    official_candidates = [
        _normalize_official_trial(document)
        for document in raw_official
        if isinstance(document, dict)
        and document.get("config") == OFFICIAL_CONFIG
        and document.get("included_in_score", True) is True
    ]
    official = [trial for trial in official_candidates if trial["task_name"] in tasks]
    _require_unique_trials(official, "official")
    _require_attempts(official, tasks, expected_attempts, "official")

    zork_summary = summarize_trials(zork)
    official_summary = summarize_trials(official)
    success_delta = zork_summary["success_rate"] - official_summary["success_rate"]
    p_value = fisher_exact_two_sided(
        zork_summary["passes"],
        zork_summary["trials"] - zork_summary["passes"],
        official_summary["passes"],
        official_summary["trials"] - official_summary["passes"],
    )

    token_ratios: dict[str, float | None] = {}
    for name in ("input", "output", "total"):
        zork_mean = zork_summary["tokens"][name]["mean"]
        official_mean = official_summary["tokens"][name]["mean"]
        token_ratios[name] = (
            zork_mean / official_mean
            if zork_mean is not None and official_mean not in (None, 0)
            else None
        )

    return {
        "scope": {
            "task_count": len(tasks),
            "attempts_per_task": expected_attempts,
            "trials_per_side": len(tasks) * expected_attempts,
            "official_config": OFFICIAL_CONFIG,
            "zork_model": ZORK_MODEL,
            "zork_provider": ZORK_PROVIDER,
        },
        "tasks": tasks,
        "zork": zork_summary,
        "official": official_summary,
        "comparison": {
            "success_rate_delta": success_delta,
            "success_rate_delta_percentage_points": success_delta * 100,
            "success_fisher_exact_two_sided_p": p_value,
            "success_degradation_observed": success_delta < 0,
            "success_degradation_detected_at_0_05": success_delta < 0
            and p_value < 0.05,
            "mean_token_ratio_zork_over_official": token_ratios,
        },
        "by_task": _compare_by_task(tasks, zork, official),
    }


def summarize_trials(trials: list[dict[str, Any]]) -> dict[str, Any]:
    passes = sum(bool(trial["passed"]) for trial in trials)
    total = len(trials)
    return {
        "trials": total,
        "passes": passes,
        "errors": sum(bool(trial["errored"]) for trial in trials),
        "success_rate": passes / total if total else None,
        "success_rate_wilson_95": list(wilson_interval(passes, total)),
        "tokens": {
            name: _numeric_summary([trial[f"{name}_tokens"] for trial in trials])
            for name in ("input", "output", "total")
        },
        "agent_steps": _numeric_summary([trial["agent_steps"] for trial in trials]),
        "provider_usage": {
            "attempts": _sum_optional([trial["provider_requests"] for trial in trials]),
            "attempts_without_usage": _sum_optional(
                [trial["provider_attempts_without_usage"] for trial in trials]
            ),
            "trials_with_attempt_counts": sum(
                trial["provider_requests"] is not None for trial in trials
            ),
            "trials_with_missing_usage_counts": sum(
                trial["provider_attempts_without_usage"] is not None for trial in trials
            ),
        },
    }


def wilson_interval(successes: int, total: int) -> tuple[float | None, float | None]:
    if total == 0:
        return None, None
    z = 1.959963984540054
    proportion = successes / total
    denominator = 1 + z * z / total
    center = (proportion + z * z / (2 * total)) / denominator
    radius = (
        z
        * math.sqrt(proportion * (1 - proportion) / total + z * z / (4 * total * total))
        / denominator
    )
    return center - radius, center + radius


def fisher_exact_two_sided(a: int, b: int, c: int, d: int) -> float:
    row_one = a + b
    row_two = c + d
    successes = a + c
    total = row_one + row_two

    def probability(value: int) -> float:
        return (
            math.comb(successes, value)
            * math.comb(total - successes, row_one - value)
            / math.comb(total, row_one)
        )

    minimum = max(0, row_one - (total - successes))
    maximum = min(row_one, successes)
    observed = probability(a)
    return min(
        1.0,
        sum(
            probability(value)
            for value in range(minimum, maximum + 1)
            if probability(value) <= observed + 1e-15
        ),
    )


def render_markdown(report: dict[str, Any]) -> str:
    zork = report["zork"]
    official = report["official"]
    comparison = report["comparison"]
    lines = [
        f"# DeepSWE {report['scope']['task_count']}-task comparison",
        "",
        f"- Tasks: {report['scope']['task_count']}",
        f"- Attempts per task: {report['scope']['attempts_per_task']}",
        f"- Trials per side: {report['scope']['trials_per_side']}",
        "",
        "| Metric | zork-agent | Official mini-swe-agent | Delta / ratio |",
        "| --- | ---: | ---: | ---: |",
        (
            f"| Success | {zork['passes']}/{zork['trials']} ({_percent(zork['success_rate'])}) "
            f"| {official['passes']}/{official['trials']} ({_percent(official['success_rate'])}) "
            f"| {comparison['success_rate_delta_percentage_points']:+.1f} pp |"
        ),
        (
            f"| Errored trials | {zork['errors']}/{zork['trials']} "
            f"| {official['errors']}/{official['trials']} "
            f"| {zork['errors'] - official['errors']:+d} |"
        ),
    ]
    for metric in ("input", "output", "total"):
        zork_tokens = zork["tokens"][metric]
        official_tokens = official["tokens"][metric]
        ratio = comparison["mean_token_ratio_zork_over_official"][metric]
        lines.append(
            f"| Mean {metric} tokens | {_number(zork_tokens['mean'])} "
            f"| {_number(official_tokens['mean'])} | {_ratio(ratio)} |"
        )
        if zork_tokens["missing"] or official_tokens["missing"]:
            lines.append(
                f"| {metric} token coverage | {zork_tokens['available']}/{zork['trials']} "
                f"| {official_tokens['available']}/{official['trials']} | — |"
            )
    lines.extend(
        [
            (
                f"| Mean agent steps | {_number(zork['agent_steps']['mean'])} "
                f"| {_number(official['agent_steps']['mean'])} | — |"
            ),
            "",
            (
                "Two-sided Fisher exact p-value for success: "
                f"{comparison['success_fisher_exact_two_sided_p']:.6g}."
            ),
            (
                "Zork provider attempts: "
                f"{_integer_or_dash(zork['provider_usage']['attempts'])}; "
                "attempts without terminal usage: "
                f"{_integer_or_dash(zork['provider_usage']['attempts_without_usage'])}."
            ),
            (
                "Token statistics include known partial usage from errored trials; "
                "provider attempts without terminal usage are not estimated."
            ),
            "",
            "## Per task",
            "",
            "| Task | zork | Official |",
            "| --- | ---: | ---: |",
        ]
    )
    for row in report["by_task"]:
        lines.append(
            f"| {row['task_name']} | {row['zork_passes']}/{row['trials_per_side']} "
            f"| {row['official_passes']}/{row['trials_per_side']} |"
        )
    return "\n".join(lines) + "\n"


def _normalize_zork_trial(document: dict[str, Any]) -> dict[str, Any]:
    task_name = _canonical_task_name(
        _required_string(document, "task_name", "zork trial")
    )
    trial_name = _required_string(document, "trial_name", "zork trial")
    agent_info = document.get("agent_info")
    if not isinstance(agent_info, dict) or agent_info.get("name") != "zork-agent":
        raise ValueError(f"trial {trial_name} was not produced by zork-agent")
    model_info = agent_info.get("model_info")
    if not isinstance(model_info, dict):
        raise TypeError(f"trial {trial_name} has no model info")
    if (
        model_info.get("name") != ZORK_MODEL
        or model_info.get("provider") != ZORK_PROVIDER
    ):
        raise ValueError(f"trial {trial_name} used a different model selection")

    context = document.get("agent_result")
    context = context if isinstance(context, dict) else {}
    input_tokens = _optional_number(context.get("n_input_tokens"))
    output_tokens = _optional_number(context.get("n_output_tokens"))
    agent_steps = _optional_number(context.get("n_agent_steps"))
    metadata = context.get("metadata")
    metadata = metadata if isinstance(metadata, dict) else {}
    provider_requests = _optional_count(metadata.get("provider_requests"))
    provider_requests_missing_usage = _optional_count(
        metadata.get("provider_requests_missing_usage")
    )
    if (
        provider_requests is not None
        and agent_steps is not None
        and provider_requests < agent_steps
    ):
        raise ValueError(
            f"trial {trial_name} has fewer provider requests than completed steps"
        )
    rewards = document.get("verifier_result")
    rewards = rewards.get("rewards") if isinstance(rewards, dict) else None
    reward = rewards.get("reward") if isinstance(rewards, dict) else None
    return {
        "task_name": task_name,
        "trial_name": trial_name,
        "passed": reward == 1,
        "errored": document.get("exception_info") is not None,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": (
            input_tokens + output_tokens
            if input_tokens is not None and output_tokens is not None
            else None
        ),
        "agent_steps": agent_steps,
        "provider_requests": provider_requests,
        "provider_attempts_without_usage": provider_requests_missing_usage,
    }


def _normalize_official_trial(document: dict[str, Any]) -> dict[str, Any]:
    task_name = _canonical_task_name(
        _required_string(document, "task_name", "official trial")
    )
    trial_name = _required_string(document, "trial_name", "official trial")
    input_tokens = _optional_number(document.get("n_input_tokens"))
    output_tokens = _optional_number(document.get("n_output_tokens"))
    return {
        "task_name": task_name,
        "trial_name": trial_name,
        "passed": document.get("passed") is True,
        "errored": document.get("errored") is True,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": (
            input_tokens + output_tokens
            if input_tokens is not None and output_tokens is not None
            else None
        ),
        "agent_steps": _optional_number(document.get("n_agent_steps")),
        "provider_requests": None,
        "provider_attempts_without_usage": None,
    }


def _require_attempts(
    trials: list[dict[str, Any]], tasks: list[str], expected_attempts: int, label: str
) -> None:
    counts = Counter(trial["task_name"] for trial in trials)
    expected = set(tasks)
    actual = set(counts)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise ValueError(f"{label} task mismatch: missing={missing}, extra={extra}")
    for task in tasks:
        if counts[task] != expected_attempts:
            raise ValueError(
                f"expected {expected_attempts} {label} trials for {task}, found {counts[task]}"
            )


def _require_unique_trials(trials: list[dict[str, Any]], label: str) -> None:
    seen: set[tuple[str, str]] = set()
    for trial in trials:
        key = (trial["task_name"], trial["trial_name"])
        if key in seen:
            raise ValueError(f"duplicate {label} trial {key[0]}/{key[1]}")
        seen.add(key)


def _numeric_summary(values: list[int | float | None]) -> dict[str, Any]:
    available = [value for value in values if value is not None]
    return {
        "available": len(available),
        "missing": len(values) - len(available),
        "sum": sum(available) if available else None,
        "mean": statistics.fmean(available) if available else None,
        "median": statistics.median(available) if available else None,
        "min": min(available) if available else None,
        "max": max(available) if available else None,
    }


def _compare_by_task(
    tasks: list[str], zork: list[dict[str, Any]], official: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for task in tasks:
        zork_trials = [trial for trial in zork if trial["task_name"] == task]
        official_trials = [trial for trial in official if trial["task_name"] == task]
        rows.append(
            {
                "task_name": task,
                "trials_per_side": len(zork_trials),
                "zork_passes": sum(bool(trial["passed"]) for trial in zork_trials),
                "official_passes": sum(
                    bool(trial["passed"]) for trial in official_trials
                ),
            }
        )
    return rows


def _required_string(document: dict[str, Any], key: str, label: str) -> str:
    value = document.get(key)
    if not isinstance(value, str) or not value:
        raise ValueError(f"{label} has no {key}")
    return value


def _optional_number(value: Any) -> int | float | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError(f"expected a number or null, got {value!r}")
    return value


def _optional_count(value: Any) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise TypeError(f"expected a non-negative integer or null, got {value!r}")
    return value


def _canonical_task_name(value: str) -> str:
    task_name = value.removeprefix("datacurve/")
    if not task_name or "/" in task_name:
        raise ValueError(f"invalid DeepSWE task name: {value!r}")
    return task_name


def _sum_optional(values: list[int | float | None]) -> int | float | None:
    available = [value for value in values if value is not None]
    return sum(available) if available else None


def _percent(value: float | None) -> str:
    return "—" if value is None else f"{value * 100:.1f}%"


def _number(value: float | None) -> str:
    return "—" if value is None else f"{value:,.1f}"


def _ratio(value: float | None) -> str:
    return "—" if value is None else f"{value:.3f}×"


def _integer_or_dash(value: float | None) -> str:
    return "—" if value is None else f"{value:,.0f}"


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--job-dir", type=Path, nargs="+", required=True)
    parser.add_argument("--expected-task-count", type=int, required=True)
    parser.add_argument("--official-trials", type=Path, required=True)
    parser.add_argument("--output-json", type=Path, required=True)
    parser.add_argument("--output-markdown", type=Path, required=True)


def execute(args: argparse.Namespace) -> None:
    official = json.loads(args.official_trials.read_text())
    if not isinstance(official, dict):
        raise TypeError("official trial artifact must be an object")
    report = build_comparison(
        load_zork_trials(args.job_dir),
        official,
        expected_attempts=4,
        expected_task_count=args.expected_task_count,
    )
    args.output_json.parent.mkdir(parents=True, exist_ok=True)
    args.output_markdown.parent.mkdir(parents=True, exist_ok=True)
    args.output_json.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    args.output_markdown.write_text(render_markdown(report))


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser()
    add_arguments(parser)
    execute(parser.parse_args(argv))


if __name__ == "__main__":
    main()
