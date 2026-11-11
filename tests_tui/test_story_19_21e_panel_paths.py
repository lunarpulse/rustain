"""Real root dispatch/render controls for Story 19.21e review finding R2.

The Rust conformance tests exercise input routing, journal folds and widgets in
isolation. These PTYs also execute event_loop::run: both real panel chords and
slash commands must paint durable rows in Mission Control (Story 21.7: the Room and
Artifacts tabs replaced the sidebar panels, so the Artifacts rows are also proved at 80 columns,
where the sidebar never fit). No model turn, constructed TuiState or source-text pin is used.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

import harness
from harness import RustainTUI
from keys import CTRL_X, ENTER


def _seed_journal(journal: Path) -> None:
    """Use the production RoomEvent envelope, with stable fixture identities.

    As in test_story_18_2_transparency_log.py, the real writer creates the file
    first. It is stopped before this fixture replaces its contents, and the
    consumer starts only after the complete journal has been flushed to disk.
    """
    host = {"host_id": "old", "workspace_id": "fixture-workspace"}
    events = [
        {"event": "node_registered", "node": "proof-node", "origin": "Interactive", "host": host},
        {"event": "node_state_changed", "node": "proof-node", "from": "created", "to": "running"},
        {"event": "node_state_changed", "node": "proof-node", "from": "running", "to": "completed"},
        {"event": "node_registered", "node": "parked", "origin": "Interactive", "host": host},
        {"event": "node_state_changed", "node": "parked", "from": "created", "to": "suspended"},
    ]
    # Evidence and decision rows have no permission-mode-dependent apply suffix.
    # The second row's real depends_on edge must survive both rendering routes.
    for seed, kind, dependencies in [
        ("a", "evidence", []),
        ("b", "decision", ["a" * 64]),
    ]:
        events.append({
            "event": "artifact_created",
            "artifact": {
                "id": seed * 64,
                "kind": kind,
                "producer": "proof-node",
                "content_hash": seed * 64,
                "authority": [0] * 32,
                "provenance": ["user_originated"],
                "depends_on": dependencies,
                "review": None,
                "host": host,
            },
        })
    with journal.open("w", encoding="utf-8") as stream:
        for seq, event in enumerate(events, start=1):
            entry = {
                "schema_version": 1,
                "seq": seq,
                "recorded_at_ms": 1_700_000_000_000,
                "record": {"kind": "room", "payload": event},
            }
            stream.write(json.dumps(entry, separators=(",", ":")) + "\n")
        stream.flush()
        os.fsync(stream.fileno())


def _populated_tui(tmp_path, monkeypatch, cols: int):
    """Private config/home/workspace; harness honors RUSTAIN_TUI_BINARY too."""
    monkeypatch.setattr(harness, "TERM_COLS", cols)
    if cols == 80:
        monkeypatch.setattr(harness, "TERM_ROWS", 24)
    workspace = tmp_path / "workspace"
    config = workspace / ".rustain"
    config.mkdir(parents=True)
    (config / "config.toml").write_text('[permissions]\nalways_tools = []\n', encoding="utf-8")
    home = tmp_path / "home"
    home.mkdir()
    env = {"HOME": str(home), "RUSTAIN_PROFILE": ""}
    # Do not duplicate workspace_hash or assume the writer's journal identity.
    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as writer:
        assert writer.wait_for_screen("Ready", timeout=15), writer.get_screen_text()
    journals = sorted((config / "rooms").glob("*.jsonl"))
    assert len(journals) == 1, f"production writer must create one private room journal: {journals}"
    _seed_journal(journals[0])
    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui:
        assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
        yield tui


@pytest.fixture
def populated_panel_tui(tmp_path, monkeypatch, build_binary):
    """The Room tab at the standard 130 columns."""
    yield from _populated_tui(tmp_path, monkeypatch, 130)


@pytest.fixture(params=[130, 80], ids=["130", "80"])
def populated_artifacts_tui(request, tmp_path, monkeypatch, build_binary):
    """The Artifacts tab at 130 columns (list + detail) and 80 (list only; the sidebar never fit)."""
    yield from _populated_tui(tmp_path, monkeypatch, request.param)


def _assert_rows(tui: RustainTUI, rows: tuple[str, ...], diagnostic: str) -> None:
    # A failed wait returns False and reaches the semantic assertion below. A
    # setup/process failure instead raises and is not a valid native mutant kill.
    for row in rows:
        assert tui.wait_for_screen(row, timeout=10), (
            f"{diagnostic}\nMissing durable row: {row!r}\nScreen:\n{tui.get_screen_text()}"
        )
    # Rows, rather than the default panel title/zero-state, are the front-door
    # proof. This additional check records that the read was actually folded.
    assert "as of" in tui.get_screen_text(), tui.get_screen_text()


def _submit(tui: RustainTUI, command: str) -> None:
    tui.send(command)
    assert tui.wait_for_screen(command, timeout=5), tui.get_screen_text()
    # A bare slash command leaves autocomplete open. Story 21-2 AC4: Esc on a
    # popup-owned slash restores the pre-trigger composer (erasing the typed
    # command), so it can no longer dismiss the popup before submit. The first
    # Enter accepts the highlighted completion and closes the popup; the second
    # submits the actual root command. The helper paces both keys.
    tui.send_keys(ENTER, ENTER)


def test_room_chord_renders_durable_rows(populated_panel_tui: RustainTUI):
    tui = populated_panel_tui
    tui.send(CTRL_X + "r")
    _assert_rows(tui, ("✓ proof-node", "host-bound: old"),
                 "Room root chord must render the seeded durable nodes")


def _assert_artifact_rows(tui: RustainTUI) -> None:
    _assert_rows(tui, ("Mission Control · Artifacts", "evidence aaaaaa", "decision bbbbbb"),
                 "Artifacts must render the seeded durable artifacts in Mission Control")
    if harness.TERM_COLS >= 120:
        # The detail preview of the selected row carries the lineage edge.
        tui.send("j")
        assert tui.wait_for_screen("depends on aaaaaa", timeout=10), tui.get_screen_text()



def test_artifacts_chord_renders_durable_rows(populated_artifacts_tui: RustainTUI):
    tui = populated_artifacts_tui
    tui.send(CTRL_X + "e")
    _assert_artifact_rows(tui)


def test_room_slash_renders_durable_rows(populated_panel_tui: RustainTUI):
    tui = populated_panel_tui
    _submit(tui, "/room")
    _assert_rows(tui, ("✓ proof-node", "host-bound: old"),
                 "Room root slash command must render the seeded durable nodes")


def test_artifacts_slash_renders_durable_rows(populated_artifacts_tui: RustainTUI):
    tui = populated_artifacts_tui
    _submit(tui, "/artifacts")
    _assert_artifact_rows(tui)
