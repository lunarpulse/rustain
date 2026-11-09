"""Structural waits for attached recipient decisions in native TUI journeys."""
import time


def rows(screen):
    """Return whole terminal rows, without border, padding or selection marker."""
    result = []
    for line in screen.splitlines():
        text = line.strip().strip("│").strip()
        if text.startswith(("▶ ", "▸ ")):
            text = text[2:]
        result.append(text)
    return tuple(result)


def has_row(screen, text):
    return any(text in row for row in rows(screen))


def _frame(screen, title, rail):
    lines = screen.splitlines()
    for start, line in enumerate(lines):
        if line.strip().startswith("┌ " + title + " "):
            for end in range(start + 1, len(lines)):
                if lines[end].strip().startswith("└") and lines[end].strip().endswith("┘"):
                    frame = "\n".join(lines[start:end + 1])
                    frame_rows = rows(frame)
                    if any(row.startswith("Needs you › ") for row in frame_rows) and all(
                        any(part in row for row in frame_rows) for part in rail
                    ):
                        return tuple(lines[start:end + 1])
                    return None
    return None

def recipient_text(screen):
    """Join only the complete recipient frame's rows, allowing wrapped body text."""
    frame = _frame(screen, "Decision: Recipient draft", ("[v] View item",))
    assert frame is not None, screen
    return " ".join(" ".join(rows("\n".join(frame))).split())


def wait_frame(tui, title, rail, timeout=45):
    """Wait for a complete, unchanged frame for 600 ms, excluding outside toasts."""
    deadline = time.monotonic() + timeout
    previous = None
    since = None
    while True:
        screen = tui.get_screen_text()
        assert "unavailable in this session" not in screen, screen
        frame = _frame(screen, title, rail)
        now = time.monotonic()
        if frame is None or frame != previous:
            previous, since = frame, now if frame else None
        elif now - since >= 0.6:
            return screen
        assert now < deadline, f"stable {title}:\n{screen}"


def _open_queued(tui, title, rail, kinds, timeout, blocked=lambda screen: False):
    """Present a decision: it may already be a frame, or sit behind the Needs-you chip.

    Ctrl+X q and Enter are sent only on a painted screen that has been unchanged for
    600 ms, and are asked again every 2 s while the list/frame has not appeared: a key
    pressed while the previous decision is still settling with the daemon is dropped.
    """
    deadline = time.monotonic() + timeout
    previous = since = asked = entered = None
    while True:
        screen = tui.get_screen_text()
        assert "unavailable in this session" not in screen, screen
        if _frame(screen, title, rail):
            return screen
        now = time.monotonic()
        if screen != previous:
            previous, since = screen, now
        stable = now - since >= 0.6
        if _frame(screen, "Needs you", ("[Enter] Open",)):
            selected = any(
                row.strip().lstrip("│ ").startswith(tuple(f"{mark} {kind} ·" for kind in kinds for mark in ("▸", "▶")))
                for row in screen.splitlines()
            )
            if selected and stable and (entered is None or now - entered >= 2.0):
                tui.send("\r")
                entered = now
        elif (stable and not blocked(screen) and has_row(screen, "Needs you (")
              and has_row(screen, "^X q") and (asked is None or now - asked >= 2.0)):
            tui.send("\x18q")
            asked = now
        assert now < deadline, f"{title} arrival:\n{screen}"


def open_decision(tui, kind, rail, timeout=45):
    """Open a selected queued decision, then wait out its structural inert window."""
    title = "Decision: " + kind
    _open_queued(tui, title, rail, (kind,), timeout)
    return wait_frame(tui, title, rail, timeout)


def open_recipient(tui, timeout=45):
    """Open the current recipient draft whether it preempted or accumulated."""
    _open_queued(tui, "Decision: Recipient draft", ("[v] View item",), ("Recipient draft",), timeout,
                 blocked=lambda screen: has_row(screen, "Decision: Peer consent"))
    return wait_frame(tui, "Decision: Recipient draft", ("[v] View item",), timeout)


def view_recipient(tui):
    """Open the associated RecipientSurface without resolving the decision."""
    tui.send("v")


def defer_frame(tui, title="Decision: Recipient draft", timeout=15):
    """Esc a presented frame and wait until it is gone, so a following paste is not eaten.

    An Esc left in the PTY buffer directly ahead of a bracketed paste (`ESC ESC [ 2 0 0 ~`)
    is parsed as one Alt sequence and the paste arrives as literal text.
    """
    tui.send("\x1b")
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if not has_row(screen, title):
            return screen
        assert time.monotonic() < deadline, f"{title} stayed after Esc:\n{screen}"


def wait_frame_resending(tui, title, rail, sender, resend, timeout=45, resends=4):
    """wait_frame for an arrival whose send may be refused while the peer card is uncached.

    The sender states the refusal itself (`AgentCard is not cached` — discovery runs once
    at startup); only then is the identical command sent again, once per new refusal.
    """
    deadline = time.monotonic() + timeout * (resends + 1)
    handled = 0
    while True:
        try:
            return wait_frame(tui, title, rail, timeout=3)
        except AssertionError:
            pass
        refused = "".join(c for c in sender.get_screen_text() if c not in "│┃ \n").count("AgentCardisnotcached")
        if refused > handled and resends > 0:
            handled = refused
            resends -= 1
            resend()
        assert time.monotonic() < deadline, f"stable {title}:\n{tui.get_screen_text()}"
