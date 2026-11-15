"""Desk regressions for queued decision opening and the first-paint safety window, and for the
exact decision-sheet predicate (`decision_frame`)."""
import itertools

import pytest

import decision_frame_helpers as helpers
from decision_frame_helpers import BUDGET_RAIL, RECOVERY_RAIL, REGION, WIDTH, decision_frame, sheet


def test_frame_predicate_accepts_the_whole_sheet_and_rejects_torn_frames():
    rail = "[Enter/Esc] Continue  [n] New session"
    whole = sheet("Session recovery", "Recovered the previous session.", rail)
    found = decision_frame(whole, "Session recovery", RECOVERY_RAIL)
    assert found is not None and found[0] == "Needs you › Session recovery | 1 pending"
    assert found[-1] == rail
    # A torn frame: the title and the first rail pair are painted, the rest of the rail is not.
    half = sheet("Session recovery", "Recovered the previous session.", "[Enter/Esc] Continue")
    assert decision_frame(half, "Session recovery", RECOVERY_RAIL) is None
    # A frame without its closing border is never accepted.
    assert decision_frame(sheet("Session recovery", "x", rail, tear="no-bottom"),
                          "Session recovery", RECOVERY_RAIL) is None
    # The old bottom-anchored pane title and a missing breadcrumb are rejected.
    assert decision_frame(whole.replace("Decision: ", "System · "), "Session recovery", RECOVERY_RAIL) is None
    assert decision_frame(sheet("Session recovery", "x", rail, crumb="no crumb here"),
                          "Session recovery", RECOVERY_RAIL) is None
    # A different decision is never mistaken for this one.
    assert decision_frame(whole, "Daily budget", RECOVERY_RAIL) is None


def test_frame_predicate_holds_for_the_budget_rail_and_a_wrapped_rail_row():
    rail = "[y] Continue  [s] Switch model  [p] Pause until tomorrow  [Esc] Later"
    assert decision_frame(sheet("Daily budget", "Daily budget ($0.00) reached.", rail),
                          "Daily budget", BUDGET_RAIL) is not None
    # At narrow widths the rail wraps by whole pairs onto two rows: the last two content
    # rows together still hold every pair.
    screen = sheet("Daily budget", "Daily budget ($0.00) reached.", "[p] Pause until tomorrow  [Esc] Later")
    rows = screen.split("\n")
    rows[REGION - 3] = "│" + "[y] Continue  [s] Switch model".ljust(WIDTH - 2) + "│"
    assert decision_frame("\n".join(rows), "Daily budget", BUDGET_RAIL) is not None



@pytest.mark.parametrize("kind,rail", [
    ("Peer consent", "[a] Always allow"),
    ("Permission", "[s] Session"),
])
def test_story21_6_queued_decision_is_opened_and_stable_before_return(monkeypatch, kind, rail):
    # Every read of the clock advances 0.1 s; the helper's polling decides how many it makes.
    now = (n / 10 for n in itertools.count())
    monkeypatch.setattr(helpers.time, "monotonic", lambda: next(now))

    class Terminal:
        def __init__(self):
            self.sent = []
            self.frame_reads = 0
            self.queue_reads = 0

        def send(self, text):
            self.sent.append(text)

        def get_screen_text(self):
            if not self.sent:
                return "Needs you (1)  ^X q"
            if self.sent == ["\x18q"]:
                self.queue_reads += 1
                bottom = "\n└────────────────────┘" if self.queue_reads > 1 else ""
                return ("┌ Needs you ─────────┐\n"
                        "│ Needs you › 1 waiting │\n"
                        f"│ ▸ {kind} · source │\n"
                        "│ [Enter] Open       │" + bottom)
            self.frame_reads += 1
            return (f"┌ Decision: {kind} ─────┐\n"
                    f"│ Needs you › {kind} │\n"
                    f"│ {rail} │\n└────────────────────┘")

    terminal = Terminal()
    helpers.open_decision(terminal, kind, (rail,))
    assert terminal.sent == ["\x18q", "\r"]
    assert terminal.queue_reads >= 2, "a torn list must not be opened"
    assert terminal.frame_reads >= 7, "a first-sight frame must not count as inert-ready"
