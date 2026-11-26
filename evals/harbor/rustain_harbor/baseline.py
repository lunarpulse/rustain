"""Eval-4 aggregation: reconcile a run against its header, then report.

    python -m rustain_harbor.baseline <jobs-dir> <run-id>

Reads `<jobs-dir>/<run-id>/header.json`, checks its hash, and binds every
expected slot to exactly one Harbor trial result in the slot's own job. It
refuses (exit 1, no totals, no `results.json`) when the header is absent or
altered, when any slot is missing, duplicated, unexpected or unidentifiable,
or when the ledger's recorded argv is not the header's argv for that slot.
Otherwise it prints the per-slot outcomes and the per-treatment summary, and
writes them to `<jobs-dir>/<run-id>/results.json`.

Outcome, completeness and cost are separate. A present result has one
outcome: `limit_stop`, `harbor_timeout`, `harbor_exception`,
`verifier_absent`, `pass` or `fail`; a reward recorded with an exception is
never a pass. A slot without a result is missing, never a fail. Cost is
`known`, `usage_unknown` or `unpriced`; unknown is never zero, and a cost
total exists only when every slot's cost is known.

Standard library only: aggregation needs no Harbor install.
"""

from __future__ import annotations

import hashlib
import json
import statistics
import sys
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path

from rustain_harbor import runplan

# Harbor 0.25.0 `harbor/trial/errors.py`: every timeout the trial raises.
TIMEOUT_EXCEPTIONS = frozenset(
    {
        "AgentTimeoutError",
        "AgentSetupTimeoutError",
        "VerifierTimeoutError",
        "EnvironmentStartTimeoutError",
    }
)
RESULTS = "results.json"


@dataclass(frozen=True)
class Trial:
    task_name: str
    task_ref: str | None
    # The `--model` Harbor ran the agent with, and the model the agent reported.
    model: str | None
    reported_model: str | None
    reward: float | None
    error: str | None
    limit_reason: str | None
    unpriced_models: tuple[str, ...]
    input_tokens: int | None
    cache_tokens: int | None
    output_tokens: int | None
    cost_usd: float | None
    provider_calls: int | None
    first_call_input_tokens: int | None
    agent_seconds: float | None

    @property
    def outcome(self) -> str:
        # rustain exits 1 on a bounded stop, which Harbor records as an
        # exception; the stream's limit reason says what it was.
        if self.limit_reason:
            return "limit_stop"
        if self.error in TIMEOUT_EXCEPTIONS:
            return "harbor_timeout"
        if self.error:
            return "harbor_exception"
        if self.reward is None:
            return "verifier_absent"
        return "pass" if self.reward >= 1 else "fail"

    @property
    def exception_with_reward(self) -> bool:
        return self.error is not None and self.reward is not None

    @property
    def cost_status(self) -> str:
        if self.cost_usd is not None:
            return "known"
        return "unpriced" if self.unpriced_models else "usage_unknown"


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
    """A trial `result.json` (Harbor 0.25.0 `TrialResult`)."""
    agent = result.get("agent_result") or {}
    metadata = agent.get("metadata") or {}
    exception = result.get("exception_info") or {}
    model_info = (result.get("agent_info") or {}).get("model_info") or {}
    reported = model_info.get("name")
    if reported and model_info.get("provider"):
        reported = f"{model_info['provider']}/{reported}"
    task_id = result.get("task_id") or {}
    return Trial(
        task_name=result["task_name"],
        task_ref=task_id.get("ref") if isinstance(task_id, dict) else None,
        model=((result.get("config") or {}).get("agent") or {}).get("model_name"),
        reported_model=reported,
        reward=_reward(result),
        error=exception.get("exception_type"),
        limit_reason=metadata.get("limit_reason"),
        unpriced_models=tuple(metadata.get("unpriced_models") or ()),
        input_tokens=agent.get("n_input_tokens"),
        cache_tokens=agent.get("n_cache_tokens"),
        output_tokens=agent.get("n_output_tokens"),
        cost_usd=agent.get("cost_usd"),
        provider_calls=metadata.get("provider_calls"),
        first_call_input_tokens=metadata.get("first_call_input_tokens"),
        agent_seconds=_seconds(result.get("agent_execution")),
    )


def trial_results(job_dir: Path) -> list[Path]:
    """Trial results of one job. Harbor writes the job's own `result.json`
    next to the trial directories, not inside one."""
    if not job_dir.is_dir():
        return []
    return [
        child / "result.json"
        for child in sorted(job_dir.iterdir())
        if child.is_dir() and (child / "result.json").is_file()
    ]


def _load_trial(path: Path) -> Trial | None:
    try:
        result = json.loads(path.read_text())
        return read_trial(result) if isinstance(result, dict) else None
    except (OSError, ValueError, KeyError, TypeError):
        return None


def read_slot_cost(job_dir: Path) -> tuple[str, float | None]:
    """What the spend guard knows about a finished slot's cost."""
    results = trial_results(job_dir)
    if len(results) != 1:
        return ("missing" if not results else "duplicate"), None
    trial = _load_trial(results[0])
    if trial is None:
        return "unidentifiable", None
    return trial.cost_status, trial.cost_usd


def _ledger(path: Path, digest: str, problems: list[str]) -> list[dict]:
    if not path.is_file():
        problems.append("header: ledger.jsonl is absent")
        return []
    events = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        try:
            event = json.loads(line)
        except ValueError:
            problems.append(f"unidentifiable: ledger line {number} is not JSON")
            continue
        if event.get("header_sha256") != digest:
            problems.append(f"header: ledger line {number} belongs to another header")
        events.append(event)
    return events


def reconcile(jobs_dir: Path, run_id: str) -> tuple[dict | None, list[dict], list[str]]:
    """The header, one row per expected slot, and every reason to refuse."""
    target = runplan.run_dir(jobs_dir, run_id)
    header_path = target / runplan.HEADER
    if not header_path.is_file():
        return None, [], [f"header: {header_path} is absent"]
    content = header_path.read_bytes()
    digest = hashlib.sha256(content).hexdigest()
    problems: list[str] = []
    recorded = target / runplan.HEADER_HASH
    if not recorded.is_file() or recorded.read_text().split()[:1] != [digest]:
        problems.append("header: header.json does not match header.sha256")
    try:
        header = json.loads(content)
    except ValueError:
        return None, [], [*problems, "header: header.json is not JSON"]
    if header.get("schema") != runplan.HEADER_SCHEMA or header.get("run_id") != run_id:
        return header, [], [*problems, "header: not an eval-4 run header for this run id"]

    slots = {slot.job_name: slot for slot in runplan.header_slots(header)}
    started: dict[str, dict] = {}
    finished: dict[str, dict] = {}
    for event in _ledger(target / runplan.LEDGER, digest, problems):
        name = event.get("job_name")
        if event.get("event") not in ("slot_started", "slot_finished"):
            continue
        slot = slots.get(name)
        if slot is None:
            problems.append(f"unexpected: ledger names job {name!r}, not in the header")
            continue
        seen = started if event["event"] == "slot_started" else finished
        if name in seen:
            problems.append(f"duplicate: {slot.key} was {event['event'][5:]} twice")
        seen[name] = event
        if event["event"] == "slot_started":
            max_cost = event.get("max_cost_usd")
            ceiling = header["spend"]["per_slot_max_cost_usd"]
            if not isinstance(max_cost, float | int) or not 0 < max_cost <= ceiling:
                problems.append(f"argv: {slot.key} ran with --max-cost {max_cost!r}")
            elif event.get("argv") != runplan.slot_argv(header, slot, max_cost):
                problems.append(f"argv: {slot.key} recorded an argv the header does not give")

    jobs_root = target / runplan.JOBS
    if jobs_root.is_dir():
        for entry in sorted(jobs_root.iterdir()):
            if entry.name not in slots:
                problems.append(f"unexpected: {entry.name} is not a slot of this run")
            elif entry.name not in started:
                problems.append(f"unidentifiable: {entry.name} is outside the ledger")

    rows = []
    for slot in slots.values():
        results = trial_results(jobs_root / slot.job_name)
        if not results:
            problems.append(f"missing: {slot.key}")
            continue
        if len(results) > 1:
            problems.append(f"duplicate: {slot.key} has {len(results)} trial results")
            continue
        trial = _load_trial(results[0])
        if trial is None:
            problems.append(f"unidentifiable: {slot.key}: {results[0]} cannot be read")
            continue
        expected_ref = slot.task_ref.rpartition("@")[2]
        mismatches = [
            f"task {trial.task_name}" if trial.task_name != slot.task_name else None,
            f"package {trial.task_ref}"
            if trial.task_ref is not None and trial.task_ref != expected_ref
            else None,
            f"model {trial.model}" if trial.model != slot.model else None,
            f"reported model {trial.reported_model}"
            if trial.reported_model is not None and trial.reported_model != slot.model
            else None,
        ]
        mismatches = [m for m in mismatches if m]
        if mismatches:
            problems.append(f"unexpected: {slot.key} holds {', '.join(mismatches)}")
            continue
        ledger_started = started.get(slot.job_name, {})
        ledger_finished = finished.get(slot.job_name, {})
        rows.append(
            {
                "slot": slot.key,
                "task": slot.task,
                "treatment": slot.treatment,
                "attempt": slot.attempt,
                "outcome": trial.outcome,
                "exception": trial.error,
                "exception_with_reward": trial.exception_with_reward,
                "limit_reason": trial.limit_reason,
                "cost_status": trial.cost_status,
                "max_cost_usd": ledger_started.get("max_cost_usd"),
                "harbor_exit": ledger_finished.get("harbor_exit"),
                **{
                    key: value
                    for key, value in asdict(trial).items()
                    if key not in ("task_name", "task_ref", "error", "limit_reason")
                },
            }
        )
    return header, rows, problems


def _total(values: list):
    """A sum, or None when any value is unknown: a partial sum would read as
    the figure for every slot."""
    return sum(values) if values and None not in values else None


def summarize(rows: list[dict]) -> dict:
    outcomes = {
        name: sum(row["outcome"] == name for row in rows)
        for name in (
            "pass",
            "fail",
            "harbor_timeout",
            "harbor_exception",
            "limit_stop",
            "verifier_absent",
        )
    }
    passed = outcomes["pass"]
    total_cost = _total([row["cost_usd"] for row in rows])
    first_calls = [
        row["first_call_input_tokens"]
        for row in rows
        if row["first_call_input_tokens"] is not None
    ]
    return {
        # Every expected slot is in the denominator: reconciliation refused
        # the run if one was missing.
        "slots": len(rows),
        "outcomes": outcomes,
        "exception_with_reward": sum(row["exception_with_reward"] for row in rows),
        "pass_rate": passed / len(rows) if rows else None,
        "cost_status": {
            name: sum(row["cost_status"] == name for row in rows)
            for name in ("known", "usage_unknown", "unpriced")
        },
        "total_cost_usd": total_cost,
        "cost_per_solved_usd": total_cost / passed if total_cost is not None and passed else None,
        "median_first_call_input_tokens": statistics.median(first_calls)
        if first_calls
        else None,
        "provider_calls": _total([row["provider_calls"] for row in rows]),
        "input_tokens": _total([row["input_tokens"] for row in rows]),
        "cache_tokens": _total([row["cache_tokens"] for row in rows]),
        "output_tokens": _total([row["output_tokens"] for row in rows]),
    }


def _cell(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.4f}"
    return str(value)


def render(rows: list[dict], report: dict) -> str:
    columns = (
        "slot",
        "outcome",
        "exception",
        "reward",
        "limit_reason",
        "cost_status",
        "cost_usd",
        "max_cost_usd",
        "provider_calls",
        "first_call_input_tokens",
        "agent_seconds",
    )
    lines = ["| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
    lines += ["| " + " | ".join(_cell(row[c]) for c in columns) + " |" for row in rows]
    for label, summary in report["by_treatment"].items():
        lines += ["", f"## {label} ({report['treatments'][label]})"]
        lines += [f"{key}: {_cell(value)}" for key, value in summary.items()]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 2:
        print("usage: python -m rustain_harbor.baseline <jobs-dir> <run-id>", file=sys.stderr)
        return 2
    jobs_dir, run_id = Path(args[0]).resolve(), args[1]
    output = runplan.run_dir(jobs_dir, run_id) / RESULTS
    header, rows, problems = reconcile(jobs_dir, run_id)
    if problems:
        output.unlink(missing_ok=True)
        print(f"aggregate: refused, {len(problems)} problem(s); no totals:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    treatments = {t["label"]: t["model"] for t in header["treatments"]}
    report = {
        "run_id": run_id,
        "header_sha256": hashlib.sha256(
            (runplan.run_dir(jobs_dir, run_id) / runplan.HEADER).read_bytes()
        ).hexdigest(),
        "comparison_set": header["manifest"],
        "product": header["product"],
        "prices": header["prices"],
        "limits": header["limits"],
        "spend": header["spend"],
        "sandbox": header["sandbox"],
        "task_timeouts_sec": header["task_timeouts_sec"],
        "treatment_kind": header["treatment_kind"],
        "treatments": treatments,
        "by_treatment": {
            label: summarize([row for row in rows if row["treatment"] == label])
            for label in treatments
        },
        "all": summarize(rows),
        "slots": rows,
    }
    print(render(rows, report))
    output.write_text(json.dumps(report, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
