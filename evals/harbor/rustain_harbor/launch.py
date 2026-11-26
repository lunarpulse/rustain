"""Eval-4 launcher: one Harbor job per slot under a cumulative spend cap.

    python -m rustain_harbor.launch check <rustain-binary> <jobs-dir> <run-id>
    python -m rustain_harbor.launch run <rustain-binary> <jobs-dir> <run-id>

`check` is offline: it validates the plan, the manifest, the price table, the
lock and the binary, and prints the header and each slot's argv that `run`
would use. It writes nothing. `run` is what `run-baseline.sh run` calls after
installing Harbor from `uv.lock`:

1. It writes `<jobs-dir>/<run-id>/header.json` once, with exclusive create,
   and refuses a run id whose directory exists. Nothing is ever re-run into a
   run: a new attempt at the matrix is a new run id.
2. It checks the treatments' provider keys (`preflight`).
3. It runs the slots one at a time, in header order. Before each slot the
   spend guard stops the run when the known cost of the finished slots has
   reached the cap, or when a finished slot's cost is unknown. Each slot runs
   `harbor run` with the argv it first records in `ledger.jsonl`.

Exit status: 0 when every slot ran, 3 when the spend guard stopped the run,
1 when the run was refused or a slot could not start, 2 for bad arguments.
Harbor's own exit status is recorded per slot; it says nothing about the
outcome, which comes from the trial result.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path

from rustain_harbor import runplan
from rustain_harbor.baseline import read_slot_cost
from rustain_harbor.runplan import PlanError

EXIT_REFUSED = 1
EXIT_USAGE = 2
EXIT_STOPPED = 3


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


class Ledger:
    """Append-only record of what the launcher did, one JSON object a line."""

    def __init__(self, path: Path, header_sha256: str):
        self.path = path
        self.header_sha256 = header_sha256
        self.started: set[str] = set()

    def append(self, event: str, **fields) -> None:
        record = {"event": event, "at": _now(), "header_sha256": self.header_sha256, **fields}
        with self.path.open("a") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())


def check(binary: Path, jobs_dir: Path, run_id: str) -> int:
    header = runplan.build_header(run_id=run_id, binary=binary, jobs_dir=jobs_dir)
    target = runplan.run_dir(jobs_dir, run_id)
    if target.exists():
        raise PlanError(f"{target} exists; a run id is used once")
    sys.stdout.buffer.write(runplan.render(header))
    ceiling = header["spend"]["per_slot_max_cost_usd"]
    for slot in runplan.header_slots(header):
        print(slot.key, json.dumps(runplan.slot_argv(header, slot, ceiling)))
    print(
        f"check: OK: {len(header['slots'])} slots, cap {header['spend']['cap_usd']} USD, "
        f"per-slot --max-cost up to {ceiling} USD; nothing was written or run",
        file=sys.stderr,
    )
    return 0


def _create_header(header: dict, jobs_dir: Path) -> tuple[Path, str]:
    target = runplan.run_dir(jobs_dir, header["run_id"])
    jobs_dir.mkdir(parents=True, exist_ok=True)
    try:
        target.mkdir()
    except FileExistsError:
        raise PlanError(f"{target} exists; a run id is used once") from None
    content = runplan.render(header)
    with open(target / runplan.HEADER, "xb") as handle:
        handle.write(content)
    digest = hashlib.sha256(content).hexdigest()
    (target / runplan.HEADER_HASH).write_text(f"{digest}  {runplan.HEADER}\n")
    os.chmod(target / runplan.HEADER, 0o444)
    # clean-host.sh removes only what is recorded here.
    record = jobs_dir / ".rustain-harbor"
    record.mkdir(exist_ok=True)
    with (record / "jobs").open("a") as handle:
        handle.write(header["run_id"] + "\n")
    return target, digest


def _preflight(header: dict) -> list[str]:
    # Imports Harbor: only `run`, inside the locked environment, gets here.
    from rustain_harbor.agent import PROVIDERS
    from rustain_harbor.preflight import check as check_key

    problems = []
    for treatment in header["treatments"]:
        provider = PROVIDERS.get(treatment["model"].partition("/")[0])
        if provider is not None and provider.api_key_env != treatment["key_env"]:
            problems.append(
                f"{treatment['label']}: key_env {treatment['key_env']} is not "
                f"{provider.api_key_env}"
            )
    for model in dict.fromkeys(t["model"] for t in header["treatments"]):
        problem = check_key(model, os.environ)
        if problem:
            problems.append(f"{model}: {problem}")
    return problems


def run(binary: Path, jobs_dir: Path, run_id: str) -> int:
    jobs_dir = jobs_dir.resolve()
    header = runplan.build_header(run_id=run_id, binary=binary, jobs_dir=jobs_dir)
    target, digest = _create_header(header, jobs_dir)
    ledger = Ledger(target / runplan.LEDGER, digest)
    jobs_root = target / runplan.JOBS
    jobs_root.mkdir()

    problems = _preflight(header)
    if problems:
        ledger.append("preflight_failed", problems=problems)
        for problem in problems:
            print(f"preflight: {problem}", file=sys.stderr)
        return EXIT_REFUSED

    env = {**os.environ, **header["harbor"]["env"]}
    known_cost = 0.0
    uncounted: str | None = None
    for slot in runplan.header_slots(header):
        max_cost = runplan.slot_max_cost(header, known_cost)
        reason = None
        if uncounted is not None:
            reason = f"{uncounted}; the spend guard cannot count it"
        elif max_cost <= 0:
            reason = (
                f"spend cap reached: known cost {known_cost:.4f} USD of "
                f"{header['spend']['cap_usd']} USD"
            )
        if reason is not None:
            reason += f"; {slot.key} and later slots not run"
            ledger.append("stopped", reason=reason, slot=slot.key)
            print(f"launch: {reason}", file=sys.stderr)
            return EXIT_STOPPED
        job_dir = jobs_root / slot.job_name
        if job_dir.exists() or slot.job_name in ledger.started:
            ledger.append("refused", slot=slot.key, reason="job directory or name already used")
            print(f"launch: refusing {slot.key}: {job_dir} already used", file=sys.stderr)
            return EXIT_REFUSED

        argv = runplan.slot_argv(header, slot, max_cost)
        ledger.append(
            "slot_started",
            slot=slot.key,
            job_name=slot.job_name,
            argv=argv,
            max_cost_usd=max_cost,
        )
        ledger.started.add(slot.job_name)
        try:
            status = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL).returncode
        except OSError as error:
            ledger.append("slot_failed_to_start", slot=slot.key, error=str(error))
            print(f"launch: cannot run {argv[0]}: {error}", file=sys.stderr)
            return EXIT_REFUSED

        cost_status, cost = read_slot_cost(job_dir)
        if cost_status == "known":
            known_cost += cost
        ledger.append(
            "slot_finished",
            slot=slot.key,
            job_name=slot.job_name,
            harbor_exit=status,
            cost_status=cost_status,
            cost_usd=cost,
            known_cost_usd=known_cost,
        )
        if cost_status != "known":
            uncounted = f"{slot.key} cost is {cost_status}"
    return 0


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 4 or args[0] not in ("check", "run"):
        print(
            "usage: python -m rustain_harbor.launch {check|run} "
            "<rustain-binary> <jobs-dir> <run-id>",
            file=sys.stderr,
        )
        return EXIT_USAGE
    command, binary, jobs_dir, run_id = args
    try:
        if command == "check":
            return check(Path(binary), Path(jobs_dir), run_id)
        return run(Path(binary), Path(jobs_dir), run_id)
    except PlanError as error:
        print(f"{command}: refused: {error}", file=sys.stderr)
        return EXIT_REFUSED


if __name__ == "__main__":
    sys.exit(main())
