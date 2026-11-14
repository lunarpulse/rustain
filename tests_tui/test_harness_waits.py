"""Self-tests of the harness wait model (tui-2, K1-K13).

The real ``RustainTUI`` waits run on a subclass that replaces only the PTY
(``get_screen_text`` / ``send``: scripted screens as a function of synthetic
time) and on a synthetic module clock, so nothing here starts a process, opens a
PTY or really sleeps.
"""
import time as real_time
from pathlib import Path

import pytest

import harness
from keys import CTRL_X, ENTER, ESC

COLS = 80
WALL0 = 1000.4  # a non-aligned wall clock: 1970-01-01 00:16:40.4 UTC


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """These controls never build or launch a binary."""


class Clock:
    """Stands in for the ``time`` module the harness reads (one synthetic timeline)."""

    def __init__(self, oversleep: float = 1.0, wall0: float = WALL0):
        self.now = 0.0
        self.wall0 = wall0
        self.oversleep = oversleep

    def monotonic(self) -> float:
        return self.now

    def time(self) -> float:
        return self.wall0 + self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds * self.oversleep

    def localtime(self, seconds: float | None = None):
        return real_time.gmtime(self.time() if seconds is None else seconds)


def frame(toasts=(), status="Ready │ Normal │ main", transcript=(), menu=False, log_rows=None) -> str:
    """An 80x24 screen: composer on the bottom rows, the status row above it, toast
    rows above that (newest first, right-aligned), optionally the leader menu or
    the S-LOG frame."""
    lines = [""] * 24
    for i, text in enumerate(transcript):
        lines[i] = text
    for k, text in enumerate(toasts):
        lines[19 - k] = text.rjust(COLS)
    lines[20] = status
    lines[21] = "┌" + "─" * (COLS - 2) + "┐"
    lines[22] = "│ " + ">".ljust(COLS - 4) + " │"
    lines[23] = "└" + "─" * (COLS - 2) + "┘"
    if menu:
        lines[10] = "┌ Ctrl+X " + "─" * (COLS - 10) + "┐"
    if log_rows is not None:
        inner = COLS - 2
        box = ["┌ System · Notifications " + "─" * (COLS - 27) + "┐"]
        box += ["│" + row.ljust(inner) + "│" for row in log_rows]
        box += ["│" + "[Esc] Close".ljust(inner) + "│", "└" + "─" * (COLS - 2) + "┘"]
        lines[1:1 + len(box)] = box
    return "\n".join(lines)


class Scene:
    """Screens as a function of synthetic time: toast spans and S-LOG records."""

    def __init__(self, clock, toasts=(), records=(), status="Ready │ Normal │ main", clip=None):
        self.clock = clock
        self.toasts = list(toasts)    # (start, end, text), listed newest first
        self.records = list(records)  # (raised_at, severity, extra, message)
        self.status = status
        self.clip = clip              # the list paints a row clipped to this many columns

    def row(self, record) -> str:
        at, severity, extra, message = record
        hms = real_time.strftime("%H:%M:%S", real_time.gmtime(int(self.clock.wall0 + at)))
        row = f"{severity} · {hms} · notify{extra} · {message}"
        return row if self.clip is None or len(row) <= self.clip else row[:self.clip - 1] + "…"

    def render(self, t: float, mode: str, sel: int = 0) -> str:
        toasts = [text for start, end, text in self.toasts if start <= t < end]
        if mode in ("log", "detail"):
            shown = [r for r in sorted(self.records, reverse=True) if r[0] <= t]
            if mode == "detail":
                return frame(toasts, self.status, log_rows=["Notifications · detail", shown[sel][3]])
            return frame(toasts, self.status, log_rows=[self.row(r) for r in shown])
        return frame(toasts, self.status, menu=mode == "menu")


class FakeTUI(harness.RustainTUI):
    """The real wait methods over a scripted screen source."""

    def get_screen_text(self) -> str:
        screen = self.scene.render(self.clock.now, self.mode, self.sel)
        self.clock.now += self.read_cost
        return screen

    def _wait_pty(self, seconds: float) -> None:
        self.clock.now += seconds

    def send(self, data: str) -> None:
        self.keys.append(data)
        if data == CTRL_X:
            self.mode, self.sel = "menu", 0
        elif data == "n" and self.mode == "menu":
            self.mode = "log"
        elif data and set(data) == {"j"} and self.mode == "log":
            self.sel += len(data)
        elif data == ENTER and self.mode == "log":
            self.mode = "detail"
        elif data == ESC:
            self.mode = "chat"


@pytest.fixture
def clock(monkeypatch):
    clock = Clock()
    monkeypatch.setattr(harness, "time", clock)
    return clock


def make_tui(clock, scene=None, read_cost=0.0, workspace=None) -> FakeTUI:
    tui = FakeTUI(build=False)
    tui.clock, tui.read_cost, tui.keys, tui.mode, tui.sel = clock, read_cost, [], "chat", 0
    tui.scene = scene or Scene(clock)
    tui._workspace_path = workspace
    return tui


# ── K1-K5: waits are bounded by the wall clock ───────────────────────────────

SLOW_READ = dict(oversleep=1.0, read_cost=1.0)
OVERSLEEP = dict(oversleep=3.0, read_cost=0.0)


@pytest.mark.parametrize("load", [pytest.param(SLOW_READ, id="slow-read"),
                                  pytest.param(OVERSLEEP, id="oversleep")])
def test_wait_for_screen_stops_at_the_wall_clock_deadline(monkeypatch, load):
    clock = Clock(oversleep=load["oversleep"])
    monkeypatch.setattr(harness, "time", clock)
    tui = make_tui(clock, read_cost=load["read_cost"])
    assert tui.wait_for_screen("never painted", timeout=3.0) is False
    # one more read, or one more oversleeping poll, may land past the deadline
    assert clock.now <= 3.0 + max(load["read_cost"], 0.5 * load["oversleep"])


def test_wait_for_screen_returns_when_the_text_paints(clock):
    scene = Scene(clock, toasts=[(2.4, 99.0, "Info painted at 2.4")])
    tui = make_tui(clock, scene, read_cost=0.5)
    assert tui.wait_for_screen("painted at 2.4", timeout=5.0) is True
    assert clock.now < 5.0


def test_wait_for_screen_not_contains_stops_at_the_wall_clock_deadline(monkeypatch):
    clock = Clock(oversleep=1.0)
    monkeypatch.setattr(harness, "time", clock)
    scene = Scene(clock, toasts=[(0.0, 99.0, "Info stays")])
    tui = make_tui(clock, scene, read_cost=1.0)
    assert tui.wait_for_screen_not_contains("stays", timeout=3.0) is False
    assert clock.now <= 3.0 + 1.0


def test_wait_for_export_file_stops_at_the_wall_clock_deadline(monkeypatch, tmp_path):
    clock = Clock(oversleep=3.0)
    monkeypatch.setattr(harness, "time", clock)
    tui = make_tui(clock, workspace=tmp_path)
    assert tui.wait_for_export_file("never.md", timeout=3.0) is False
    assert clock.now <= 3.0 + 0.25 * 3.0


# ── K6-K11: notification waits ───────────────────────────────────────────────

MODE_TEXT = "Permission mode: AutoEdit"


def test_wait_for_notification_never_takes_a_toast_for_the_notice(clock):
    """A toast is not evidence: a stack with the requested toast and no fresh log row fails; the
    row raised after the mark is what returns, through the Notification Log."""
    toasts = [(0.0, 99.0, "Warning Model discovery for 'anthropic' returned 0 models."),
              (0.7, 99.0, f"Info {MODE_TEXT} (changed by you)")]
    tui = make_tui(clock, Scene(clock, toasts=toasts), read_cost=0.25)
    mark = tui.notification_mark()
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)

    clock.now = 0.0
    tui = make_tui(clock, Scene(clock, toasts=toasts, records=[(0.7, "Info", "", MODE_TEXT)]), read_cost=0.25)
    mark = tui.notification_mark()
    assert tui.wait_for_notification(MODE_TEXT, mark, "Info") == "log"
    assert tui.keys == [CTRL_X, "n", ESC]


def test_wait_for_notification_reads_the_log_when_the_toast_lived_between_two_reads(monkeypatch):
    def scene(clock):  # painted and gone between the reads at 0.85 and 1.1
        return Scene(clock, toasts=[(0.9, 1.05, f"Info {MODE_TEXT}")],
                     records=[(0.9, "Info", "", MODE_TEXT)])

    old = Clock()
    monkeypatch.setattr(harness, "time", old)
    assert make_tui(old, scene(old), read_cost=0.25).wait_for_screen(MODE_TEXT, timeout=3.0) is False

    clock = Clock()
    monkeypatch.setattr(harness, "time", clock)
    tui = make_tui(clock, scene(clock), read_cost=0.25)
    mark = tui.notification_mark()
    assert tui.wait_for_notification(MODE_TEXT, mark, "Info") == "log"
    assert tui.keys == [CTRL_X, "n", ESC]


def _log_scene(clock, record_at, extra="", severity="Info", toast=False):
    toasts = [(0.0, 99.0, f"Info {MODE_TEXT}")] if toast else []
    return Scene(clock, toasts=toasts, records=[(record_at, severity, extra, MODE_TEXT)])


def test_wait_for_notification_rejects_a_log_row_older_than_the_mark(clock):
    wall = clock.time()
    tui = make_tui(clock, _log_scene(clock, 0.5), read_cost=0.25)  # stamped 1000: before the mark
    mark = tui.notification_mark()
    assert isinstance(mark.second, int) and mark.second >= wall and mark.second == 1001
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)

    clock.now = 0.0
    tui = make_tui(clock, _log_scene(clock, 1.05), read_cost=0.25)  # stamped 1001: at the mark
    mark = tui.notification_mark()
    assert mark.second == 1001
    assert tui.wait_for_notification(MODE_TEXT, mark, "Info") == "log"


def test_wait_for_notification_skips_an_identical_toast_already_on_screen_at_the_mark(clock):
    tui = make_tui(clock, _log_scene(clock, 0.5, toast=True), read_cost=0.25)
    mark = tui.notification_mark()
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)

    clock.now = 0.0  # the same toast, coalesced: the record is restamped after the mark
    tui = make_tui(clock, _log_scene(clock, 1.1, extra=" · ×2", toast=True), read_cost=0.25)
    mark = tui.notification_mark()
    assert tui.wait_for_notification(MODE_TEXT, mark, "Info") == "log"


def test_wait_for_notification_matches_the_requested_severity_only(clock):
    tui = make_tui(clock, _log_scene(clock, 1.05, severity="Warning"), read_cost=0.25)
    mark = tui.notification_mark()
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)


def test_notification_mark_excludes_records_raised_while_it_was_being_taken(clock):
    """The log is the only freshness authority: a row counts only if it is stamped at or after the
    mark's second, whatever was painted before or after the baseline."""
    early = Scene(clock, toasts=[(0.0, 99.0, f"Info {MODE_TEXT}")],
                  records=[(0.3, "Info", "", MODE_TEXT), (0.55, "Info", "", MODE_TEXT)])
    tui = make_tui(clock, early, read_cost=0.25)
    mark = tui.notification_mark()
    assert mark.second == 1001
    assert clock.time() - mark.second < 0.05  # returned at the turn, not one PTY read after it
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)

    # (a) delayed presentation: an OLDER notice is painted after the baseline, its row is older than
    # the mark, the caller's key raised nothing: not accepted (a toast is never evidence)
    clock.now = 0.0
    delayed = Scene(clock, toasts=[(0.7, 99.0, f"Info {MODE_TEXT}")], records=[(0.3, "Info", "", MODE_TEXT)])
    tui = make_tui(clock, delayed, read_cost=0.25)
    mark = tui.notification_mark()
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)

    clock.now = 0.0  # the same toast coalesced after the turn: its row is restamped, so it is new
    late = Scene(clock, toasts=[(0.45, 99.0, f"Info {MODE_TEXT}")],
                 records=[(0.45, "Info", "", MODE_TEXT), (0.7, "Info", " · ×2", MODE_TEXT)])
    tui = make_tui(clock, late, read_cost=0.25)
    mark = tui.notification_mark()
    assert tui.wait_for_notification(MODE_TEXT, mark, "Info") == "log"


def test_notification_mark_realigns_after_a_slow_drain(clock):
    """(b) One drain takes longer than a whole second: a boundary fixed before it would be older than
    the second of return, and a record raised during that drain would count as fresh."""
    scene = Scene(clock, records=[(1.5, "Info", "", MODE_TEXT)])  # raised at wall 1001.9, during the drain
    tui = make_tui(clock, scene, read_cost=1.5)
    mark = tui.notification_mark()
    assert mark.second == int(clock.time()) == 1002 and clock.time() - mark.second < 0.05
    with pytest.raises(AssertionError):
        tui.wait_for_notification(MODE_TEXT, mark, "Info", timeout=5.0)


def test_wait_for_notification_confirms_a_clipped_row_in_its_details(clock):
    """Two records share their first columns and differ in the suffix: the list clips both to the
    same row, so only the Details can say which one the requested notice is."""
    wanted = "Unknown command: /nope — not a built-in or a custom command."
    other = "Unknown command: /nope — not a built-in or a custom SHELL."
    both = Scene(clock, clip=60, records=[(0.65, "Warning", "", wanted), (0.7, "Warning", "", other)])
    tui = make_tui(clock, both, read_cost=0.25)
    mark = tui.notification_mark()
    assert tui.wait_for_notification(wanted, mark, "Warning") == "log"
    assert "j" in tui.keys  # the older row was selected after the newest one's Details did not match

    clock.now = 0.0
    third = "Unknown command: /nope — not a built-in or a custom plugin."
    neither = Scene(clock, clip=60, records=[(0.65, "Warning", "", third), (0.7, "Warning", "", other)])
    tui = make_tui(clock, neither, read_cost=0.25)
    mark = tui.notification_mark()
    with pytest.raises(AssertionError, match="Details"):
        tui.wait_for_notification(wanted, mark, "Warning", timeout=5.0)


def test_log_row_time_guard_wraps_at_midnight(monkeypatch):
    clock = Clock(wall0=86399.0)  # 1970-01-01 23:59:59 UTC
    monkeypatch.setattr(harness, "time", clock)
    mark_second = 86399
    assert harness._log_row_at_or_after("Info · 00:00:01 · notify · x", mark_second)
    assert harness._log_row_at_or_after("Info · 23:59:59 · notify · x", mark_second)
    assert not harness._log_row_at_or_after("Info · 23:59:57 · notify · x", mark_second)


# ── K12-K13: durable mode fact, config ───────────────────────────────────────

def test_mode_segment_reads_the_status_row_only():
    assert harness.mode_segment(frame(status="Ready │ PLAN │ main")) == "PLAN"
    assert harness.mode_segment(frame(status="Ready │ AUTOEDIT")) == "AUTOEDIT"
    assert harness.mode_segment(frame(status="Ready │ main", transcript=["agent said │ PLAN │ is on"])) is None


def test_harness_config_carries_the_requested_toast_lifetime(tmp_path):
    allow = ["Read", "Write"]
    today = '[permissions]\nalways_tools = ["Read", "Write"]\n'
    assert harness.workspace_config_text(allow, None) == today
    text = harness.workspace_config_text(allow, 30)
    head, _, notifications = text.partition("[notifications]\n")
    assert head == today + "\n"
    assert "info_ttl_secs = 30\n" in notifications and "success_ttl_secs = 30\n" in notifications

    config = tmp_path / ".rustain" / "config.toml"
    config.parent.mkdir()
    config.write_text(today)
    tui = harness.RustainTUI(build=False, workspace=tmp_path, notification_ttl_secs=30, binary=Path(__file__))
    with pytest.raises(ValueError, match="notification_ttl_secs"):
        tui.start()  # a pre-existing config would silently ignore the lifetime
