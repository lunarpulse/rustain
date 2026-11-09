"""Desk regressions for queued decision opening and the first-paint safety window."""
import pytest

import decision_frame_helpers as helpers


@pytest.mark.parametrize("kind,rail", [
    ("Peer consent", "[a] Always allow"),
    ("Permission", "[s] Session"),
])
def test_story21_6_queued_decision_is_opened_and_stable_before_return(monkeypatch, kind, rail):
    now = iter(n / 10 for n in range(100))
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
