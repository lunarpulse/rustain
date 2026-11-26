"""The plan's spend ledger and clean-host.sh through their front doors
(E4-R7..R9): run ids are used once, the ledger is append-only and strict, and
cleanup never resets spend."""

import json
import subprocess

import pytest

from test_launch import SET, expected_keys, started

DIND = (
    "docker:29.9.3-dind@sha256:7bf70dcb85c822e3fa6f2d332143e474b99147b9d7b39102b0cfe8b04a7dd61b"
)


def ledger_file(harness):
    return harness.jobs / "eval4-spend-eval-4-cmp-benign-v1.jsonl"


def append(harness, *events: dict) -> None:
    with ledger_file(harness).open("a") as handle:
        for event in events:
            handle.write(json.dumps(event) + "\n")


def test_a_cleaned_run_id_cannot_be_reused(harness):
    assert harness.run().returncode == 0
    ledger_before = ledger_file(harness).read_bytes()

    cleaned = harness.clean_host("--apply", "--jobs-dir", str(harness.jobs), "--results")

    assert cleaned.returncode == 0, cleaned.stderr
    assert not harness.run_dir().exists()
    assert ledger_file(harness).read_bytes() == ledger_before
    assert (harness.jobs / ".eval4-launcher-eval-4-cmp-benign-v1.lock").exists()
    calls = len(harness.harbor_calls())

    reused = harness.run()

    assert reused.returncode == 1
    assert "run id run-0001 is already in the spend ledger" in reused.stderr
    assert len(harness.harbor_calls()) == calls
    assert ledger_file(harness).read_bytes() == ledger_before
    # A new id pays from what the cleaned run spent.
    harness.edit_plan(lambda plan: plan["spend"].update(cap_usd=1.6))
    assert harness.run("run-0002").returncode == 3
    assert harness.header("run-0002")["spend"]["state"]["prior_known_usd"] == pytest.approx(1.5)


def test_a_run_that_spent_nothing_still_uses_up_its_id(harness):
    assert harness.run(key="sk-test-rejected-9999").returncode == 1
    assert harness.clean_host("--apply", "--jobs-dir", str(harness.jobs), "--results").returncode == 0

    reused = harness.run()

    assert reused.returncode == 1
    assert "already in the spend ledger" in reused.stderr
    assert harness.harbor_calls() == {}


@pytest.mark.parametrize(("event", "problem"), [("slot_started", "started twice"), ("slot_finished", "finished twice")])
def test_a_repeated_slot_identity_is_ledger_corruption(harness, event, problem):
    assert harness.run().returncode == 0
    first = next(e for e in harness.spend_ledger() if e["event"] == event)
    # Appended last: a new start after the old finish, or a second finish.
    append(harness, {**first, "cost_status": "known", "cost_usd": 0.0})

    for result in (
        harness.run("run-0002"),
        harness.launcher("check", str(harness.binary), str(harness.jobs), "run-0003"),
    ):
        assert result.returncode == 1
        assert problem in result.stderr and "spend ledger is corrupt" in result.stderr
    assert not harness.run_dir("run-0002").exists()


def test_an_unfinished_start_is_charged_its_max_cost_until_accepted(harness):
    harness.edit_plan(lambda plan: plan["spend"].update(cap_usd=2.0))
    assert harness.run().returncode == 0  # 30 x 0.05 = 1.5 USD
    entry = f"run-crash/{expected_keys()[0]}"
    append(
        harness,
        {"event": "run_started", "run_id": "run-crash"},
        {"event": "slot_started", "entry": entry, "run_id": "run-crash", "max_cost_usd": 0.46},
    )

    blocked = harness.run("run-0002")

    assert blocked.returncode == 1
    assert "charged their --max-cost (0.4600 USD)" in blocked.stderr
    assert "the plan has spent 1.9600 USD of 2.0 USD" in blocked.stderr

    accepted = harness.run("run-0003", "--accept-unknown-spend", entry)

    assert accepted.returncode == 3
    state = harness.header("run-0003")["spend"]["state"]
    assert state["spent_usd"] == pytest.approx(1.96)
    assert started(harness, "run-0003")[0]["max_cost_usd"] == 0.04


def test_an_acceptance_is_recorded_once(harness):
    _, task, treatment, _ = expected_keys()[0].split("/")
    harness.spec(jobs={f"--{SET}--{task}--{treatment}--a1": {"cost": None, "unpriced": []}})
    assert harness.run().returncode == 3
    harness.spec()
    entry = f"run-0001/{expected_keys()[0]}"
    assert harness.run("run-0002", "--accept-unknown-spend", entry).returncode == 0

    # Naming an already accepted slot again is allowed but adds nothing.
    again = harness.run("run-0003", "--accept-unknown-spend", entry)

    assert again.returncode == 0, again.stderr
    assert harness.header("run-0003")["spend"]["state"]["accepted_unknown_usd"] == {}
    acceptances = [e for e in harness.spend_ledger() if e["event"] == "accepted_unknown"]
    assert [a["entries"] for a in acceptances] == [[entry]]


def test_clean_host_help_says_a_cleaned_run_id_is_not_reusable(harness):
    result = harness.clean_host("--help")

    assert result.returncode == 0
    assert "A cleaned run id cannot be reused" in result.stdout
    assert "never touches the eval-4 spend ledger" in result.stdout


def test_clean_host_never_removes_the_ledger_even_when_a_record_names_it(harness):
    assert harness.run().returncode == 0
    record = harness.jobs / ".rustain-harbor" / "jobs"
    record.write_text(record.read_text() + "eval4-spend-eval-4-cmp-benign-v1.jsonl\n")
    before = ledger_file(harness).read_bytes()

    result = harness.clean_host("--apply", "--jobs-dir", str(harness.jobs), "--results")

    assert result.returncode == 0, result.stderr
    assert ledger_file(harness).read_bytes() == before


def test_clean_host_recognises_the_digest_pinned_dind_record(harness):
    harness.jobs.mkdir()
    record = harness.jobs / ".rustain-harbor"
    record.mkdir()
    (record / "pulled-image").write_text(DIND + "\n")
    harness.docker_state.write_text(
        json.dumps({"refs": {DIND: "sha256:dind"}, "digests": {"sha256:dind": []}})
    )

    result = harness.clean_host("--jobs-dir", str(harness.jobs))

    assert f"would remove: image {DIND} (pulled by an interrupted run)" in result.stdout


def test_concurrent_aggregations_never_tear_results(harness):
    assert harness.run().returncode == 0
    processes = [
        subprocess.Popen(
            ["bash", str(harness.dir / "run-baseline.sh"), "aggregate", str(harness.jobs), "run-0001"],
            env=harness.env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        for _ in range(6)
    ]
    errors = [process.communicate(timeout=300)[1] for process in processes]

    assert [process.returncode for process in processes] == [0] * 6, errors
    assert json.loads((harness.run_dir() / "results.json").read_text())["all"]["slots"] == 30
    assert not list(harness.run_dir().glob(".results.json*"))


def test_aggregation_does_not_depend_on_a_fixed_temporary_name(harness):
    assert harness.run().returncode == 0
    # Another writer's leftover at a fixed name must not break this writer.
    (harness.run_dir() / ".results.json.tmp").mkdir()

    result = harness.aggregate()

    assert result.returncode == 0, result.stderr
    assert json.loads((harness.run_dir() / "results.json").read_text())["all"]["slots"] == 30
