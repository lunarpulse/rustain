"""19.18f: real RAP ingress and /team log on an attached 80x24 debug TUI.

Reuse reciprocal tickets, daemon listeners and consent from the one-way RAP
smoke; the only provider stub handles boot probes, not the RAP front door.
"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import tempfile
import time
import traceback

import pytest

import test_story_19_20b_rap_one_way as rap_smoke

# The live acceptance notice already shows `via RAP` and the one-way summary,
# so only `/team log` output can prove the command: its rows carry a timestamp
# label (`format_unix_millis`) and the block ends with the structural-replay
# claim. Compared whitespace- and border-free, because the 80-column block wraps.
TEAM_LOG_CLAIM = "append-only,crash-safe,structurallyreplayable"
TEAM_LOG_RAP_ROW = re.compile(
    r"\d{4}-\d{2}-\d{2}\d{2}:\d{2}:\d{2}Z←inbound✓acceptedviaRAP·peer[0-9a-f]+·task[^·]+"
    r"·messagedeliveredoneway;notaskrunsandnothingissentback")


def _compact(screen):
    return "".join(ch for ch in screen if not ch.isspace() and ch not in "│┆")


def _read_team_log(tui, evidence):
    tui.child.send("/team log\r")
    deadline = time.monotonic() + 30
    while True:
        screen = tui.get_screen_text()
        compact = _compact(screen)
        assert "taskacceptedandexecuting" not in compact, screen
        if TEAM_LOG_CLAIM in compact and TEAM_LOG_RAP_ROW.search(compact):
            break
        assert time.monotonic() < deadline, screen
    (evidence / "team-log.screen.txt").write_text(screen)
    with (evidence / "assertions.txt").open("a") as stream:
        stream.write("PASS: /team log rendered (timestamped row + structural-replay claim) at 80x24.\n"
                     "PASS: its RAP accepted row reads via RAP and message delivered one way.\n"
                     "PASS: /team log never claims task accepted and executing.\n")


@pytest.mark.a2a_binary
@pytest.mark.timeout(240)
def test_rap_rail_is_named_in_attached_team_log_80x24(monkeypatch):
    configured = os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR")
    evidence = Path(configured or tempfile.mkdtemp(prefix="r18f-evidence-")).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    # Each run writes to its own directory (a fresh one when the caller names
    # none); only passing evidence is retained under implementation-artifacts.
    manifest = {"started_at": datetime.now(timezone.utc).isoformat(), "viewport": [80, 24],
                "result": "RUNNING", "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file(), binary
        assert "release" not in binary.parts, "debug a2a,p2p binary required"
        manifest.update({"binary": str(binary), "binary_sha256": rap_smoke._sha256(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r18f-", dir="/tmp") as workspace:
            rap_smoke._exercise(binary, evidence, Path(workspace), monkeypatch,
                                after_delivery=_read_team_log)
        assert rap_smoke._sha256(binary) == manifest["binary_sha256"], "binary changed during smoke"
    except BaseException:
        manifest.update({"result": "FAIL", "failure": traceback.format_exc()})
        raise
    else:
        manifest["result"] = "PASS"
    finally:
        manifest["finished_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "smoke-result.txt").write_text(manifest["result"] + "\n" + manifest.get("failure", ""))
        manifest["captures"] = {
            path.name: {"sha256": rap_smoke._sha256(path), "bytes": path.stat().st_size}
            for path in sorted(evidence.iterdir()) if path.is_file() and path != identity}
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
