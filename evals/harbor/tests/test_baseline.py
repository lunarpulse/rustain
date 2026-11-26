"""Aggregation through `run-baseline.sh aggregate` on runs the launcher made
with the Harbor double: outcome, completeness and cost axes, and refusal of
an incomplete or altered matrix."""

import json
import shutil

import pytest

SET = "cmp-benign-v1"


def job_suffix(task: str, treatment: str, attempt: int) -> str:
    return f"--{SET}--{task}--{treatment}--a{attempt}"


def slot_row(results: dict, key: str) -> dict:
    (row,) = [row for row in results["slots"] if row["slot"] == key]
    return row


def trial_result_path(harness, task: str, treatment: str, attempt: int):
    job = harness.run_dir() / "jobs" / f"run-0001{job_suffix(task, treatment, attempt)}"
    (trial,) = [path for path in job.iterdir() if path.is_dir()]
    return trial / "result.json"


def test_outcomes_are_classified_and_an_exception_is_never_a_pass(harness):
    harness.spec(
        default={"reward": 1.0, "cost": 0.05},
        jobs={
            job_suffix("build-pmars", "v4-flash", 1): {
                "reward": 1.0,
                "exception": "AgentTimeoutError",
            },
            job_suffix("build-pmars", "v4-pro", 1): {
                "reward": 1.0,
                "exception": "NonZeroAgentExitCodeError",
                "limit_reason": "max_cost",
            },
            job_suffix("mteb-retrieve", "v4-flash", 2): {"reward": None},
            job_suffix("mteb-retrieve", "v4-pro", 2): {
                "reward": 1.0,
                "exception": "RuntimeError",
            },
            job_suffix("circuit-fibsqrt", "v4-pro", 3): {"reward": 0.0},
            job_suffix("circuit-fibsqrt", "v4-flash", 3): {
                "reward": 0.0,
                "exception": "VerifierTimeoutError",
            },
        },
    )

    result = harness.run()

    assert result.returncode == 0, result.stderr
    results = harness.results()
    row = slot_row(results, f"{SET}/build-pmars/v4-flash/1")
    assert (row["outcome"], row["exception_with_reward"]) == ("harbor_timeout", True)
    assert slot_row(results, f"{SET}/build-pmars/v4-pro/1")["outcome"] == "limit_stop"
    assert slot_row(results, f"{SET}/mteb-retrieve/v4-flash/2")["outcome"] == "verifier_absent"
    row = slot_row(results, f"{SET}/mteb-retrieve/v4-pro/2")
    assert (row["outcome"], row["exception_with_reward"]) == ("harbor_exception", True)
    assert slot_row(results, f"{SET}/circuit-fibsqrt/v4-pro/3")["outcome"] == "fail"
    assert slot_row(results, f"{SET}/circuit-fibsqrt/v4-flash/3")["outcome"] == "harbor_timeout"

    flash, pro = results["by_treatment"]["v4-flash"], results["by_treatment"]["v4-pro"]
    assert flash["slots"] == pro["slots"] == 15
    assert flash["outcomes"] == {
        "pass": 12,
        "fail": 0,
        "harbor_timeout": 2,
        "harbor_exception": 0,
        "limit_stop": 0,
        "verifier_absent": 1,
    }
    assert pro["outcomes"]["pass"] == 12
    assert pro["exception_with_reward"] == 2
    assert flash["pass_rate"] == pytest.approx(12 / 15)
    assert flash["total_cost_usd"] == pytest.approx(15 * 0.05)
    assert flash["cost_per_solved_usd"] == pytest.approx(15 * 0.05 / 12)
    assert results["all"]["slots"] == 30
    assert results["prices"]["id"] == "deepseek-2026-11-26"
    assert results["comparison_set"]["label"] == SET
    assert "## v4-pro (deepseek/deepseek-v4-pro)" in result.stdout


@pytest.mark.parametrize(
    ("spec", "status"),
    [
        ({"cost": None, "unpriced": []}, "usage_unknown"),
        ({"cost": None, "unpriced": ["deepseek-v4-pro"]}, "unpriced"),
    ],
)
def test_unknown_cost_is_never_zero_and_blocks_cost_totals(harness, spec, status):
    # The last slot: the spend guard has no later slot to stop.
    harness.spec(jobs={job_suffix("build-pmars", "v4-pro", 3): {**spec, "reward": 1.0}})

    assert harness.run().returncode == 0
    results = harness.results()
    pro = results["by_treatment"]["v4-pro"]
    assert slot_row(results, f"{SET}/build-pmars/v4-pro/3")["cost_status"] == status
    assert pro["cost_status"][status] == 1
    assert pro["total_cost_usd"] is None
    assert pro["cost_per_solved_usd"] is None
    assert results["all"]["total_cost_usd"] is None
    # Outcome counts still stand.
    assert pro["outcomes"]["pass"] == 15
    assert results["by_treatment"]["v4-flash"]["total_cost_usd"] == pytest.approx(0.75)


def test_a_missing_slot_is_named_and_blocks_totals_until_it_is_restored(harness):
    assert harness.run().returncode == 0
    assert harness.aggregate().returncode == 0
    path = trial_result_path(harness, "schemelike-metacircular-eval", "v4-pro", 2)
    kept = path.read_bytes()
    path.unlink()

    refused = harness.aggregate()

    assert refused.returncode == 1
    assert f"missing: {SET}/schemelike-metacircular-eval/v4-pro/2" in refused.stderr
    assert "no totals" in refused.stderr
    assert refused.stdout == ""
    assert not (harness.run_dir() / "results.json").exists()

    path.write_bytes(kept)
    restored = harness.aggregate()

    assert restored.returncode == 0, restored.stderr
    assert harness.results()["all"]["slots"] == 30


def test_a_second_trial_result_in_a_slot_is_a_duplicate(harness):
    assert harness.run().returncode == 0
    path = trial_result_path(harness, "mteb-retrieve", "v4-flash", 1)
    shutil.copytree(path.parent, path.parent.with_name(path.parent.name + "-again"))

    result = harness.aggregate()

    assert result.returncode == 1
    assert f"duplicate: {SET}/mteb-retrieve/v4-flash/1 has 2 trial results" in result.stderr


def test_a_job_from_another_run_is_unexpected(harness):
    assert harness.run().returncode == 0
    stray = harness.run_dir() / "jobs" / f"run-0002{job_suffix('mteb-retrieve', 'v4-flash', 1)}"
    stray.mkdir()

    result = harness.aggregate()

    assert result.returncode == 1
    assert f"unexpected: {stray.name} is not a slot of this run" in result.stderr


@pytest.mark.parametrize(
    ("override", "problem"),
    [
        ({"model": "deepseek/deepseek-v4-flash"}, "model deepseek/deepseek-v4-flash"),
        ({"task_name": "terminal-bench/build-pmars"}, "task terminal-bench/build-pmars"),
        ({"ref": "sha256:" + "0" * 64}, "package sha256:"),
    ],
)
def test_a_result_that_does_not_match_its_slot_is_unexpected(harness, override, problem):
    harness.spec(jobs={job_suffix("mteb-retrieve", "v4-pro", 1): override})

    result = harness.run()

    assert result.returncode == 1
    assert f"unexpected: {SET}/mteb-retrieve/v4-pro/1 holds {problem}" in result.stderr


def test_an_unreadable_result_is_unidentifiable(harness):
    assert harness.run().returncode == 0
    trial_result_path(harness, "build-pmars", "v4-flash", 3).write_text("{not json")

    result = harness.aggregate()

    assert result.returncode == 1
    assert f"unidentifiable: {SET}/build-pmars/v4-flash/3" in result.stderr


def test_an_altered_header_is_refused(harness):
    assert harness.run().returncode == 0
    header_path = harness.run_dir() / "header.json"
    header = json.loads(header_path.read_text())
    header["spend"]["cap_usd"] = 100.0
    header_path.chmod(0o644)
    header_path.write_text(json.dumps(header, indent=2, sort_keys=True) + "\n")

    result = harness.aggregate()

    assert result.returncode == 1
    assert "header.json does not match header.sha256" in result.stderr
    assert "belongs to another header" in result.stderr


def test_an_absent_header_is_refused(harness):
    result = harness.aggregate("run-0404")

    assert result.returncode == 1
    assert "header" in result.stderr and "is absent" in result.stderr


def test_a_recorded_argv_the_header_does_not_give_is_refused(harness):
    assert harness.run().returncode == 0
    ledger = harness.run_dir() / "ledger.jsonl"
    events = [json.loads(line) for line in ledger.read_text().splitlines()]
    first = next(e for e in events if e["event"] == "slot_started")
    first["argv"] = [*first["argv"], "--n-attempts", "3"]
    ledger.write_text("".join(json.dumps(e, sort_keys=True) + "\n" for e in events))

    result = harness.aggregate()

    assert result.returncode == 1
    assert f"argv: {first['slot']} recorded an argv the header does not give" in result.stderr
