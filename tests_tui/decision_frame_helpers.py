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


def open_decision(tui, kind, rail, timeout=45):
    """Open a selected queued decision, then wait out its structural inert window."""
    title = "Decision: " + kind
    deadline = time.monotonic() + timeout
    phase = "waiting"
    while True:
        screen = tui.get_screen_text()
        assert "unavailable in this session" not in screen, screen
        if _frame(screen, title, rail):
            return wait_frame(tui, title, rail, timeout)
        if phase == "waiting" and has_row(screen, "Needs you (") and has_row(screen, "^X q"):
            tui.send("\x18q")
            phase = "list"
        if phase == "list":
            lines = screen.splitlines()
            start = next((i for i, line in enumerate(lines)
                          if line.strip().startswith("┌ Needs you ")), None)
            end = next((i for i in range(start + 1, len(lines))
                        if lines[i].strip().startswith("└") and lines[i].strip().endswith("┘")),
                       None) if start is not None else None
            frame = lines[start:end + 1] if end is not None else ()
            list_rail = any("[Enter] Open" in line for line in frame)
            selected = any(
                line.strip().lstrip("│ ").startswith(("▸ " + kind + " ·", "▶ " + kind + " ·"))
                for line in frame
            )
            if frame and list_rail and selected:
                tui.send("\r")
                phase = "opening"
        assert time.monotonic() < deadline, f"{title} arrival:\n{screen}"


def open_recipient(tui, timeout=45):
    """Open the current recipient draft whether it preempted or accumulated."""
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        assert "unavailable in this session" not in screen, screen
        if _frame(screen, "Decision: Recipient draft", ("[v] View item",)):
            break
        if (not has_row(screen, "Decision: Peer consent")
                and has_row(screen, "Needs you (") and has_row(screen, "^X q")):
            tui.send("\x18q")
            while True:
                screen = tui.get_screen_text()
                if _frame(screen, "Needs you", ("[Enter] Open",)) and any(
                    row.strip().lstrip("│ ").startswith(("▸ Recipient draft ·", "▶ Recipient draft ·"))
                    for row in screen.splitlines()
                ):
                    tui.send("\r")
                    break
                assert time.monotonic() < deadline, f"recipient queue item:\n{screen}"
            break
        assert time.monotonic() < deadline, f"recipient decision arrival:\n{screen}"
    return wait_frame(tui, "Decision: Recipient draft", ("[v] View item",), timeout)


def view_recipient(tui):
    """Open the associated RecipientSurface without resolving the decision."""
    tui.send("v")
