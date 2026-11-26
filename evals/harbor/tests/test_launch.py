"""The eval-4 launcher through its front door, `run-baseline.sh`, with
process-boundary doubles for uv, Harbor and the provider (conftest.py)."""

import json
import subprocess
import sys
import tomllib

from conftest import HARNESS, sha256

SET = "cmp-benign-v1"
TASKS = [
    "merge-diff-arc-agi-task",
    "schemelike-metacircular-eval",
    "mteb-retrieve",
    "circuit-fibsqrt",
    "build-pmars",
]
TREATMENTS = {"v4-flash": "deepseek/deepseek-v4-flash", "v4-pro": "deepseek/deepseek-v4-pro"}


def expected_keys() -> list[str]:
    return [
        f"{SET}/{task}/{label}/{attempt}"
        for attempt in (1, 2, 3)
        for task in TASKS
        for label in TREATMENTS
    ]


def started(harness, run_id="run-0001") -> list[dict]:
    return [e for e in harness.ledger(run_id) if e["event"] == "slot_started"]


def test_header_lists_every_slot_once_and_a_used_run_id_is_refused(harness):
    result = harness.run()

    assert result.returncode == 0, result.stderr
    header = harness.header()
    assert [slot["key"] for slot in header["slots"]] == expected_keys()
    assert header["attempts"] == 3 and header["concurrency"] == 1
    assert header["product"]["binary_sha256"] == sha256(harness.binary)
    assert header["harbor"]["version"] == "0.25.0"
    assert header["harbor"]["lock_sha256"] == sha256(harness.dir / "uv.lock")
    assert header["prices"]["sha256"] == sha256(
        harness.dir / "rustain_harbor" / "prices-2026-11-26.json"
    )
    assert header["manifest"]["sha256"] == sha256(
        harness.dir / "eval4" / "cmp-benign-v1.manifest.json"
    )
    assert header["spend"]["cap_usd"] == 14.0
    assert header["limits"]["max_turns"] == "none"
    assert header["task_timeouts_sec"]["circuit-fibsqrt"] == {"agent": 3600.0, "verifier": 3600.0}
    digest = sha256(harness.run_dir() / "header.json")
    assert (harness.run_dir() / "header.sha256").read_text().split()[0] == digest
    assert {event["header_sha256"] for event in harness.ledger()} == {digest}
    # The header was written before the provider key check and any Harbor call.
    assert harness.provider_log.read_text().splitlines() == [
        "https://api.deepseek.com/models"
    ] * 2

    header_bytes = (harness.run_dir() / "header.json").read_bytes()
    calls_before = len(harness.harbor_calls())
    again = harness.run()

    assert again.returncode == 1
    assert "used once" in again.stderr
    assert (harness.run_dir() / "header.json").read_bytes() == header_bytes
    assert len(harness.harbor_calls()) == calls_before


def test_each_slot_is_one_harbor_job_and_the_recorded_argv_is_the_executed_argv(harness):
    result = harness.run()

    assert result.returncode == 0, result.stderr
    assert harness.uv_log.read_text().split()[:3] == ["sync", "--locked", "--no-install-project"]
    header = harness.header()
    calls = harness.harbor_calls()
    events = started(harness)
    assert len(events) == 30 and len(calls) == 30
    for slot, event in zip(header["slots"], events, strict=True):
        assert event["slot"] == slot["key"]
        assert slot["job_name"] == (
            f"run-0001--{SET}--{slot['task']}--{slot['treatment']}--a{slot['attempt']}"
        )
        argv = calls[slot["job_name"]]
        assert argv == event["argv"]
        assert argv[0] == header["harbor"]["executable"]
        assert argv[1] == "run"
        option = {argv[i]: argv[i + 1] for i in range(2, len(argv) - 1, 2)}
        assert len(argv) == 2 + 2 * len(option) + 2  # two --ak options
        assert option["--n-attempts"] == "1"
        assert option["--max-retries"] == "0"
        assert option["--n-concurrent"] == "1"
        assert option["--model"] == TREATMENTS[slot["treatment"]]
        assert option["--task"] == slot["task_ref"]
        assert option["--task"].startswith(f"terminal-bench/{slot['task']}@sha256:")
        assert option["--job-name"] == slot["job_name"]
        assert option["--agent"] == "rustain_harbor.agent:RustainAgent"
        assert argv.count("--ak") == 2
        assert f"binary_path={harness.binary}" in argv
        assert "max_cost_usd=0.46" in argv


def test_package_digests_in_the_argv_come_from_the_manifest(harness):
    manifest = json.loads((HARNESS / "eval4" / "cmp-benign-v1.manifest.json").read_text())

    assert harness.run().returncode == 0
    refs = {slot["task"]: slot["task_ref"] for slot in harness.header()["slots"]}
    assert refs == {
        task["name"]: f"terminal-bench/{task['name']}@{task['package_digest']}"
        for task in manifest["tasks"]
    }


def test_no_free_argument_reaches_harbor(harness):
    run = harness.launcher(
        "run", str(harness.binary), str(harness.jobs), "run-0001", "--n-attempts", "3"
    )
    check = harness.launcher("check", str(harness.binary), str(harness.jobs), "run-0001", "-x")

    assert run.returncode == 2 and check.returncode == 2
    assert "usage" in run.stderr
    assert not harness.jobs.exists()
    assert harness.harbor_calls() == {}


def test_a_failed_or_crashed_slot_is_not_retried_into_the_run(harness):
    harness.spec(
        jobs={
            f"--{SET}--mteb-retrieve--v4-flash--a1": {
                "exit": 1,
                "exception": "NonZeroAgentExitCodeError",
                "reward": 0.0,
            }
        }
    )

    assert harness.run().returncode == 0
    names = [event["job_name"] for event in started(harness)]
    assert len(names) == len(set(names)) == 30
    finished = {e["job_name"]: e for e in harness.ledger() if e["event"] == "slot_finished"}
    failed = f"run-0001--{SET}--mteb-retrieve--v4-flash--a1"
    assert finished[failed]["harbor_exit"] == 1
    assert "--max-retries" in harness.harbor_calls()[failed]


def test_spend_guard_stops_at_the_cap_and_narrows_the_last_max_cost(harness):
    harness.edit_plan(lambda plan: plan["spend"].update(cap_usd=1.0))
    harness.spec(default={"cost": 0.4})

    result = harness.run()

    assert result.returncode == 3
    assert "spend cap reached" in result.stderr
    events = started(harness)
    assert [e["max_cost_usd"] for e in events] == [0.46, 0.46, 0.2]
    assert [arg for arg in events[2]["argv"] if arg.startswith("max_cost_usd=")] == [
        "max_cost_usd=0.2"
    ]
    assert len(harness.harbor_calls()) == 3
    stop = [e for e in harness.ledger() if e["event"] == "stopped"]
    assert stop[0]["slot"] == expected_keys()[3]
    # The slots that did not run are missing, so aggregation refuses totals.
    assert f"missing: {expected_keys()[3]}" in result.stderr
    assert not (harness.run_dir() / "results.json").exists()


def test_spend_guard_stops_after_a_slot_whose_cost_is_unknown(harness):
    second = expected_keys()[1].split("/")
    harness.spec(
        jobs={f"--{SET}--{second[1]}--{second[2]}--a1": {"cost": None, "unpriced": []}}
    )

    result = harness.run()

    assert result.returncode == 3
    assert f"{expected_keys()[1]} cost is usage_unknown" in result.stderr
    assert len(harness.harbor_calls()) == 2


def test_spend_guard_stops_after_a_slot_without_a_result(harness):
    first = expected_keys()[0].split("/")
    harness.spec(jobs={f"--{SET}--{first[1]}--{first[2]}--a1": {"results": 0, "exit": 1}})

    result = harness.run()

    assert result.returncode == 3
    assert f"{expected_keys()[0]} cost is missing" in result.stderr
    assert len(harness.harbor_calls()) == 1


def test_rejected_key_stops_before_any_harbor_job(harness):
    result = harness.run(key="sk-test-rejected-9999")

    assert result.returncode == 1
    assert "rejected DEEPSEEK_API_KEY (HTTP 401)" in result.stderr
    assert (harness.run_dir() / "header.json").is_file()
    assert harness.harbor_calls() == {}


def test_binary_other_than_the_plans_is_refused_before_the_header(harness):
    harness.binary.write_bytes(b"another build")

    result = harness.run()

    assert result.returncode == 1
    assert "the plan requires" in result.stderr
    assert not harness.run_dir().exists()


def test_check_is_offline_and_writes_nothing(harness):
    result = harness.launcher("check", str(harness.binary), str(harness.jobs), "run-0001")

    assert result.returncode == 0, result.stderr
    assert not harness.jobs.exists()
    assert not harness.uv_log.exists()
    assert not harness.provider_log.exists()
    lines = result.stdout.splitlines()
    argv_lines = [line for line in lines if line.startswith(f"{SET}/")]
    assert [line.split(" ", 1)[0] for line in argv_lines] == expected_keys()


def test_harbor_is_pinned_to_0_25_0_with_hashes():
    pyproject = tomllib.loads((HARNESS / "pyproject.toml").read_text())
    lock = tomllib.loads((HARNESS / "uv.lock").read_text())

    assert "harbor==0.25.0" in pyproject["project"]["dependencies"]
    (harbor,) = [p for p in lock["package"] if p["name"] == "harbor"]
    assert harbor["version"] == "0.25.0"
    assert harbor["wheels"][0]["hash"] == (
        "sha256:51923ac567fc57b03279e93468b105f689628bcac2cd3d541d4e44aa5c0ec309"
    )
    assert all(
        "hash" in artifact
        for package in lock["package"]
        if "registry" in package.get("source", {})
        for artifact in [*package.get("wheels", []), *filter(None, [package.get("sdist")])]
    )


AUDIT = """
import sys
events = []
sys.addaudithook(
    lambda event, args: events.append(event)
    if event in ("socket.connect", "socket.getaddrinfo", "urllib.Request")
    else None
)
import rustain_harbor.agent as agent
assert agent.PRICE_TABLE.prices("deepseek", "deepseek-v4-pro") is not None
print(events, "litellm" in sys.modules)
"""


def test_importing_the_adapter_makes_no_network_request():
    result = subprocess.run(
        [sys.executable, "-c", AUDIT],
        cwd=HARNESS,
        env={"PATH": "/usr/bin:/bin", "PYTHONPATH": str(HARNESS)},
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "[] False"


def test_offline_entries_import_no_harbor():
    code = (
        "import sys, rustain_harbor.launch, rustain_harbor.baseline;"
        "print(sorted(m for m in ('harbor', 'litellm', 'pydantic') if m in sys.modules))"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        env={"PATH": "/usr/bin:/bin", "PYTHONPATH": str(HARNESS)},
        capture_output=True,
        text=True,
    )

    assert result.stdout.strip() == "[]", result.stderr


def test_run_id_must_be_filesystem_safe(harness):
    result = harness.run(run_id="../escape")

    assert result.returncode == 1
    assert "run id" in result.stderr
    assert not (harness.root / "escape").exists()


def test_preflight_runs_once_per_model_before_the_first_slot(harness):
    assert harness.run().returncode == 0
    # Both treatments share the DeepSeek key check: one request per model.
    assert harness.provider_log.read_text().splitlines() == [
        "https://api.deepseek.com/models"
    ] * 2
