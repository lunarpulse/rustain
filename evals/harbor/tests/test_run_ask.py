import json
import os
import subprocess
from pathlib import Path

import pytest

RUNNER = Path(__file__).resolve().parents[1] / "rustain_harbor" / "run-ask.sh"
SECRET = "sk-test-0123456789abcdef"

# A `rustain` that leaks its key the way an agent's `env` call does: into the
# stream, the saved session and stderr.
STUB_RUSTAIN = r"""#!/bin/sh
printf '{"type":"tool_result","content":"HOME=/root DEEPSEEK_API_KEY=%s PATH=/bin"}\n' "$DEEPSEEK_API_KEY"
mkdir -p .claude/sessions/s1
printf '{"content":"DEEPSEEK_API_KEY=%s"}\n' "$DEEPSEEK_API_KEY" > .claude/sessions/s1/conversation.json
echo "warning: key=$DEEPSEEK_API_KEY" >&2
"""


@pytest.fixture
def run(tmp_path):
    """Runs the runner in a fresh workspace with the stub on PATH; the key is
    handed over as a file, as the adapter does."""
    workspace, logs, bin_dir = tmp_path / "ws", tmp_path / "logs", tmp_path / "bin"
    for directory in (workspace, logs, bin_dir):
        directory.mkdir()
    stub = bin_dir / "rustain"
    stub.write_text(STUB_RUSTAIN)
    stub.chmod(0o755)
    instruction = tmp_path / "instruction.txt"
    instruction.write_text("fix the build")
    key_file = tmp_path / "api-key"
    key_file.write_text(SECRET)

    def invoke():
        return subprocess.run(
            ["bash", str(RUNNER), str(logs), str(instruction)],
            cwd=workspace,
            env={
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "RUSTAIN_HARBOR_KEY_ENV": "DEEPSEEK_API_KEY",
                "RUSTAIN_HARBOR_KEY_FILE": str(key_file),
            },
            capture_output=True,
            text=True,
        )

    invoke.workspace, invoke.logs, invoke.key_file = workspace, logs, key_file
    return invoke


def test_provider_key_never_reaches_the_collected_logs(run):
    result = run()

    assert result.returncode == 0
    collected = {p: p.read_text() for p in run.logs.rglob("*") if p.is_file()}
    assert not [p for p, text in collected.items() if SECRET in text]
    assert SECRET not in result.stderr
    # The stub saw the key, so it was read from the file, which is then gone.
    stream_event = json.loads(collected[run.logs / "rustain.stream.jsonl"])
    assert stream_event["content"] == (
        "HOME=/root DEEPSEEK_API_KEY=[redacted:DEEPSEEK_API_KEY] PATH=/bin"
    )
    assert "[redacted:DEEPSEEK_API_KEY]" in collected[
        run.logs / "sessions" / "s1" / "conversation.json"
    ]
    assert not run.key_file.exists()
    assert not (run.workspace / ".claude").exists()


def test_sessions_already_in_the_workspace_stay_there(run):
    earlier = run.workspace / ".claude" / "sessions" / "earlier"
    earlier.mkdir(parents=True)
    (earlier / "conversation.json").write_text('{"content":"kept"}\n')

    result = run()

    assert result.returncode == 0
    assert (earlier / "conversation.json").read_text() == '{"content":"kept"}\n'
    assert not (run.workspace / ".claude" / "sessions" / "s1").exists()
    assert (run.logs / "sessions" / "s1" / "conversation.json").is_file()
    assert not (run.logs / "sessions" / "earlier").exists()


def test_a_session_that_cannot_be_collected_is_kept_and_reported(run):
    # A file where the sessions directory should go makes the copy fail.
    (run.logs / "sessions").write_text("in the way")

    result = run()

    assert result.returncode == 3
    assert "could not collect session s1" in result.stderr
    kept = run.workspace / ".claude" / "sessions" / "s1" / "conversation.json"
    assert "[redacted:DEEPSEEK_API_KEY]" in kept.read_text()
