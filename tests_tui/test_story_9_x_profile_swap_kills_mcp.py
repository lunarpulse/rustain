"""Story 9.x R5 — Profile swap actually kills the running MCP subprocess (TUI).

The Rust integration tests in `tests/integration_mcp_resilience.rs` prove that
`CompositeToolsetAdapter::prepare_detach()` kills a non-persistent MCP child.
This test drives the same code path through the TUI: open the profile-switcher
overlay (Ctrl+X, P), navigate to a profile without composite-MCP, confirm,
and verify via pgrep that the child died.

Risk closed:
  * R5 (TUI tier) — user-driven profile swap deterministically tears down
    non-persistent MCP processes; no zombies left behind.
"""
from __future__ import annotations

import time
from pathlib import Path

import pytest

from keys import CTRL_X, ENTER, UP


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
    tui.chat_mode()
    tui.send(CTRL_X)
    assert tui.wait_for_screen("Press a key", timeout=3.0), (
        f"Ctrl+X must open which-key. Screen:\n{tui.get_screen_text()}"
    )
    tui.send("p")
    # The switcher overlay renders a list of profiles; the active one is
    # marked. Wait until it's visible.
    assert tui.wait_for_screen("Profile", timeout=4.0), (
        f"Profile switcher overlay did not open. Screen:\n{tui.get_screen_text()}"
    )


def test_profile_swap_to_coding_kills_mcp_child(tui_with_mcp):
    """Pre-swap: echo MCP child running. Swap to `coding` profile (which uses
    the `builtin-full` tools adapter, no MCP). Post-swap: no fixture child
    remains.
    """
    tui = tui_with_mcp
    # Let the MCP handshake settle so prepare_detach actually has something
    # to disconnect.
    time.sleep(3.0)

    root_pid = tui.child.pid
    pre_pids = _fixture_children(root_pid)
    assert pre_pids, (
        "Pre-swap: at least one MCP fixture child of this TUI must be running. "
        "Fixture wiring or handshake timing is broken."
    )

    _open_profile_switcher(tui)

    # The switcher lists profiles; the cursor starts on the active profile
    # (tests-mcp). Navigate up to a different profile. The embedded profiles
    # ship in alphabetical order: base, coding, personal-assistant. "coding"
    # is the default and the most stable swap target.
    #
    # We send Up arrows liberally — the switcher clamps at the top, so
    # extras are harmless and keep the test resilient to profile-list
    # additions.
    for _ in range(5):
        tui.send(UP)
        time.sleep(0.1)

    # Some switchers wrap the highlight to "coding" naturally; assert the
    # selected highlight matches before confirming. We just look for the
    # string "coding" in the visible region.
    assert tui.wait_for_screen("coding", timeout=3.0), (
        f"Profile 'coding' should appear in the switcher list. "
        f"Screen:\n{tui.get_screen_text()}"
    )

    # Enter opens the diff preview; press Enter again (or 'y') to confirm.
    tui.send(ENTER)
    time.sleep(0.5)
    # Confirm via 'y' which is the explicit accept binding for the preview.
    tui.send("y")

    # Profile swap is async: prepare_detach disconnects clients, then the
    # new composite is built. Give it generously.
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        if not _survivors(pre_pids, root_pid):
            break
        time.sleep(0.5)

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
