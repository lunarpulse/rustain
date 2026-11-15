"""Structural waits for attached recipient decisions in native TUI journeys, and the exact L3
decision-sheet predicate (`decision_frame`) that 21.1 S2 and 21.6 share."""
import re
import time

WIDTH, HEIGHT = 80, 24
REGION = HEIGHT - 4  # the modal region: everything above the composer and status row
RECOVERY_RAIL = ("[Enter/Esc] Continue", "[n] New session")
BUDGET_RAIL = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")


def decision_frame(screen, title, rail, width=WIDTH, height=HEIGHT, crumb_title=None):
    """The decision frame as the exact L3 sheet (full width below 100 columns),
    every border painted: returns its content rows, or None while it is torn,
    partial, bottom-anchored or missing a rail pair. Never accepts half a frame."""
    lines = screen.split("\n")
    if len(lines) != height or any(len(line) != width for line in lines):
        return None
    # Story 21.7: a mission strip row ("Decisions n", "Tasks d/n", the idle hint) sits between
    # the sheet and the status row whenever the Stage can spare it; the sheet ends above it.
    strip = re.fullmatch(
        r"(?:(?:Agents|A|Tasks|T|Decisions|D|Writer decisions|W) .*|\^X \S+ (?:Mission Control|Messages))",
        lines[height - 5].rstrip(),
    )
    region = height - 4 - (1 if strip else 0)
    top, bottom = lines[0], lines[region - 1]
    if not (top.startswith(f"┌ Decision: {title} ") and top.endswith("┐")):
        return None
    if not (bottom.startswith("└") and bottom.endswith("┘") and set(bottom[1:-1]) == {"─"}):
        return None
    interior = lines[1:region - 1]
    if any(not (line.startswith("│") and line.endswith("│")) for line in interior):
        return None
    content = [line[1:-1].strip() for line in interior if line[1:-1].strip()]
    if not content or not content[0].startswith(f"Needs you › {crumb_title or title} |"):
        return None
    # The rail sits under the body; a Ctrl+X menu may paint over the frame's bottom rows
    # (the 21-1 scenario captures the frame with it open), so search the whole body.
    body = " ".join(content[1:])
    if not all(item in body for item in rail):
        return None
    return tuple(content)


def sheet(title, body, rail_row, width=WIDTH, height=HEIGHT, crumb=None, tear=None):
    """An offline 80x24 screen: the decision sheet over a composer and status row."""
    region = height - 4
    crumb = crumb or f"Needs you › {title} | 1 pending"
    inner = width - 2
    top = ("┌ Decision: " + title + " ").ljust(width - 1, "─") + "┐"
    lines = [top]
    content = [crumb, body]
    for index in range(1, region - 1):
        text = content[index - 1] if index <= len(content) else ""
        if index == region - 2:
            text = rail_row
        lines.append("│" + text.ljust(inner) + "│")
    lines.append("└" + "─" * inner + "┘")
    lines += ["┌" + "─" * inner + "┐", "│" + " " * inner + "│", "└" + "─" * inner + "┘",
              "Ready".ljust(width)]
    if tear == "no-bottom":
        lines[region - 1] = " " * width
    return "\n".join(lines)



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
