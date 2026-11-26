"""The experiment's spend ledger: one budget across every run id.

The owner authorised one spend cap for the run plan, not one per run id. Every
launcher of that plan in a jobs directory reads and appends one file,
`<jobs-dir>/eval4-spend-<plan-id>.jsonl`, under an exclusive `flock`. It is
append-only and lives outside `.rustain-harbor/`, so `clean-host.sh` never
removes it.

A slot is recorded as `slot_started` (with its `--max-cost`) before Harbor
runs and `slot_finished` (with its cost status) after. A started slot without
a finish, or one whose cost is not `known`, is unresolved: its spend is not
known, so no launcher admits another slot until the owner accepts it with
`--accept-unknown-spend <run-id>/<slot-key>[,...]`. An accepted slot is
charged its recorded `--max-cost`: an assumption, not a bound, since one call
can overshoot it. The acceptance is appended to the ledger and recorded in the
accepting run's header.

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
    # entry id -> debit (its --max-cost) for unresolved slots the owner accepted.
    accepted: dict[str, float] = field(default_factory=dict)
    unresolved: list[str] = field(default_factory=list)
    entries: int = 0

    @property
    def spent_usd(self) -> float:
        return self.known_usd + sum(self.accepted.values())


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
                raise SpendError(f"{self.path} line {number} is not a spend event")
            events.append(event)
        return events

    def append(self, event: str, **fields) -> None:
        record = {"event": event, "at": datetime.now(UTC).isoformat(timespec="seconds")}
        self._handle.seek(0, os.SEEK_END)
        self._handle.write(json.dumps({**record, **fields}, sort_keys=True) + "\n")
        self._handle.flush()
        os.fsync(self._handle.fileno())

    def balance(self, accepting: frozenset[str] = frozenset()) -> Balance:
        """What the plan has spent. `accepting`: unresolved entries the
        current launcher was told to accept (not yet in the ledger)."""
        started: dict[str, dict] = {}
        finished: dict[str, dict] = {}
        accepted: set[str] = set(accepting)
        events = self.entries()
        for event in events:
            kind = event["event"]
            if kind == "slot_started":
                started[event["entry"]] = event
            elif kind == "slot_finished":
                finished[event["entry"]] = event
            elif kind == "accepted_unknown":
                accepted.update(event["entries"])
        balance = Balance(entries=len(events))
        for entry, start in started.items():
            end = finished.get(entry)
            if end is not None and end.get("cost_status") == "known":
                balance.known_usd += end["cost_usd"]
            elif entry in accepted:
                balance.accepted[entry] = start["max_cost_usd"]
            else:
                balance.unresolved.append(entry)
        return balance
