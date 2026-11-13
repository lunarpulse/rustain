"""Cost baseline of a Harbor job run with `RustainAgent`.

    python -m rustain_harbor.baseline <job-dir> [--json <out.json>]

Reads every trial `result.json` under the job directory and prints one row per
trial plus the totals: pass count, cost per task, cost per solved task and the
first-call prompt size.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path


@dataclass(frozen=True)
class Trial:
    task: str
    reward: float | None
    error: str | None
    input_tokens: int | None
    cache_tokens: int | None
    output_tokens: int | None
    cost_usd: float | None
    provider_calls: int | None
    first_call_input_tokens: int | None
    agent_seconds: float | None

    @property
    def passed(self) -> bool:
        return self.reward is not None and self.reward >= 1


def _reward(result: dict) -> float | None:
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    if "reward" in rewards:
        return rewards["reward"]
    if len(rewards) == 1:
        return next(iter(rewards.values()))
    return None


def _seconds(timing: dict | None) -> float | None:
    if not timing or not timing.get("started_at") or not timing.get("finished_at"):
        return None
    started = datetime.fromisoformat(timing["started_at"])
    finished = datetime.fromisoformat(timing["finished_at"])
    return (finished - started).total_seconds()


def read_trial(result: dict) -> Trial:
    agent = result.get("agent_result") or {}
    metadata = agent.get("metadata") or {}
    exception = result.get("exception_info") or {}
    return Trial(
        task=result["task_name"],
        reward=_reward(result),
        error=exception.get("exception_type"),
        input_tokens=agent.get("n_input_tokens"),
        cache_tokens=agent.get("n_cache_tokens"),
        output_tokens=agent.get("n_output_tokens"),
        cost_usd=agent.get("cost_usd"),
        provider_calls=metadata.get("provider_calls"),
        first_call_input_tokens=metadata.get("first_call_input_tokens"),
        agent_seconds=_seconds(result.get("agent_execution")),
    )


def load_trials(job_dir: Path) -> list[Trial]:
    """Every trial result under `job_dir`. The job-level `result.json` has no
    `task_name` and is skipped."""
    trials = []
    for path in sorted(job_dir.rglob("result.json")):
        result = json.loads(path.read_text())
        if isinstance(result, dict) and "task_name" in result:
            trials.append(read_trial(result))
    return trials


def _total(values: list):
    """Sum over the job, or None when a trial lacks the value: a total over
    some of the trials would read as the figure for all of them."""
    return sum(values) if values and None not in values else None


def summarize(trials: list[Trial]) -> dict:
    first_calls = [
        t.first_call_input_tokens
        for t in trials
        if t.first_call_input_tokens is not None
    ]
    passed = sum(t.passed for t in trials)
    total_cost = _total([t.cost_usd for t in trials])
    return {
        "trials": len(trials),
        "passed": passed,
        "trials_with_cost": sum(t.cost_usd is not None for t in trials),
        "total_cost_usd": total_cost,
        "cost_per_trial_usd": total_cost / len(trials)
        if total_cost is not None
        else None,
        # Money spent on every trial, divided by the trials that passed.
        "cost_per_solved_usd": total_cost / passed
        if total_cost is not None and passed
        else None,
        "median_first_call_input_tokens": statistics.median(first_calls)
        if first_calls
        else None,
        "input_tokens": _total([t.input_tokens for t in trials]),
        "cache_tokens": _total([t.cache_tokens for t in trials]),
        "output_tokens": _total([t.output_tokens for t in trials]),
    }


def _cell(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.4f}"
    return str(value)


def render(trials: list[Trial], summary: dict) -> str:
    header = (
        "| task | reward | error | calls | first-call input | input | cached "
        "| output | cost USD | agent s |"
    )
    lines = [header, "|---|---|---|---|---|---|---|---|---|---|"]
    for t in trials:
        lines.append(
            "| "
            + " | ".join(
                _cell(v)
                for v in (
                    t.task,
                    t.reward,
                    t.error,
                    t.provider_calls,
                    t.first_call_input_tokens,
                    t.input_tokens,
                    t.cache_tokens,
                    t.output_tokens,
                    t.cost_usd,
                    t.agent_seconds,
                )
            )
            + " |"
        )
    lines.append("")
    lines += [f"{key}: {_cell(value)}" for key, value in summary.items()]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("job_dir", type=Path)
    parser.add_argument("--json", type=Path, help="also write the numbers as JSON")
    args = parser.parse_args(argv)

    trials = load_trials(args.job_dir)
    if not trials:
        print(f"no trial results under {args.job_dir}", file=sys.stderr)
        return 1
    summary = summarize(trials)
    print(render(trials, summary))
    if args.json:
        args.json.write_text(
            json.dumps(
                {"summary": summary, "trials": [asdict(t) for t in trials]}, indent=2
            )
            + "\n"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
