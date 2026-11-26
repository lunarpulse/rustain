"""Process-boundary doubles for the eval-4 launcher tests.

`harness` copies evals/harbor into a temporary directory, sets the copied run
plan's binary hash to a stand-in binary, and puts doubles on PATH:

- `uv`: records its argv and builds the "locked environment": a `python` that
  is this test interpreter, and a `harbor`.
- `harbor`: records its argv per job and writes the job directory Harbor
  0.25.0 would (`config.json`, the job `result.json`, one trial directory with
  `result.json`), shaped by a per-job spec. It never starts a container.
- the provider: `sitecustomize.py`, loaded by the environment's `python`,
  answers the key check at https://api.deepseek.com/models and fails any other
  request, so no test reaches the network.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

HARNESS = Path(__file__).resolve().parents[1]
GOOD_KEY = "sk-test-accepted-0123"

HARBOR_DOUBLE = r'''
import json, os, sys
from pathlib import Path

args = sys.argv[1:]
def option(name):
    return [args[i + 1] for i, a in enumerate(args) if a == name]
job_name = option("--job-name")[0]
jobs_dir = Path(option("--jobs-dir")[0])
task_ref = option("--task")[0]
model = option("--model")[0]
log = Path(os.environ["HARBOR_DOUBLE_LOG"])
log.mkdir(parents=True, exist_ok=True)
(log / f"{job_name}.argv.json").write_text(json.dumps([os.environ["HARBOR_DOUBLE_ARGV0"], *args]))

specs = json.loads(Path(os.environ["HARBOR_DOUBLE_SPEC"]).read_text())
spec = dict(specs.get("default", {}))
for suffix, override in specs.get("jobs", {}).items():
    if job_name.endswith(suffix):
        spec.update(override)

task_name, _, digest = task_ref.partition("@")
org, _, short = task_name.partition("/")
job = jobs_dir / job_name
job.mkdir(parents=True, exist_ok=True)
(job / "config.json").write_text(json.dumps({"job_name": job_name, "n_attempts": 1}))
(job / "result.json").write_text(json.dumps({"n_total_trials": 1}))
for index in range(spec.get("results", 1)):
    trial = job / f"{short[:32]}__trial{index}"
    trial.mkdir()
    reward = spec.get("reward", 1.0)
    exception = spec.get("exception")
    result = {
        "task_name": spec.get("task_name", task_name),
        "trial_name": trial.name,
        "task_id": {"org": org, "name": short, "ref": spec.get("ref", digest)},
        "config": {"agent": {"model_name": spec.get("model", model)}},
        "agent_info": {
            "name": "rustain",
            "version": "0.6.0+512509c",
            "model_info": {"name": model.partition("/")[2], "provider": "deepseek"},
        },
        "verifier_result": None if reward is None else {"rewards": {"reward": reward}},
        "exception_info": None if exception is None else {"exception_type": exception},
        "agent_result": {
            "n_input_tokens": spec.get("input_tokens", 1000),
            "n_cache_tokens": 400,
            "n_output_tokens": 100,
            "cost_usd": spec.get("cost", 0.05),
            "metadata": {
                "provider_calls": spec.get("provider_calls", 3),
                "first_call_input_tokens": 7000,
                "limit_reason": spec.get("limit_reason"),
                "unpriced_models": spec.get("unpriced", []),
            },
        },
        "agent_execution": {
            "started_at": "2026-11-27T10:00:00+00:00",
            "finished_at": "2026-11-27T10:01:30+00:00",
        },
    }
    (trial / "result.json").write_text(json.dumps(result))
sys.exit(spec.get("exit", 0))
'''

SITECUSTOMIZE = r'''
import io, os, urllib.error, urllib.request

def _urlopen(request, *args, **kwargs):
    url = getattr(request, "full_url", request)
    with open(os.environ["PROVIDER_DOUBLE_LOG"], "a") as log:
        log.write(url + "\n")
    if url != "https://api.deepseek.com/models":
        raise OSError(f"test double: no network for {url}")
    if request.get_header("Authorization") != "Bearer %s":
        raise urllib.error.HTTPError(url, 401, "Unauthorized", {}, io.BytesIO(b"{}"))
    return urllib.request.addinfourl(io.BytesIO(b"{}"), {}, url, 200)

urllib.request.urlopen = _urlopen
''' % GOOD_KEY


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Harness:
    def __init__(self, root: Path):
        self.root = root
        self.dir = root / "harbor"
        self.jobs = root / "jobs"
        self.doubles = root / "doubles"
        self.binary = root / "rustain-musl"
        self.spec_file = self.doubles / "spec.json"
        self.harbor_log = self.doubles / "harbor-argv"
        self.provider_log = self.doubles / "provider.log"
        self.uv_log = self.doubles / "uv.argv"

    @property
    def plan_path(self) -> Path:
        return self.dir / "eval4" / "run-plan.json"

    def edit_plan(self, change) -> None:
        plan = json.loads(self.plan_path.read_text())
        change(plan)
        self.plan_path.write_text(json.dumps(plan, indent=2) + "\n")

    def spec(self, default: dict | None = None, jobs: dict | None = None) -> None:
        self.spec_file.write_text(json.dumps({"default": default or {}, "jobs": jobs or {}}))

    def env(self, key: str | None = GOOD_KEY) -> dict[str, str]:
        env = {
            "PATH": f"{self.doubles / 'bin'}:/usr/bin:/bin",
            "HOME": str(self.root / "home"),
            "HARBOR_DOUBLE_LOG": str(self.harbor_log),
            "HARBOR_DOUBLE_SPEC": str(self.spec_file),
            "PROVIDER_DOUBLE_LOG": str(self.provider_log),
        }
        if key is not None:
            env["DEEPSEEK_API_KEY"] = key
        return env

    def launcher(self, *args: str, key: str | None = GOOD_KEY) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(self.dir / "run-baseline.sh"), *args],
            env=self.env(key),
            capture_output=True,
            text=True,
            timeout=600,
        )

    def run(self, run_id: str = "run-0001", **kwargs) -> subprocess.CompletedProcess:
        return self.launcher("run", str(self.binary), str(self.jobs), run_id, **kwargs)

    def aggregate(self, run_id: str = "run-0001") -> subprocess.CompletedProcess:
        return self.launcher("aggregate", str(self.jobs), run_id)

    def run_dir(self, run_id: str = "run-0001") -> Path:
        return self.jobs / run_id

    def ledger(self, run_id: str = "run-0001") -> list[dict]:
        return [
            json.loads(line)
            for line in (self.run_dir(run_id) / "ledger.jsonl").read_text().splitlines()
        ]

    def header(self, run_id: str = "run-0001") -> dict:
        return json.loads((self.run_dir(run_id) / "header.json").read_text())

    def harbor_calls(self) -> dict[str, list[str]]:
        if not self.harbor_log.is_dir():
            return {}
        return {
            path.name.removesuffix(".argv.json"): json.loads(path.read_text())
            for path in self.harbor_log.iterdir()
        }

    def results(self, run_id: str = "run-0001") -> dict:
        return json.loads((self.run_dir(run_id) / "results.json").read_text())


def _write_executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o755)


@pytest.fixture
def harness(tmp_path) -> Harness:
    h = Harness(tmp_path)
    shutil.copytree(
        HARNESS,
        h.dir,
        ignore=shutil.ignore_patterns(".venv", "__pycache__", ".pytest_cache", "tests"),
    )
    h.binary.write_bytes(b"\x7fELF stand-in for the static rustain binary\n")
    h.edit_plan(lambda plan: plan["product"].update(binary_sha256=sha256(h.binary)))

    bin_dir = h.doubles / "bin"
    bin_dir.mkdir(parents=True)
    site = h.doubles / "site"
    site.mkdir()
    (site / "sitecustomize.py").write_text(SITECUSTOMIZE)
    (h.doubles / "harbor.py").write_text(HARBOR_DOUBLE)
    os.symlink(sys.executable, bin_dir / "python3")
    _write_executable(
        bin_dir / "uv",
        f"""#!/bin/sh
printf '%s\\n' "$*" >>"{h.uv_log}"
venv="$UV_PROJECT_ENVIRONMENT"
mkdir -p "$venv/bin"
cat >"$venv/bin/python" <<'EOF'
#!/bin/sh
PYTHONPATH="${{PYTHONPATH:+$PYTHONPATH:}}{site}" exec "{sys.executable}" "$@"
EOF
cat >"$venv/bin/harbor" <<'EOF'
#!/bin/sh
HARBOR_DOUBLE_ARGV0="$0" exec "{sys.executable}" "{h.doubles / 'harbor.py'}" "$@"
EOF
chmod 755 "$venv/bin/python" "$venv/bin/harbor"
""",
    )
    h.spec()
    return h
