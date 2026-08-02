"""Story 18-2: Transparency & Visible Surfacing — TUI contract tests.

Deterministic (non-API) coverage for the ``Ctrl+X, L`` Transparency Log panel
and the ``/team log`` in-chat surface. These mirror the Story 10-4 subagent
panel tests: real chord, real slash command, real render — no live LLM.

Chord reference: ``Ctrl+X, L`` → ``OpenPanel(PanelType::TransparencyLog)``.
Before this story the same chord was a ``ChordAction::Noop("Log panel — Epic
14")`` that the help screen advertised as ``available: true`` — a rendered,
documented binding backed by nothing. The first test below is what makes that
impossible to ship again.

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

from harness import BINARY, PROJECT_ROOT, RustainTUI
from keys import CTRL_X


def _chord_ctrl_x_l(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 'l' → OpenPanel(TransparencyLog)."""
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("l")
    tui.wait(0.5)


# ── Scenario 1: the chord opens a real panel, not a "not yet available" block ──


@pytest.mark.story_18_2
def test_chord_opens_transparency_log_panel(tui_monitor: RustainTUI):
    """Ctrl+X, L at the standard 130-col terminal opens the sidebar panel."""
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    tui.assert_screen_contains(
        "Transparency Log",
        msg="Ctrl+X, L must open the Transparency Log panel (it was a Noop before 18.2)",
    )
    screen = tui.get_screen_text()
    assert "Epic 14" not in screen, (
        "the stale 'Log panel — Epic 14' Noop copy must be gone. " f"Screen:\n{screen}"
    )


# ── Scenario 2: an empty log is honest, not a dead screen ───────────────────


@pytest.mark.story_18_2
def test_empty_state_is_honest(tui_monitor: RustainTUI):
    """With no A2A activity the panel says so rather than rendering blank."""
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    screen = tui.get_screen_text()
    assert "no A2A interactions" in screen, (
        "Expected the honest empty-state copy (UX: dim '·' line, not a dead "
        f"screen). Screen:\n{screen}"
    )


# ── Scenario 3: the panel never claims to be live ───────────────────────────


@pytest.mark.story_18_2
def test_panel_never_claims_to_be_live(tui_monitor: RustainTUI):
    """`NodeJournal::load()` is a consistent read, not a subscription (P-6).

    A panel advertising itself as live would be a false-green with a UI: the
    daemon can append between two reads.
    """
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    screen = tui.get_screen_text().lower()
    assert "live" not in screen, (
        "the panel must never present itself as live. " f"Screen:\n{screen}"
    )


# ── Scenario 4: the chord toggles the panel closed ──────────────────────────


@pytest.mark.story_18_2
def test_chord_toggles_panel_closed(tui_monitor: RustainTUI):
    """A second Ctrl+X, L closes the panel — the same chord, both ways."""
    tui = tui_monitor
    _chord_ctrl_x_l(tui)
    tui.assert_screen_contains("Transparency Log")
    _chord_ctrl_x_l(tui)
    screen = tui.get_screen_text()
    assert "Transparency Log" not in screen, (
        f"the same chord must close the panel. Screen:\n{screen}"
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


# ── Scenario 6: an unknown sub-verb names the valid set ─────────────────────


@pytest.mark.story_18_2
def test_unknown_subverb_names_the_valid_set(tui_monitor: RustainTUI):
    """`/team roster` must refuse by naming what IS valid."""
    tui = tui_monitor
    tui.send("/team roster")
    tui.wait(0.3)
    tui.send("\r")
    tui.wait(1.5)
    screen = tui.get_screen_text()
    assert "team log" in screen, (
        "an unknown sub-verb must answer with a Warning naming the valid set. "
        f"Screen:\n{screen}"
    )


# ── Scenario 7: the help screen advertises the real name ────────────────────


@pytest.mark.story_18_2
def test_help_advertises_the_transparency_log(tui_monitor: RustainTUI):
    """`?` help must name the spec's surface, not the stale Epic-14 label.

    The CHORDS section sits below the first screenful, so scroll while
    collecting text: assert content presence, never a screen position.
    """
    tui = tui_monitor
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("?")
    tui.wait(0.8)

    seen = tui.get_screen_text()
    # The overlay's own footer says "j/k scroll" — use its keys, not the chat
    # pane's.
    for _ in range(60):
        if "Transparency Log panel" in seen:
            break
        tui.send("j")
        tui.wait(0.05)
        seen += tui.get_screen_text()

    assert "Ctrl+X, L" in seen, f"help must document the chord. Screen:\n{seen}"
    assert "Transparency Log panel" in seen, (
        f"help must name Ctrl+X, L by its real surface. Screen:\n{seen}"
    )
    assert "Epic 14" not in seen, f"the stale Noop label must be gone. Screen:\n{seen}"


# ── Scenario 8: a POPULATED panel — the composition wiring, end to end ──────


@pytest.fixture
def tui_with_journal(build_binary):
    """A monitor-density TUI whose workspace already has A2A records.

    Seeded by running the real ``rustain team log`` once (which creates the
    room file) and then writing the pinned pre-18.2 fixture into it. That makes
    this the only test that exercises the whole composition chain in a real
    binary: ``AppState.transparency`` → ``RoomJournalReader`` → the domain fold
    → the widget. Without it the composition-root wiring has no end-to-end
    proof, and a wiring hole is precisely the class ``DF-CR-14-3a-1`` names.
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

    # `team log` opens the room journal, which creates it. Then seed it.
    subprocess.run(
        [str(BINARY), "team", "log"],
        cwd=str(ws),
        capture_output=True,
        check=True,
        timeout=60,
    )
    rooms = sorted((config_dir / "rooms").glob("*.jsonl"))
    assert rooms, "`rustain team log` must have created the room journal"
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
    """The panel shows rows that are actually on disk."""
    tui = tui_with_journal
    _chord_ctrl_x_l(tui)
    screen = tui.get_screen_text()
    assert "Transparency Log" in screen, f"Screen:\n{screen}"
    assert "as of" in screen, (
        "the header must say 'as of <time>' — a point-in-time read, never live. "
        f"Screen:\n{screen}"
    )
    # Direction and kind each carry a glyph AND a word (monochrome rule).
    assert "inbound" in screen or "unknown" in screen, f"Screen:\n{screen}"
    assert "refused" in screen or "accepted" in screen, f"Screen:\n{screen}"
    assert "no A2A interactions" not in screen, (
        "the journal has records, so the empty state is a wiring failure. "
        f"Screen:\n{screen}"
    )


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
