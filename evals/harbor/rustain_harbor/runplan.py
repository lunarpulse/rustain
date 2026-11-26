"""The eval-4 run plan, its slots and the immutable run header.

A run executes the frozen run plan `eval4/run-plan.json`: every task of its
manifest × every treatment × attempts 1..N is one slot, and every slot is one
Harbor job. Before anything runs, the launcher writes the run header from this
module; aggregation reads the slots back from that header. Standard library
only, so the offline check and the aggregation need no Harbor install.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
import tomllib
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from rustain_harbor import prices
from rustain_harbor.spend import ledger_name

HARNESS = Path(__file__).resolve().parents[1]
PLAN = HARNESS / "eval4" / "run-plan.json"
LOCK = HARNESS / "uv.lock"
AGENT = "rustain_harbor.agent:RustainAgent"
HEADER_SCHEMA = "rustain-harbor-eval4-run-header/1"
HEADER = "header.json"
HEADER_HASH = "header.sha256"
LEDGER = "ledger.jsonl"
JOBS = "jobs"
# Filesystem-safe labels: lower-case words joined by single hyphens.
_LABEL = re.compile(r"[a-z0-9]+(?:[-.][a-z0-9]+)*")

# The `harbor run` argv of one slot. Every setting is fixed here or comes from
# the header; nothing else reaches Harbor. `--max-retries 0`: a Harbor retry
# deletes the failed trial's directory before it runs again.
ARGV_TEMPLATE = (
    "{harbor}",
    "run",
    "--task",
    "{task_ref}",
    "--agent",
    AGENT,
    "--model",
    "{model}",
    "--ak",
    "binary_path={binary}",
    "--ak",
    "max_cost_usd={max_cost_usd}",
    "--n-attempts",
    "1",
    "--n-concurrent",
    "1",
    "--max-retries",
    "0",
    "--jobs-dir",
    "{jobs_dir}",
    "--job-name",
    "{job_name}",
)


class PlanError(Exception):
    """The plan, its inputs or the requested run cannot be used."""


@dataclass(frozen=True)
class Slot:
    key: str
    job_name: str
    task: str
    task_name: str
    task_ref: str
    treatment: str
    model: str
    attempt: int


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run_dir(jobs_dir: Path, run_id: str) -> Path:
    return jobs_dir / run_id


def venv_dir(jobs_dir: Path) -> Path:
    """Where `run-baseline.sh run` installs Harbor from `uv.lock`."""
    return jobs_dir / ".rustain-harbor" / "venv"


def check_run_id(run_id: str) -> None:
    if not _LABEL.fullmatch(run_id) or len(run_id) > 40 or "." in run_id:
        raise PlanError(
            f"run id {run_id!r} must be 1-40 characters of a-z, 0-9 and single hyphens"
        )


def _load_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise PlanError(f"cannot read {path}: {error}") from None


def _harbor_lock(lock: Path) -> tuple[str, str]:
    """Harbor's version in the lock, and the lock's SHA-256."""
    try:
        data = tomllib.loads(lock.read_text())
    except (OSError, ValueError) as error:
        raise PlanError(f"cannot read {lock}: {error}") from None
    versions = [p["version"] for p in data.get("package", []) if p.get("name") == "harbor"]
    if len(versions) != 1:
        raise PlanError(f"{lock} must lock exactly one harbor version")
    return versions[0], sha256_file(lock)


def build_header(
    *,
    run_id: str,
    binary: Path,
    jobs_dir: Path,
    plan_path: Path = PLAN,
    lock_path: Path = LOCK,
    price_table: Path = prices.PRICE_TABLE,
) -> dict:
    """The run header: every setting of the run and every expected slot.

    Refuses a binary whose SHA-256 differs from the plan's.
    """
    check_run_id(run_id)
    plan = _load_json(plan_path)
    manifest_path = plan_path.parent / plan["manifest"]
    manifest = _load_json(manifest_path)
    table = prices.load(price_table)
    harbor_version, lock_sha256 = _harbor_lock(lock_path)
    if harbor_version != plan["harbor_version"]:
        raise PlanError(
            f"uv.lock pins harbor {harbor_version}, the plan requires {plan['harbor_version']}"
        )

    binary = binary.resolve()
    if not binary.is_file():
        raise PlanError(f"rustain binary not found: {binary}")
    binary_sha256 = sha256_file(binary)
    if binary_sha256 != plan["product"]["binary_sha256"]:
        raise PlanError(
            f"{binary} has SHA-256 {binary_sha256}; the plan requires "
            f"{plan['product']['binary_sha256']}"
        )

    attempts = plan["attempts"]
    if attempts != 3:
        raise PlanError("the plan must run 3 attempts per task and treatment")
    treatments = plan["treatments"]
    labels = [t["label"] for t in treatments]
    if len(set(labels)) != len(labels) or len({t["model"] for t in treatments}) != len(
        treatments
    ):
        raise PlanError("treatment labels and models must be distinct")
    for treatment in treatments:
        if not _LABEL.fullmatch(treatment["label"]):
            raise PlanError(f"treatment label {treatment['label']!r} is not filesystem-safe")
        provider, _, model = treatment["model"].partition("/")
        if table.prices(provider, model) is None:
            raise PlanError(
                f"{treatment['model']} has no price in {price_table.name}; "
                "the spend guard needs a known cost for every slot"
            )
    set_id = manifest["id"]
    names = [task["name"] for task in manifest["tasks"]]
    if len(set(names)) != len(names):
        raise PlanError(f"{manifest_path.name} lists a task twice")
    for label in [set_id, *names]:
        if not _LABEL.fullmatch(label):
            raise PlanError(f"label {label!r} is not filesystem-safe")

    spend = plan["spend"]
    if not 0 < spend["per_slot_max_cost_usd"] <= spend["cap_usd"]:
        raise PlanError("per-slot cost ceiling must be positive and within the cap")

    # Attempt first, then the manifest's task order, then treatment order:
    # if the spend cap stops the run, the slots run so far stay paired.
    slots = [
        Slot(
            key=f"{set_id}/{task['name']}/{treatment['label']}/{attempt}",
            job_name=f"{run_id}--{set_id}--{task['name']}--{treatment['label']}--a{attempt}",
            task=task["name"],
            task_name=task["package"],
            task_ref=f"{task['package']}@{task['package_digest']}",
            treatment=treatment["label"],
            model=treatment["model"],
            attempt=attempt,
        )
        for attempt in range(1, attempts + 1)
        for task in manifest["tasks"]
        for treatment in treatments
    ]
    keys = "".join(f"{slot.key}\n" for slot in slots)
    jobs_dir = jobs_dir.resolve()
    images = {}
    for task in manifest["tasks"]:
        repository, _, tag = task["docker_image"].rpartition(":")
        if not repository or "/" in tag or not re.fullmatch(
            r"sha256:[0-9a-f]{64}", task["image_digest"]
        ):
            raise PlanError(f"{task['name']}: image must be <repo>:<tag> with a sha256 digest")
        images[task["name"]] = {
            "tag": task["docker_image"],
            "digest": task["image_digest"],
            "pinned": f"{repository}@{task['image_digest']}",
        }

    return {
        "schema": HEADER_SCHEMA,
        "run_id": run_id,
        "created_at": datetime.now(UTC).isoformat(timespec="seconds"),
        "run_plan": {"id": plan["id"], "file": plan_path.name, "sha256": sha256_file(plan_path)},
        "product": {**plan["product"], "binary_path": str(binary)},
        "manifest": {
            "id": set_id,
            "label": manifest["label"],
            "file": manifest_path.name,
            "sha256": sha256_file(manifest_path),
            "dataset": manifest["dataset"],
            "dataset_revision": manifest["dataset_revision"],
            "receipt_sha256": manifest["receipt"]["selected_tsv_sha256"],
            "license_label": manifest["license_label"],
            "license_note": manifest["license_note"],
        },
        "treatments": treatments,
        "treatment_kind": plan["treatment_kind"],
        "attempts": attempts,
        "slot_order": "attempt, then manifest task order, then treatment order",
        "slots": [slot.__dict__ for slot in slots],
        "slot_keys_sha256": hashlib.sha256(keys.encode()).hexdigest(),
        "concurrency": 1,
        # Pulled by digest and tagged before the first slot; the tag's image
        # id is checked before and after every slot (Harbor runs the task's
        # tag and pulls it only when missing).
        "task_images": images,
        "docker": "docker",
        "task_timeouts_sec": {
            task["name"]: {
                "agent": task["agent_timeout_sec"],
                "verifier": task["verifier_timeout_sec"],
            }
            for task in manifest["tasks"]
        },
        "harbor": {
            "version": harbor_version,
            "lock_file": lock_path.name,
            "lock_sha256": lock_sha256,
            "executable": str(venv_dir(jobs_dir) / "bin" / "harbor"),
            "env": {"PYTHONPATH": str(HARNESS), "LITELLM_LOCAL_MODEL_COST_MAP": "True"},
        },
        "prices": {
            "id": table.id,
            "date": table.date,
            "source": table.source,
            "file": price_table.name,
            "sha256": table.sha256,
        },
        "spend": {
            "plan_id": plan["id"],
            "ledger_file": ledger_name(plan["id"]),
            "cap_usd": spend["cap_usd"],
            "per_slot_max_cost_usd": spend["per_slot_max_cost_usd"],
            "guard": (
                "one cap for the run plan across every run id, from the spend "
                "ledger: before each slot, stop when the plan's known cost plus "
                "accepted unknown debits reaches the cap, or when any started slot "
                "of the plan has no known cost and was not accepted with "
                "--accept-unknown-spend; the slot gets --max-cost min(cap - spent, "
                "per-slot ceiling), floored to 0.0001 USD"
            ),
        },
        "limits": plan["limits"],
        "sandbox": plan["sandbox"],
        "jobs_dir": str(run_dir(jobs_dir, run_id) / JOBS),
        "argv_template": list(ARGV_TEMPLATE),
    }


def header_slots(header: dict) -> list[Slot]:
    return [Slot(**slot) for slot in header["slots"]]


def slot_max_cost(header: dict, known_cost_usd: float) -> float:
    """The `--max-cost` of the next slot after `known_cost_usd` was spent."""
    spend = header["spend"]
    remaining = min(spend["cap_usd"] - known_cost_usd, spend["per_slot_max_cost_usd"])
    # Rounded first, so float noise (1.0 - 0.8 = 0.1999...) does not floor away 0.0001.
    return math.floor(round(remaining * 10_000, 6)) / 10_000


def slot_argv(header: dict, slot: Slot, max_cost_usd: float) -> list[str]:
    values = {
        "harbor": header["harbor"]["executable"],
        "task_ref": slot.task_ref,
        "model": slot.model,
        "binary": header["product"]["binary_path"],
        "max_cost_usd": repr(max_cost_usd),
        "jobs_dir": header["jobs_dir"],
        "job_name": slot.job_name,
    }
    return [part.format(**values) for part in header["argv_template"]]


def render(header: dict) -> bytes:
    return (json.dumps(header, indent=2, sort_keys=True) + "\n").encode()
