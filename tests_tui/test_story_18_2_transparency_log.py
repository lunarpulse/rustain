"""Story 18-2: Transparency & Visible Surfacing — TUI contract tests.

Deterministic (non-API) coverage for the ``Ctrl+X, L`` Messages surface (Story 21.7: Mission
Control's Messages tab, the A2A log's one home) and the ``/team log`` slash command, which opens the same Messages frame. These
mirror the Story 10-4 subagent panel tests: real chord, real slash command, real render — no
live LLM.

Chord reference: ``Ctrl+X, L`` → ``InputAction::OpenMessages`` (local host). Before 18.2 the
same chord was a ``ChordAction::Noop("Log panel — Epic 14")`` that the help screen advertised
as ``available: true`` — a rendered, documented binding backed by nothing. The single
survivor below is what makes that impossible to ship again: the chord and ``/team log`` must
both open a Messages frame whose rows come from the durable journal.

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

from harness import PROJECT_ROOT, RustainTUI, _resolve_binary, composer_top
from keys import CTRL_X, ENTER, ESC


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


def _composer_text(screen: str) -> str:
    """The composer box (its top border down to the screen's last row)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[top:]) if top is not None else ""


def _assert_messages_populated(tui: RustainTUI) -> None:
    """The Messages frame shows rows that are actually on disk: the title, a direction word,
    an outcome word, no empty state, and (after ``G``) the structural-replay closing claim."""
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


def test_panel_renders_real_rows_from_the_durable_journal(tui_with_journal: RustainTUI):
    """The Messages tab shows rows that are actually on disk, opened by the chord and by `/team log`.

    ``Ctrl+X, L`` opens the frame with the journal's rows (it was a Noop before 18.2) and Esc
    closes it. ``/team log`` must actually execute: without the ``team`` entry in
    ``submit_message``'s allowlist the command falls through to ``SubmitWithContext`` and
    silently dispatches an empty turn, exactly the failure ``/fanout`` shipped with in 14.3c.
    Typed after the chord leg closed the frame, it must open the same Messages frame with the
    same rows (``src/adapters/tui/handlers/team_command.rs``).
    """
    tui = tui_with_journal
    assert MESSAGES_TITLE not in tui.get_screen_text(), "the Messages frame must be closed at spawn"
    _chord_ctrl_x_l(tui)
    _assert_messages_populated(tui)

    tui.send(ESC)
    assert tui.wait_for_screen_not_contains(MESSAGES_TITLE, timeout=5.0), tui.get_screen_text()

    tui.send("/team log")
    tui.wait_until(lambda s: "/team log" in _composer_text(s), "`/team log` typed into the composer")
    tui.send(ENTER)
    tui.wait_until(lambda s: MESSAGES_TITLE in s, "the Messages frame opened by `/team log`")
    _assert_messages_populated(tui)
