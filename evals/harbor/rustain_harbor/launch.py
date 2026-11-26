"""Eval-4 launcher: one Harbor job per slot under the plan's spend cap.

    python -m rustain_harbor.launch check <rustain-binary> <jobs-dir> <run-id>
        [--accept-unknown-spend <run-id>/<slot-key>[,...]]
    python -m rustain_harbor.launch run <rustain-binary> <jobs-dir> <run-id>
        [--accept-unknown-spend <run-id>/<slot-key>[,...]]

`check` is offline: it validates the plan, the manifest, the price table, the
lock and the binary, reads the plan's spend ledger, and prints the header and
each slot's argv that `run` would use. It writes nothing. `run` is what
`run-baseline.sh run` calls after installing Harbor from `uv.lock`:

1. It takes the plan's launcher lock (one launcher at a time per jobs
   directory) and reads the plan's spend ledger (`spend.py`): the cap holds
   across every run id. A run is refused while the plan has spent its cap,
   or while any started slot of the plan has no known cost and the owner has
   not accepted it with `--accept-unknown-spend`.
2. It writes `<jobs-dir>/<run-id>/header.json` once, with exclusive create,
   and refuses a run id whose directory exists. Nothing is ever re-run into
   a run.
3. It checks the treatments' provider keys (`preflight`), then pulls every
   task image by its manifest digest and tags it as the task's image, so
   Harbor (which pulls a task image only when it is missing) runs the pinned
   image. The tag's image id is checked before and after every slot.
4. It runs the slots one at a time, in header order. Before each slot the
   spend guard re-reads the spend ledger and stops the run when the plan's
   spend reaches the cap or any started slot's cost is unresolved. Each slot
   runs `harbor run` with the argv it first records in `ledger.jsonl`.

Exit status: 0 when every slot ran, 3 when the spend guard stopped the run,
1 when the run was refused or a slot could not start or ran on another image,
2 for bad arguments. Harbor's own exit status is recorded per slot; it says
nothing about the outcome, which comes from the trial result.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path

from rustain_harbor import runplan, spend
from rustain_harbor.baseline import read_slot_cost
from rustain_harbor.runplan import PlanError
from rustain_harbor.spend import SpendError

EXIT_REFUSED = 1
EXIT_USAGE = 2
EXIT_STOPPED = 3
ACCEPT_FLAG = "--accept-unknown-spend"


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


class Ledger:
    """Append-only record of what the launcher did in one run."""

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


def _spend_state(header: dict, jobs_dir: Path, accepting: frozenset[str]) -> dict:
    """The plan's spend before this run, or SpendError when it may not start."""
    path = spend.ledger_path(jobs_dir, header["spend"]["plan_id"])
    with spend.Ledger(path).open() as ledger:
        balance = ledger.balance(accepting)
    unknown = sorted(accepting - set(balance.accepted))
    if unknown:
        raise SpendError(
            f"{ACCEPT_FLAG} names {', '.join(unknown)}, which the spend ledger "
            "does not hold as an unresolved slot"
        )
    if balance.unresolved:
        raise SpendError(
            f"{len(balance.unresolved)} started slot(s) of {header['spend']['plan_id']} "
            f"have no known cost: {', '.join(balance.unresolved)}. Inspect them; to "
            f"charge each its recorded --max-cost and go on, pass {ACCEPT_FLAG} "
            f"{','.join(balance.unresolved)}"
        )
    remaining = header["spend"]["cap_usd"] - balance.spent_usd
    if runplan.slot_max_cost(header, balance.spent_usd) <= 0:
        raise SpendError(
            f"the plan has spent {balance.spent_usd:.4f} USD of its "
            f"{header['spend']['cap_usd']} USD cap; nothing remains"
        )
    return {
        "ledger": str(path),
        "prior_entries": balance.entries,
        "prior_known_usd": balance.known_usd,
        "accepted_unknown_usd": {
            entry: debit for entry, debit in balance.accepted.items() if entry in accepting
        },
        "previously_accepted_unknown_usd": {
            entry: debit
            for entry, debit in balance.accepted.items()
            if entry not in accepting
        },
        "override": f"{ACCEPT_FLAG} {','.join(sorted(accepting))}" if accepting else None,
        "spent_usd": balance.spent_usd,
        "remaining_usd": remaining,
    }


def check(binary: Path, jobs_dir: Path, run_id: str, accepting: frozenset[str]) -> int:
    header = runplan.build_header(run_id=run_id, binary=binary, jobs_dir=jobs_dir)
    target = runplan.run_dir(jobs_dir, run_id)
    if target.exists():
        raise PlanError(f"{target} exists; a run id is used once")
    path = spend.ledger_path(jobs_dir.resolve(), header["spend"]["plan_id"])
    if path.exists():
        header["spend"]["state"] = _spend_state(header, jobs_dir.resolve(), accepting)
    elif accepting:
        raise SpendError(f"{ACCEPT_FLAG} given, but {path} does not exist")
    sys.stdout.buffer.write(runplan.render(header))
    spent = header["spend"].get("state", {}).get("spent_usd", 0.0)
    ceiling = runplan.slot_max_cost(header, spent)
    for slot in runplan.header_slots(header):
        print(slot.key, json.dumps(runplan.slot_argv(header, slot, ceiling)))
    print(
        f"check: OK: {len(header['slots'])} slots, cap {header['spend']['cap_usd']} USD "
        f"for the plan, first slot --max-cost {ceiling} USD; nothing was written or run",
        file=sys.stderr,
    )
    return 0


def _create_header(header: dict, jobs_dir: Path) -> tuple[Path, str]:
    target = runplan.run_dir(jobs_dir, header["run_id"])
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


def _docker(header: dict, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [header["docker"], *args], capture_output=True, text=True, stdin=subprocess.DEVNULL
    )


def image_id(header: dict, reference: str) -> str | None:
    result = _docker(header, "image", "inspect", "--format", "{{.Id}}", reference)
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def _pin_images(header: dict) -> tuple[dict[str, str], list[str]]:
    """Pull each task image by digest and tag it as the task's image."""
    ids: dict[str, str] = {}
    problems = []
    for task, image in header["task_images"].items():
        pulled = _docker(header, "pull", image["pinned"])
        if pulled.returncode != 0:
            problems.append(f"{task}: docker pull {image['pinned']} failed: {pulled.stderr[-300:]}")
            continue
        digests = _docker(
            header, "image", "inspect", "--format", "{{json .RepoDigests}}", image["pinned"]
        )
        try:
            repo_digests = json.loads(digests.stdout) if digests.returncode == 0 else []
        except ValueError:
            repo_digests = []
        pinned_id = image_id(header, image["pinned"])
        if image["pinned"] not in (repo_digests or []) or pinned_id is None:
            problems.append(f"{task}: {image['pinned']} is not the pulled image's digest")
            continue
        tagged = _docker(header, "tag", image["pinned"], image["tag"])
        if tagged.returncode != 0 or image_id(header, image["tag"]) != pinned_id:
            problems.append(f"{task}: cannot point {image['tag']} at {image['pinned']}")
            continue
        ids[task] = pinned_id
    return ids, problems


def _stop(ledger: Ledger, slot: runplan.Slot, reason: str, status: int) -> int:
    reason += f"; {slot.key} and later slots not run"
    ledger.append("stopped", reason=reason, slot=slot.key)
    print(f"launch: {reason}", file=sys.stderr)
    return status


def run(binary: Path, jobs_dir: Path, run_id: str, accepting: frozenset[str]) -> int:
    jobs_dir = jobs_dir.resolve()
    header = runplan.build_header(run_id=run_id, binary=binary, jobs_dir=jobs_dir)
    jobs_dir.mkdir(parents=True, exist_ok=True)
    plan_id = header["spend"]["plan_id"]
    with spend.launcher_lock(jobs_dir, plan_id):
        header["spend"]["state"] = _spend_state(header, jobs_dir, accepting)
        target, digest = _create_header(header, jobs_dir)
        spend_ledger = spend.Ledger(spend.ledger_path(jobs_dir, plan_id))
        if accepting:
            with spend_ledger.open() as book:
                book.append(
                    "accepted_unknown",
                    run_id=run_id,
                    header_sha256=digest,
                    entries=sorted(accepting),
                    debits_usd=header["spend"]["state"]["accepted_unknown_usd"],
                )
        return _run_slots(header, target, digest, spend_ledger)


def _run_slots(header: dict, target: Path, digest: str, spend_ledger: spend.Ledger) -> int:
    ledger = Ledger(target / runplan.LEDGER, digest)
    jobs_root = target / runplan.JOBS
    jobs_root.mkdir()

    problems = _preflight(header)
    if problems:
        ledger.append("preflight_failed", problems=problems)
        for problem in problems:
            print(f"preflight: {problem}", file=sys.stderr)
        return EXIT_REFUSED
    pinned, problems = _pin_images(header)
    if problems:
        ledger.append("images_failed", problems=problems)
        for problem in problems:
            print(f"images: {problem}", file=sys.stderr)
        return EXIT_REFUSED
    ledger.append("images_pinned", image_ids=pinned)

    env = {**os.environ, **header["harbor"]["env"]}
    run_id = header["run_id"]
    for slot in runplan.header_slots(header):
        entry = spend.entry_id(run_id, slot.key)
        job_dir = jobs_root / slot.job_name
        if job_dir.exists() or slot.job_name in ledger.started:
            ledger.append("refused", slot=slot.key, reason="job directory or name already used")
            print(f"launch: refusing {slot.key}: {job_dir} already used", file=sys.stderr)
            return EXIT_REFUSED
        tag = header["task_images"][slot.task]["tag"]
        before = image_id(header, tag)
        if before != pinned[slot.task]:
            return _stop(
                ledger, slot, f"{tag} is image {before}, not the pinned {pinned[slot.task]}",
                EXIT_REFUSED,
            )
        with spend_ledger.open() as book:
            balance = book.balance()
            if balance.unresolved:
                return _stop(
                    ledger,
                    slot,
                    f"{', '.join(balance.unresolved)} has no known cost; the spend "
                    "guard cannot count it",
                    EXIT_STOPPED,
                )
            max_cost = runplan.slot_max_cost(header, balance.spent_usd)
            if max_cost <= 0:
                return _stop(
                    ledger,
                    slot,
                    f"spend cap reached: the plan has spent {balance.spent_usd:.4f} USD "
                    f"of {header['spend']['cap_usd']} USD",
                    EXIT_STOPPED,
                )
            argv = runplan.slot_argv(header, slot, max_cost)
            book.append(
                "slot_started",
                entry=entry,
                run_id=run_id,
                header_sha256=digest,
                max_cost_usd=max_cost,
            )
        ledger.append(
            "slot_started",
            slot=slot.key,
            job_name=slot.job_name,
            argv=argv,
            max_cost_usd=max_cost,
            image_id_before=before,
        )
        ledger.started.add(slot.job_name)
        try:
            status = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL).returncode
        except OSError as error:
            ledger.append("slot_failed_to_start", slot=slot.key, error=str(error))
            print(f"launch: cannot run {argv[0]}: {error}", file=sys.stderr)
            return EXIT_REFUSED

        after = image_id(header, tag)
        cost_status, cost = read_slot_cost(job_dir)
        with spend_ledger.open() as book:
            book.append(
                "slot_finished",
                entry=entry,
                run_id=run_id,
                cost_status=cost_status,
                cost_usd=cost,
            )
        ledger.append(
            "slot_finished",
            slot=slot.key,
            job_name=slot.job_name,
            harbor_exit=status,
            cost_status=cost_status,
            cost_usd=cost,
            image_id_after=after,
        )
        if after != pinned[slot.task]:
            return _stop(
                ledger,
                slot,
                f"{tag} became image {after} during {slot.key}, not the pinned "
                f"{pinned[slot.task]}",
                EXIT_REFUSED,
            )
    return 0


def _accepting(args: list[str]) -> frozenset[str] | None:
    if not args:
        return frozenset()
    if len(args) == 2 and args[0] == ACCEPT_FLAG and args[1]:
        return frozenset(part for part in args[1].split(",") if part)
    return None


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    accepting = _accepting(args[4:]) if len(args) >= 4 else None
    if accepting is None or args[0] not in ("check", "run"):
        print(
            "usage: python -m rustain_harbor.launch {check|run} "
            f"<rustain-binary> <jobs-dir> <run-id> [{ACCEPT_FLAG} <run-id>/<slot-key>,...]",
            file=sys.stderr,
        )
        return EXIT_USAGE
    command, binary, jobs_dir, run_id = args[:4]
    try:
        if command == "check":
            return check(Path(binary), Path(jobs_dir), run_id, accepting)
        return run(Path(binary), Path(jobs_dir), run_id, accepting)
    except (PlanError, SpendError) as error:
        print(f"{command}: refused: {error}", file=sys.stderr)
        return EXIT_REFUSED


if __name__ == "__main__":
    sys.exit(main())
