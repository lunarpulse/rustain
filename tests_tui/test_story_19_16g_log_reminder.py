"""Story 19.16g — the recipient learns a retract happened, on the real surface.

The real binary under a PTY, a real isolated workspace journal, and product-side
evidence only: the rendered status bar, the persisted seen-through preference
(`.rustain/transparency-seen.json`), and the scene provider's request log.

What this lane proves and what it does NOT:

- It proves the SURFACE: a closed log discovers a newer durable row at a later
  scheduled observation, renders a passive bounded `log: N`, keeps it through
  unrelated input and a streamed turn, clears it only through a displayed log
  visit, and keeps the boundary across a restart — on the standalone chord and
  slash rails and on the daemon-attached slash rail (writable and read-only).
- The rows are appended from this process under the journal writers' own
  `flock` protocol (lock sidecar, contiguous `seq`, fsync). ⚠ That is a
  stand-in for "another process durably appends", NOT the production writer:
  the production writer (`NodeJournal`) and the real served retract are proven
  by the Rust keystones `closed_log_observes_a_later_durable_retract` and
  `tests/a2a_server.rs::story_19_16g_a_served_retract_reaches_a_closed_log_reminder`.
- The latency printed below is measured, never promised: the client polls on a
  ≥ 1 s interval and nothing here is an NFR64 propagation claim.
"""

from __future__ import annotations

import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub
from harness import RustainTUI, _load_env, _resolve_binary
from keys import CTRL_X

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "smoke.json"
PEER = "12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"

pytestmark = pytest.mark.story_19_16g


# ── Helpers ──────────────────────────────────────────────────────────────────


def _wait_for(predicate, timeout: float = 15.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    value = predicate()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = predicate()
    return value


def _status_line(tui: RustainTUI) -> str:
    """The status bar row: the last screen line that carries the model chip."""
    lines = tui.get_screen_text().splitlines()
    candidates = [line for line in lines if " │ " in line]
    return candidates[-1] if candidates else ""


def _journal(workspace: Path) -> Path:
    rooms = sorted((workspace / ".rustain" / "rooms").glob("*.jsonl"))
    assert len(rooms) == 1, f"exactly one room journal expected: {rooms}"
    return rooms[0]


def _create_journal_with_the_writer(workspace: Path, env: dict[str, str]) -> Path:
    """Run the TUI once: its writer composition (`NodeJournal::open_workspace`)
    creates the room journal. The read-only `rustain team log` never does."""
    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env):
        pass
    return _journal(workspace)


def _append(journal: Path, payload: dict) -> int:
    """Append one room record under the writers' exclusive `flock`."""
    lock = os.open(journal.with_suffix(".lock"), os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        lines = [line for line in journal.read_text().splitlines() if line.strip()]
        seq = json.loads(lines[-1])["seq"] + 1 if lines else 1
        entry = {
            "schema_version": 1,
            "seq": seq,
            "recorded_at_ms": int(time.time() * 1000),
            "record": {"kind": "room", "payload": payload},
        }
        with open(journal, "a", encoding="utf-8") as out:
            out.write(json.dumps(entry, separators=(",", ":")) + "\n")
            out.flush()
            os.fsync(out.fileno())
        return seq
    finally:
        os.close(lock)


def _rejection(detail: str) -> dict:
    return {
        "event": "remote_envelope_rejected",
        "peer": PEER,
        "reason": {"reason": "policy", "detail": detail},
    }


def _retract(item: str) -> dict:
    return {
        "event": "recipient_item_retracted",
        "address": {"principal": {"kind": "a2a_pseudonym", "peer": PEER}, "item": item},
        "retracted_at_ms": int(time.time() * 1000),
        "principal_collapsed": False,
    }


def _seen(workspace: Path) -> dict | None:
    path = workspace / ".rustain" / "transparency-seen.json"
    return json.loads(path.read_text()) if path.exists() else None


def _turn_rows(stub: SceneStub) -> list[dict]:
    return [row for row in stub.rows() if "turn" in row]


@pytest.fixture
def stub(tmp_path):
    instance = SceneStub(SCENE, tmp_path / "requests.jsonl").start()
    try:
        yield instance
    finally:
        assert instance.stop() == 0, instance.rows()


@pytest.fixture
def scene_env(stub, tmp_path) -> dict[str, str]:
    home = tmp_path / "home"
    home.mkdir()
    return stub.env("scene-ci", home)


# ── Standalone rail ──────────────────────────────────────────────────────────


def test_standalone_reminder_discovers_clears_and_survives_restart(tmp_path, stub, scene_env):
    workspace = tmp_path / "ws"
    (workspace / ".rustain").mkdir(parents=True)
    # Monitor density: Focus hides the sidebar, so the chord's panel would
    # never be painted (and, correctly, never clear anything).
    (workspace / ".rustain" / "config.toml").write_text(
        '[permissions]\nalways_tools = ["Read"]\n\n[layout]\ndensity_mode = "monitor"\n'
    )
    journal = _create_journal_with_the_writer(workspace, scene_env)
    assert _seen(workspace) is None, "polling an unused log writes no preference"
    for index in range(3):
        _append(journal, _rejection(f"refused {index}"))

    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=scene_env) as tui:
        # First run: every existing row counts — never "start caught up".
        assert tui.wait_for_screen("log: 3", timeout=10), tui.get_screen_text()
        assert _status_line(tui).rstrip().endswith("log: 3"), _status_line(tui)

        # A retract arrives while the log is closed; no key is pressed.
        appended = time.monotonic()
        _append(journal, _retract("ri_scene_retracted"))
        assert tui.wait_for_screen("log: 4", timeout=10), tui.get_screen_text()
        print(f"measured append→render latency: {time.monotonic() - appended:.2f}s")

        # Unrelated input never clears it.
        tui.send("unrelated")
        for _ in "unrelated":
            tui.send("\x7f")
        time.sleep(2.5)
        assert "log: 4" in _status_line(tui), tui.get_screen_text()

        # Streaming positive control: a turn streams and completes normally
        # while the reminder is showing, and the reminder is untouched.
        tui.send_message("scene ping")
        assert tui.wait_for_screen("scene pong", timeout=30), tui.get_screen_text()
        assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
        assert "log: 4" in _status_line(tui), tui.get_screen_text()
        assert len(_turn_rows(stub)) == 1, stub.rows()

        # The displayed, unfiltered in-chat log clears it — durably.
        tui.send("/team log")
        tui.wait(0.3)
        tui.send("\r")
        assert tui.wait_for_screen("item-retracted", timeout=10), tui.get_screen_text()
        assert _wait_for(lambda: "log:" not in _status_line(tui), timeout=10), (
            tui.get_screen_text()
        )
        assert _wait_for(lambda: (_seen(workspace) or {}).get("seen_seq") == 4), _seen(
            workspace
        )
        assert len(_turn_rows(stub)) == 1, "a log visit is never a model turn"

    # Restart: the boundary survives, a later row counts one.
    _append(journal, _rejection("after restart"))
    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=scene_env) as tui:
        assert tui.wait_for_screen("log: 1", timeout=10), tui.get_screen_text()
        # The standalone chord clears it once the panel is drawn.
        tui.send(CTRL_X)
        tui.wait(0.2)
        tui.send("l")
        assert tui.wait_for_screen("Transparency Log", timeout=10), tui.get_screen_text()
        assert _wait_for(lambda: "log:" not in _status_line(tui), timeout=10), (
            tui.get_screen_text()
        )
        assert _wait_for(lambda: (_seen(workspace) or {}).get("seen_seq") == 5), _seen(
            workspace
        )

    # The offline CLI (even exporting) never touches the TUI's reminder.
    before = (workspace / ".rustain" / "transparency-seen.json").read_bytes()
    binary, _ = _resolve_binary()
    env = _load_env()
    env.update(scene_env)
    env["RUSTAIN_CONFIG_DIR"] = str(workspace / ".rustain")
    env["RUSTAIN_DATA_DIR"] = str(workspace / ".rustain_data")
    _append(journal, _rejection("cli observer"))
    subprocess.run(
        [str(binary), "team", "log", "--export"],
        cwd=workspace,
        env=env,
        capture_output=True,
        check=True,
        timeout=60,
    )
    assert (workspace / ".rustain" / "transparency.jsonl").exists()
    assert (workspace / ".rustain" / "transparency-seen.json").read_bytes() == before


# ── Daemon-attached rail ─────────────────────────────────────────────────────


def test_attached_slash_log_is_local_and_reachable_read_only(tmp_path, stub, scene_env):
    workspace = tmp_path / "ws"
    workspace.mkdir()
    journal = _create_journal_with_the_writer(workspace, scene_env)
    _append(journal, _rejection("before attach"))
    _append(journal, _retract("ri_attached"))

    # A Unix socket path is capped near 108 bytes: keep the daemon's data
    # directory (which holds its socket) short, and share it with the clients.
    data_dir = Path(tempfile.mkdtemp(prefix="r16g-", dir="/tmp"))
    scene_env = {**scene_env, "RUSTAIN_DATA_DIR": str(data_dir)}
    binary, _ = _resolve_binary()
    env = _load_env()
    env["RUSTAIN_CONFIG_DIR"] = str(workspace / ".rustain")
    env["RUSTAIN_LOG_PATH"] = str(workspace / "daemon-run.log")
    env.update(scene_env)
    started = subprocess.run(
        [str(binary), "daemon", "start"],
        cwd=workspace,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert started.returncode == 0, (started.stdout, started.stderr)
    writer = reader = None
    try:
        writer = RustainTUI(
            fresh=False,
            build=False,
            workspace=workspace,
            env_overrides=scene_env,
            extra_args=["daemon", "attach"],
        )
        assert _wait_for(lambda: _try_start(writer), timeout=30), "attach never connected"
        assert writer.wait_for_screen("attached", timeout=10), writer.get_screen_text()
        assert writer.wait_for_screen("log: 2", timeout=10), writer.get_screen_text()

        # A leading-space `/team log`: local rows, never a model turn.
        writer.send("  /team log")
        writer.wait(0.3)
        writer.send("\r")
        assert writer.wait_for_screen("item-retracted", timeout=10), writer.get_screen_text()
        assert _wait_for(lambda: "log:" not in _status_line(writer), timeout=10), (
            writer.get_screen_text()
        )
        assert _wait_for(lambda: (_seen(workspace) or {}).get("seen_seq") == 2), _seen(
            workspace
        )
        assert _turn_rows(stub) == [], "zero model turns: the log never reached the daemon"

        # A second, read-only client: composer editing and the local log work.
        _append(journal, _rejection("while attached"))
        assert writer.wait_for_screen("log: 1", timeout=10), writer.get_screen_text()
        reader = RustainTUI(
            fresh=False,
            build=False,
            workspace=workspace,
            env_overrides=scene_env,
            extra_args=["daemon", "attach"],
        ).start()
        assert reader.wait_for_screen("read-only", timeout=10), reader.get_screen_text()
        assert reader.wait_for_screen("log: 1", timeout=10), reader.get_screen_text()
        reader.send("hello model")
        reader.wait(0.3)
        reader.send("\r")
        assert reader.wait_for_screen("can't send here", timeout=5), reader.get_screen_text()
        for _ in "hello model":
            reader.send("\x7f")
        reader.send("/team log")
        reader.wait(0.3)
        reader.send("\r")
        assert reader.wait_for_screen("append-only", timeout=10), reader.get_screen_text()
        assert _wait_for(lambda: "log:" not in _status_line(reader), timeout=10), (
            reader.get_screen_text()
        )
        # The writer learns of the other client's visit at its next poll,
        # with no journal append.
        assert _wait_for(lambda: "log:" not in _status_line(writer), timeout=10), (
            writer.get_screen_text()
        )
        assert _seen(workspace)["seen_seq"] == 3
        assert _turn_rows(stub) == [], "the read-only client sent no model turn"
    finally:
        for client in (reader, writer):
            if client is not None:
                try:
                    client.send("\x1b")
                except Exception:
                    pass
                client.stop()
        subprocess.run(
            [str(binary), "daemon", "stop"],
            cwd=workspace,
            env=env,
            capture_output=True,
            timeout=60,
        )
        shutil.rmtree(data_dir, ignore_errors=True)


def _try_start(client: RustainTUI) -> bool:
    """Start the attach client once the daemon socket accepts it."""
    try:
        client.start()
    except Exception:
        return False
    time.sleep(0.5)
    if client._child is not None and client._child.isalive():
        return True
    client.stop()
    return False
