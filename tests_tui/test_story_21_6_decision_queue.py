"""Story 21.6 K15: the decision layer and the Needs-you queue, native smoke.

One file, one leg per slice, 80x24, DEBUG binary (`RUSTAIN_TUI_BINARY`), isolated
HOME/config/data dirs, no sleeps: every wait is a bounded structural predicate.

* Leg A (slice 6-A, generic lane): the six interim kinds are L3 frames. The proven
  21-1 S2 scenario (recovery after `kill -9`, budget arrival while a draft is typed,
  `Ctrl+X q`, `Esc` defers, the chip) runs on the real binary; then the frames it
  captured are checked as the sheet they are: title `Decision: <kind>`, the
  breadcrumb row, the body, the rail, every border, nothing bottom-anchored.

The frame predicate is desk-checked offline first (a torn frame that paints the
title but only half the rail must fail it): those tests need no binary. The
native leg skips when no binary is built (`RUSTAIN_TUI_BINARY` absent).
"""
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import tempfile
import traceback

import harness
import pytest

import test_story_21_1_notification_lifecycle as s21_1

WIDTH, HEIGHT = 80, 24
REGION = HEIGHT - 4  # the modal region: everything above the composer and status row
RECOVERY_RAIL = ("[Enter/Esc] Continue", "[n] New session")
BUDGET_RAIL = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")


def decision_frame(screen, title, rail, width=WIDTH, height=HEIGHT):
    """The decision frame as the exact L3 sheet (full width below 100 columns),
    every border painted: returns its content rows, or None while it is torn,
    partial, bottom-anchored or missing a rail pair. Never accepts half a frame."""
    rows = screen.split("\n")
    if len(rows) != height or any(len(row) != width for row in rows):
        return None
    region = height - 4
    top, bottom = rows[0], rows[region - 1]
    if not (top.startswith(f"┌ Decision: {title} ") and top.endswith("┐")):
        return None
    if not (bottom.startswith("└") and bottom.endswith("┘") and set(bottom[1:-1]) == {"─"}):
        return None
    interior = rows[1:region - 1]
    if any(not (row.startswith("│") and row.endswith("│")) for row in interior):
        return None
    content = [row[1:-1].strip() for row in interior if row[1:-1].strip()]
    if not content or not content[0].startswith(f"Needs you › {title} |"):
        return None
    # The rail sits under the body; a Ctrl+X menu may paint over the frame's bottom rows
    # (the 21-1 scenario captures the frame with it open), so search the whole body.
    body = " ".join(content[1:])
    if not all(item in body for item in rail):
        return None
    return tuple(content)


def _sheet(title, body, rail_row, width=WIDTH, height=HEIGHT, crumb=None, tear=None):
    """An offline 80x24 screen: the decision sheet over a composer and status row."""
    region = height - 4
    crumb = crumb or f"Needs you › {title} | 1 pending"
    inner = width - 2
    top = ("┌ Decision: " + title + " ").ljust(width - 1, "─") + "┐"
    rows = [top]
    content = [crumb, body]
    for index in range(1, region - 1):
        text = content[index - 1] if index <= len(content) else ""
        if index == region - 2:
            text = rail_row
        rows.append("│" + text.ljust(inner) + "│")
    rows.append("└" + "─" * inner + "┘")
    rows += ["┌" + "─" * inner + "┐", "│" + " " * inner + "│", "└" + "─" * inner + "┘",
             "Ready".ljust(width)]
    if tear == "no-bottom":
        rows[region - 1] = " " * width
    return "\n".join(rows)


def test_frame_predicate_accepts_the_whole_sheet_and_rejects_torn_frames():
    rail = "[Enter/Esc] Continue  [n] New session"
    whole = _sheet("Session recovery", "Recovered the previous session.", rail)
    found = decision_frame(whole, "Session recovery", RECOVERY_RAIL)
    assert found is not None and found[0] == "Needs you › Session recovery | 1 pending"
    assert found[-1] == rail
    # A torn frame: the title and the first rail pair are painted, the rest of the rail is not.
    half = _sheet("Session recovery", "Recovered the previous session.", "[Enter/Esc] Continue")
    assert decision_frame(half, "Session recovery", RECOVERY_RAIL) is None
    # A frame without its closing border is never accepted.
    assert decision_frame(_sheet("Session recovery", "x", rail, tear="no-bottom"),
                          "Session recovery", RECOVERY_RAIL) is None
    # The old bottom-anchored pane title and a missing breadcrumb are rejected.
    assert decision_frame(whole.replace("Decision: ", "System · "), "Session recovery", RECOVERY_RAIL) is None
    assert decision_frame(_sheet("Session recovery", "x", rail, crumb="no crumb here"),
                          "Session recovery", RECOVERY_RAIL) is None
    # A different decision is never mistaken for this one.
    assert decision_frame(whole, "Daily budget", RECOVERY_RAIL) is None


def test_frame_predicate_holds_for_the_budget_rail_and_a_wrapped_rail_row():
    rail = "[y] Continue  [s] Switch model  [p] Pause until tomorrow  [Esc] Later"
    assert decision_frame(_sheet("Daily budget", "Daily budget ($0.00) reached.", rail),
                          "Daily budget", BUDGET_RAIL) is not None
    # At narrow widths the rail wraps by whole pairs onto two rows: the last two content
    # rows together still hold every pair.
    screen = _sheet("Daily budget", "Daily budget ($0.00) reached.", "[p] Pause until tomorrow  [Esc] Later")
    rows = screen.split("\n")
    rows[REGION - 3] = "│" + "[y] Continue  [s] Switch model".ljust(WIDTH - 2) + "│"
    assert decision_frame("\n".join(rows), "Daily budget", BUDGET_RAIL) is not None


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(400)
def test_leg_a_interim_kinds_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    if not binary.is_file():
        pytest.skip("no native binary: build `cargo build --bin rustain` and set RUSTAIN_TUI_BINARY")
    assert "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-6-a-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size,
                "viewport": [WIDTH, HEIGHT], "leg": "A",
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r216a-", dir="/tmp") as root:
            s21_1._exercise_s2(binary, evidence, Path(root), monkeypatch)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        # The frames the scenario captured are the L3 sheet with the generated rails.
        recovery = (evidence / "21-recovery-pane.screen.txt").read_text()[:-1]
        assert decision_frame(recovery, "Session recovery", RECOVERY_RAIL), recovery
        budget = (evidence / "32-budget-pane.screen.txt").read_text()[:-1]
        assert decision_frame(budget, "Daily budget", BUDGET_RAIL), budget
        for screen in (recovery, budget):
            assert "System ·" not in screen.split("\n")[0], "no bottom-anchored pane title survives"
            assert "more waiting" not in screen
        assert s21_1._cue(budget) == 1 and s21_1._cue(recovery) == 1
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_6_A_EVIDENCE={evidence}", flush=True)
