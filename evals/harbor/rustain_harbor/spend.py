"""The experiment's spend ledger: one budget across every run id.

The owner authorised one spend cap for the run plan, not one per run id. Every
launcher of that plan in a jobs directory reads and appends one file,
`<jobs-dir>/eval4-spend-<plan-id>.jsonl`, under an exclusive `flock`. It is
append-only and lives outside `.rustain-harbor/`, so `clean-host.sh` never
removes it.

Every run id is registered (`run_started`) when its header is written, and a
run id already in the ledger is refused, even when its run directory is gone:
`clean-host.sh --results` removes run directories, never this ledger, so a
cleaned run id cannot be reused.

A slot is recorded as `slot_started` (with its `--max-cost`) before Harbor
runs and `slot_finished` (with its cost status) after. Each identity
(`<run-id>/<slot-key>`) starts at most once and finishes at most once after
its start; anything else is ledger corruption and refuses every launcher.
Charges are summed over every event, never overwritten.

A started slot without a finish, or one whose cost is not `known`, is
unresolved: it is charged its recorded `--max-cost` (an assumption, not a
bound, since one call can overshoot it), and no launcher admits another slot
until the owner accepts it with `--accept-unknown-spend
<run-id>/<slot-key>[,...]`. The acceptance is appended to the ledger once and
recorded in the accepting run's header; the charge stays.

A second launcher of the same plan and jobs directory is refused while one is
running (`launcher_lock`), so an unresolved entry is never a live slot.
"""

from __future__ import annotations

import fcntl
import json
import os
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path


class SpendError(Exception):
    """The spend ledger refuses the next slot or cannot be read."""


def ledger_name(plan_id: str) -> str:
    return f"eval4-spend-{plan_id}.jsonl"


def ledger_path(jobs_dir: Path, plan_id: str) -> Path:
    return jobs_dir / ledger_name(plan_id)


def entry_id(run_id: str, slot_key: str) -> str:
    return f"{run_id}/{slot_key}"


@contextmanager
def launcher_lock(jobs_dir: Path, plan_id: str) -> Iterator[None]:
    """Held for a whole `run`; a second launcher of the plan is refused."""
    path = jobs_dir / f".eval4-launcher-{plan_id}.lock"
    with path.open("a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SpendError(
                f"another launcher of {plan_id} holds {path}; one launcher at a time"
            ) from None
        yield


@dataclass
class Balance:
    known_usd: float = 0.0
    # entry id -> charge (its --max-cost) for slots whose cost is not known.
    accepted: dict[str, float] = field(default_factory=dict)
    unresolved: dict[str, float] = field(default_factory=dict)
    # Entries accepted by an earlier launcher (in the ledger already).
    accepted_before: set[str] = field(default_factory=set)
    run_ids: set[str] = field(default_factory=set)
    entries: int = 0

    @property
    def spent_usd(self) -> float:
        return (
            self.known_usd + sum(self.accepted.values()) + sum(self.unresolved.values())
        )


def _corrupt(path: Path, number: int, why: str) -> SpendError:
    return SpendError(
        f"{path} line {number}: {why}; the spend ledger is corrupt and no launcher "
        "may go on until the owner repairs it"
    )


def _amount(value) -> bool:
    return not isinstance(value, bool) and isinstance(value, int | float) and value >= 0


class Ledger:
    """The open spend ledger, locked exclusively for the duration of `open`."""

    def __init__(self, path: Path):
        self.path = path

    @contextmanager
    def open(self) -> Iterator[Ledger]:
        with self.path.open("a+") as handle:
            fcntl.flock(handle, fcntl.LOCK_EX)
            self._handle = handle
            try:
                yield self
            finally:
                self._handle = None

    def entries(self) -> list[dict]:
        self._handle.seek(0)
        events = []
        for number, line in enumerate(self._handle.read().splitlines(), 1):
            try:
                event = json.loads(line)
            except ValueError:
                event = None
            if not isinstance(event, dict) or not isinstance(event.get("event"), str):
                raise _corrupt(self.path, number, "not a spend event")
            events.append(event)
        return events

    def append(self, event: str, **fields) -> None:
        record = {"event": event, "at": datetime.now(UTC).isoformat(timespec="seconds")}
        self._handle.seek(0, os.SEEK_END)
        self._handle.write(json.dumps({**record, **fields}, sort_keys=True) + "\n")
        self._handle.flush()
        os.fsync(self._handle.fileno())

    def balance(self, accepting: frozenset[str] = frozenset()) -> Balance:
        """What the plan has spent, from every event in order. `accepting`:
        unresolved entries the current launcher was told to accept (not yet in
        the ledger). Raises SpendError on any duplicate or orphan identity."""
        runs: set[str] = set()
        started: dict[str, float] = {}
        finished: dict[str, dict] = {}
        accepted: set[str] = set()
        events = self.entries()
        for number, event in enumerate(events, 1):
            kind = event["event"]
            if kind == "run_started":
                run_id = event.get("run_id")
                if not isinstance(run_id, str) or run_id in runs:
                    raise _corrupt(self.path, number, f"run id {run_id!r} registered twice")
                runs.add(run_id)
            elif kind == "slot_started":
                entry = event.get("entry")
                if event.get("run_id") not in runs or not isinstance(entry, str):
                    raise _corrupt(self.path, number, f"slot {entry!r} of an unregistered run")
                if entry in started:
                    raise _corrupt(self.path, number, f"{entry} started twice")
                if not _amount(event.get("max_cost_usd")):
                    raise _corrupt(self.path, number, f"{entry} has no --max-cost")
                started[entry] = event["max_cost_usd"]
            elif kind == "slot_finished":
                entry = event.get("entry")
                if entry not in started:
                    raise _corrupt(self.path, number, f"{entry!r} finished without a start")
                if entry in finished:
                    raise _corrupt(self.path, number, f"{entry} finished twice")
                if event.get("cost_status") == "known" and not _amount(event.get("cost_usd")):
                    raise _corrupt(self.path, number, f"{entry} has a known cost without a sum")
                finished[entry] = event
            elif kind == "accepted_unknown":
                entries = event.get("entries")
                if not isinstance(entries, list):
                    raise _corrupt(self.path, number, "an acceptance without entries")
                for entry in entries:
                    if entry not in started or entry in accepted:
                        raise _corrupt(
                            self.path, number, f"acceptance of {entry!r} twice or unstarted"
                        )
                    accepted.add(entry)
            else:
                raise _corrupt(self.path, number, f"unknown event {kind!r}")
        balance = Balance(entries=len(events), run_ids=runs, accepted_before=set(accepted))
        for entry, max_cost in started.items():
            end = finished.get(entry)
            if end is not None and end.get("cost_status") == "known":
                balance.known_usd += end["cost_usd"]
            elif entry in accepted or entry in accepting:
                balance.accepted[entry] = max_cost
            else:
                balance.unresolved[entry] = max_cost
        return balance
