"""Story 9.x R5 — Profile swap actually kills the running MCP subprocess (TUI).

The Rust integration tests in `tests/integration_mcp_resilience.rs` prove that
`CompositeToolsetAdapter::prepare_detach()` kills a non-persistent MCP child.
This test drives the same code path through the TUI: open the profile-switcher
overlay (Ctrl+X, P), navigate to the ``base`` profile (tools ``builtin-only``, no
MCP), confirm, and verify through the process tree of this TUI (``/proc``,
identity = PID + start time) that the child died.

The swap must have completed: the status row's profile segment names ``base``
(``ProfileSwitched`` is the only event that sets it), the TUI is alive and no
fixture process, recorded or descendant, survives. A quit, a failed swap or a swap
to another profile leaves a missing or different segment.

Risk closed:
  * R5 (TUI tier) — user-driven profile swap deterministically tears down
    non-persistent MCP processes; no zombies left behind.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

from harness import status_row
from keys import CTRL_X, ENTER, UP
from mcp_support import close_adapters, wait_mcp_ready

SWAP_TARGET = "base"

pytestmark = [pytest.mark.story_9_4, pytest.mark.mcp]


MCP_FIXTURE_SIG = "fixtures/mcp_fixture.py"


def _proc_stat(pid, proc_root):
    """(state, ppid, start time) from ``<proc_root>/<pid>/stat`` (the command may hold spaces and
    parentheses: split after the last one), or None when the process is gone. The start time
    (clock ticks after boot) identifies a process: a PID that was reused has another one."""
    try:
        fields = (proc_root / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
        return fields[0], int(fields[1]), int(fields[19])
    except (OSError, IndexError, ValueError):
        return None


def _runs_fixture(pid, proc_root):
    try:
        return MCP_FIXTURE_SIG in (proc_root / str(pid) / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
    except OSError:
        return False


def _fixture_children(root_pid, proc_root=Path("/proc")):
    """{pid: start time} of the MCP fixture processes in the process tree below ``root_pid``:
    never a sibling xdist worker's fixture child."""
    parents, starts = {}, {}
    for entry in proc_root.iterdir():
        stat = _proc_stat(entry.name, proc_root) if entry.name.isdigit() else None
        if stat is not None:
            parents[int(entry.name)], starts[int(entry.name)] = stat[1], stat[2]
    below, frontier = set(), {root_pid}
    while frontier:
        frontier = {pid for pid, ppid in parents.items() if ppid in frontier and pid not in below}
        below |= frontier
    return {pid: starts[pid] for pid in sorted(below) if _runs_fixture(pid, proc_root)}


def _alive(pid, start, proc_root=Path("/proc")):
    """The recorded fixture process (same PID and start time) is a running (not zombie) process
    that still has the fixture command line, wherever it was reparented: a reused PID is another
    process."""
    stat = _proc_stat(pid, proc_root)
    return stat is not None and stat[0] != "Z" and stat[2] == start and _runs_fixture(pid, proc_root)


def _survivors(recorded, root_pid, proc_root=Path("/proc")):
    """The one survivor decision: every recorded fixture process still alive (an orphan that a
    broken teardown reparented away from the TUI included), plus any fixture process still below
    the TUI."""
    alive = {pid for pid, start in recorded.items() if _alive(pid, start, proc_root)}
    return sorted(alive | set(_fixture_children(root_pid, proc_root)))


def _open_profile_switcher(tui) -> None:
    """``Ctrl+X p`` from the Input focus the survivor starts in; wait for the list frame."""
    tui.send(CTRL_X + "p")
    tui.wait_until(lambda s: " Profile Switcher " in s, "the Profile Switcher list frame", timeout=10)


# A list row: cursor marker (``> `` on the selected one), active marker, colour swatch, name.
_PROFILE_ROW = re.compile(r"(>| ) (?:● |  )█ ([\w.-]+)")


def _profile_rows(screen: str) -> list[tuple[bool, str]]:
    """``(selected, name)`` of every profile row below the switcher's title, in list order."""
    rows = screen.split("\n")
    title = next((i for i, row in enumerate(rows) if " Profile Switcher " in row), None)
    if title is None:
        return []
    found = (_PROFILE_ROW.search(row) for row in rows[title + 1:])
    return [(match.group(1) == ">", match.group(2)) for match in found if match]


def _selected_profile(screen: str) -> str | None:
    selected = [name for is_selected, name in _profile_rows(screen) if is_selected]
    return selected[0] if len(selected) == 1 else None


def _select_profile(tui, name: str) -> None:
    """Press ``UP`` one key at a time, waiting after each key for the selection to move, until the
    selected row names ``name``: at most one full cycle of the list (the switcher wraps, so a
    fixed number of presses lands anywhere); a list without ``name`` fails loudly."""
    screen = tui.wait_until(lambda s: _selected_profile(s) is not None, "a selected profile row")
    names = [profile for _, profile in _profile_rows(screen)]
    seen = [_selected_profile(screen)]
    for _ in range(len(names)):
        if seen[-1] == name:
            return
        tui.send(UP)
        previous = seen[-1]
        screen = tui.wait_until(
            lambda s: _selected_profile(s) not in (None, previous), "the profile selection to move")
        seen.append(_selected_profile(screen))
    assert seen[-1] == name, f"Profile {name!r} was not selected in one cycle of {names}; visited {seen}"


def _profile_segment(screen: str, name: str) -> bool:
    """The status row's profile segment (``█ <name>``, ``█ <name> (preview)`` for a preview
    profile), read positionally on the status row."""
    return re.search(rf"█ {re.escape(name)}(?: \(preview\))?(?:\s|│|$)", status_row(screen)) is not None


def test_profile_swap_to_coding_kills_mcp_child(tui_with_mcp):
    """Pre-swap: the echo MCP child runs under this TUI and the status row names ``tests-mcp``.
    Swap to ``SWAP_TARGET`` (``base``: tools ``composite → builtin-only``, no MCP). Post-swap: the
    status row names it (the one success-only state change, ``ProfileSwitched``), the TUI is
    still alive and no fixture process, recorded or descendant, survives.

    The node id keeps its historical ``coding`` name (ledger and CI history). ``coding`` is NOT
    a target: it is the config's active profile, so the product answers a request for it with
    the already-active shortcut (``ProfileSwitched``, no transition executed) and the child
    keeps running (measured; the fault-injection negative control of the tui-3b record)."""
    tui = tui_with_mcp
    # The handshake completed: ``prepare_detach`` has something to disconnect.
    wait_mcp_ready(tui)
    close_adapters(tui)

    root_pid = tui.child.pid
    pre_pids = _fixture_children(root_pid)
    assert pre_pids, (
        "Pre-swap: at least one MCP fixture child of this TUI must be running. "
        "Fixture wiring or handshake timing is broken."
    )
    # Positive control: the predicate that proves the swap reads the real segment.
    before = tui.get_screen_text()
    assert _profile_segment(before, "tests-mcp") and not _profile_segment(before, SWAP_TARGET), (
        f"Pre-swap: the status row should name 'tests-mcp'. Screen:\n{before}"
    )

    _open_profile_switcher(tui)
    _select_profile(tui, SWAP_TARGET)

    # Enter opens the diff preview; ``y`` is the explicit accept binding for the preview.
    tui.send(ENTER)
    tui.wait_until(lambda s: f" Preview: {SWAP_TARGET} " in s, f"the {SWAP_TARGET} diff preview")
    tui.send("y")

    # The swap completed: only ``ProfileSwitched`` sets the active profile; a failed swap closes
    # the switcher too but leaves the segment on ``tests-mcp``.
    tui.wait_until(
        lambda s: _profile_segment(s, SWAP_TARGET) and " Preview: " not in s and " Profile Switcher " not in s,
        f"the status row naming the swapped-to profile {SWAP_TARGET!r}",
        timeout=15,
    )
    # Teardown is async: ``prepare_detach`` disconnects the clients, then the new composite is built.
    tui.wait_until(
        lambda _: not _survivors(pre_pids, root_pid),
        f"no MCP fixture process of this TUI (recorded {sorted(pre_pids)}) to survive the swap",
        timeout=10,
    )

    assert tui.child.isalive(), "The TUI exited during the swap."
    assert _profile_segment(tui.get_screen_text(), SWAP_TARGET)
    survivors = _survivors(pre_pids, root_pid)
    assert not survivors, (
        "Post-swap: no MCP fixture children of this TUI should remain after swapping "
        f"to a non-MCP profile, but pids={survivors} are still alive.\n"
        f"Recent log tail:\n" + "\n".join(tui.log_lines()[-40:])
    )


def _write_proc(proc_root, pid, ppid, cmdline, state="S", start=1000):
    entry = proc_root / str(pid)
    entry.mkdir(exist_ok=True)
    (entry / "stat").write_text(
        f"{pid} (a (b) c) {state} {ppid} {pid} {pid} 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0\n")
    (entry / "cmdline").write_bytes(("\0".join(cmdline) + "\0").encode())


def test_fixture_children_are_scoped_to_one_process_tree(tmp_path):
    """The survivor decision (the one the live test uses) is scoped to one TUI's tree and keyed on
    process identity: a sibling xdist worker's fixture child is not counted, a reused PID (any
    command, the same command included) and a zombie are not the recorded process, and a recorded
    child that a broken teardown orphaned (reparented to PID 1) is still a survivor."""
    fixture = ["python3", f"/repo/tests_tui/{MCP_FIXTURE_SIG}"]
    _write_proc(tmp_path, 100, 1, ["rustain", "--new"])
    _write_proc(tmp_path, 101, 100, fixture, start=11)
    _write_proc(tmp_path, 102, 101, ["python3", "-c", f"import runpy; runpy.run_path('{MCP_FIXTURE_SIG}')"], start=12)
    _write_proc(tmp_path, 103, 100, ["sh", "-c", "true"])
    _write_proc(tmp_path, 200, 1, ["rustain", "--new"])  # a sibling worker's TUI and its child
    _write_proc(tmp_path, 201, 200, fixture, start=21)
    recorded = _fixture_children(100, tmp_path)
    assert recorded == {101: 11, 102: 12} and _fixture_children(200, tmp_path) == {201: 21}
    assert _survivors(recorded, 100, tmp_path) == [101, 102]  # still running; never the sibling's 201

    # orphaned by a broken teardown: no longer below the TUI, still a survivor
    _write_proc(tmp_path, 101, 1, fixture, start=11)
    assert _fixture_children(100, tmp_path) == {}
    assert _survivors(recorded, 100, tmp_path) == [101, 102]

    # the recorded 101 exited and its PID went to another worker's fixture: same command, new start
    _write_proc(tmp_path, 101, 1, fixture, start=99)
    assert _survivors(recorded, 100, tmp_path) == [102]

    # a zombie is exited, an unrelated process on a reused PID is not the fixture, a missing PID is gone
    _write_proc(tmp_path, 102, 1, fixture, state="Z", start=12)
    assert _survivors(recorded, 100, tmp_path) == []
    _write_proc(tmp_path, 300, 1, ["bash"], start=12)
    assert not _alive(300, 12, tmp_path) and not _alive(999, 12, tmp_path)
