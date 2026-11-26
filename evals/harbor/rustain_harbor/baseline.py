"""Eval-4 aggregation: reconcile a run against its header, then report.

    python -m rustain_harbor.baseline <jobs-dir> <run-id>

Reads `<jobs-dir>/<run-id>/header.json`, checks its hash, and binds every
expected slot to exactly one Harbor trial result in the slot's own job. It
refuses (exit 1, no totals, no `results.json`) when the header is absent or
altered, when any slot is missing, duplicated, unexpected or unidentifiable
(including a result without the slot's package digest, a malformed result or
ledger event, or a task image other than the pinned one), or when the
ledger's recorded argv is not the header's argv for that slot. Any earlier
`results.json` is removed first, so a refusal never leaves one behind.
Otherwise it prints the publication labels, the per-slot outcomes and the
per-treatment summary, and writes them atomically to
`<jobs-dir>/<run-id>/results.json`.

Outcome, completeness and cost are separate. A present result has one
outcome: `limit_stop`, `harbor_timeout`, `harbor_exception`,
`verifier_absent`, `pass` or `fail`; a reward recorded with an exception is
never a pass. A slot without a result is missing, never a fail. Cost is
`known` (complete and priced), `incomplete` (the stream shows only some of
the calls paid for), `usage_unknown` or `unpriced`; unknown is never zero,
and a cost total exists only when every slot's cost is known.

Standard library only: aggregation needs no Harbor install.
"""

from __future__ import annotations

import hashlib
import json
import os
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
# Limit reasons whose stop leaves every paid call in the stream: rustain
# stops at a settled call boundary. Mirrors `usage.RunSummary`.
COMPLETE_LIMITS = frozenset({"max_cost", "max_turns", "daily_budget"})


class ShapeError(ValueError):
    """A result or ledger record does not have the shape this run needs."""


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
    # The adapter's verdict: every call the turn paid for is in `cost_usd`.
    cost_complete: bool
    input_tokens: int | None
    cache_tokens: int | None
    output_tokens: int | None
    cost_usd: float | None
    # What the calls the stream shows cost; a lower bound when incomplete.
    observed_cost_usd: float | None
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
        """`known` only for a complete, priced cost. A partial sum of the
        calls a stream shows is not the slot's cost."""
        complete = self.cost_complete and (
            self.limit_reason is None or self.limit_reason in COMPLETE_LIMITS
        )
        if self.cost_usd is not None and complete:
            return "known"
        if self.unpriced_models:
            return "unpriced"
        if self.cost_usd is not None or self.observed_cost_usd is not None:
            return "incomplete"
        return "usage_unknown"


def _object(value, what: str, *, optional: bool = True) -> dict:
    if value is None and optional:
        return {}
    if not isinstance(value, dict):
        raise ShapeError(f"{what} is not an object")
    return value


def _number(value, what: str) -> float | int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int | float):
        raise ShapeError(f"{what} is not a number")
    return value


def _text(value, what: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str):
        raise ShapeError(f"{what} is not a string")
    return value


def _reward(result: dict) -> float | None:
    verifier = _object(result.get("verifier_result"), "verifier_result")
    rewards = _object(verifier.get("rewards"), "verifier_result.rewards")
    if "reward" in rewards:
        return _number(rewards["reward"], "reward")
    if len(rewards) == 1:
        return _number(next(iter(rewards.values())), "reward")
    return None


def _seconds(timing) -> float | None:
    timing = _object(timing, "agent_execution")
    started = _text(timing.get("started_at"), "agent_execution.started_at")
    finished = _text(timing.get("finished_at"), "agent_execution.finished_at")
    if not started or not finished:
        return None
    return (datetime.fromisoformat(finished) - datetime.fromisoformat(started)).total_seconds()


def read_trial(result) -> Trial:
    """A trial `result.json` (Harbor 0.25.0 `TrialResult`). Raises
    `ShapeError` for anything this run cannot trust."""
    result = _object(result, "result", optional=False)
    agent = _object(result.get("agent_result"), "agent_result")
    metadata = _object(agent.get("metadata"), "agent_result.metadata")
    exception = _object(result.get("exception_info"), "exception_info")
    agent_info = _object(result.get("agent_info"), "agent_info")
    model_info = _object(agent_info.get("model_info"), "agent_info.model_info")
    reported = _text(model_info.get("name"), "agent_info.model_info.name")
    provider = _text(model_info.get("provider"), "agent_info.model_info.provider")
    if reported and provider:
        reported = f"{provider}/{reported}"
    task_id = _object(result.get("task_id"), "task_id")
    config_agent = _object(_object(result.get("config"), "config").get("agent"), "config.agent")
    unpriced = metadata.get("unpriced_models") or []
    if not isinstance(unpriced, list) or not all(isinstance(m, str) for m in unpriced):
        raise ShapeError("unpriced_models is not a list of strings")
    task_name = _text(result.get("task_name"), "task_name")
    if not task_name:
        raise ShapeError("task_name is absent")
    return Trial(
        task_name=task_name,
        task_ref=_text(task_id.get("ref"), "task_id.ref"),
        model=_text(config_agent.get("model_name"), "config.agent.model_name"),
        reported_model=reported,
        reward=_reward(result),
        error=_text(exception.get("exception_type"), "exception_type"),
        limit_reason=_text(metadata.get("limit_reason"), "limit_reason"),
        unpriced_models=tuple(unpriced),
        cost_complete=metadata.get("cost_complete") is True,
        input_tokens=_number(agent.get("n_input_tokens"), "n_input_tokens"),
        cache_tokens=_number(agent.get("n_cache_tokens"), "n_cache_tokens"),
        output_tokens=_number(agent.get("n_output_tokens"), "n_output_tokens"),
        cost_usd=_number(agent.get("cost_usd"), "cost_usd"),
        observed_cost_usd=_number(metadata.get("observed_cost_usd"), "observed_cost_usd"),
        provider_calls=_number(metadata.get("provider_calls"), "provider_calls"),
        first_call_input_tokens=_number(
            metadata.get("first_call_input_tokens"), "first_call_input_tokens"
        ),
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


def _load_trial(path: Path) -> Trial | str:
    """The trial, or why it cannot be read."""
    try:
        return read_trial(json.loads(path.read_text()))
    except (OSError, ValueError) as error:
        return str(error) or type(error).__name__


def read_slot_cost(job_dir: Path) -> tuple[str, float | None]:
    """What the spend guard knows about a finished slot's cost."""
    results = trial_results(job_dir)
    if len(results) != 1:
        return ("missing" if not results else "duplicate"), None
    trial = _load_trial(results[0])
    if isinstance(trial, str):
        return "unidentifiable", None
    status = trial.cost_status
    return status, trial.cost_usd if status == "known" else None


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
        if not isinstance(event, dict) or not isinstance(event.get("event"), str):
            problems.append(f"unidentifiable: ledger line {number} is not a ledger event")
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
    if (
        not isinstance(header, dict)
        or header.get("schema") != runplan.HEADER_SCHEMA
        or header.get("run_id") != run_id
    ):
        return None, [], [*problems, "header: not an eval-4 run header for this run id"]

    slots = {slot.job_name: slot for slot in runplan.header_slots(header)}
    started: dict[str, dict] = {}
    finished: dict[str, dict] = {}
    pinned: dict[str, str] | None = None
    for event in _ledger(target / runplan.LEDGER, digest, problems):
        if event["event"] == "images_pinned":
            if pinned is not None or not isinstance(event.get("image_ids"), dict):
                problems.append("unidentifiable: the ledger pins task images twice or badly")
            else:
                pinned = event["image_ids"]
            continue
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
        if isinstance(trial, str):
            problems.append(f"unidentifiable: {slot.key}: {results[0]}: {trial}")
            continue
        if trial.task_ref is None:
            # The frozen package revision is this run's contract; a result
            # that does not name it is not evidence of that revision.
            problems.append(f"unidentifiable: {slot.key} names no package digest")
            continue
        expected_ref = slot.task_ref.rpartition("@")[2]
        mismatches = [
            f"task {trial.task_name}" if trial.task_name != slot.task_name else None,
            f"package {trial.task_ref}" if trial.task_ref != expected_ref else None,
            f"model {trial.model}" if trial.model != slot.model else None,
            f"reported model {trial.reported_model}"
            if trial.reported_model is not None and trial.reported_model != slot.model
            else None,
        ]
        ledger_started = started.get(slot.job_name, {})
        ledger_finished = finished.get(slot.job_name, {})
        expected_image = (pinned or {}).get(slot.task)
        if expected_image is None:
            mismatches.append("no pinned task image in the ledger")
        else:
            for when, event, field in (
                ("before", ledger_started, "image_id_before"),
                ("after", ledger_finished, "image_id_after"),
            ):
                if event.get(field) != expected_image:
                    mismatches.append(f"image {event.get(field)!r} {when} the job")
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
    known = [row["cost_usd"] if row["cost_status"] == "known" else None for row in rows]
    total_cost = _total(known)
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
            for name in ("known", "incomplete", "usage_unknown", "unpriced")
        },
        # Only when every slot's cost is known and complete.
        "total_cost_usd": total_cost,
        # Known costs plus what incomplete slots' streams show: a lower bound,
        # never a total.
        "observed_cost_lower_bound_usd": sum(
            (row["cost_usd"] if row["cost_status"] == "known" else row["observed_cost_usd"])
            or 0.0
            for row in rows
        ),
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


def labels(header: dict) -> list[str]:
    """The qualifications every copy of these scores must carry."""
    product = header["product"]
    return [
        f"comparison set: {header['manifest']['label']} "
        f"(receipt sha256 {header['manifest']['receipt_sha256']})",
        f"task licences: {header['manifest']['license_label']}",
        f"sandbox: {header['sandbox']['label']}",
        f"binary: {product['binary']}; sha256 {product['binary_sha256']}; "
        f"rustain {product['version']} commit {product['commit']}",
        f"prices: {header['prices']['id']} sha256 {header['prices']['sha256']}",
    ]


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
    lines = [*report["labels"], ""]
    lines += ["| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
    lines += ["| " + " | ".join(_cell(row[c]) for c in columns) + " |" for row in rows]
    for label, summary in report["by_treatment"].items():
        lines += ["", f"## {label} ({report['treatments'][label]})"]
        lines += [f"{key}: {_cell(value)}" for key, value in summary.items()]
    lines += ["", *report["labels"]]
    return "\n".join(lines)


def _write_atomic(path: Path, text: str) -> None:
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(text)
    os.replace(temporary, path)


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 2:
        print("usage: python -m rustain_harbor.baseline <jobs-dir> <run-id>", file=sys.stderr)
        return 2
    jobs_dir, run_id = Path(args[0]).resolve(), args[1]
    output = runplan.run_dir(jobs_dir, run_id) / RESULTS
    # Whatever happens next, no earlier results.json survives an aggregation
    # that does not succeed.
    output.unlink(missing_ok=True)
    try:
        header, rows, problems = reconcile(jobs_dir, run_id)
        if not problems:
            report_labels = labels(header)
    except Exception as error:  # noqa: BLE001 - any failure is a refusal
        problems = [f"unidentifiable: aggregation failed: {type(error).__name__}: {error}"]
    if problems:
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
        "labels": report_labels,
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
    _write_atomic(output, json.dumps(report, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
