import json

import pytest

from rustain_harbor.baseline import load_trials, summarize


def trial_result(task, reward, cost, first_call, exception=None, tokens=1000):
    """The subset of Harbor's `TrialResult` JSON that the baseline reads."""
    return {
        "task_name": task,
        "verifier_result": None if reward is None else {"rewards": {"reward": reward}},
        "exception_info": None if exception is None else {"exception_type": exception},
        "agent_result": {
            "n_input_tokens": tokens,
            "n_cache_tokens": None if tokens is None else 400,
            "n_output_tokens": None if tokens is None else 100,
            "cost_usd": cost,
            "metadata": {"provider_calls": 3, "first_call_input_tokens": first_call},
        },
        "agent_execution": {
            "started_at": "2026-11-13T10:00:00Z",
            "finished_at": "2026-11-13T10:01:30Z",
        },
    }


def write_job(tmp_path, results):
    for index, result in enumerate(results):
        trial_dir = tmp_path / f"trial-{index}"
        trial_dir.mkdir()
        (trial_dir / "result.json").write_text(json.dumps(result))
    # Harbor also writes a job-level result.json next to the trial directories.
    (tmp_path / "result.json").write_text(json.dumps({"n_total_trials": len(results)}))


def test_cost_per_solved_task_counts_money_spent_on_failures(tmp_path):
    write_job(
        tmp_path,
        [
            trial_result("a", 1.0, 0.10, 7000),
            trial_result("b", 0.0, 0.30, 7200),
            trial_result("c", None, 0.20, 9000, exception="AgentTimeoutError"),
        ],
    )

    trials = load_trials(tmp_path)
    summary = summarize(trials)

    assert [t.task for t in trials] == ["a", "b", "c"]
    assert summary["trials"] == 3
    assert summary["passed"] == 1
    assert summary["total_cost_usd"] == pytest.approx(0.60)
    assert summary["cost_per_trial_usd"] == pytest.approx(0.20)
    assert summary["cost_per_solved_usd"] == pytest.approx(0.60)
    assert summary["median_first_call_input_tokens"] == 7200
    assert trials[2].error == "AgentTimeoutError"
    assert trials[0].agent_seconds == 90


def test_no_passes_and_no_prices_leave_ratios_empty(tmp_path):
    write_job(tmp_path, [trial_result("a", 0.0, None, 7000)])

    summary = summarize(load_trials(tmp_path))

    assert summary["passed"] == 0
    assert summary["total_cost_usd"] is None
    assert summary["cost_per_solved_usd"] is None


def test_a_trial_without_usage_leaves_the_job_totals_empty(tmp_path):
    write_job(
        tmp_path,
        [
            trial_result("a", 1.0, 0.10, 7000),
            trial_result("b", 1.0, None, None, tokens=None),
        ],
    )

    summary = summarize(load_trials(tmp_path))

    assert summary["passed"] == 2
    assert summary["trials_with_cost"] == 1
    assert summary["total_cost_usd"] is None
    assert summary["cost_per_trial_usd"] is None
    assert summary["cost_per_solved_usd"] is None
    assert summary["input_tokens"] is None
    assert summary["median_first_call_input_tokens"] == 7000
