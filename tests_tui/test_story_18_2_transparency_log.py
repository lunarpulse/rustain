"""Story 18-2: Transparency & Visible Surfacing — TUI contract tests.

Deterministic (non-API) coverage for the ``Ctrl+X, L`` Messages surface (Story 21.7: Mission
Control's Messages tab, the A2A log's one home) and the ``/team log`` in-chat surface. These
mirror the Story 10-4 subagent panel tests: real chord, real slash command, real render — no
live LLM.

Chord reference: ``Ctrl+X, L`` → ``InputAction::OpenMessages`` (local host). Before 18.2 the
same chord was a ``ChordAction::Noop("Log panel — Epic 14")`` that the help screen advertised
as ``available: true`` — a rendered, documented binding backed by nothing. The first test
below is what makes that impossible to ship again.

⚠ **What these tests deliberately do NOT assert.**
Peer-controlled text reaching the terminal without escape sequences is AC8, and
it CANNOT be tested here: ``pyte`` *interprets* escape sequences, so a screen
scrape sees the rendered result and passes regardless of what was written. That
keystone asserts on **bytes**, in
``tests/conformance_transparency_surfacing.rs::team_log_stdout_carries_no_control_bytes``.
"""

from __future__ import annotations

import subprocess
import tempfile
from pathlib import Path

import pytest

from harness import PROJECT_ROOT, RustainTUI, _resolve_binary
from keys import CTRL_X


MESSAGES_TITLE = "Mission Control · Messages"


def _chord_ctrl_x_l(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 'l' → OpenMessages."""
    tui.send(CTRL_X)
    tui.wait_until(
        lambda s: any(line.strip().startswith("┌") and "Ctrl+X" in line for line in s.split("\n")),
        "the Ctrl+X leader menu",
    )
    tui.send("l")
    tui.wait_for_screen(MESSAGES_TITLE, timeout=5.0)


# ── Scenario 1: the chord opens a real panel, not a "not yet available" block ──


@pytest.mark.story_18_2
def test_chord_opens_transparency_log_panel(tui_monitor: RustainTUI):
    """Ctrl+X, L at the standard 130-col terminal opens Mission Control's Messages tab."""
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    tui.assert_screen_contains(
        MESSAGES_TITLE,
        msg="Ctrl+X, L must open the Messages frame (it was a Noop before 18.2)",
    )
    screen = tui.get_screen_text()
    assert "Epic 14" not in screen, (
        "the stale 'Log panel — Epic 14' Noop copy must be gone. " f"Screen:\n{screen}"
    )


# ── Scenario 4: the chord toggles the panel closed ──────────────────────────


@pytest.mark.story_18_2
def test_chord_toggles_panel_closed(tui_monitor: RustainTUI):
    """Esc closes the Messages frame (the sidebar panel's same-chord toggle is gone: the chord
    retargets the one Mission Control frame, and Esc is the way out)."""
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    tui.assert_screen_contains(MESSAGES_TITLE)
    tui.send("\x1b")
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert MESSAGES_TITLE not in screen, (
        f"Esc must close the Messages frame. Screen:\n{screen}"
    )


# ── Scenario 5: `/team log` renders in-chat (the sub-120-col path) ──────────


@pytest.mark.story_18_2
def test_slash_team_log_runs_and_renders_in_chat(tui_monitor: RustainTUI):
    """`/team log` must actually execute.

    This is the mutant that matters: without the ``team`` entry in
    ``submit_message``'s allowlist the command falls through to
    ``SubmitWithContext``, resolves no command file, and silently dispatches an
    empty turn — exactly the failure ``/fanout`` shipped with in 14.3c. A
    handler-only test would stay green while the command was dead.
    """
    tui = tui_monitor
    tui.send("/team log")
    tui.wait(0.3)
    tui.send("\r")
    tui.wait(1.5)
    screen = tui.get_screen_text()
    assert "append-only" in screen or "no A2A interactions" in screen, (
        "`/team log` must render the transparency surface in-chat. "
        f"Screen:\n{screen}"
    )
    # A silently-unresolved command produces a provider error, never rows.
    assert "Empty input messages" not in screen, (
        f"`/team log` fell through to an empty turn. Screen:\n{screen}"
    )


# ── Scenario 7: the help screen advertises the real name ────────────────────




# ── Scenario 8: a POPULATED panel — the composition wiring, end to end ──────


@pytest.fixture
def tui_with_journal(build_binary):
    """A monitor-density TUI whose workspace already has A2A records.

    Seeded explicitly (``DF-19-13-TRANSPARENCY-FIXTURE-NO-JOURNAL``, repaired
    by Story 19.16g): the read-only ``rustain team log`` observer never creates
    a journal, so a first TUI run — whose writer composition
    (``NodeJournal::open_workspace``) creates the room journal — makes the
    file, and the pinned pre-18.2 fixture is written into it before the TUI
    under test starts. That makes this the only test that exercises the whole
    composition chain in a real binary: ``AppState.transparency`` →
    ``RoomJournalReader`` → the domain fold → the widget. Without it the
    composition-root wiring has no end-to-end proof, and a wiring hole is
    precisely the class ``DF-CR-14-3a-1`` names.
    """
    fixture = (
        PROJECT_ROOT
        / "tests"
        / "fixtures"
        / "transparency"
        / "FIXTURE_room_journal_pre_18_2.jsonl"
    )
    tmp = tempfile.TemporaryDirectory(prefix="rustain_transparency_test_")
    ws = Path(tmp.name)
    config_dir = ws / ".rustain"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text(
        '[layout]\ndensity_mode = "monitor"\n'
    )

    # The read-only CLI must NOT have created anything.
    subprocess.run(
        [str(_resolve_binary()[0]), "team", "log"],
        cwd=str(ws),
        capture_output=True,
        check=True,
        timeout=60,
    )
    assert not (config_dir / "rooms").exists(), "a read-only observer creates no journal"
    # The writer composition creates exactly one room journal; seed it.
    with RustainTUI(fresh=True, build=False, workspace=ws):
        pass
    rooms = sorted((config_dir / "rooms").glob("*.jsonl"))
    assert len(rooms) == 1, f"the TUI's writer must create one room journal: {rooms}"
    rooms[0].write_bytes(fixture.read_bytes())

    harness = RustainTUI(fresh=True, build=False, workspace=ws)
    harness.start()
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()


@pytest.mark.story_18_2
def test_panel_renders_real_rows_from_the_durable_journal(tui_with_journal: RustainTUI):
    """The Messages tab shows rows that are actually on disk."""
    tui = tui_with_journal
    _chord_ctrl_x_l(tui)
    screen = tui.wait_until(
        lambda s: MESSAGES_TITLE in s and ("inbound" in s or "unknown" in s)
        and ("refused" in s or "accepted" in s),
        "the populated Messages tab",
    )
    assert "no A2A interactions" not in screen, (
        "the journal has records, so the empty state is a wiring failure. "
        f"Screen:\n{screen}"
    )
    # The closing claim says what the rows are (a structural replay, never live); `G` jumps to it.
    tui.send("G")
    tui.wait_until(lambda s: "append-only" in s, "the closing structural-replay claim")


@pytest.mark.story_18_2
def test_slash_team_log_renders_real_rows(tui_with_journal: RustainTUI):
    """`/team log` renders the same records in-chat."""
    tui = tui_with_journal
    tui.send("/team log")
    tui.wait(0.3)
    tui.send("\r")
    tui.wait(1.5)
    screen = tui.get_screen_text()
    assert "append-only" in screen, f"Screen:\n{screen}"
    assert "no A2A interactions" not in screen, (
        f"the journal has records; `/team log` must show them. Screen:\n{screen}"
    )
