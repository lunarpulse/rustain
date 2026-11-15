"""Story 21.10 — the Epic 21 capture journey (J21): seven legs, three viewports, one receipt each.

J21 is an Epic 21 capture journey, not a PRD journey. Each leg drives the real binary through a
scene provider at the viewport named by ``RUSTAIN_J21_VIEWPORT`` (``80x24`` default, ``60x16`` and
``130x30`` accepted), waits structurally for settled frames, saves the labelled panes and writes a
``journey-J21-<STAMP>.transcript.txt`` receipt the unchanged ``.github/scripts/check-receipt.sh``
accepts (the journey arm: ledger millisecond window, scene hash, stamp). The writer runs that
verifier on its own receipt before returning and fails the case on any ``FAIL``.

Receipts never go into the product tree (D-17): ``RUSTAIN_J21_RECEIPT_DIR`` names where they are
written (the campaign points it at a scratch directory that is then archived in the outer repo);
without it the receipt lives and dies inside the run's scratch root. The 80x24 legs are the Epic 21
pre-release smoke in the ``native_nightly`` lane; 60x16 and 130x30 exist only as a campaign
environment value, never as a test id.

Behaviour truth lives in Rust and in the T1 core survivors; every leg names its owners in the
receipt's NOT PROVEN block. What a receipt proves: the named binary (sha256) under the named scene
(sha256) painted the asserted rows at the named viewport when each structural wait settled, inside
the recorded hour. It does not prove authenticity, colour or style, real terminal emulators, a real
provider, latency, or the behaviour the Rust tests own. Colour and style are never evidence
(``NO_COLOR=1``); no ``sleep`` is an oracle.

The offline controls (``test_j21_receipt_writer_controls`` and ``test_j21_predicate_*``) need no
binary: each predicate accepts a whole synthetic frame at 60x16, 80x24 and 130x30 and rejects its
named torn or mutated variant.
"""

from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, NamedTuple

import pytest

sys.path.insert(0, str(Path(__file__).parent))

import harness
from decision_frame_helpers import wait_frame  # the attach leg's structural wait
from fixtures.scene_provider import SceneStub
from harness import RustainTUI
from keys import BACKSPACE, CTRL_X, ENTER, ESC  # noqa: F401 - the legs' common keys
from wcwidth import wcwidth

REPO = Path(__file__).resolve().parent.parent
SOURCE_REL = "tests_tui/test_story_21_10_epic21_capture.py"
SCENE_REL = "tests_tui/journeys/j21/scenes/j21-{leg}-v1.json"
CHECKER = REPO / ".github" / "scripts" / "check-receipt.sh"
JOURNEYS_DIR = REPO / "tests_tui" / "journeys"

LEGS = ("notify", "keys", "inspect", "decide", "attach", "markdown", "onboard")
VIEWPORTS = {"60x16": (60, 16), "80x24": (80, 24), "130x30": (130, 30)}
MAX_RECEIPT_BYTES = 192 * 1024
JOURNEY_TITLE = "journey J21 — Epic 21 capture journey (Story 21.10), not a PRD journey"


# ── Viewport, binary, tree ───────────────────────────────────────────────────


def viewport_from_env(environ=None) -> tuple[str, int, int]:
    """``(name, cols, rows)`` from ``RUSTAIN_J21_VIEWPORT``; anything but the three sizes fails."""
    environ = os.environ if environ is None else environ
    name = environ.get("RUSTAIN_J21_VIEWPORT", "80x24").strip() or "80x24"
    if name not in VIEWPORTS:
        raise ValueError(f"RUSTAIN_J21_VIEWPORT={name!r}: expected one of {sorted(VIEWPORTS)}")
    cols, rows = VIEWPORTS[name]
    return name, cols, rows


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _tree_label() -> str:
    """``rustain@<short>[-dirty]``; ``-dirty`` is scoped to the binary's inputs, as the J0 wrapper does."""
    short = subprocess.run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
                           capture_output=True, text=True).stdout.strip() or "unknown"
    dirty = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain", "--", "src/", "Cargo.toml", "Cargo.lock"],
                           capture_output=True, text=True).stdout.strip()
    return f"rustain@{short}{'-dirty' if dirty else ''}"


# ── The receipt writer ───────────────────────────────────────────────────────


def render_receipt(*, recorded_at: str, binary_label: str, binary_sha: str, version: str, tree: str,
                   scene_rel: str, scene_sha: str, source_sha: str, viewport: str, beats: list[str],
                   panes: list[tuple[str, str]], ledger: str, sessions: str, requests: str,
                   not_proven: str) -> str:
    """The receipt text. Raises on a duplicate pane label, a missing ``source:`` hash, a ``/home/``
    substring anywhere, or a receipt over 192 KiB (labelled panes only)."""
    if not source_sha:
        raise ValueError("the receipt needs a source: hash (the driver bytes it was recorded by)")
    labels = [label for label, _ in panes]
    duplicates = sorted({label for label in labels if labels.count(label) > 1})
    if duplicates:
        raise ValueError(f"duplicate pane label(s): {duplicates}")
    out = [
        JOURNEY_TITLE,
        f"recorded-at: {recorded_at}",
        f"binary:      {binary_label}",
        f"binary-sha256: {binary_sha}",
        f"version:     {version}",
        f"tree:        {tree}",
        f"scene:       {scene_rel} sha256:{scene_sha}",
        f"source:      {SOURCE_REL} sha256:{source_sha}",
        f"viewport:    {viewport}",
        "",
        "=== the beats",
        *[f"[beat] {beat}" for beat in beats],
    ]
    for label, body in panes:
        out += [f"--- pane: {label} ---", body.rstrip("\n"), "--- end pane ---"]
    out += [
        "",
        "=== --- ledger --- (product-minted milliseconds, one row per stream POST; the title call writes none)",
        ledger.rstrip("\n") or "(no ledger rows)",
        "=== --- sessions --- (ids, titles, is_default_resume — session list is cwd-bound)",
        sessions.rstrip("\n"),
        "=== --- requests --- (every request the binary made, in order)",
        requests.rstrip("\n"),
        "",
        "=== --- what this run did not prove ---",
        not_proven.rstrip("\n"),
        "",
    ]
    text = "\n".join(out)
    if "/home/" in text:
        raise ValueError("the receipt carries a /home/ path (AD-10-13: receipts name no home directory)")
    if len(text.encode("utf-8")) > MAX_RECEIPT_BYTES:
        raise ValueError(f"the receipt is {len(text.encode('utf-8'))} bytes, over {MAX_RECEIPT_BYTES}")
    return text


def verify_receipt(path: Path) -> str:
    """Run the unchanged verifier against the product root; return its output; raise on any FAIL."""
    result = subprocess.run(["bash", str(CHECKER), "--product-root", str(REPO), str(path)],
                            capture_output=True, text=True)
    output = result.stdout + result.stderr
    if result.returncode != 0 or "FAIL" in output:
        raise AssertionError(f"check-receipt.sh rejected {path}:\n{output}")
    return output


def _stamp(recorded_at: str) -> str:
    """``2026-11-15T10:11:12Z`` → the filename stamp ``2026-11-15T10-11-12Z``."""
    return re.sub(r"T(\d\d):(\d\d):(\d\d)Z$", r"T\1-\2-\3Z", recorded_at)


def write_receipt(text: str, *, receipt_root: Path, leg: str, viewport: str, recorded_at: str) -> Path:
    """Write ``<root>/<leg>-<WxH>-<YYYYmmddTHHMMSSffffffZ>-<uuid32>/journey-J21-<STAMP>.transcript.txt``
    and verify it. Refuses any root inside the product checkout (receipts never go into the tree)."""
    root = receipt_root.resolve()
    if root == REPO or REPO in root.parents:
        raise ValueError(f"refusing to write a receipt inside the product checkout {REPO} (D-17: receipts are archived)")
    now = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    directory = root / f"{leg}-{viewport}-{now}-{uuid.uuid4().hex}"
    directory.mkdir(parents=True)
    path = directory / f"journey-J21-{_stamp(recorded_at)}.transcript.txt"
    path.write_text(text, encoding="utf-8")
    verify_receipt(path)
    return path


# ── Run context ──────────────────────────────────────────────────────────────

LegRunner = Callable[["J21Run"], None]
_LEG_RUNNERS: dict[str, LegRunner] = {}


def leg(name: str) -> Callable[[LegRunner], LegRunner]:
    """Register the runner of one leg (``LEGS``)."""
    if name not in LEGS:
        raise ValueError(f"unknown J21 leg {name!r}")

    def register(runner: LegRunner) -> LegRunner:
        _LEG_RUNNERS[name] = runner
        return runner

    return register


class J21Run:
    """One leg's scratch root, scene stub, panes and beats.

    The root is ``/tmp/j21-<leg>-…`` (short: the daemon socket path must stay under 108 bytes) and is
    removed only after a clean run. Every TUI the leg starts comes from ``tui()``: hermetic env (the
    scene stub's credentials, scratch ``HOME``/``XDG_*``/``TMPDIR``, ``NO_COLOR=1``), an empty
    allow-list unless asked, the run's viewport.
    """

    def __init__(self, leg_name: str, viewport: str, receipt_root: Path | None = None) -> None:
        self.leg = leg_name
        self.viewport = viewport
        self.cols, self.rows = VIEWPORTS[viewport]
        self.persona = f"scene-j21-{leg_name}"
        self.scene_rel = SCENE_REL.format(leg=leg_name)
        self.scene = REPO / self.scene_rel
        self.root = Path(tempfile.mkdtemp(prefix=f"j21-{leg_name}-", dir="/tmp"))
        self.home = self.root / "home"
        self.ws = self.root / "ws"
        self.tmp = self.root / "tmp"
        for path in (self.home, self.ws, self.tmp, self.root / "xdg"):
            path.mkdir()
        self.receipt_root = receipt_root or Path(os.environ.get("RUSTAIN_J21_RECEIPT_DIR", "") or self.root / "receipts")
        self.recorded_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        self.binary, _ = harness._resolve_binary()
        self.stub = SceneStub(self.scene, self.root / "requests.jsonl")
        self.beats: list[str] = []
        self.panes: list[tuple[str, str]] = []
        self.data_dirs: list[Path] = [self.ws / ".rustain_data"]
        self.not_proven = ""
        self.keep = False

    # lifecycle

    def __enter__(self) -> "J21Run":
        if not self.scene.is_file():
            raise FileNotFoundError(self.scene)
        self.stub.start()
        assert self.stub.sha256 == _sha256_file(self.scene), "the stub announced another scene hash"
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.stub.stop()
        if exc_type is not None:
            print(f"[j21] run root kept for diagnosis: {self.root}", flush=True)
        elif not self.keep:
            shutil.rmtree(self.root, ignore_errors=True)

    # environment and processes

    def env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        env = self.stub.env(self.persona, self.home)
        env.update({
            "XDG_CONFIG_HOME": str(self.root / "xdg" / "config"),
            "XDG_DATA_HOME": str(self.root / "xdg" / "data"),
            "XDG_CACHE_HOME": str(self.root / "xdg" / "cache"),
            "XDG_STATE_HOME": str(self.root / "xdg" / "state"),
            "TMPDIR": str(self.tmp),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "RUSTAIN_PROFILE": "",
        })
        env.update(extra or {})
        return env

    def tui(self, *, workspace: Path | None = None, extra_args: list[str] | None = None,
            env: dict[str, str] | None = None, allowed_tools: list[str] | None = None,
            notification_ttl_secs: int | None = None) -> RustainTUI:
        """An unstarted harness (use ``with run.tui(...) as tui:``); an empty allow-list by default."""
        return RustainTUI(fresh=True, build=False, workspace=workspace or self.ws,
                          allowed_tools=[] if allowed_tools is None else allowed_tools,
                          env_overrides=self.env(env), extra_args=extra_args, binary=self.binary,
                          notification_ttl_secs=notification_ttl_secs)

    # evidence

    def beat(self, message: str) -> None:
        self.beats.append(message)
        print(f"[beat] {message}", flush=True)

    def pane(self, label: str, screen: str) -> None:
        """Save one labelled pane (the whole screen, trailing blanks trimmed). A label is used once."""
        if any(label == existing for existing, _ in self.panes):
            raise ValueError(f"duplicate pane label {label!r}")
        self.panes.append((label, "\n".join(line.rstrip() for line in screen.split("\n"))))

    def _ledger(self) -> str:
        parts = []
        for data_dir in self.data_dirs:
            for path in sorted((data_dir / "usage").glob("*.jsonl")):
                parts.append(path.read_text(encoding="utf-8").rstrip("\n"))
        return "\n".join(part for part in parts if part)

    def _sessions(self) -> str:
        env = self.env({"RUSTAIN_CONFIG_DIR": str(self.ws / ".rustain"), "RUSTAIN_DATA_DIR": str(self.ws / ".rustain_data")})
        result = subprocess.run([str(self.binary), "session", "list", "--json"], cwd=self.ws, env=env,
                                capture_output=True, text=True, timeout=60)
        return result.stdout.strip() or f"(session list produced no output; exit {result.returncode})"

    def finish(self) -> Path:
        """Stop the stub (a desync or unknown path fails here), write and verify the receipt."""
        status = self.stub.stop()
        assert status == 0, f"scene provider exited {status}: {self.stub.rows()}"
        assert self.not_proven, "the leg named no NOT PROVEN block"
        assert self.panes, "the leg saved no pane"
        ledger = self._ledger()
        assert '"timestampMs"' in ledger, "no product-minted ledger row: the leg sent no scene turn"
        version = subprocess.run([str(self.binary), "--version"], capture_output=True, text=True,
                                 env={"HOME": str(self.home), "PATH": os.environ.get("PATH", "")}
                                 ).stdout.strip().splitlines()[0]
        text = render_receipt(
            recorded_at=self.recorded_at,
            binary_label=os.environ.get("RUSTAIN_J21_BINARY_LABEL", "").strip() or self.binary.name,
            binary_sha=_sha256_file(self.binary), version=version, tree=_tree_label(),
            scene_rel=self.scene_rel, scene_sha=self.stub.sha256, source_sha=_sha256_file(Path(__file__)),
            viewport=self.viewport, beats=self.beats, panes=self.panes, ledger=ledger,
            sessions=self._sessions(), requests=self.stub.log_path.read_text(encoding="utf-8"),
            not_proven=self.not_proven)
        path = write_receipt(text, receipt_root=self.receipt_root, leg=self.leg, viewport=self.viewport,
                             recorded_at=self.recorded_at)
        self.beat(f"receipt verified: {path.name}")
        return path


def run_leg(leg_name: str, monkeypatch, receipt_root: Path | None = None) -> Path:
    """Run one leg at ``RUSTAIN_J21_VIEWPORT`` and return its verified receipt path."""
    viewport, cols, rows = viewport_from_env()
    monkeypatch.setattr(harness, "TERM_COLS", cols)
    monkeypatch.setattr(harness, "TERM_ROWS", rows)
    run = J21Run(leg_name, viewport, receipt_root)
    with run:
        _LEG_RUNNERS[leg_name](run)
        path = run.finish()
    return path


# ── Screen predicates (shared by the legs and their controls) ────────────────


def grid(screen: str) -> list[list[str]]:
    """The screen as rows of terminal cells: a wide glyph is followed by an empty continuation cell,
    so a column index is a terminal column on rows holding CJK or emoji."""
    cells = []
    for line in screen.split("\n"):
        row: list[str] = []
        for char in line:
            row.append(char)
            if wcwidth(char) == 2:
                row.append("")
        cells.append(row)
    return cells


class Window(NamedTuple):
    top: int
    bottom: int
    left: int
    right: int


def frame_window(screen: str, title: str) -> Window | None:
    """The frame whose title row reads ``┌<title>… ─┐``: top, bottom, left and right columns, only
    when it is whole — a continuous ``│`` on both edges of every body row and a closing ``└──┘`` at
    the same columns. None while it is torn, partial or absent."""
    cells = grid(screen)
    pattern = re.compile(r"┌ ?" + re.escape(title) + r"[^┌┐│└┘]*?─*┐")
    for top, row in enumerate(cells):
        match = pattern.search("".join(row))
        if match is None:
            continue
        left = sum(2 if wcwidth(c) == 2 else 1 for c in "".join(row)[:match.start()])
        right = left + sum(2 if wcwidth(c) == 2 else 1 for c in match.group(0)) - 1
        for index in range(top + 1, len(cells)):
            line = cells[index]
            edge_l = line[left] if left < len(line) else " "
            edge_r = line[right] if right < len(line) else " "
            if edge_l == "└" and edge_r == "┘":
                inner = line[left + 1:right]
                return Window(top, index, left, right) if inner and set(inner) == {"─"} else None
            if edge_l != "│" or edge_r != "│":
                return None
        return None
    return None


class Chrome(NamedTuple):
    composer: int  # the composer's top border row
    status: int  # the status row directly above it
    strip: int | None  # the mission-strip row above the status row, when painted


def chrome(screen: str, rows: int) -> Chrome | None:
    """The bottom chrome: a closed three-row composer on the last rows, a painted status row directly
    above it and the strip row above that when it matches the A22 grammar. None when any is torn."""
    lines = screen.split("\n")
    if len(lines) != rows:
        return None
    top = harness.composer_top(lines)
    if top is None or top != rows - 3:
        return None
    if not (lines[rows - 1].strip().startswith("└") and lines[rows - 1].strip().endswith("┘")):
        return None
    if not (lines[top].strip().endswith("┐") and lines[top + 1].strip().startswith("│")):
        return None
    if not lines[top - 1].strip():
        return None
    return Chrome(top, top - 1, top - 2 if harness.strip_row(screen) else None)


def settled_ok(screen: str, title: str, rows: int) -> bool:
    """K3: the frame is whole, the composer is closed, the status row is painted and the frame ends
    above the chrome."""
    window = frame_window(screen, title)
    bar = chrome(screen, rows)
    if window is None or bar is None:
        return False
    return window.bottom < (bar.strip if bar.strip is not None else bar.status)


def geometry_ok(screen: str, title: str, cols: int, rows: int) -> bool:
    """K4 (LR-5): below 100 columns the sheet is full width; at 100+ it is a centred window with equal
    margins; at every size the status row (and the strip) stay uncovered."""
    window = frame_window(screen, title)
    bar = chrome(screen, rows)
    if window is None or bar is None or window.top < 0:
        return False
    if window.bottom >= (bar.strip if bar.strip is not None else bar.status):
        return False
    if cols < 100:
        return window.left == 0 and window.right == cols - 1
    margins = (window.left, cols - 1 - window.right)
    return window.left > 0 and abs(margins[0] - margins[1]) <= 1


_STAGE_TOAST = re.compile(r"(?<![A-Za-z0-9])(?:Info|Success|Warning|Error) \S")
_STAGE_TOAST_BOX = re.compile(r"│(?:Info|Success|Warning|Error) \S.*│\s*$")
_STAGE_TOAST_DEPTH = 9  # three stacked bordered toasts of the 100+ column pane


def stage_toast_rows(screen: str, rows: int) -> frozenset[int]:
    """Indices of the toast rows painted over the Stage's bottom edge: below 100 columns a borderless
    row right-aligned to the pane edge (a transcript line that merely starts with a severity word stops
    short of the edge), at 100+ a three-row bordered box (``┌``/``│<Severity> <text>│``/``└`` at one
    column). Only these rows are toast; every other row above the chrome is Stage."""
    bar = chrome(screen, rows)
    if bar is None:
        raise ValueError("no whole chrome: the Stage rows cannot be located")
    lines = screen.split("\n")
    cols = len(lines[-1])  # the composer's closing border spans the screen
    floor = bar.strip if bar.strip is not None else bar.status
    found: set[int] = set()
    for index in range(floor - 1, max(floor - 1 - _STAGE_TOAST_DEPTH, -1), -1):
        row = lines[index].rstrip()
        if cols < 100:
            if _STAGE_TOAST.search(row) and not row.startswith("│") and len(row) >= cols - 2:
                found.add(index)
        else:
            match = _STAGE_TOAST_BOX.search(row)
            if match and 0 < index < len(lines) - 1:
                left = match.start()
                above, below = lines[index - 1], lines[index + 1]
                if left < len(above) and left < len(below) and above[left] == "┌" and below[left] == "└":
                    found.update((index - 1, index, index + 1))
    return frozenset(found)


def stage_rows(screen: str, rows: int, exclude: frozenset[int] = frozenset()) -> tuple[str, ...]:
    """The Stage rows: every row above the chrome except the identified toast rows (and ``exclude``)."""
    bar = chrome(screen, rows)
    if bar is None:
        raise ValueError("no whole chrome: the Stage rows cannot be located")
    lines = screen.split("\n")
    floor = bar.strip if bar.strip is not None else bar.status
    skip = stage_toast_rows(screen, rows) | exclude
    return tuple(line.rstrip() for index, line in enumerate(lines[:floor]) if index not in skip)


def stage_unchanged(before: str, after: str, rows: int) -> bool:
    """K5: every Stage row is equal, the final non-toast row included. Toast rows (either frame's) are
    excluded from both frames, so a toast that came or went is not a Stage change."""
    skip = stage_toast_rows(before, rows) | stage_toast_rows(after, rows)
    return stage_rows(before, rows, skip) == stage_rows(after, rows, skip)


def settle(tui: RustainTUI, predicate: Callable[[str], bool], label: str, timeout: float = 30.0,
           stable_for: float = 0.6) -> str:
    """Wait until ``predicate(screen)`` holds and the screen has not changed for ``stable_for`` seconds
    of PTY reads; return that screen. A structural wait: the loop is paced by the PTY read."""
    deadline = time.monotonic() + timeout
    previous: str | None = None
    since = 0.0
    while True:
        screen = tui.get_screen_text()
        now = time.monotonic()
        if not predicate(screen):
            previous = None
        elif screen != previous:
            previous, since = screen, now
        elif now - since >= stable_for:
            return screen
        assert now < deadline, f"settled {label}:\n{screen}"


# ── Synthetic frames for the offline controls ────────────────────────────────

SYNTH_STATUS = " anthropic/claude-sonnet-4-6 [F] │ Normal │ ctx: 0/1m (0%) │ Ready │"
SYNTH_STRIP = "^X s Mission Control"


def synth_frame(cols: int, rows: int, title: str | None, body: list[str] | None = None, *, strip: bool = True,
                toast: str = "", stage_label: str = "stage", centred: bool | None = None) -> str:
    """A whole synthetic screen: a window titled ``title`` (full width below 100 columns, centred at
    100+ with equal margins), Stage rows around it, the optional toast row, the strip row, the status
    row and a closed three-row composer."""
    chrome_rows = 4 + (1 if strip else 0)
    region = rows - chrome_rows
    body = body or ["body row"]
    lines = [f"{stage_label} {index}".ljust(cols) for index in range(rows)]
    if not (cols >= 100 if centred is None else centred):
        left, right, top, bottom = 0, cols - 1, 0, region - 1
    else:
        width = int(cols * 0.9)
        left = (cols - width) // 2
        right = left + width - 1
        top, bottom = 1, region - 1
    inner = right - left - 1
    window = ["┌ " + (title or "") + " " + "─" * (inner - len(title or "") - 2) + "┐"]
    for index in range(bottom - top - 1):
        text = body[index] if index < len(body) else ""
        window.append("│" + text.ljust(inner)[:inner] + "│")
    window.append("└" + "─" * inner + "┘")
    for offset, row in enumerate(window if title is not None else []):
        base = lines[top + offset]
        lines[top + offset] = base[:left] + row + base[right + 1:]
    next_row = region
    if toast:
        lines[region - 1] = toast.rjust(cols)
    if strip:
        lines[next_row] = SYNTH_STRIP.ljust(cols)
        next_row += 1
    lines[next_row] = SYNTH_STATUS.ljust(cols)
    lines[rows - 3] = "┌" + "─" * (cols - 2) + "┐"
    lines[rows - 2] = "│" + " " * (cols - 2) + "│"
    lines[rows - 1] = "└" + "─" * (cols - 2) + "┘"
    return "\n".join(lines)


SYNTH_VIEWPORTS = tuple((cols, rows) for cols, rows in VIEWPORTS.values())


# ── Offline controls ─────────────────────────────────────────────────────────


def _genuine_receipt_text(recorded_at: str, *, scene_leg: str = "notify") -> str:
    scene = REPO / SCENE_REL.format(leg=scene_leg)
    millis = int(datetime.strptime(recorded_at, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp() * 1000)
    return render_receipt(
        recorded_at=recorded_at, binary_label="rustain-four-release", binary_sha="0" * 64,
        version="rustain 0.0.0 (synthetic)", tree="rustain@0000000",
        scene_rel=SCENE_REL.format(leg=scene_leg), scene_sha=_sha256_file(scene),
        source_sha=_sha256_file(Path(__file__)), viewport="80x24", beats=["synthetic run"],
        panes=[("one", "┌ x ┐"), ("two", "└ x ┘")],
        ledger=f'{{"timestampMs":{millis},"sessionId":"s","usage":{{"tokensIn":1,"tokensOut":1}}}}',
        sessions="{}", requests='{"n":1}', not_proven="synthetic: proves nothing about a binary")


def _verifier_rejects(path: Path) -> str:
    result = subprocess.run(["bash", str(CHECKER), "--product-root", str(REPO), str(path)],
                            capture_output=True, text=True)
    output = result.stdout + result.stderr
    assert result.returncode != 0 and "FAIL" in output, output
    return output


def test_j21_receipt_writer_controls(tmp_path):
    """K1: a genuine synthetic receipt passes the unchanged verifier; MD1–MD5 each fail."""
    recorded_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    genuine = _genuine_receipt_text(recorded_at)

    path = write_receipt(genuine, receipt_root=tmp_path / "receipts", leg="notify", viewport="80x24",
                         recorded_at=recorded_at)
    assert path.parent.parent == (tmp_path / "receipts").resolve()
    assert re.fullmatch(r"notify-80x24-\d{8}T\d{12}Z-[0-9a-f]{32}", path.parent.name), path.parent.name
    assert "PASS" in verify_receipt(path)
    assert "/home/" not in genuine

    # MD1: a space after `sha256:` in the scene line is not the file's hash.
    scene_line = next(line for line in genuine.split("\n") if line.startswith("scene:"))
    spaced = path.parent / "spaced" / path.name
    spaced.parent.mkdir()
    spaced.write_text(genuine.replace(scene_line, scene_line.replace(" sha256:", " sha256: ")), encoding="utf-8")
    assert "scene hash mismatch" in _verifier_rejects(spaced)

    # MD2: the file stamp names another instant than recorded-at.
    shifted = path.parent / "shifted" / "journey-J21-2001-01-01T00-00-00Z.transcript.txt"
    shifted.parent.mkdir()
    shifted.write_text(genuine, encoding="utf-8")
    assert "disagree" in _verifier_rejects(shifted)

    # MD3: a duplicate pane label; MD4: a /home/ path; MD5: a write under tests_tui/journeys/; and a
    # receipt with no source: hash.
    common = dict(recorded_at=recorded_at, binary_label="x", binary_sha="0" * 64, version="v", tree="t",
                  scene_rel="s", scene_sha="0" * 64, source_sha="0" * 64, viewport="80x24", beats=[],
                  ledger="", sessions="", requests="", not_proven="n")
    with pytest.raises(ValueError, match="duplicate pane label"):
        render_receipt(**common, panes=[("a", "1"), ("a", "2")])
    with pytest.raises(ValueError, match="/home/"):
        render_receipt(**{**common, "version": "v at /home/someone/rustain"}, panes=[("a", "1")])
    with pytest.raises(ValueError, match="source"):
        render_receipt(**{**common, "source_sha": ""}, panes=[("a", "1")])
    with pytest.raises(ValueError, match="D-17"):
        write_receipt(genuine, receipt_root=JOURNEYS_DIR / "j21" / "receipts", leg="notify", viewport="80x24",
                      recorded_at=recorded_at)
    assert not (JOURNEYS_DIR / "j21" / "receipts").exists()
    # F03: no destination inside the product checkout at all (not only tests_tui/journeys/).
    for inside in (REPO, REPO / "tests_tui", REPO / "tests_tui" / "j21-receipts", REPO / "target" / "receipts"):
        with pytest.raises(ValueError, match="D-17"):
            write_receipt(genuine, receipt_root=inside, leg="notify", viewport="80x24", recorded_at=recorded_at)
    assert not (REPO / "tests_tui" / "j21-receipts").exists() and not (REPO / "target" / "receipts").exists()


def test_j21_predicate_settled():
    """K3: the settled-frame predicate accepts the whole frame at each viewport and rejects a frame
    missing its closing border, a torn body edge and a composer drawn open."""
    for cols, rows in SYNTH_VIEWPORTS:
        whole = synth_frame(cols, rows, "Tool · Read #1")
        assert settled_ok(whole, "Tool · Read #1", rows), whole
        lines = whole.split("\n")
        window = frame_window(whole, "Tool · Read #1")
        assert window is not None

        no_bottom = list(lines)
        no_bottom[window.bottom] = " " * cols
        assert not settled_ok("\n".join(no_bottom), "Tool · Read #1", rows)

        torn = list(lines)
        torn[window.top + 1] = torn[window.top + 1][:window.right] + " " + torn[window.top + 1][window.right + 1:]
        assert not settled_ok("\n".join(torn), "Tool · Read #1", rows)

        open_composer = list(lines)
        open_composer[rows - 1] = " " * cols
        assert not settled_ok("\n".join(open_composer), "Tool · Read #1", rows)


def test_j21_predicate_geometry():
    """K4: full-width sheets below 100 columns, a centred window at 130, status and strip uncovered;
    rejects a frame painted over the status row and a centred frame at 80."""
    for cols, rows in SYNTH_VIEWPORTS:
        whole = synth_frame(cols, rows, "System · Notifications")
        assert geometry_ok(whole, "System · Notifications", cols, rows), whole

        bar = chrome(whole, rows)
        assert bar is not None
        # MP2: the window grows down through the strip and over the status row (its closing border
        # lands on the status row).
        covered = whole.split("\n")
        window = frame_window(whole, "System · Notifications")
        assert window is not None
        inner = window.right - window.left - 1
        for index in range(window.bottom, bar.status):
            covered[index] = covered[index][:window.left] + "│" + " " * inner + "│" + covered[index][window.right + 1:]
        covered[bar.status] = covered[bar.status][:window.left] + "└" + "─" * inner + "┘" + covered[bar.status][window.right + 1:]
        assert not geometry_ok("\n".join(covered), "System · Notifications", cols, rows)

    # A centred window is not an 80-column sheet, and a full-width sheet is not a centred 130-column window.
    centred_at_80 = synth_frame(80, 24, "System · Notifications", centred=True)
    assert not geometry_ok(centred_at_80, "System · Notifications", 80, 24)
    full_at_130 = synth_frame(130, 30, "System · Notifications", centred=False)
    assert not geometry_ok(full_at_130, "System · Notifications", 130, 30)


def test_j21_predicate_stage():
    """K5: identical Stage rows are accepted (toast rows excluded); one changed Stage row is rejected;
    then the inspect leg's predicates (`inspect_predicate_checks`)."""
    def with_toast(cols: int, rows: int, text: str) -> str:
        """A Stage-only frame with a toast: one right-aligned row below 100 columns, a three-row bordered
        box at 100+, the newest adjacent to the strip row."""
        lines = synth_frame(cols, rows, None).split("\n")
        floor = rows - 5  # the strip row of the synthetic chrome
        if cols < 100:
            lines[floor - 1] = text.rjust(cols)
        else:
            width = len(text) + 2
            left = cols - 3 - width
            lines[floor - 3] = " " * left + "┌" + "─" * (width - 2) + "┐"
            lines[floor - 2] = " " * left + "│" + text + "│"
            lines[floor - 1] = " " * left + "└" + "─" * (width - 2) + "┘"
            lines[floor - 3:floor] = [row.ljust(cols) for row in lines[floor - 3:floor]]
        return "\n".join(lines)

    for cols, rows in SYNTH_VIEWPORTS:
        clean = synth_frame(cols, rows, None)
        before = with_toast(cols, rows, "Info Copied ×2")
        after = with_toast(cols, rows, "Warning Unknown command")
        assert stage_unchanged(before, after, rows)
        assert stage_unchanged(clean, after, rows)  # a toast that came or went is not a Stage change

        bar = chrome(after, rows)
        assert bar is not None
        lines = after.split("\n")
        lines[1] = lines[1].replace("stage", "stagE", 1)
        assert not stage_unchanged(before, "\n".join(lines), rows)

        # F04: the final non-toast Stage row (the one directly above the strip when no toast covers it)
        # is part of the comparison: corrupting it after a sheet closed is a Stage change.
        floor = bar.strip if bar.strip is not None else bar.status
        last = clean.split("\n")
        last[floor - 1] = last[floor - 1].replace("stage", "stagE", 1)
        assert not stage_unchanged(clean, "\n".join(last), rows)
        assert stage_rows(clean, rows)[-1] == f"stage {floor - 1}"

    # The inspect leg's cue, S-TOOL, breadcrumb, Mission Control and monitor predicates ride on this
    # control: the Stage-invariance family owns them (the nine-control census has no tenth id).
    inspect_predicate_checks()


# ── notify leg (21.1 notification lifecycle on the real binary) ──────────────────────────────────

NOTIFY_CONFIG = "\n[notifications]\ninfo_ttl_secs = 1\nwarning_ttl_secs = 1\nerror_ttl_secs = 3600\n"
NOTIFY_LOG_TITLE = "System · Notifications"
NOTIFY_SEVERITIES = ("Info", "Success", "Warning", "Error")
NOTIFY_GONE_WITHIN_SECS = 3.0  # a 1 s lifetime must be gone well inside the default 4 s / 10 s lifetimes
NOTIFY_CHORD_TEXT = "No binding for Ctrl+X j in Input"
NOTIFY_UNKNOWN_TEXT = "Unknown command: /nosuchcommand"
NOTIFY_CHORD_LOG_PREFIX = "No binding for Ctrl+X j"
NOTIFY_UNKNOWN_LOG_PREFIX = "Unknown command: /nosuch"
NOTIFY_MESSAGE = "J21-NOTIFY-MSG-1"
NOTIFY_REPLY = "J21-NOTIFY-REPLY-1"
NOTIFY_TOAST_DEPTH = 9  # three stacked bordered toasts of the 100+ column pane
_NOTIFY_SEVERITY_ALT = "|".join(NOTIFY_SEVERITIES)
_NOTIFY_NARROW_TOAST = re.compile(rf"(?<![A-Za-z0-9])({_NOTIFY_SEVERITY_ALT}) (\S.*?)(?: ×(\d+))?$")
_NOTIFY_WIDE_TOAST = re.compile(rf"│({_NOTIFY_SEVERITY_ALT}) (\S.*?)(?: ×(\d+))? *│\s*$")
_NOTIFY_LOG_ROW = re.compile(
    rf"^({_NOTIFY_SEVERITY_ALT}) · (\d{{2}}):(\d{{2}}):(\d{{2}}) · (\S+) · (?:×(\d+) · )?(.*)$")
_NOTIFY_BADGE = re.compile(r"(?<![A-Za-z0-9])Log !(\d+)(\+?)")


def notify_toasts(screen: str, cols: int, rows: int) -> list[tuple[str, str, int]]:
    """Every toast painted on the chat pane's bottom rows, newest first: ``(severity, painted text,
    count)``. Below 100 columns a toast is one borderless row, right-aligned to the pane edge (a
    transcript line that merely starts with a severity word stops short of the edge and is not one);
    at 100+ it is a three-row bordered box: ``│<Severity> <text>[ ×N]│`` with ``┌`` directly above and
    ``└`` directly below its left border. Empty when the chrome is torn."""
    bar = chrome(screen, rows)
    if bar is None:
        return []
    lines = screen.split("\n")
    floor = bar.strip if bar.strip is not None else bar.status
    found: list[tuple[str, str, int]] = []
    for index in range(floor - 1, max(floor - 1 - NOTIFY_TOAST_DEPTH, -1), -1):
        row = lines[index].rstrip()
        if cols < 100:
            match = _NOTIFY_NARROW_TOAST.search(row)
            if match and len(row) >= cols - 2:
                found.append((match.group(1), match.group(2), int(match.group(3) or 1)))
        else:
            match = _NOTIFY_WIDE_TOAST.search(row)
            if match and 0 < index < len(lines) - 1:
                left = match.start()
                above, below = lines[index - 1], lines[index + 1]
                if left < len(above) and left < len(below) and above[left] == "┌" and below[left] == "└":
                    found.append((match.group(1), match.group(2), int(match.group(3) or 1)))
    return found


def notify_toast(screen: str, cols: int, rows: int, severity: str, prefix: str, count: int | None = None) -> bool:
    """K6: a toast of exactly ``severity`` whose painted text starts with ``prefix`` (a clipped toast
    keeps its prefix and ends in ``…``), with ``×count`` when given. A Warning painted as Info fails."""
    return any(sev == severity and text.startswith(prefix) and (count is None or seen == count)
               for sev, text, seen in notify_toasts(screen, cols, rows))


def notify_gone(screen: str, cols: int, rows: int) -> bool:
    """K6: the chrome is whole and no toast is painted (the toast lifetime ran out)."""
    return chrome(screen, rows) is not None and not notify_toasts(screen, cols, rows)


def notify_badge(screen: str) -> int | None:
    """The ``Log !N`` badge of the status row (``99+`` reads as 99); None when none is painted."""
    match = _NOTIFY_BADGE.search(harness.status_row(screen))
    return int(match.group(1)) if match else None


def notify_log_entries(screen: str) -> list[tuple[str, int, str, int, str]]:
    """The S-LOG list rows, newest first: ``(severity, seconds of day, source, count, message)`` —
    ``Severity · HH:MM:SS · source · [×N · ]message``. A row that does not read so is skipped."""
    entries = []
    for row in harness.notification_log_rows(screen):
        match = _NOTIFY_LOG_ROW.match(row)
        if match is None:
            continue
        hours, minutes, seconds = (int(match.group(i)) for i in (2, 3, 4))
        if hours > 23 or minutes > 59 or seconds > 59:
            return []  # an impossible clock time: the row is not a log row
        entries.append((match.group(1), hours * 3600 + minutes * 60 + seconds, match.group(5),
                        int(match.group(6) or 1), match.group(7)))
    return entries


def notify_log_ok(screen: str, cols: int, rows: int, expected: list[tuple[str, str, str, int]]) -> bool:
    """K6: the whole S-LOG frame (K4 geometry, the rail painted) lists, newest first, the ``expected``
    ``(severity, source, message prefix, count)`` rows at its top, every row carrying its severity word
    and a ``HH:MM:SS`` time, times non-increasing down the list (modular over one day)."""
    if not geometry_ok(screen, NOTIFY_LOG_TITLE, cols, rows):
        return False
    if "[Esc] Close" not in harness.notification_log_text(screen):
        return False
    entries = notify_log_entries(screen)
    if len(entries) < len(expected):
        return False
    for (severity, source, prefix, count), entry in zip(expected, entries):
        if (entry[0], entry[2], entry[3]) != (severity, source, count) or not entry[4].startswith(prefix):
            return False
    return all((newer[1] - older[1]) % 86400 < 43200 for newer, older in zip(entries, entries[1:]))


def notify_clamp_lines(log_text: str) -> list[str]:
    """The run-log lines that report ``notifications.error_ttl_secs`` clamped to ``[1, 60]``."""
    return [line for line in log_text.splitlines()
            if "[1, 60]" in line and "notifications.error_ttl_secs" in line
            and "value=3600" in line and "clamped=60" in line]


def notify_other_clamp_lines(log_text: str) -> list[str]:
    """Run-log clamp lines for the Info/Warning lifetimes (1 s is in range: there must be none)."""
    return [line for line in log_text.splitlines()
            if "[1, 60]" in line and ("notifications.info_ttl_secs" in line or "notifications.warning_ttl_secs" in line)]


def notify_run_log(tui: RustainTUI) -> str:
    """The per-run log of the harness workspace (any rotation of ``rustain.log``)."""
    return "\n".join(path.read_text(encoding="utf-8", errors="replace")
                     for path in sorted(tui.wp.glob("rustain.log*")) if path.is_file())


def notify_type(tui: RustainTUI, text: str, label: str) -> None:
    """Type ``text`` into the composer one key at a time, each key paced by a PTY read, and wait
    until the composer row shows it."""
    for char in text:
        tui.send(char)
        tui.get_screen_text()
    tui.wait_until(lambda s: text.strip() in s.split("\n")[-2], label, timeout=10)


@leg("notify")
def run_notify(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    config = run.ws / ".rustain"
    config.mkdir(parents=True)
    # The harness writes this file only when absent: the allow-list section it writes itself, plus
    # the three lifetimes (error_ttl_secs = 3600 is outside [1, 60] on purpose).
    (config / "config.toml").write_text(harness.workspace_config_text([]) + NOTIFY_CONFIG, encoding="utf-8")
    badge_painted = cols >= 80  # the 60-column status row yields its right-hand cues (no `Log !N`, no `^X menu`)

    with run.tui() as tui:
        # 1. the launchpad, settled, with the two startup notices (1 s lifetimes) already gone.
        screen = settle(tui, lambda s: "Welcome to Rustain." in s and harness.mode_segment(s) == "Normal"
                        and notify_gone(s, cols, rows), "the launchpad with no toast")
        run.beat("launchpad: config has info/warning 1 s and error 3600 s; startup toasts expired")
        run.pane("01-launchpad", screen)
        before = (notify_badge(screen) or 0) if badge_painted else None

        # 2. Ctrl+X j twice, in one write: one coalesced Info toast row (`×2`), gone within 3 s.
        tui.send(CTRL_X + "j" + CTRL_X + "j")
        screen = tui.wait_until(lambda s: notify_toast(s, cols, rows, "Info", NOTIFY_CHORD_TEXT, count=2),
                                "the coalesced Info toast", timeout=10)
        seen_at = time.monotonic()
        assert [toast[:2] for toast in notify_toasts(screen, cols, rows)] == [("Info", NOTIFY_CHORD_TEXT)], \
            f"exactly one toast row:\n{screen}"
        if badge_painted:
            assert notify_badge(screen) == (before or 0), f"an Info notice raises no Log badge:\n{screen}"
        run.beat("Ctrl+X j x2: one coalesced Info toast row (x2)")
        run.pane("02-info-toast", screen)
        screen = tui.wait_until(lambda s: notify_gone(s, cols, rows), "the Info toast to expire",
                                timeout=NOTIFY_GONE_WITHIN_SECS)
        assert time.monotonic() - seen_at < NOTIFY_GONE_WITHIN_SECS
        run.beat("the Info toast is gone within 3 s (info_ttl_secs = 1)")
        run.pane("03-info-gone", screen)

        # 3. /nosuchcommand: a Warning toast row, gone within 3 s, one more unseen on the Log badge.
        notify_type(tui, "/nosuchcommand ", "the unknown slash typed")
        tui.send(ENTER)
        screen = tui.wait_until(lambda s: notify_toast(s, cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT, count=1),
                                "the Warning toast", timeout=10)
        seen_at = time.monotonic()
        assert len(notify_toasts(screen, cols, rows)) == 1, f"exactly one toast row:\n{screen}"
        run.beat("/nosuchcommand: a Warning toast row")
        run.pane("04-warning-toast", screen)
        screen = tui.wait_until(lambda s: notify_gone(s, cols, rows), "the Warning toast to expire",
                                timeout=NOTIFY_GONE_WITHIN_SECS)
        assert time.monotonic() - seen_at < NOTIFY_GONE_WITHIN_SECS
        if badge_painted:
            screen = tui.wait_until(lambda s: notify_badge(s) == (before or 0) + 1, "the Log badge counts the Warning",
                                    timeout=10)
        run.beat("the Warning toast is gone within 3 s (warning_ttl_secs = 1)"
                 + (f"; Log badge {before} -> {(before or 0) + 1}" if badge_painted else "; no Log badge at 60 columns"))
        run.pane("05-warning-gone", screen)

        # 4. Ctrl+X n: S-LOG newest first with `Severity · HH:MM:SS`, the badge cleared.
        tui.send(CTRL_X + "n")
        expected = [("Warning", "system", NOTIFY_UNKNOWN_LOG_PREFIX, 1), ("Info", "chord", NOTIFY_CHORD_LOG_PREFIX, 2)]
        screen = settle(tui, lambda s: notify_log_ok(s, cols, rows, expected)
                        and (not badge_painted or notify_badge(s) is None), "the notification log")
        run.beat("Ctrl+X n: System · Notifications, Warning above Info, severity + HH:MM:SS, badge cleared")
        run.pane("06-notification-log", screen)
        tui.send(ESC)
        screen = settle(tui, lambda s: NOTIFY_LOG_TITLE not in s and notify_gone(s, cols, rows)
                        and (not badge_painted or notify_badge(s) is None), "the log closed")
        run.pane("07-log-closed", screen)

        # 5. the run log carries the clamp line of the out-of-range error lifetime only.
        tui.wait_until(lambda _s: bool(notify_clamp_lines(notify_run_log(tui))), "the error_ttl_secs clamp line",
                       timeout=10)
        log_text = notify_run_log(tui)
        clamp = notify_clamp_lines(log_text)
        assert not notify_other_clamp_lines(log_text), "1 s lifetimes are in range: no clamp line for them"
        run.beat("run log clamp line: " + clamp[0].split(" WARN ", 1)[-1])
        run.pane("08-run-log-clamp-line", "\n".join(clamp))

        # 6. one real scene turn (the ledger needs a product-minted row).
        notify_type(tui, NOTIFY_MESSAGE, "the message typed")
        tui.send(ENTER)
        screen = settle(tui, lambda s: NOTIFY_REPLY in s and "Ready" in harness.status_row(s)
                        and chrome(s, rows) is not None, "the scene turn's reply")
        run.beat("one scene turn")
        run.pane("09-turn-reply", screen)

    turns = [row for row in run.stub.rows() if row.get("turn") and not row.get("desync")]
    assert len(turns) == 1 and NOTIFY_MESSAGE in turns[0].get("last_user", ""), turns
    run.not_proven = (
        "NOT PROVEN by this receipt: the Error toast lifetime at the [1, 60] clamp (60 s) is not waited out; "
        "no Error notice is raised (the configured error_ttl_secs = 3600 is proven only by its clamp line in the run log); "
        "at 60 columns the status row paints no `Log !N` badge, so the badge count and its clearing are asserted at 80 and 130 only; "
        "the startup notices (a Warning 'Model discovery ... returned 0 models' and an Info 'Mouse scroll enabled') are "
        "environment facts of the scene stub, so the badge is asserted relative (before -> before + 1) and the log rows "
        "by their own prefix; toast colour and style, the 99+ cap, retention, and the exact wording of notices other than "
        "the two raised here are not covered. Behaviour owners (Rust): tests/conformance_21_1_notification_lifecycle/"
        "k1_ttl.rs:19, :57, :94, :138 (lifetimes, coalescing, expiry); k3_geometry.rs:14, :60 (toast rows and Stage "
        "invariance); k4_retention.rs:142, :182; k7_log.rs:84, :137 (S-LOG rows, severity, HH:MM:SS, newest first); "
        "k8_chords.rs:19 (Ctrl+X n / x); tui-5c K1 (conformance_tui_orchestration.rs:843: the local store is built "
        "from the loaded config).")


def test_j21_predicate_toast():
    """K6: a toast row then a log row with severity and HH:MM:SS, at 60x16, 80x24 and 130x30. Accepts
    the whole frames; rejects MP4 (an Info toast still alive after its lifetime), MP5 (a Warning
    painted as Info, as toast and as log row) and the torn variants."""
    log_lines = ["Notifications (4)",
                 "Warning · 20:23:49 · system · Unknown command: /nosuchcommand — not a built-in or a custom command.",
                 "Info · 20:23:45 · chord · ×2 · No binding for Ctrl+X j in Input",
                 "Info · 20:23:44 · system · Mouse scroll enabled. Hold Shift to select text for copy.",
                 "[j/k] Move [Enter] Detail [c] Copy [x] Dismiss [/] Search",
                 "[Esc] Close"]

    def log_frame(cols: int, rows: int, lines: list[str]) -> str:
        """The synthetic S-LOG sheet; a row wider than the box is clipped with ``…`` as the list does."""
        inner = cols - 2 if cols < 100 else int(cols * 0.9) - 2
        return synth_frame(cols, rows, NOTIFY_LOG_TITLE, [row if len(row) <= inner else row[:inner - 1] + "…" for row in lines])
    expected = [("Warning", "system", NOTIFY_UNKNOWN_LOG_PREFIX, 1), ("Info", "chord", NOTIFY_CHORD_LOG_PREFIX, 2)]

    def with_toast(cols: int, rows: int, severity: str, text: str, count: int = 1) -> str:
        suffix = f" ×{count}" if count > 1 else ""
        frame = synth_frame(cols, rows, None).split("\n")
        floor = rows - 5  # the strip row of the synthetic chrome
        painted = f"{severity} {text}{suffix}"
        if cols < 100:
            painted = painted if len(painted) <= cols else painted[:cols - 1] + "…"
            frame[floor - 1] = painted.rjust(cols)
        else:
            width = len(painted) + 2
            left = cols - 3 - width
            frame[floor - 3] = " " * left + "┌" + "─" * (width - 2) + "┐"
            frame[floor - 2] = " " * left + "│" + painted + "│"
            frame[floor - 1] = " " * left + "└" + "─" * (width - 2) + "┘"
            for index in (floor - 3, floor - 2, floor - 1):
                frame[index] = frame[index].ljust(cols)
        return "\n".join(frame)

    for cols, rows in SYNTH_VIEWPORTS:
        # the toast: the whole frame is accepted with its severity, text and count
        info = with_toast(cols, rows, "Info", NOTIFY_CHORD_TEXT, 2)
        assert notify_toast(info, cols, rows, "Info", NOTIFY_CHORD_TEXT, count=2), info
        warning = with_toast(cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT + " — not a built-in or a custom command.")
        assert notify_toast(warning, cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT, count=1), warning
        clipped = with_toast(cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT + " — not a built-in or…")
        assert notify_toast(clipped, cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT), clipped
        assert [t[:2] for t in notify_toasts(info, cols, rows)] == [("Info", NOTIFY_CHORD_TEXT)]
        # MP5: a Warning painted as Info is not a Warning toast
        as_info = with_toast(cols, rows, "Info", NOTIFY_UNKNOWN_TEXT + " — not a built-in or a custom command.")
        assert not notify_toast(as_info, cols, rows, "Warning", NOTIFY_UNKNOWN_TEXT), as_info
        # a coalesced toast read as one: the count is part of the claim
        assert not notify_toast(with_toast(cols, rows, "Info", NOTIFY_CHORD_TEXT), cols, rows, "Info",
                                NOTIFY_CHORD_TEXT, count=2)
        # other text, a left-aligned transcript line that only starts with a severity word, a torn box
        assert not notify_toast(info, cols, rows, "Info", "Mouse scroll enabled")
        lines = synth_frame(cols, rows, None).split("\n")
        lines[rows - 6] = f"Warning {NOTIFY_UNKNOWN_TEXT} in the transcript"
        assert not notify_toasts("\n".join(lines), cols, rows)
        if cols >= 100:
            torn = info.split("\n")
            torn[rows - 8] = " " * cols  # the box's top border
            assert not notify_toast("\n".join(torn), cols, rows, "Info", NOTIFY_CHORD_TEXT, count=2)
        # MP4: the Info lifetime is ignored — the toast is still painted after 3 s: not gone
        clean = synth_frame(cols, rows, None)
        assert notify_gone(clean, cols, rows), clean
        assert not notify_gone(info, cols, rows), "MP4: an Info toast alive after its 1 s lifetime"
        assert not notify_gone(clean.replace("┌" + "─" * (cols - 2) + "┐", " " * cols), cols, rows)  # torn chrome

        # the log row: toast row, then the S-LOG list with severity and HH:MM:SS
        log = log_frame(cols, rows, log_lines)
        assert notify_log_ok(log, cols, rows, expected), log
        assert [e[0] for e in notify_log_entries(log)] == ["Warning", "Info", "Info"]
        # MP5 in the log: the Warning row painted as Info; severity swapped; order reversed
        assert not notify_log_ok(log_frame(cols, rows, [log_lines[0], log_lines[1].replace("Warning", "Info", 1), *log_lines[2:]]),
                                 cols, rows, expected)
        assert not notify_log_ok(log_frame(cols, rows, [log_lines[0], log_lines[2], log_lines[1], *log_lines[3:]]),
                                 cols, rows, expected)
        # no HH:MM:SS, an impossible clock time, a missing coalesced count, a missing rail, wrong geometry
        for mutated in (log_lines[1].replace(" · 20:23:49", ""), log_lines[1].replace("20:23:49", "20:61:49")):
            assert not notify_log_ok(log_frame(cols, rows, [log_lines[0], mutated, *log_lines[2:]]), cols, rows, expected)
        assert not notify_log_ok(
            log_frame(cols, rows, [log_lines[0], log_lines[1], log_lines[2].replace("×2 · ", ""), *log_lines[3:]]),
            cols, rows, expected)
        assert not notify_log_ok(log_frame(cols, rows, log_lines[:-1]), cols, rows, expected)
        other_cols, other_rows = (80, 24) if cols != 80 else (130, 30)
        assert not notify_log_ok(log_frame(other_cols, other_rows, log_lines), cols, rows, expected)
        # times must not rise down the list
        rising = [log_lines[0], log_lines[1], log_lines[2].replace("20:23:45", "20:25:45"), *log_lines[3:]]
        assert not notify_log_ok(log_frame(cols, rows, rising), cols, rows, expected)

    # the badge and the clamp lines
    assert notify_badge(" anthropic/claude-sonnet-4-6 │ Normal │ Ready │ █ coding          Log !1 ^X menu") is None  # not the status row
    assert notify_badge(synth_frame(80, 24, None).replace("Ready │", "Ready │ Log !2")) == 2
    assert notify_badge(synth_frame(80, 24, None)) is None
    line = ('2026-11-15T20:19:37.860617Z  WARN rustain::domain::models::config: notifications ttl outside '
            '[1, 60] seconds; clamped key="notifications.error_ttl_secs" value=3600 clamped=60')
    assert notify_clamp_lines(line) == [line]
    assert not notify_clamp_lines(line.replace("error_ttl_secs", "info_ttl_secs"))
    assert notify_other_clamp_lines(line.replace("error_ttl_secs", "info_ttl_secs"))
    assert not notify_clamp_lines(line.replace("value=3600", "value=30"))


# ── keys leg (AD-10-9): the Ctrl+X leader menu, derived from the generated fixture ─────────────────────────────

KEYS_CTRL_P = "\x10"
KEYS_FIXTURE = REPO / "tests_tui" / "fixtures" / "keymap_portable_rows.json"
KEYS_HOSTS = ("local, attached", "attached", "local")
KEYS_MONITOR_MIN_COLS = 120  # `SIDEBAR_MIN_WIDTH`: below it `Ctrl+X w` answers its remedy toast
KEYS_TAIL_MIN_COLS = 80  # at 60 the status facts need the whole row and the tail yields whole (status_bar.rs)
KEYS_HELP_TITLE = "Help — Rustain Keybindings"
KEYS_PALETTE_TITLE = "Command Palette"
KEYS_SLASH_TITLE = "Commands"
KEYS_MONITOR_TITLE = "Monitor · Tasks"
KEYS_NOT_PROVEN = """\
Not proven here (Rust and core own these):
- Every Leader row resolves BY VALUE to its command, menu name, qualifier and remedy; the generated fixture equals the
  registry; the menu paints names and `(n/a)` qualifiers: tests/conformance_21_3_keymap/k2_leader.rs (every_leader_follow_up_runs_its_command_by_value,
  escape_and_ctrl_c_cancel_the_leader_silently), k3_generated.rs (width_and_host_unavailable_chords_are_qualified_and_answer_with_remedy,
  menu_keeps_the_relative_order_of_21_1_and_21_2, status_tail_menu_offered_iff_ctrl_x_opens_leader, portable_rows_fixture_equals_the_registry_output),
  k6_docs.rs, and the attached host in src/infrastructure/runtime/attach_keys.rs (story21_3_attached_leader_runs_every_registry_row).
- The Ctrl-modified follow-ups (Ctrl+X Ctrl+C, Ctrl+X Ctrl+Q) carry no menu label (keymap/render.rs leader_menu): k2_leader.rs, k20_review_items.rs.
- The Attached-only row (Ctrl+X d) is not in the local menu and this run is a local session.
- The status tail yields whole when the status facts need the row (status_bar.rs story21_3_tail_yields_whole_when_status_facts_need_the_space):
  at 60 columns this receipt shows it absent, not painted.
- No per-row native sweep: the fixture rows other than ? and w (and the unbound j) were NOT pressed; the whole menu was only read.
- Not a real terminal emulator, colour, style, latency or a real provider; the scene's words prove nothing; the rendering of
  the reply needle is the only use of the stub's prose.
"""


def keys_fixture_rows() -> list[dict]:
    """The generated fixture rows (the Rust-checked registry output), parsed from the repo file."""
    return json.loads(KEYS_FIXTURE.read_text(encoding="utf-8"))["rows"]


def keys_menu_tokens(rows: list) -> list[str]:
    """The `[<KEY>]` tokens of the LOCAL Ctrl+X menu, derived from the fixture rows the way
    `keymap/render.rs::leader_menu` derives them: a character follow-up is uppercased (`P` → `[P]`), `Enter` and
    `Esc` keep their label; a Ctrl-modified follow-up carries no menu label (empty label, filtered out) and an
    Attached-only row is not in the local menu. A row this function cannot map RAISES (never skipped): a wrong
    context or host, a key that is not `Ctrl+X, <follow-up>`, an unknown follow-up name, or a pty_hex that is not the
    follow-up's wire bytes."""
    tokens: list[str] = []
    for index, row in enumerate(rows):
        where = f"fixture row {index}: {row!r}"
        if not isinstance(row, dict) or not all(isinstance(row.get(k), str) for k in ("context", "host", "key", "pty_hex")):
            raise ValueError(f"{where}: not a context/host/key/pty_hex row")
        if row["context"] != "Leader":
            raise ValueError(f"{where}: the leg maps Leader rows only")
        if row["host"] not in KEYS_HOSTS:
            raise ValueError(f"{where}: unknown host")
        match = re.fullmatch(r"Ctrl\+X, (.+)", row["key"])
        if match is None:
            raise ValueError(f"{where}: not a Ctrl+X follow-up")
        follow = match.group(1)
        try:
            wire = bytes.fromhex(row["pty_hex"])
        except ValueError as exc:
            raise ValueError(f"{where}: pty_hex is not hex") from exc
        modified = re.fullmatch(r"Ctrl\+([A-Za-z])", follow)
        if len(follow) == 1 and follow.isprintable() and not follow.isspace():
            expected_wire = b"\x18" + follow.lower().encode("utf-8")
            token = follow.upper()
        elif follow == "Enter":
            expected_wire, token = b"\x18\r", "Enter"
        elif follow == "Esc":
            expected_wire, token = b"\x18\x1b", "Esc"
        elif modified is not None:
            expected_wire, token = b"\x18" + bytes([ord(modified.group(1).lower()) - 96]), None
        else:
            raise ValueError(f"{where}: unknown follow-up {follow!r}")
        if wire != expected_wire:
            raise ValueError(f"{where}: pty_hex {row['pty_hex']} is not the wire of {row['key']}")
        if token is not None and "local" in row["host"]:
            tokens.append(token)
    return tokens


def keys_row_wire(rows: list, key: str) -> str:
    """The wire string (ESC-free text sent to the PTY) of the fixture row named ``key``: ``pty_hex`` decoded."""
    matches = [row for row in rows if row.get("key") == key]
    if len(matches) != 1:
        raise ValueError(f"fixture rows named {key!r}: {len(matches)}")
    return bytes.fromhex(matches[0]["pty_hex"]).decode("latin-1")


def keys_row_witness(rows: list, key: str) -> str:
    matches = [row for row in rows if row.get("key") == key]
    if len(matches) != 1 or not matches[0].get("witness"):
        raise ValueError(f"fixture row {key!r} has no unique witness")
    return matches[0]["witness"]


def keys_menu_box(screen: str, cols: int) -> list[str] | None:
    """The inner rows of the painted `Ctrl+X` box when it is whole: a `┌ Ctrl+X ──┐` title row from column 0 to the
    last column, a `│` on both edges of every body row and a closing `└──┘` of the same width. None otherwise."""
    lines = screen.split("\n")
    for top, line in enumerate(lines):
        if not line.startswith("┌ Ctrl+X "):
            continue
        row = line.rstrip()
        if len(row) != cols or not row.endswith("┐") or set(row[len("┌ Ctrl+X "):-1]) != {"─"}:
            return None
        body: list[str] = []
        for text in lines[top + 1:]:
            text = text.rstrip()
            if text.startswith("└"):
                return body if body and len(text) == cols and text.endswith("┘") and set(text[1:-1]) == {"─"} else None
            if len(text) != cols or not text.startswith("│") or not text.endswith("│"):
                return None
            body.append(text[1:-1])
        return None
    return None


def keys_menu_ok(screen: str, cols: int, tokens: list[str]) -> bool:
    """K7: the whole `Ctrl+X` box carries, in reading order, exactly the derived `[<KEY>]` tokens (every one, no
    row clipped, none extra), over at least two wrapped rows."""
    box = keys_menu_box(screen, cols)
    if box is None or len(box) < 2 or not tokens:
        return False
    painted = [token for row in box for token in re.findall(r"\[([^\[\]]+)\]", row)]
    return painted == tokens


def keys_menu_gone(screen: str) -> bool:
    return not any(line.startswith("┌ Ctrl+X ") for line in screen.split("\n"))


def keys_tail_ok(screen: str, cols: int, pending: bool) -> bool:
    """The status row's rightmost segment: `^X…` while the chord is pending, `^X menu` otherwise, flush to the right
    edge, from 80 columns; at 60 the tail yields whole (no `^X` in the status row, never a torn tail)."""
    status = harness.status_row(screen).rstrip()
    text = "^X…" if pending else "^X menu"
    if cols >= KEYS_TAIL_MIN_COLS:
        return status.endswith(text) and len(status) == cols and "│" in status
    return bool(status) and "^X" not in status


def keys_frame(screen: str, title: str) -> Window | None:
    """Like `frame_window`, but the closing border may carry a footer text (`└ 1/55 ───┘`): a title row, a `│` on both
    edges of every body row and a `└…─┘` at the same columns."""
    cells = grid(screen)
    pattern = re.compile(r"┌ ?" + re.escape(title) + r"[^┌┐│└┘]*?─*┐")
    for top, row in enumerate(cells):
        text = "".join(row)
        match = pattern.search(text)
        if match is None:
            continue
        left = sum(2 if wcwidth(c) == 2 else 1 for c in text[:match.start()])
        right = left + sum(2 if wcwidth(c) == 2 else 1 for c in match.group(0)) - 1
        for index in range(top + 1, len(cells)):
            line = cells[index]
            edge_l = line[left] if left < len(line) else " "
            edge_r = line[right] if right < len(line) else " "
            if edge_l == "└" and edge_r == "┘":
                inner = line[left + 1:right]
                ok = bool(inner) and inner[-1] == "─" and not set(inner) & set("┌┐│└┘")
                return Window(top, index, left, right) if ok and index > top + 1 else None
            if edge_l != "│" or edge_r != "│":
                return None
        return None
    return None


def keys_sheet_ok(screen: str, title: str, cols: int, rows: int) -> bool:
    """LR-5 for the Help frame and the palette: whole, full width below 100 columns, centred with equal margins at 100+,
    ending above the strip/status rows, which stay uncovered."""
    window = keys_frame(screen, title)
    bar = chrome(screen, rows)
    if window is None or bar is None:
        return False
    if window.bottom >= (bar.strip if bar.strip is not None else bar.status):
        return False
    if cols < 100:
        return window.left == 0 and window.right == cols - 1
    return window.left > 0 and abs(window.left - (cols - 1 - window.right)) <= 1


def keys_popup_ok(screen: str, cols: int, rows: int) -> bool:
    """The `/` slash popup: a whole `Commands [n/m]` box, the full width of the main column, ending directly above the
    status row (which stays painted) with the typed `/` in the composer."""
    window = keys_frame(screen, KEYS_SLASH_TITLE)
    bar = chrome(screen, rows)
    if window is None or bar is None or window.left != 0 or window.right != cols - 1:
        return False
    return (window.bottom == bar.status - 1 and harness.mode_segment(screen) is not None
            and screen.split("\n")[bar.composer + 1].startswith("│/"))


def keys_toast_text(screen: str, severity: str, text: str, rows: int = 4) -> bool:
    """A toast row (right-aligned, severity word first, optionally clipped by a trailing `…`, optionally followed by the
    monitor column's border) reads ``<severity> <text>``; ``text`` is a prefix of the painted message."""
    pattern = re.compile(rf"(?<![A-Za-z0-9]){severity} {re.escape(text)}")
    return any(pattern.search(row) for row in harness.toast_rows(screen, rows))


def keys_monitor_ok(screen: str, cols: int, rows: int) -> bool:
    """At 120+ columns `Ctrl+X w` shows the read-only monitor: a whole `Monitor · Tasks` column on the right edge."""
    window = frame_window(screen, KEYS_MONITOR_TITLE)
    return window is not None and window.right == cols - 1 and window.left >= cols // 2 and window.bottom == rows - 1


def keys_send(tui: RustainTUI, wire: str) -> None:
    tui.send(wire)


def keys_ready(screen: str) -> bool:
    return "Ready" in harness.status_row(screen)


@leg("keys")
def run_keys(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    fixture = keys_fixture_rows()
    tokens = keys_menu_tokens(fixture)  # raises on a row it cannot map
    help_wire = keys_row_wire(fixture, "Ctrl+X, ?")
    monitor_wire = keys_row_wire(fixture, "Ctrl+X, W")
    clear_wire = keys_row_wire(fixture, "Ctrl+X, X")
    witness = keys_row_witness(fixture, "Ctrl+X, W")
    assert help_wire == "\x18?" and monitor_wire == "\x18w", (help_wire, monitor_wire)
    assert not any(row["key"].lower() == "ctrl+x, j" for row in fixture), "j is a Leader row: not an unbound follow-up"
    run.beat(f"fixture: {len(fixture)} rows, {len(tokens)} local menu tokens derived: {' '.join(f'[{t}]' for t in tokens)}")

    with run.tui(notification_ttl_secs=120) as tui:
        # One real scene turn (the ledger carries the product-minted row).
        tui.send("J21-KEYS-MSG-1")
        tui.wait_until(lambda s: "J21-KEYS-MSG-1" in s, "the typed message in the composer")
        tui.send(ENTER)
        screen = settle(tui, lambda s: "J21-KEYS-REPLY-1" in s and keys_ready(s), "the reply, then Ready")
        run.beat("one scene turn answered; Ready")
        run.pane("01-after-turn", screen)

        # `Ctrl+X x` (pty 1878) clears the boot toasts so the remedy/unbound toasts below stand alone.
        keys_send(tui, clear_wire)
        screen = settle(tui, lambda s: "Model discovery" not in s and keys_menu_gone(s) and keys_tail_ok(s, cols, False),
                        "toasts cleared, leader closed, tail idle")
        run.beat("pty 1878 (Ctrl+X x) cleared the boot toasts")

        # (a)+(b) The whole menu, then the pending tail; Esc closes it and the tail returns to `^X menu`.
        tui.send(CTRL_X)
        screen = settle(tui, lambda s: keys_menu_ok(s, cols, tokens) and keys_tail_ok(s, cols, True),
                        "the whole Ctrl+X menu carrying every derived token, pending tail")
        box = keys_menu_box(screen, cols)
        assert box is not None
        run.beat(f"Ctrl+X: whole menu, {len(box)} wrapped rows, {len(tokens)} tokens in registry order; "
                 + ("pending tail ^X… painted" if cols >= KEYS_TAIL_MIN_COLS else "status tail yielded whole at this width"))
        run.pane("02-menu-pending", screen)
        tui.send(ESC)
        screen = settle(tui, lambda s: keys_menu_gone(s) and keys_tail_ok(s, cols, False), "menu closed, tail back to ^X menu")
        run.beat("Esc: menu closed" + (", tail back to ^X menu" if cols >= KEYS_TAIL_MIN_COLS else ""))
        run.pane("03-after-esc", screen)

        # (c1) `183f`: the Help frame.
        keys_send(tui, help_wire)
        screen = settle(tui, lambda s: keys_sheet_ok(s, KEYS_HELP_TITLE, cols, rows), "the Help frame, whole and placed")
        run.beat("pty 183f (Ctrl+X ?): the Help frame, " + ("full-width sheet" if cols < 100 else "centred window"))
        run.pane("04-help", screen)
        tui.send(ESC)
        screen = settle(tui, lambda s: keys_frame(s, KEYS_HELP_TITLE) is None and keys_ready(s), "Help closed")

        # (d1) Ctrl+P palette.
        tui.send(KEYS_CTRL_P)
        screen = settle(tui, lambda s: keys_sheet_ok(s, KEYS_PALETTE_TITLE, cols, rows), "the palette, whole and placed")
        run.beat("Ctrl+P: the Command Palette, " + ("full-width sheet" if cols < 100 else "centred window"))
        run.pane("05-palette", screen)
        tui.send(ESC)
        screen = settle(tui, lambda s: keys_frame(s, KEYS_PALETTE_TITLE) is None and keys_ready(s), "palette closed")

        # (d2) `/` slash popup.
        tui.send("/")
        screen = settle(tui, lambda s: keys_popup_ok(s, cols, rows), "the slash popup above the status row")
        run.beat("/: the slash popup, full width, directly above the status row")
        run.pane("06-slash", screen)
        tui.send(ESC)
        screen = settle(tui, lambda s: keys_frame(s, KEYS_SLASH_TITLE) is None and keys_ready(s), "slash popup closed")

        # (c2) `1877`: the remedy toast below 120 columns, the monitor column at 130.
        keys_send(tui, monitor_wire)
        if cols < KEYS_MONITOR_MIN_COLS:
            screen = settle(tui, lambda s: keys_toast_text(s, "Info", witness) and keys_menu_gone(s),
                            "the monitor remedy toast")
            run.beat("pty 1877 (Ctrl+X w): the remedy toast naming the 120-column minimum")
        else:
            screen = settle(tui, lambda s: keys_monitor_ok(s, cols, rows) and witness not in s and keys_menu_gone(s),
                            "the monitor column, no remedy toast")
            run.beat("pty 1877 (Ctrl+X w): the read-only monitor column on the right edge, no remedy")
        run.pane("07-monitor-w", screen)

        # (c3) the unbound follow-up `j`.
        tui.send(CTRL_X + "j")
        screen = settle(tui, lambda s: keys_toast_text(s, "Info", "No binding for Ctrl+X j in Input") and keys_menu_gone(s),
                        "No binding for Ctrl+X j in Input")
        run.beat("Ctrl+X j: Info toast `No binding for Ctrl+X j in Input`, leader closed")
        run.pane("08-unbound-j", screen)
    run.not_proven = KEYS_NOT_PROVEN


# ── offline control: leader menu completeness (K7) ────────────────────────────────────────────────────────────

KEYS_SYNTH_NAMES = {
    "P": "Profile", "M": "Model", "I": "Sign in", "A": "Adapters", "S": "Agents", "L": "A2A log", "R": "Room",
    "E": "Artifacts", "T": "Tasks", "U": "Usage", "H": "History", "W": "Monitor", "?": "Help",
    "N": "Notifications", "X": "Clear toasts", "Q": "Decisions", "Enter": "Send", "Esc": "Cancel",
}


def keys_synth_menu(cols: int, rows: int, tokens: list[str]) -> str:
    """A whole screen carrying the Ctrl+X box as `which_key_bar.rs` paints it: greedy rows of `[K] Name` with two-cell
    separators, centred, over the status row, `(n/a)` on the monitor below 120 columns."""
    inner = cols - 2
    items = [f"[{t}] {KEYS_SYNTH_NAMES[t]}" + (" (n/a)" if t == "W" and cols < KEYS_MONITOR_MIN_COLS else "") for t in tokens]
    wrapped: list[str] = []
    for item in items:
        if wrapped and len(wrapped[-1]) + 2 + len(item) <= inner:
            wrapped[-1] += "  " + item
        else:
            wrapped.append(item)
    lines = synth_frame(cols, rows, None).split("\n")
    region = rows - 5
    height = len(wrapped) + 2
    first = region - height
    lines[first] = "┌ Ctrl+X " + "─" * (cols - 10) + "┐"
    for offset, text in enumerate(wrapped):
        lines[first + 1 + offset] = "│" + text.center(inner) + "│"
    lines[region - 1] = "└" + "─" * inner + "┘"
    return "\n".join(lines)


def keys_synth_tail(screen: str, rows: int, cols: int, tail: str) -> str:
    lines = screen.split("\n")
    status = lines[rows - 4]
    lines[rows - 4] = (status[:cols - len(tail)].rstrip().ljust(cols - len(tail)) + tail) if tail else status
    return "\n".join(lines)


def test_j21_predicate_menu():
    """K7: the derived tokens and the painted whole menu. Accepts the full menu at 60x16, 80x24 and 130x30 (the box,
    every `[<KEY>]` in registry order, the pending and idle tails); rejects MP6 (a row clipped at 60 columns),
    a torn box, MD8 (a row the leg cannot map is skipped instead of failing) and MD11 (a derivation that ignores
    Enter/Esc)."""
    fixture = keys_fixture_rows()
    tokens = keys_menu_tokens(fixture)
    assert tokens == ["P", "M", "I", "A", "S", "L", "R", "E", "T", "U", "H", "W", "?", "N", "X", "Q", "Enter", "Esc"], tokens

    for cols, rows in SYNTH_VIEWPORTS:
        whole = keys_synth_menu(cols, rows, tokens)
        assert keys_menu_ok(whole, cols, tokens), whole
        box = keys_menu_box(whole, cols)
        assert box is not None and len(box) >= 2
        # the tail: pending `^X…`, idle `^X menu`; yields whole at 60
        pending = keys_synth_tail(whole, rows, cols, "^X…" if cols >= KEYS_TAIL_MIN_COLS else "")
        idle = keys_synth_tail(whole, rows, cols, "^X menu" if cols >= KEYS_TAIL_MIN_COLS else "")
        assert keys_tail_ok(pending, cols, True) and keys_tail_ok(idle, cols, False)
        assert (not keys_tail_ok(idle, cols, True)) == (cols >= KEYS_TAIL_MIN_COLS)
        if cols < KEYS_TAIL_MIN_COLS:
            assert not keys_tail_ok(keys_synth_tail(whole, rows, cols, "^X menu"), cols, False)  # a torn tail
        else:
            assert not keys_tail_ok(whole, cols, False)  # no tail at all

        lines = whole.split("\n")
        top = next(i for i, line in enumerate(lines) if line.startswith("┌ Ctrl+X "))
        bottom = next(i for i in range(top, rows) if lines[i].startswith("└"))
        # MP6: the last wrapped row clipped away (the menu loses `[Esc]` or the whole last row).
        clipped = list(lines)
        clipped[bottom - 1] = "│" + " " * (cols - 2) + "│"
        assert not keys_menu_ok("\n".join(clipped), cols, tokens)
        # MP6: a row cut at the right edge in the middle of a token.
        cut = list(lines)
        row = cut[bottom - 1]
        index = row.rindex("[")
        cut[bottom - 1] = row[:index + 2] + " " * (cols - index - 3) + "│"
        assert not keys_menu_ok("\n".join(cut), cols, tokens)
        # a missing closing border, a torn side edge, a box that does not span the screen
        no_bottom = list(lines)
        no_bottom[bottom] = " " * cols
        assert not keys_menu_ok("\n".join(no_bottom), cols, tokens)
        torn = list(lines)
        torn[top + 1] = torn[top + 1][:-1] + " "
        assert not keys_menu_ok("\n".join(torn), cols, tokens)
        assert not keys_menu_ok(whole, cols + 1, tokens)
        # an extra or swapped token
        assert not keys_menu_ok(whole, cols, tokens + ["D"])
        assert not keys_menu_ok(whole, cols, tokens[1:] + tokens[:1])

    # MD11: a derivation that ignores Enter/Esc yields 16 tokens; the whole real menu does not match them
    ignoring = [t for t in tokens if t not in ("Enter", "Esc")]
    assert len(ignoring) == 16 and not keys_menu_ok(keys_synth_menu(80, 24, tokens), 80, ignoring)
    assert keys_menu_tokens([row for row in fixture if row["key"] not in ("Ctrl+X, Enter", "Ctrl+X, Esc")]) == ignoring

    # MD8: a fixture row the leg cannot map FAILS (the mutant skips it and would still go green)
    def skipping(rows_in):
        out = []
        for row in rows_in:
            try:
                out += keys_menu_tokens([row])
            except ValueError:
                continue
        return out

    unmappable = [
        {**fixture[0], "key": "Ctrl+X, F13", "pty_hex": "18f3"},
        {**fixture[0], "key": "Alt+X, P"},
        {**fixture[0], "context": "Global"},
        {**fixture[0], "host": "daemon"},
        {**fixture[0], "pty_hex": "1871"},  # the wire of Q on the P row
        {**fixture[0], "pty_hex": "zz"},
        {"context": "Leader"},
        "not a row",
    ]
    for bad in unmappable:
        with pytest.raises(ValueError):
            keys_menu_tokens(fixture + [bad])
        assert skipping(fixture + [bad]) == tokens  # the MD8 mutant stays green: the real derivation must not
    # an Attached-only row and the Ctrl-modified rows are mapped, and carry no local token
    assert [t for t in keys_menu_tokens([fixture[12]])] == [] and fixture[12]["key"] == "Ctrl+X, D"
    assert keys_menu_tokens(fixture[-2:]) == []

    # the representative rows: wire bytes and witnesses straight from the fixture
    assert keys_row_wire(fixture, "Ctrl+X, ?") == "\x18?" and keys_row_wire(fixture, "Ctrl+X, W") == "\x18w"
    with pytest.raises(ValueError):
        keys_row_wire(fixture, "Ctrl+X, J")
    assert keys_row_witness(fixture, "Ctrl+X, W").startswith("The monitor needs a terminal at least 120")

    # the frame predicates: a footer border is whole, a missing bottom is not; the slash popup sits over the status row
    for cols, rows in SYNTH_VIEWPORTS:
        sheet = keys_synth_menu(cols, rows, tokens)  # any chrome-carrying frame
        framed = synth_frame(cols, rows, KEYS_PALETTE_TITLE)
        footer = framed.split("\n")
        window = frame_window(framed, KEYS_PALETTE_TITLE)
        assert window is not None
        footer[window.bottom] = footer[window.bottom][:window.left + 1] + " 1/55 " + "─" * (window.right - window.left - 7) + footer[window.bottom][window.right:]
        assert keys_sheet_ok("\n".join(footer), KEYS_PALETTE_TITLE, cols, rows)
        assert not keys_sheet_ok(synth_frame(cols, rows, None), KEYS_PALETTE_TITLE, cols, rows)
        assert not keys_sheet_ok(sheet, KEYS_PALETTE_TITLE, cols, rows)
        bare = framed.split("\n")
        bare[window.bottom] = " " * cols
        assert not keys_sheet_ok("\n".join(bare), KEYS_PALETTE_TITLE, cols, rows)
        wrong = synth_frame(130 if cols != 130 else 80, 30 if cols != 130 else 24, KEYS_PALETTE_TITLE)
        assert not keys_sheet_ok(wrong, KEYS_PALETTE_TITLE, cols, rows)

        # the slash popup: full width, bottom directly above the status row, `/` in the composer
        bar = chrome(framed, rows)
        assert bar is not None
        popup = synth_frame(cols, rows, None, strip=False).split("\n")
        status_index = rows - 4
        height = 6
        popup[status_index - height] = "┌ Commands [1/29] " + "─" * (cols - 19) + "┐"
        for index in range(status_index - height + 1, status_index - 1):
            popup[index] = "│" + "/new  Start".ljust(cols - 2) + "│"
        popup[status_index - 1] = "└" + "─" * (cols - 2) + "┘"
        popup[rows - 2] = "│/" + " " * (cols - 3) + "│"
        assert keys_popup_ok("\n".join(popup), cols, rows)
        covered = list(popup)
        covered[status_index] = popup[status_index - 1]
        assert not keys_popup_ok("\n".join(covered), cols, rows)
        assert not keys_popup_ok(synth_frame(cols, rows, None), cols, rows)
        no_slash = list(popup)
        no_slash[rows - 2] = "│" + " " * (cols - 2) + "│"
        assert not keys_popup_ok("\n".join(no_slash), cols, rows)


# ── inspect: constants and predicates ────────────────────────────────────────

INSPECT_TOOL = "Tool · Read #1"
INSPECT_CRUMB = "Tool · Read #1 › System · Notifications"
INSPECT_CUE = "[Enter] Inspect"
INSPECT_TABS = ("Agents", "Tasks", "Messages", "Room", "Artifacts")
INSPECT_FILE = "j21-inspect-notes.txt"
INSPECT_FILE_LINES = ("J21-INSPECT-FILE-LINE-1 alpha", "J21-INSPECT-FILE-LINE-2 beta")
INSPECT_RIGHT = "\x1b[C"
INSPECT_LEFT = "\x1b[D"
INSPECT_MONITOR_REMEDY = "The monitor needs a terminal at least 120"
INSPECT_SEVERITIES = ("Info", "Success", "Warning", "Error")


def inspect_inner(screen: str, title: str) -> list[str] | None:
    """The rows strictly inside the whole frame titled ``title`` (its own columns, borders cut off),
    trailing blanks trimmed; None while the frame is not whole."""
    window = frame_window(screen, title)
    if window is None:
        return None
    cells = grid(screen)
    return ["".join(cells[row][window.left + 1:window.right]).rstrip() for row in range(window.top + 1, window.bottom)]


def inspect_toast_free(screen: str, rows: int) -> bool:
    """No toast anywhere on the Stage (a borderless row below 100 columns, a bordered box at 100+):
    no severity word followed by text on a row above the strip (or status) row."""
    bar = chrome(screen, rows)
    if bar is None:
        return False
    notice = re.compile(r"(?<![✓✗] )\b(?:" + "|".join(INSPECT_SEVERITIES) + r") (?=\S)")
    above = screen.split("\n")[:bar.strip if bar.strip is not None else bar.status]
    return not any(notice.search(row) for row in above)


def inspect_cue_ok(screen: str, rows: int) -> bool:
    """Chat focus over the finished turn: the chrome is whole, no frame is open, exactly one Stage row is
    the focused Read header (`▸ <mark> Read <path> … [Enter] Inspect`, the cue ending the row) and the
    status row carries the same cue."""
    bar = chrome(screen, rows)
    if bar is None or "Tool · " in screen:
        return False
    lines = screen.split("\n")[:bar.strip if bar.strip is not None else bar.status]
    marked = [line for line in lines if re.match(r"^│ ▸ [●✓✗⊘] Read \S", line)]
    return (len(marked) == 1 and marked[0].rstrip().endswith(INSPECT_CUE)
            and INSPECT_CUE in harness.status_row(screen))


def inspect_tool_ok(screen: str, cols: int, rows: int) -> bool:
    """The S-TOOL frame for ``Read #1``: whole, geometry per viewport, the Output tab marked in the
    tab row, the file the product read painted in the body, the rail naming `[Esc] Back` last."""
    if not (settled_ok(screen, INSPECT_TOOL, rows) and geometry_ok(screen, INSPECT_TOOL, cols, rows)):
        return False
    inner = inspect_inner(screen, INSPECT_TOOL)
    if inner is None or len(inner) < 4:
        return False
    return (any(row == "[Output]  Input  Raw" for row in inner)
            and all(any(line in row for row in inner) for line in INSPECT_FILE_LINES)
            and inner[-1].startswith("[Esc] Back"))


def inspect_crumb_ok(screen: str, cols: int, rows: int) -> bool:
    """The S-LOG window stacked on S-TOOL: the breadcrumb title, whole, geometry per viewport, the
    log's own heading and its `[Esc] Close` rail."""
    if not (settled_ok(screen, INSPECT_CRUMB, rows) and geometry_ok(screen, INSPECT_CRUMB, cols, rows)):
        return False
    inner = inspect_inner(screen, INSPECT_CRUMB)
    return (inner is not None and len(inner) >= 3 and inner[0].startswith("Notifications (")
            and any("[Esc] Close" in row for row in inner))


def inspect_mc_ok(screen: str, tab: str, cols: int, rows: int) -> bool:
    """Mission Control on ``tab``: titled `Mission Control · <tab>`, whole, geometry per viewport, the
    tab row listing the five tabs in order with exactly ``tab`` marked `▸`, `[Esc] Back` in the rail."""
    title = f"Mission Control · {tab}"
    if not (settled_ok(screen, title, rows) and geometry_ok(screen, title, cols, rows)):
        return False
    inner = inspect_inner(screen, title)
    if inner is None or len(inner) < 3:
        return False
    labels = re.findall(r"([▸ ])(" + "|".join(INSPECT_TABS) + r")\b", inner[0])
    return ([name for _, name in labels] == list(INSPECT_TABS)
            and [name for mark, name in labels if mark == "▸"] == [tab]
            and "[Esc] Back" in inner[-1])


def inspect_monitor_ok(screen: str, rows: int) -> bool:
    """The monitor column (130 columns): a title row `┌Monitor · <Tab>──┐`, every row down to its
    `└──┘` closed by `│` at the border's column and at the right edge, the main column's status row
    painted beside it, and no Mission Control frame."""
    lines = screen.split("\n")
    if harness.composer_top(lines) is None:
        return False
    for top, row in enumerate(lines):
        match = re.search(r"┌Monitor · \w+─*┐$", row)
        if match is None:
            continue
        x = match.start()
        bottom = next((i for i in range(top + 1, len(lines)) if lines[i][x:].rstrip() and re.fullmatch(r"└─*┘", lines[i][x:].rstrip())), None)
        if bottom is None or bottom < top + 3:
            return False
        inside = lines[top + 1:bottom]
        closed = all(len(r) > x and r[x] == "│" and r.rstrip().endswith("│") for r in inside)
        beside = any("│ Normal │" in r[:x] for r in inside)
        return closed and beside and re.search(r"┌ ?Mission Control ·", screen) is None
    return False


def inspect_monitor_off_ok(screen: str, rows: int) -> bool:
    return chrome(screen, rows) is not None and "Monitor ·" not in screen


def inspect_monitor_remedy_ok(screen: str, rows: int) -> bool:
    """Below 120 columns: the width remedy toast on a toast row (cut by `…` when long), no column."""
    return (inspect_monitor_off_ok(screen, rows)
            and any(re.match(r"\s*Info " + re.escape(INSPECT_MONITOR_REMEDY), row)
                    for row in harness.toast_rows(screen, 3)))


# ── inspect: the live leg ────────────────────────────────────────────────────


@leg("inspect")
def run_inspect(run: J21Run) -> None:
    rows, cols = run.rows, run.cols
    (run.ws / INSPECT_FILE).write_text("\n".join(INSPECT_FILE_LINES) + "\n", encoding="utf-8")
    with run.tui(allowed_tools=["Read"]) as tui:
        settle(tui, lambda s: chrome(s, rows) is not None, "launchpad with its whole chrome")
        run.beat("a streamed reply with a Read call over a real file")
        tui.send("read the inspect notes\r")

        def turn_done(screen: str) -> bool:
            turns = [row for row in run.stub.rows() if "turn" in row]
            return (len(turns) == 2 and turns[1].get("tool_results") == 1 and chrome(screen, rows) is not None
                    and harness.status_row(screen).count("Ready") == 1)

        settle(tui, turn_done, "second request carried the Read result, Ready painted")

        run.beat("Esc → Chat: the focused Read header carries [Enter] Inspect")
        tui.send(ESC)
        # The Stage is compared before/after the inspector: wait until no toast overpaints it.
        stage = settle(tui, lambda s: inspect_cue_ok(s, rows) and inspect_toast_free(s, rows), "Chat cue, Stage free of toasts", timeout=60)
        run.pane("01-chat-cue", stage)

        run.beat("Enter → Tool · Read #1")
        tui.send(ENTER)
        tool = settle(tui, lambda s: inspect_tool_ok(s, cols, rows), "S-TOOL Read #1")
        run.pane("02-tool-read", tool)

        run.beat("Ctrl+X n → breadcrumb Tool · Read #1 › System · Notifications")
        tui.send(CTRL_X + "n")
        crumb = settle(tui, lambda s: inspect_crumb_ok(s, cols, rows), "breadcrumb over S-TOOL")
        run.pane("03-tool-notifications-crumb", crumb)

        run.beat("Esc → the same S-TOOL rows")
        tui.send(ESC)
        back = settle(tui, lambda s: inspect_tool_ok(s, cols, rows) and stage_unchanged(tool, s, rows), "S-TOOL restored, same rows")
        run.pane("04-tool-restored", back)

        run.beat("Esc → the Stage rows equal the capture taken before the inspector")
        tui.send(ESC)
        after = settle(tui, lambda s: inspect_cue_ok(s, rows) and stage_unchanged(stage, s, rows), "Stage restored, same rows")
        run.pane("05-stage-restored", after)

        run.beat("Ctrl+X s → Mission Control · Agents, then the tab cycle")
        tui.send(CTRL_X + "s")
        hub = settle(tui, lambda s: inspect_mc_ok(s, "Agents", cols, rows), "Mission Control · Agents")
        run.pane("06-mc-agents", hub)
        for index in range(1, len(INSPECT_TABS) + 1):
            name = INSPECT_TABS[index % len(INSPECT_TABS)]
            tui.send(INSPECT_RIGHT)
            hub = settle(tui, lambda s, name=name: inspect_mc_ok(s, name, cols, rows), f"Mission Control · {name}")
            run.pane(f"07-mc-right-{index}-{name.lower()}", hub)
        tui.send(INSPECT_LEFT)
        hub = settle(tui, lambda s: inspect_mc_ok(s, INSPECT_TABS[-1], cols, rows), "Mission Control · Artifacts (Left wraps)")
        run.pane("08-mc-left-wrap-artifacts", hub)

        run.beat("Esc → Stage rows equal the capture")
        tui.send(ESC)
        closed = settle(tui, lambda s: inspect_cue_ok(s, rows) and stage_unchanged(stage, s, rows), "hub closed, Stage unchanged")
        run.pane("09-hub-closed", closed)

        if cols >= 120:
            run.beat("Ctrl+X w → the monitor column (130 columns only)")
            tui.send(CTRL_X + "w")
            monitor = settle(tui, lambda s: inspect_monitor_ok(s, rows), "monitor column")
            run.pane("10-monitor-column", monitor)
            tui.send(CTRL_X + "w")
            off = settle(tui, lambda s: inspect_monitor_off_ok(s, rows) and inspect_cue_ok(s, rows), "monitor removed")
            run.pane("11-monitor-off", off)
        else:
            run.beat("Ctrl+X w → the width remedy toast, no column (below 120 columns)")
            tui.send(CTRL_X + "w")
            remedy = settle(tui, lambda s: inspect_monitor_remedy_ok(s, rows), "monitor width remedy")
            run.pane("10-monitor-remedy", remedy)

    run.not_proven = (
        "NOT PROVEN by this leg (stated, not hidden):\n"
        "- The inspector's tab content, wrap, search, copy and `j/k` navigation: owned by "
        "tests/conformance_21_5_tool_inspector/ (k1_open, k2_tabs, k4_nav, k5_invariance, k11_search, "
        "k12_copy, k13_wrap) and core 21_5 test_tool_inspector_smoke_leg_a.\n"
        "- The modal region geometry rule (centred 90% x 85% at 100+ columns, full sheet below) and the "
        "stack restore: owned by tests/conformance_21_4_modal_stack/k4_geometry.rs and the k5_restore "
        "tests in conformance_21_4_modal_stack.rs; this leg reads the painted columns only.\n"
        "- Mission Control tab bodies, the monitor's contents and the narrow-width remedy text: owned by "
        "tests/conformance_21_7_mission_control/ (k1_open, k2_keys, k11_monitor) and core 21_7 leg a; the "
        "tab row here proves presence and order only.\n"
        "- Stage restoration after the stack closes is compared against a capture taken after Stage toasts "
        "had expired; toast lifecycle is owned by tests/conformance_21_1_notification_lifecycle/.\n"
        "- tui-5b K1 (tests/conformance_tui_orchestration.rs:707) owns the orchestration of the Ctrl+X s "
        "and Ctrl+X w actions; this leg proves their painted outcome on a real terminal.\n"
        "- Colour, style and the cursor are not evidence; the served reply text is not evidence (the "
        "scene's file lines are the product's own Read output).\n"
        "- No pixel, scroll-position or timing claim; no Edit/Write diff tab (this scene calls only Read).")


# ── inspect: offline predicate control (synthetic frames built from the observed real shapes) ──

INSPECT_SYNTH_RAIL = "[Esc] Back  [Left/Right] Tab  [v] Rendered  [j/k] Scroll  [/] Search"
INSPECT_SYNTH_HEADER = "│ ▸ ✓ Read j21-inspect-notes.txt · 0.0s [Enter] Inspect"
INSPECT_SYNTH_STATUS = " anthr │ Normal │ ↑80 ↓12 │ Ready │ █ coding [Enter] Inspect"


def inspect_synth_window(cols: int, rows: int, title: str, head: list[str], tail: list[str], *,
                         centred: bool | None = None) -> str:
    """A synthetic frame whose first inner rows are ``head`` and whose last inner rows are ``tail``."""
    window = frame_window(synth_frame(cols, rows, title, centred=centred), title)
    assert window is not None
    height = window.bottom - window.top - 1
    return synth_frame(cols, rows, title, head + [""] * (height - len(head) - len(tail)) + tail, centred=centred)


def inspect_synth_tool(cols: int, rows: int, *, tabs: str = "[Output]  Input  Raw", rail: str = INSPECT_SYNTH_RAIL,
                       lines: tuple[str, ...] = INSPECT_FILE_LINES, centred: bool | None = None) -> str:
    head = ["[w] Wrap  [c] Copy · tc_x_1 · Success", tabs,
            *[f"{number}       {line}" for number, line in enumerate(lines, start=1)]]
    return inspect_synth_window(cols, rows, INSPECT_TOOL, head, ["Wrap on · Lines 1–3 of 3", rail], centred=centred)


def inspect_synth_stage(cols: int, rows: int, *, header: str = INSPECT_SYNTH_HEADER, status: str = INSPECT_SYNTH_STATUS,
                        extra: dict[int, str] | None = None) -> str:
    lines = synth_frame(cols, rows, None).split("\n")
    lines[3] = header.ljust(cols)[:cols]
    bar = chrome("\n".join(lines), rows)
    assert bar is not None
    lines[bar.status] = status.ljust(cols)[:cols]
    for index, text in (extra or {}).items():
        lines[index] = text.ljust(cols)[:cols]
    return "\n".join(lines)


def inspect_synth_hub(cols: int, rows: int, tab: str, *, order: tuple[str, ...] = INSPECT_TABS, marked: str | None = None,
                      rail: str = "[←/→] Tab  [Esc] Back") -> str:
    marked = marked or tab
    row = "  ".join(("▸" if name == marked else " ") + name for name in order)
    return inspect_synth_window(cols, rows, f"Mission Control · {tab}", [row, "No agents active."], [rail])


def inspect_synth_monitor(cols: int, rows: int, *, close: bool = True, tear: bool = False, hub: bool = False,
                          status: bool = True, title: bool = True) -> str:
    base = synth_frame(cols, rows, None).split("\n")
    x = cols - 39
    width = cols - x
    main = ["┌" + "─" * (x - 2) + "┐", "│" + " " * (x - 2) + "│", "└" + "─" * (x - 2) + "┘"]
    for index in range(rows):
        if index >= rows - 3:
            base[index] = main[index - (rows - 3)]
        else:
            base[index] = base[index][:x].ljust(x)
        if index == 0:
            part = "┌" + ("Monitor · Artifacts" if title else "") + "─" * (width - 2 - (19 if title else 0)) + "┐"
        elif index == rows - 1:
            part = ("└" + "─" * (width - 2) + "┘") if close else " " * width
        else:
            part = "│" + " " * (width - 2) + "│"
        if tear and index == 5:
            part = " " + part[1:]
        base[index] += part
    bar_status = rows - 4
    if not status:
        base[bar_status] = " " * x + base[bar_status][x:]
    if hub:
        base[2] = base[2][:3] + "┌Mission Control · Agents─┐" + base[2][29:]
    return "\n".join(base)


def inspect_predicate_checks():
    """The inspect leg's predicates accept the whole synthetic frame at 60x16 / 80x24 / 130x30 and reject
    each named torn or mutated variant (a missing cue, rail, tab mark, file row, border, status, a column
    left open, a toast on the Stage)."""
    for cols, rows in SYNTH_VIEWPORTS:
        # the Chat cue
        stage = inspect_synth_stage(cols, rows)
        assert inspect_cue_ok(stage, rows), stage
        assert not inspect_cue_ok(inspect_synth_stage(cols, rows, header=INSPECT_SYNTH_HEADER.replace(" [Enter] Inspect", "")), rows)
        assert not inspect_cue_ok(inspect_synth_stage(cols, rows, status=" anthr │ Normal │ Ready │"), rows)
        assert not inspect_cue_ok(inspect_synth_stage(cols, rows, extra={5: INSPECT_SYNTH_HEADER}), rows)  # two marked headers
        assert not inspect_cue_ok(inspect_synth_stage(cols, rows, extra={5: "┌Tool · Read #1────┐"}), rows)  # a frame is open
        assert not inspect_cue_ok(inspect_synth_stage(cols, rows, header=INSPECT_SYNTH_HEADER.replace("▸", "┄")), rows)  # not the focused tool

        # toast-free Stage: a borderless toast row and a bordered box both overpaint it
        assert inspect_toast_free(stage, rows)
        assert not inspect_toast_free(synth_frame(cols, rows, None, toast="Warning Model discovery for 'anthropic' returned 0 models."), rows)
        assert not inspect_toast_free(inspect_synth_stage(cols, rows, extra={6: "│Info Mouse scroll enabled. Hold Shift to select text│"}), rows)

        # S-TOOL
        tool = inspect_synth_tool(cols, rows)
        assert inspect_tool_ok(tool, cols, rows), tool
        assert not inspect_tool_ok(inspect_synth_tool(cols, rows, rail=""), cols, rows)  # no rail
        assert not inspect_tool_ok(inspect_synth_tool(cols, rows, tabs="Output  [Input]  Raw"), cols, rows)  # wrong tab marked
        assert not inspect_tool_ok(inspect_synth_tool(cols, rows, lines=(INSPECT_FILE_LINES[0], "")), cols, rows)  # file row missing
        window = frame_window(tool, INSPECT_TOOL)
        assert window is not None
        torn = tool.split("\n")
        torn[window.bottom] = " " * cols
        assert not inspect_tool_ok("\n".join(torn), cols, rows)  # closing border missing
        bar = chrome(tool, rows)
        assert bar is not None
        covered = tool.split("\n")
        inner_width = window.right - window.left - 1
        for index in range(window.bottom, bar.status):  # the window grows down over the strip and the status row
            covered[index] = covered[index][:window.left] + "│" + " " * inner_width + "│" + covered[index][window.right + 1:]
        covered[bar.status] = covered[bar.status][:window.left] + "└" + "─" * inner_width + "┘" + covered[bar.status][window.right + 1:]
        assert not inspect_tool_ok("\n".join(covered), cols, rows)  # frame painted over the status row
        assert not inspect_tool_ok(inspect_synth_tool(cols, rows, centred=cols < 100), cols, rows)  # the other viewport's geometry

        # breadcrumb over S-TOOL
        crumb = inspect_synth_window(cols, rows, INSPECT_CRUMB, ["Notifications (2)", "Info · 20:41:16 · system · Mouse"],
                                     ["[j/k] Move [Enter] Detail [c] Copy [x] Dismiss [/] Search", "[Esc] Close"]
                                     if cols < 80 else ["[j/k] Move [Enter] Detail [c] Copy [x] Dismiss [/] Search [Esc] Close"])
        assert inspect_crumb_ok(crumb, cols, rows), crumb
        assert not inspect_crumb_ok(inspect_synth_window(cols, rows, INSPECT_TOOL, ["Notifications (2)"], ["[Esc] Close"]), cols, rows)  # no breadcrumb
        assert not inspect_crumb_ok(inspect_synth_window(cols, rows, INSPECT_CRUMB, ["Notifications (2)"], [""]), cols, rows)  # no rail

        # Mission Control and its tab cycle
        for tab in INSPECT_TABS:
            hub = inspect_synth_hub(cols, rows, tab)
            assert inspect_mc_ok(hub, tab, cols, rows), hub
            assert not inspect_mc_ok(hub, INSPECT_TABS[(INSPECT_TABS.index(tab) + 1) % len(INSPECT_TABS)], cols, rows)
        assert not inspect_mc_ok(inspect_synth_hub(cols, rows, "Agents", marked="Tasks"), "Agents", cols, rows)  # wrong tab marked
        assert not inspect_mc_ok(inspect_synth_hub(cols, rows, "Agents", order=("Tasks", "Agents", "Messages", "Room", "Artifacts")), "Agents", cols, rows)
        assert not inspect_mc_ok(inspect_synth_hub(cols, rows, "Agents", rail=""), "Agents", cols, rows)

        # the monitor: a column at 130 only
        if cols >= 120:
            column = inspect_synth_monitor(cols, rows)
            assert inspect_monitor_ok(column, rows), column
            assert not inspect_monitor_ok(inspect_synth_monitor(cols, rows, close=False), rows)
            assert not inspect_monitor_ok(inspect_synth_monitor(cols, rows, tear=True), rows)
            assert not inspect_monitor_ok(inspect_synth_monitor(cols, rows, hub=True), rows)
            assert not inspect_monitor_ok(inspect_synth_monitor(cols, rows, status=False), rows)
            assert not inspect_monitor_ok(inspect_synth_monitor(cols, rows, title=False), rows)
            assert not inspect_monitor_ok(synth_frame(cols, rows, None), rows)  # no column
            assert not inspect_monitor_off_ok(column, rows)
            assert inspect_monitor_off_ok(synth_frame(cols, rows, None), rows)
        else:
            remedy = synth_frame(cols, rows, None, toast="Info The monitor needs a terminal at least 120 columns wide…")
            assert inspect_monitor_remedy_ok(remedy, rows), remedy
            assert not inspect_monitor_remedy_ok(synth_frame(cols, rows, None, toast="Warning The monitor needs a terminal at least 120 columns wide…"), rows)
            assert not inspect_monitor_remedy_ok(synth_frame(cols, rows, None), rows)  # no toast
            assert not inspect_monitor_remedy_ok(inspect_synth_monitor(130, 30), rows)  # a column painted


# ── The markdown leg (Epic 21: 21.9 stage polish, 21.11 display width, 21.12 markdown, 21.13 S-BLOCK) ──

MARKDOWN_USER = "explain <T> 漢字 👍🏽 é"
# pyte keeps no cell for the skin-tone modifier U+1F3FD: the painted row is the text without it, and the
# column of the trailing `é` is the same either way (the base emoji is two cells wide).
MARKDOWN_USER_PAINTED = MARKDOWN_USER.replace("\U0001F3FD", "")
MARKDOWN_USER_TAIL_COL = sum(2 if wcwidth(c) == 2 else 1 for c in MARKDOWN_USER_PAINTED.split("é")[0])
MARKDOWN_CAP = 30  # `code_max_rows` default: the Stage shows this many code rows of a committed fence
MARKDOWN_TITLE = "Block · rust #3"
MARKDOWN_TABLE12_COLLAPSES_BELOW = 70  # 12 columns need 6 * 12 + 1 = 73 cells: only the 60-column Stage collapses
MARKDOWN_STATUS = re.compile(r"^code 3/3 · rust · 40 lines · Rendered · col (\d+)-(\d+)/(\d+)$")
MARKDOWN_COLS = re.compile(r"col (\d+)-(\d+)/(\d+)$")
MARKDOWN_QUIET_NOT = ("Model discovery", "Mouse scroll enabled")


def markdown_scene_fence() -> list[str]:
    """The fence source embedded in the scene (the scene is only the INPUT of the copy assertion)."""
    scene = json.loads((REPO / SCENE_REL.format(leg="markdown")).read_text(encoding="utf-8"))
    turn = scene["personas"]["scene-j21-markdown"]["turns"][0]
    text = next(block["text"] for block in turn["content"] if block["type"] == "text")
    match = re.search(r"```rust\n(.*)\n```\Z", text, re.DOTALL)
    assert match, "the scene carries no closing rust fence"
    return match.group(1).split("\n")


def markdown_width(text: str) -> int:
    return sum(max(wcwidth(char), 0) for char in text)


def markdown_body(row: str) -> str:
    """A Stage row without its two-cell gutter (`│ ` or the focus marker `▸ `)."""
    row = row.rstrip()
    return row[2:] if row[:2] in ("│ ", "▸ ") else row


def markdown_ready(screen: str, rows: int) -> bool:
    bar = chrome(screen, rows)
    return bar is not None and "Ready" in screen.split("\n")[bar.status]


def markdown_quiet(screen: str) -> bool:
    return not any(text in screen for text in MARKDOWN_QUIET_NOT)


def markdown_stage(screen: str, rows: int) -> list[str]:
    try:
        return list(stage_rows(screen, rows))
    except ValueError:
        return []


def markdown_box(lines: list[str], ties: int):
    """The table box with `ties` column joints (`┬`) as bodies, or None while it is torn or absent."""
    bodies = [markdown_body(line) for line in lines]
    for top, body in enumerate(bodies):
        if body.startswith("┌") and body.endswith("┐") and body.count("┬") == ties:
            for end in range(top + 1, len(bodies)):
                edge = bodies[end]
                if edge.startswith("└") and edge.endswith("┘"):
                    box = bodies[top:end + 1]
                    return box if len({markdown_width(row) for row in box}) == 1 else None
                if not edge[:1] in ("│", "├"):
                    break
    return None


def markdown_claim_user(lines: list[str], cols: int) -> bool:
    for index, row in enumerate(lines):
        if row.rstrip() == MARKDOWN_USER_PAINTED and index and lines[index - 1].rstrip() == "You:":
            cells = grid(row)[0]
            return len(cells) > MARKDOWN_USER_TAIL_COL and cells[MARKDOWN_USER_TAIL_COL] == "é"
    return False


def markdown_claim_think(lines: list[str], cols: int) -> bool:
    rows = [line.rstrip() for line in lines]
    for index, row in enumerate(rows):
        if re.fullmatch(r"│ ◇ Thinking · \S.*", row) and "Inspect" not in row:
            folded = index + 1 < len(rows) and rows[index + 1] == "│"
            return folded and not any("J21-MARKDOWN-THINK" in other for other in rows)
    return False


def markdown_claim_headings(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    for index, body in enumerate(bodies):
        if body == "J21-MARKDOWN-H1" and index + 2 < len(bodies):
            rule = re.fullmatch(r"─{20,}", bodies[index + 1]) is not None
            h2 = "J21-MARKDOWN-H2" in bodies
            return rule and h2 and bodies[index + 2] in ("", "│")
    return False


def markdown_claim_lists(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    nested = ["• list one J21-MARKDOWN-LIST", "  • nested two", "    • nested three"]
    return any(bodies[i:i + 3] == nested for i in range(len(bodies)))


def markdown_claim_tasks(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    return any(bodies[i:i + 2] == ["• [ ] task todo", "• [x] task done"] for i in range(len(bodies)))


def markdown_claim_quote_link(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    return "│ J21-MARKDOWN-QUOTE" in bodies and "claude.ai (evil.test)" in bodies


def markdown_claim_table5(lines: list[str], cols: int) -> bool:
    box = markdown_box(lines, 4)
    if box is None or len(box) != 5:
        return False
    data = "".join(box[3])
    return "…" in data and "漢" in data and "Description" in box[1]


def markdown_claim_table12(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    if cols < MARKDOWN_TABLE12_COLLAPSES_BELOW:
        summary = re.compile(r"table · 12 cols × 1 row(?: \[Enter\] Inspect)?")
        return any(summary.fullmatch(body) for body in bodies) and markdown_box(lines, 11) is None
    box = markdown_box(lines, 11)
    return box is not None and len(box) == 5 and "c0" in box[1] and "c11" in box[1] and "11" in box[3]


def markdown_claim_marker(lines: list[str], cols: int) -> bool:
    bodies = [markdown_body(line) for line in lines]
    fence = markdown_scene_fence()
    marker = re.compile(rf"│… {len(fence) - MARKDOWN_CAP} more lines +│")
    for index, body in enumerate(bodies[:-1]):
        if marker.fullmatch(body):
            edge = bodies[index + 1]
            return edge.startswith("└") and edge.endswith("┘") and markdown_width(edge) == markdown_width(body)
    return False


def markdown_claim_focus(lines: list[str], cols: int) -> bool:
    return any(re.fullmatch(r"▸ ┌─ rust ─+ \[Enter\] Inspect ─┐", line.rstrip()) for line in lines)


def markdown_fence_expectations() -> list[re.Pattern]:
    """One painted-row pattern per source line the Stage shows (the first `MARKDOWN_CAP`)."""
    patterns = []
    for line in markdown_scene_fence()[:MARKDOWN_CAP]:
        if markdown_width(line) > 100:  # the long line is clipped with a continuation arrow, never wrapped
            patterns.append(re.compile(r"│let w = \"x+→│"))
        else:
            patterns.append(re.compile("│" + re.escape(line.expandtabs(8)) + " *│"))
    return patterns


def markdown_fence_ok(stages: list[list[str]]) -> bool:
    """K9 (MP8): over every Stage page, the capped fence is one box of equal-width rows holding exactly the
    first `MARKDOWN_CAP` source lines (the long one clipped, never wrapped), then `… N more lines`."""
    bodies = [markdown_body(line) for page in stages for line in page]
    headers = [body for body in bodies if body.startswith("┌─ rust ") and body.endswith("┐")]
    if not headers or len({markdown_width(header) for header in headers}) != 1:
        return False
    width = markdown_width(headers[0])
    fence = markdown_scene_fence()
    seen_numbers = {int(m.group(1)) for body in bodies for m in [re.match(r"│fn line_(\d\d)\(\) \{\}", body)] if m}
    if max(seen_numbers, default=0) != MARKDOWN_CAP:  # line_30 shown, line_31.. never
        return False
    for pattern in markdown_fence_expectations():
        hits = [body for body in bodies if pattern.fullmatch(body)]
        if not hits or any(markdown_width(body) != width for body in hits):
            return False
    if any("TAIL" in body or re.fullmatch(r"│x+ *│", body) for body in bodies):
        return False  # a reflowed fence paints the tail of the long line on a row of its own
    if any(body.startswith("│// J21-MARKDOWN-LAST") for body in bodies):
        return False
    return any(markdown_claim_marker(page, 0) for page in stages) and len(fence) > MARKDOWN_CAP


def markdown_viewer(screen: str, cols: int, rows: int):
    """S-BLOCK facts when the whole frame is painted with its geometry (full width below 100 columns,
    centred at 100+, status and strip uncovered): `(status_groups, body_rows, rail)`; None otherwise."""
    if not geometry_ok(screen, MARKDOWN_TITLE, cols, rows):
        return None
    window = frame_window(screen, MARKDOWN_TITLE)
    cells = grid(screen)
    inner = []
    for y in range(window.top + 1, window.bottom):
        row = cells[y]
        inner.append("".join(row[window.left + 1:window.right]).rstrip())
    if len(inner) < 4:
        return None
    status = MARKDOWN_STATUS.fullmatch(inner[0])
    if status is None or not inner[-1].startswith("[Esc] Back  [c] Copy"):
        return None
    return tuple(int(group) for group in status.groups()), inner[1:-1], inner[-1]


def markdown_copy_ok(copied: bytes, source: str) -> bool:
    """K9 (MP9): the clipboard file holds the fence source byte for byte."""
    return copied == source.encode("utf-8")


def markdown_pane_claims():
    return {
        "user": markdown_claim_user, "think": markdown_claim_think, "headings": markdown_claim_headings,
        "lists": markdown_claim_lists, "tasks": markdown_claim_tasks, "quote-link": markdown_claim_quote_link,
        "table5": markdown_claim_table5, "table12": markdown_claim_table12, "marker": markdown_claim_marker,
        "focus": markdown_claim_focus,
    }


def markdown_step(tui, key: str, previous: str, rows: int, label: str, timeout: float = 15.0) -> str:
    """Send one key, wait (PTY-paced, bounded) for the screen to change, then for it to settle."""
    tui.send(key)
    deadline = time.monotonic() + timeout
    while tui.get_screen_text() == previous:
        assert time.monotonic() < deadline, f"{label}: the key {key!r} changed nothing:\n{previous}"
    return settle(tui, lambda s: chrome(s, rows) is not None, label, timeout=timeout, stable_for=0.4)


@leg("markdown")
def run_markdown(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    fence = markdown_scene_fence()
    source = "\n".join(fence)
    clip = run.ws / ".rustain_data" / "clipboard.txt"
    run.not_proven = (
        "NOT PROVEN by this leg: the behaviour of each markdown shape, which Rust owns: "
        "tests/conformance_21_9_stage_polish/ (folded reasoning row k1_fold.rs, k2_reasoning_facts.rs), "
        "tests/conformance_21_11_display_width/ (CJK and emoji cell arithmetic, k2_composer.rs, k3_live_text.rs), "
        "tests/conformance_21_12_markdown/ (k1_lists.rs headings, lists, quotes; k4_tables.rs shrink and collapse; "
        "k9_k13_links.rs host suffix; k8_literal_user.rs) and tests/conformance_21_13_block_viewer/ "
        "(k2_focus.rs, k3_inspect.rs, k4_navigation.rs `$`/`0`, k5_copy.rs, k8_cap.rs; tui-5a K1-K5), "
        "src/adapters/tui/clipboard.rs K1 (file strategy bytes), core 21_12 test_markdown_structure_and_stage_links_at_80x24. "
        "Also not proven: the skin-tone modifier of the user row is not retained by the terminal emulator "
        "(pyte), so the painted user row is compared without U+1F3FD at the same columns while the full "
        "text is carried by the request log; colour, style, OSC 8 links and the attached host are not "
        "asserted; the 12-column table collapses only at 60 columns (a 5-column table cannot collapse: "
        "it needs 3 * 5 + 1 + 5 * 3 = 31 cells), so the collapse is shown on the 12-column table and the "
        "5-column table shows its cut cells (`…`) at every viewport.")
    with run.tui(env={"RUSTAIN_CLIPBOARD": "file"}) as tui:
        screen = settle(tui, lambda s: markdown_ready(s, rows), "launch", timeout=60)
        run.beat("typing the user row with <T>, wide glyphs and a skin-tone emoji")
        tui.send(f"\x1b[200~{MARKDOWN_USER}\x1b[201~\r")
        screen = settle(
            tui, lambda s: markdown_ready(s, rows) and markdown_quiet(s)
            and re.search(r"… \d+ more lines", s) is not None,
            "reply painted and quiet", timeout=90)
        assert not clip.exists(), "the clipboard file exists before any copy"

        pages: list[tuple[str, str]] = []  # (kind, screen)
        tui.send("\x1b")  # Chat focus
        screen = markdown_step(tui, "g", screen, rows, "jump to the top")
        pages.append(("top", screen))
        screen = markdown_step(tui, "i", screen, rows, "back to the composer (no focus marker)")
        pages.append(("input", screen))
        screen = markdown_step(tui, "\x1b", screen, rows, "Chat focus again")
        for _ in range(60):
            if markdown_claim_marker(markdown_stage(screen, rows), cols):
                break
            screen = markdown_step(tui, "\x04", screen, rows, "half page down")
            pages.append(("down", screen))
        else:
            raise AssertionError(f"the end of the reply was never reached:\n{screen}")
        # the focus follows what the Stage paints: scroll back to the fence header
        for _ in range(60):
            if markdown_claim_focus(markdown_stage(screen, rows), cols):
                break
            screen = markdown_step(tui, "\x15", screen, rows, "half page up")
            pages.append(("up", screen))
        else:
            raise AssertionError(f"the fence header never took the Stage focus:\n{screen}")
        focus_screen = screen

        stages = [markdown_stage(page, rows) for _, page in pages]
        claims = markdown_pane_claims()
        first: dict[str, int] = {}
        for index, stage in enumerate(stages):
            for name, claim in claims.items():
                if name not in first and claim(stage, cols):
                    first[name] = index
        missing = [name for name in claims if name not in first]
        assert not missing, f"claims never held on any page: {missing}\n" + "\n\n".join(p for _, p in pages)
        assert markdown_fence_ok(stages), "the capped fence is not one straight box of its first lines:\n" + "\n\n".join(
            p for _, p in pages)
        by_page: dict[int, list[str]] = {}
        for name, index in first.items():
            by_page.setdefault(index, []).append(name)
        for index in sorted(by_page):
            run.pane(f"stage-{pages[index][0]}{index}-" + "+".join(by_page[index]), pages[index][1])
        run.beat(f"{len(claims)} Stage claims held across {len(pages)} pages")

        run.beat("Enter on the focused fence opens S-BLOCK")
        screen = markdown_step(tui, "\r", screen, rows, "open the block viewer")
        screen = settle(tui, lambda s: markdown_viewer(s, cols, rows) is not None, "S-BLOCK whole", timeout=30)
        (_, view_width, widest), _, _ = markdown_viewer(screen, cols, rows)
        assert widest > view_width >= 40 and widest >= markdown_width(max(fence, key=markdown_width)), (view_width, widest)
        run.pane("s-block-open", screen)

        run.beat("$ shows the last cells, 0 returns")
        screen = markdown_step(tui, "$", screen, rows, "dollar")
        screen = settle(tui, lambda s: (v := markdown_viewer(s, cols, rows)) is not None and v[0][1] == widest, "$ at the end")
        (left, right, _), body, _ = markdown_viewer(screen, cols, rows)
        assert right == widest and left == widest - view_width + 1 > 1, (left, right, widest, view_width)
        for _ in range(20):
            wide = [row for row in body if 'TAIL";' in row]
            if wide:
                break
            screen = markdown_step(tui, "j", screen, rows, "scroll to the long row")
            body = markdown_viewer(screen, cols, rows)[1]
        else:
            raise AssertionError(f"the long row never painted:\n{screen}")
        assert wide[0].startswith("←") and wide[0].endswith('TAIL";│'), wide[0]
        assert all(not row.endswith("→") for row in body), "nothing continues right at the last cells"
        run.pane("s-block-dollar", screen)
        screen = markdown_step(tui, "0", screen, rows, "zero")
        screen = settle(tui, lambda s: (v := markdown_viewer(s, cols, rows)) is not None and v[0][0] == 1, "0 at the start")
        body = markdown_viewer(screen, cols, rows)[1]
        wide = [row for row in body if row.startswith('│let w = "x')]
        assert wide and wide[0].endswith("→"), body
        run.pane("s-block-zero", screen)

        run.beat("the viewer keeps every row: scroll to the bottom")
        if not markdown_viewer(screen, cols, rows)[1][0].startswith("┌─ rust "):
            screen = markdown_step(tui, "g", screen, rows, "top")
        seen: set[int] = set()
        for _ in range(60):
            body = markdown_viewer(screen, cols, rows)[1]
            assert not any("more line" in row for row in body), "the viewer must not cap"
            seen.update(int(m.group(1)) for row in body for m in [re.match(r"│fn line_(\d\d)\(\) \{\}", row)] if m)
            ends = [i for i, row in enumerate(body) if row.startswith("│// J21-MARKDOWN-LAST")]
            if ends and ends[0] + 1 < len(body) and body[ends[0] + 1].startswith("└"):
                break
            screen = markdown_step(tui, "\x04", screen, rows, "half page down in S-BLOCK")
        else:
            raise AssertionError(f"S-BLOCK never reached the last row:\n{screen}")
        expected = {n for n in range(2, len(fence)) if fence[n - 1] == f"fn line_{n:02}() {{}}"}
        assert expected <= seen and max(seen) == 39 and any(n > MARKDOWN_CAP for n in seen), sorted(seen)
        run.pane("s-block-bottom", screen)

        run.beat("c copies the fence source byte for byte (RUSTAIN_CLIPBOARD=file)")
        tui.send("c")
        screen = tui.wait_until(lambda s: clip.exists(), "the clipboard file", timeout=30)
        copied = clip.read_bytes()
        assert markdown_copy_ok(copied, source), (copied, source.encode("utf-8"))
        assert copied != "\n".join(line.rstrip() for line in fence).encode("utf-8"), "control: the source has trailing spaces"
        run.pane("s-block-copied", screen)

        run.beat("Esc returns to the same Stage rows")
        screen = markdown_step(tui, "\x1b", screen, rows, "close S-BLOCK")
        screen = settle(
            tui, lambda s: markdown_claim_focus(markdown_stage(s, rows), cols) and "Copied to" not in s
            and "Block · " not in s, "Stage back", timeout=60)
        assert stage_unchanged(focus_screen, screen, rows), "the Stage rows changed across S-BLOCK"
        run.pane("stage-back", screen)

    requests = run.stub.rows()
    sent = [row for row in requests if row.get("turn") == 1]
    assert sent and MARKDOWN_USER in sent[0]["last_user"], sent


def test_j21_predicate_markdown():
    """K9: the markdown / S-BLOCK predicates accept the real shapes and reject MP8 (a reflowed fence), MP9
    (a copy that trims a trailing space), a table that does not collapse at 60, a torn S-BLOCK frame."""
    fence = markdown_scene_fence()
    total = len(fence)
    assert total > MARKDOWN_CAP
    source = "\n".join(fence)

    def stage_pages(cols: int, rows: int, full: list[str]) -> list[list[str]]:
        """The Stage rows of half-page windows over `full`, each through the real chrome predicates (the
        last Stage row is the toast row and is not part of the Stage rows)."""
        region = rows - 5
        step = max(1, (region - 1) // 2)
        pages = []
        for start in list(range(0, max(1, len(full) - region + 1), step)) + [max(0, len(full) - region)]:
            base = synth_frame(cols, rows, None, strip=True).split("\n")
            window = full[start:start + region]
            window += ["│"] * (region - len(window))
            lines = [row.ljust(cols) for row in window]
            pages.append(markdown_stage("\n".join(lines + base[region:]), rows))
        return pages

    def fence_rows(cols: int, cap: int = MARKDOWN_CAP, reflow: bool = False, remainder: int = 0) -> list[str]:
        inner = cols - 4
        rows = ["│ ┌─ rust " + "─" * (inner - 7) + "┐"]
        for line in fence[:cap]:
            if markdown_width(line) > 100:
                rows.append("│ │" + ('let w = "' + "x" * (inner - 10) + "→").ljust(inner) + "│")
                if reflow:
                    rows.append("│ │" + ('x' * 30 + 'TAIL";').ljust(inner) + "│")
            else:
                text = line.expandtabs(8)
                rows.append("│ │" + text + " " * (inner - markdown_width(text)) + "│")
        rows.append("│ │" + f"… {remainder or total - MARKDOWN_CAP} more lines".ljust(inner) + "│")
        rows.append("│ └" + "─" * inner + "┘")
        return rows + ["│", "│"]

    def box_rows(ties: int, cells: list[str], width: int) -> list[str]:
        """A table box the way the Stage paints it: `│ ` gutter, `ties + 1` columns of `width` cells."""
        pad = lambda text: " " + text + " " * (width - 2 - markdown_width(text)) + " "  # noqa: E731
        rule = lambda a, b, c: "│ " + a + b.join(["─" * width] * (ties + 1)) + c  # noqa: E731
        row = lambda texts: "│ │" + "│".join(pad(text) for text in texts) + "│"  # noqa: E731
        return [rule("┌", "┬", "┐"), row(cells), rule("├", "┼", "┤"), row(cells), rule("└", "┴", "┘"), "│"]

    for cols, rows in SYNTH_VIEWPORTS:
        # the fence: one capped box over overlapping half-page windows, like the real walk
        assert markdown_fence_ok(stage_pages(cols, rows, fence_rows(cols))), f"the real fence shape rejected at {cols}"
        # MP8: the fence reflowed on the Stage (the tail of the long line wrapped onto a row of its own)
        assert not markdown_fence_ok(stage_pages(cols, rows, fence_rows(cols, reflow=True))), f"MP8 accepted at {cols}"
        assert not markdown_fence_ok(stage_pages(cols, rows, fence_rows(cols, cap=MARKDOWN_CAP + 5))), f"uncapped fence accepted at {cols}"
        assert not markdown_fence_ok(stage_pages(cols, rows, fence_rows(cols, remainder=9))), f"wrong remainder accepted at {cols}"
        dropped = fence_rows(cols)
        del dropped[5]
        assert not markdown_fence_ok(stage_pages(cols, rows, dropped)), f"a dropped code row accepted at {cols}"
        assert markdown_claim_marker(fence_rows(cols), cols)
        assert not markdown_claim_marker(fence_rows(cols, remainder=9), cols)
        # tables: a 5-column box with cut cells; the 12-column table collapses below 70 columns only
        box5 = box_rows(4, ["c0…", "漢…", "Description", "cut…", "x"], 14)
        assert markdown_claim_table5(box5, cols)
        assert not markdown_claim_table5(box5[:4] + box5[5:], cols), "a torn table accepted"
        uneven = box5[:]
        uneven[3] = uneven[3][:-1] + " │"
        assert not markdown_claim_table5(uneven, cols), "a table with an uneven row accepted"
        summary = ["│ table · 12 cols × 1 row", "│"]
        box12 = box_rows(11, [f"c{n}" for n in range(12)], 5)
        if cols < MARKDOWN_TABLE12_COLLAPSES_BELOW:
            assert markdown_claim_table12(summary, cols)
            assert not markdown_claim_table12(box12, cols), "a box at 60 columns accepted"
        else:
            assert markdown_claim_table12(box12, cols), f"the natural 12-column box rejected at {cols}"
            assert not markdown_claim_table12(summary, cols), f"a collapsed table accepted at {cols}"
        # the user row (grid column math over CJK and the emoji) and the folded reasoning row
        assert markdown_claim_user(["You:", MARKDOWN_USER_PAINTED, ""], cols)
        assert not markdown_claim_user(["You:", MARKDOWN_USER_PAINTED.replace("<T>", "T"), ""], cols)
        assert not markdown_claim_user(["You:", MARKDOWN_USER_PAINTED.replace("漢字", "漢"), ""], cols)
        assert markdown_claim_think(["│ ◇ Thinking · <0.1s · ~19 tokens", "│", "│ J21-MARKDOWN-H1"], cols)
        assert not markdown_claim_think(["│ ◇ Thinking · <0.1s · ~19 tokens", "│ J21-MARKDOWN-THINK body"], cols)

        # S-BLOCK: full width below 100 columns, centred at 130, status and strip uncovered
        inner_rows = (rows - 5) - (2 if cols < 100 else 3)
        status = "code 3/3 · rust · 40 lines · Rendered · col 1-%d/157" % (cols - 2)
        rail = "[Esc] Back  [c] Copy  [v] Source"
        body = [status, "┌─ rust ─", "│// J21-MARKDOWN-FENCE"]
        body += [""] * (inner_rows - len(body) - 1) + [rail]
        frame = synth_frame(cols, rows, MARKDOWN_TITLE, body)
        assert markdown_viewer(frame, cols, rows) is not None, cols
        assert markdown_viewer(frame, cols, rows)[0][2] == 157
        assert markdown_viewer(synth_frame(cols, rows, "Tool · Read #1", body), cols, rows) is None
        assert markdown_viewer(synth_frame(cols, rows, MARKDOWN_TITLE, ["code 3/3 · rust · 39 lines"] + body[1:]), cols, rows) is None
        lines = frame.split("\n")
        window = frame_window(frame, MARKDOWN_TITLE)
        lines[window.bottom] = lines[window.bottom][:window.left] + " " * (window.right - window.left + 1) + lines[window.bottom][window.right + 1:]
        assert markdown_viewer("\n".join(lines), cols, rows) is None, "a frame without its closing border accepted"

    # MP9: the copy trims a trailing space (and the tab line / blank line must survive byte for byte)
    assert markdown_copy_ok(source.encode("utf-8"), source)
    assert not markdown_copy_ok("\n".join(line.rstrip() for line in fence).encode("utf-8"), source)
    assert not markdown_copy_ok(source.replace("\t", "        ").encode("utf-8"), source)
    assert not markdown_copy_ok((source + "\n").encode("utf-8"), source)
    assert not markdown_copy_ok(source.encode("utf-8")[:-1], source)
    assert "  \n" in source and "\t" in source, "control: the scene's fence carries a trailing space and a tab"


# ── attach leg: constants ────────────────────────────────────────────────────

ATTACH_ITEM = "ri_j21_attach"
ATTACH_TOKEN = "architecture_update"
ATTACH_PEER = "12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
ATTACH_THREAD_TITLE = f"Thread · {ATTACH_TOKEN}"
ATTACH_LOG_TITLE = "System · Notifications"
ATTACH_HELP_TITLE = "Help — Rustain Keybindings"
ATTACH_PERMISSION_TITLE = "Decision: Permission"
ATTACH_PERMISSION_RAIL = ("[y] Allow  [s] Session  [a] Always", "[n] Deny  [f] Deny + feedback  [r] Review")
ATTACH_READONLY_FOOTER = "read-only — view only; item actions require the attached writer"
ATTACH_LOCAL_FOOTER = "local view only — replying, acknowledgement and item context require a daemon-attached writer"
ATTACH_MUTATION_KEYS = ("[a] Acknowledge", "[d] Discuss", "[u] Use as context")
ATTACH_WRITER_KEYS = ("[d] Discuss", "[a] Acknowledge", "[u] Use as context")
ATTACH_PAGE_KEYS = ("[p] Earlier", "[l] Newer")
ATTACH_RAIL_SPAN = 6  # inner rows from the bottom that may hold the footer and the rail
ATTACH_SOCKET_MAX = 100  # bytes: the sun_path limit is 108, the leg keeps a margin
ATTACH_DETACH_ITEM = "[D] Detach"


# ── attach leg: predicates (shared by the live leg and the offline control) ──


def attach_status_mode(screen: str) -> str | None:
    """``readonly`` when the status row says ``read-only``, ``writer`` when it says ``attached``."""
    row = harness.status_row(screen)
    if "read-only" in row:
        return "readonly"
    if "attached" in row:
        return "writer"
    return None


def attach_composer(screen: str) -> str | None:
    """The composer's first inner row text (None while there is no composer)."""
    lines = screen.split("\n")
    top = harness.composer_top(lines)
    if top is None or top + 1 >= len(lines):
        return None
    return lines[top + 1].strip().strip("│").strip()


def attach_inner(screen: str, title: str) -> list[str] | None:
    """The inner rows (borders removed, right-stripped) of the whole frame ``title``; None when torn."""
    window = attach_window(screen, title)
    if window is None:
        return None
    cells = grid(screen)
    return ["".join(cells[index][window.left + 1:window.right]).rstrip()
            for index in range(window.top + 1, window.bottom)]


def attach_menu_rows(screen: str) -> list[str] | None:
    """The inner rows of the painted ``Ctrl+X`` leader menu (a box whose title row names ``Ctrl+X``),
    None while it is torn or absent."""
    lines = screen.split("\n")
    for top, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("┌") and "Ctrl+X" in stripped and stripped.endswith("┐"):
            left = line.index("┌")
            right = left + len(stripped) - 1
            rows = []
            for index in range(top + 1, len(lines)):
                row = lines[index]
                if len(row) > right and row[left] == "└" and row[right] == "┘":
                    return rows
                if len(row) <= right or row[left] != "│" or row[right] != "│":
                    return None
                rows.append(row[left + 1:right].strip())
            return None
    return None


def attach_menu_ok(screen: str, rows: int, *, detach: bool) -> bool:
    """The leader menu is painted whole above an uncovered status row; ``detach`` says whether the
    attached ``[D] Detach`` item is in it (the attached host) or must not be (the local host)."""
    menu = attach_menu_rows(screen)
    if menu is None or chrome(screen, rows) is None or (detach and attach_status_mode(screen) is None):
        return False
    text = " ".join(" ".join(menu).split())
    return (ATTACH_DETACH_ITEM in text) == detach


def attach_window(screen: str, title: str) -> Window | None:
    """Like ``frame_window`` but the closing border may carry a footer label (the Help frame paints
    ``└ j/k scroll  ? close  [/] Search ───┘``): a continuous ``│`` on both edges of every body row and a
    closing ``└…┘`` at the same columns whose inside holds no box character and ends in ``─``."""
    cells = grid(screen)
    pattern = re.compile(r"┌ ?" + re.escape(title) + r"[^┌┐│└┘]*?─*┐")
    for top, row in enumerate(cells):
        match = pattern.search("".join(row))
        if match is None:
            continue
        left = sum(2 if wcwidth(c) == 2 else 1 for c in "".join(row)[:match.start()])
        right = left + sum(2 if wcwidth(c) == 2 else 1 for c in match.group(0)) - 1
        for index in range(top + 1, len(cells)):
            line = cells[index]
            edge_l = line[left] if left < len(line) else " "
            edge_r = line[right] if right < len(line) else " "
            if edge_l == "└" and edge_r == "┘":
                inner = "".join(line[left + 1:right])
                ok = bool(inner) and inner.endswith("─") and not set(inner) & set("┌┐│└┘")
                return Window(top, index, left, right) if ok else None
            if edge_l != "│" or edge_r != "│":
                return None
        return None
    return None


def attach_titled_ok(screen: str, title: str, cols: int, rows: int) -> bool:
    """A system frame is whole, geometrically legal (full sheet below 100 columns, centred with equal
    margins from 100) and ends above the strip row (or the status row when the strip yielded)."""
    window = attach_window(screen, title)
    bar = chrome(screen, rows)
    if window is None or bar is None:
        return False
    if window.bottom >= (bar.strip if bar.strip is not None else bar.status):
        return False
    if cols < 100:
        return window.left == 0 and window.right == cols - 1
    return window.left > 0 and abs(window.left - (cols - 1 - window.right)) <= 1


def attach_same_window(left: str, right: str, title: str) -> bool:
    """K10: the same title painted on both hosts occupies the same window (rows and columns)."""
    window = attach_window(left, title)
    return window is not None and window == attach_window(right, title)


def attach_rail_ok(screen: str, cols: int, rows: int) -> bool:
    """The writer's ``Decision: Permission`` frame: whole and legal, the crumb row first, the two
    rail rows in order behind it, the request body rows between."""
    if not attach_titled_ok(screen, ATTACH_PERMISSION_TITLE, cols, rows):
        return False
    inner = [row.strip() for row in (attach_inner(screen, ATTACH_PERMISSION_TITLE) or []) if row.strip()]
    if not inner or not re.fullmatch(r"Needs you › Permission Write \| 1 pending", inner[0]):
        return False
    first = next((i for i, row in enumerate(inner) if ATTACH_PERMISSION_RAIL[0] in row), None)
    second = next((i for i, row in enumerate(inner) if ATTACH_PERMISSION_RAIL[1] in row), None)
    return (first is not None and second is not None and 0 < first < second
            and any(re.match(r"│? ?file_path: ", row) for row in inner[1:first]))


def attach_mirror_quiet(screen: str) -> bool:
    """MP10: the read-only mirror never paints a decision: no ``Decision:`` frame, no rail, no
    decision key, no ``Needs you`` list or cue; a waiting cue, when painted, is the mirror's own."""
    if attach_status_mode(screen) != "readonly":
        return False
    if "Decision:" in screen or "Needs you" in screen:
        return False
    if any(item in screen for item in ATTACH_PERMISSION_RAIL):
        return False
    if any(key in screen for key in ("[y] Allow", "[n] Deny", "[r] Review")):
        return False
    strip = harness.strip_row(screen)
    return not re.match(r"(?:Decisions|D) \d+", strip)


def attach_mirror_chip(screen: str) -> str | None:
    """The mirror's waiting chip when one is painted: the status row's ``<n> approval(s) waiting for
    the writer`` or the strip's ``Writer decisions <n>``/``W <n>``; None when none is painted."""
    status = re.search(r"\d+ approval\(s\) waiting for the writer", harness.status_row(screen))
    if status:
        return status.group(0)
    strip = re.match(r"(?:Writer decisions|W) \d+", harness.strip_row(screen))
    return strip.group(0) if strip else None


def attach_thread_ok(screen: str, mode: str, cols: int, rows: int) -> bool:
    """K10 for S-THREAD: the whole ``Thread · <token>`` frame; ``writer`` carries the writer rail
    ``[d] [a] [u]``, ``readonly`` the read-only footer and no mutation key."""
    if not attach_titled_ok(screen, ATTACH_THREAD_TITLE, cols, rows):
        return False
    inner = attach_inner(screen, ATTACH_THREAD_TITLE) or []
    if len(inner) < 4 or not re.fullmatch(rf"\s*item actions · {re.escape(ATTACH_ITEM)}(?: · peer-tainted)?", inner[0]):
        return False
    if not re.fullmatch(rf"\s*{re.escape(ATTACH_TOKEN)}(?: · peer-tainted)?", inner[1]):
        return False
    # the frame hard-wraps at its width, even inside a token (`[E` / `sc] Close` at 60 columns):
    # compare the rows joined and stripped of every blank
    rail = "".join("".join(inner[-ATTACH_RAIL_SPAN:]).split())
    whole = "".join("".join(inner).split())

    def squash(text: str) -> str:
        return "".join(text.split())

    if squash("[PgUp/PgDn] Scroll") not in rail or squash("[Esc] Close") not in rail:
        return False
    if any(squash(key) in whole for key in ATTACH_PAGE_KEYS):
        return False
    if mode == "writer":
        return (all(squash(key) in rail for key in ATTACH_WRITER_KEYS)
                and squash(ATTACH_LOCAL_FOOTER) not in rail
                and squash(ATTACH_READONLY_FOOTER) not in rail)
    if mode == "readonly":
        return (squash(ATTACH_READONLY_FOOTER) in rail
                and not any(squash(key) in whole for key in ATTACH_MUTATION_KEYS))
    raise ValueError(mode)


# ── attach leg: daemon, seeding ──────────────────────────────────────────────


def attach_env(run: J21Run, log_name: str) -> dict[str, str]:
    """The env of one process of this leg: the scene stub, the shared config and data dirs."""
    return run.env({
        "RUSTAIN_CONFIG_DIR": str(run.ws / ".rustain"),
        "RUSTAIN_DATA_DIR": str(run.ws / ".rustain_data"),
        "RUSTAIN_LOG_PATH": str(run.ws / log_name),
        "RUST_LOG": "debug",
    })


def attach_sockets(run: J21Run) -> list[Path]:
    return sorted((run.ws / ".rustain_data" / "daemons").glob("*.sock"))


def attach_socket_ready(path: Path, deadline: float) -> bool:
    """A real daemon socket that accepts and sends its challenge byte (bounded by ``deadline``)."""
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return False
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
        probe.settimeout(min(0.2, remaining))
        try:
            probe.connect(str(path))
            return bool(probe.recv(1))
        except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
            return False


@contextlib.contextmanager
def attach_daemon(run: J21Run):
    """``daemon start --foreground`` in its own process group; yields once its socket answers. The
    socket path is asserted short; the group is terminated (then killed) on exit."""
    env = harness._load_env()
    env.update(attach_env(run, "daemon.log"))
    data_dir = run.ws / ".rustain_data"
    if data_dir not in run.data_dirs:
        run.data_dirs.append(data_dir)
    log_path = run.tmp / "daemon.stdout.txt"
    with log_path.open("w") as log:
        proc = subprocess.Popen([str(run.binary), "daemon", "start", "--foreground"], cwd=run.ws, env=env,
                                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = time.monotonic() + 30
            ready = None
            while ready is None:
                assert proc.poll() is None, f"the daemon exited early:\n{log_path.read_text()}"
                assert time.monotonic() < deadline, f"no daemon socket answered:\n{log_path.read_text()}"
                for path in attach_sockets(run):
                    assert len(os.fsencode(path)) <= ATTACH_SOCKET_MAX, f"socket path too long: {len(os.fsencode(path))}"
                    if attach_socket_ready(path, deadline):
                        ready = path
                        break
            yield proc, ready
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait(timeout=10)


def attach_seed_room_item(run: J21Run) -> None:
    """Append one received recipient item to the room journal the first local session created (the
    19.16g pattern: under the writers' exclusive ``flock``, as the production writer serialises it)."""
    journals = sorted((run.ws / ".rustain" / "rooms").glob("*.jsonl"))
    assert len(journals) == 1, f"exactly one room journal expected: {journals}"
    journal = journals[0]
    payload = {
        "event": "recipient_item_received",
        "address": {"principal": {"kind": "a2a_pseudonym", "peer": ATTACH_PEER}, "item": ATTACH_ITEM},
        "task": "j21-attach-task",
        "alias": "j21-peer",
        "message_type": ATTACH_TOKEN,
        "content": "\n".join(["# J21-ATTACH-THREAD heading", "", "J21-ATTACH-BODY first row", "",
                               *[f"J21-ATTACH-LINE {n}\n" for n in range(1, 41)]]),
    }
    lock = os.open(journal.with_suffix(".lock"), os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        lines = [line for line in journal.read_text().splitlines() if line.strip()]
        seq = json.loads(lines[-1])["seq"] + 1 if lines else 1
        entry = {"schema_version": 1, "seq": seq, "recorded_at_ms": int(time.time() * 1000),
                 "record": {"kind": "room", "payload": payload}}
        with open(journal, "a", encoding="utf-8") as out:
            out.write(json.dumps(entry, separators=(",", ":"), ensure_ascii=False) + "\n")
            out.flush()
            os.fsync(out.fileno())
    finally:
        os.close(lock)


def attach_wait_exit(tui: RustainTUI, label: str, timeout: float = 20.0) -> None:
    """Wait (bounded, paced by the PTY read) until the client's process has exited."""
    deadline = time.monotonic() + timeout
    while tui.child.isalive():
        assert time.monotonic() < deadline, f"{label}: the client is still running"
        try:
            tui.get_screen_text()
        except RuntimeError:
            break


# ── attach leg: the live leg ─────────────────────────────────────────────────


def attach_titled(tui: RustainTUI, run: J21Run, keys: str, title: str, label: str) -> str:
    """Open the frame ``title`` with ``keys``; return its settled screen (the caller closes it)."""
    tui.send(keys)
    return settle(tui, lambda s: attach_titled_ok(s, title, run.cols, run.rows), label)


def attach_close(tui: RustainTUI, run: J21Run, title: str, label: str) -> str:
    tui.send(ESC)
    return settle(tui, lambda s: title not in s and chrome(s, run.rows) is not None
                  and attach_composer(s) == "", label)


def attach_open_thread(tui: RustainTUI, run: J21Run, mode: str, label: str) -> str:
    command = f"/team view {ATTACH_ITEM}"
    tui.send(command)
    tui.wait_until(lambda s: attach_composer(s) == command, f"{label}: command typed", 15.0)
    tui.send(ENTER)
    return settle(tui, lambda s: attach_thread_ok(s, mode, run.cols, run.rows), label)


@leg("attach")
def run_attach(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    # 1. The local host: the reference titles, and the session that creates the room journal.
    run.beat("local host: the leader menu (no Detach), Ctrl+X n and Ctrl+X ? titles")
    with run.tui() as local:
        local.wait_until(lambda s: attach_composer(s) == "" and chrome(s, rows) is not None, "local composer", 30.0)
        local.send(CTRL_X)
        screen = settle(local, lambda s: attach_menu_ok(s, rows, detach=False), "local leader menu")
        run.pane("local-menu", screen)
        local.send(ESC)
        settle(local, lambda s: attach_menu_rows(s) is None and chrome(s, rows) is not None, "local menu closed")
        local_log = attach_titled(local, run, CTRL_X + "n", ATTACH_LOG_TITLE, "local notifications")
        run.pane("local-log", local_log)
        attach_close(local, run, ATTACH_LOG_TITLE, "local notifications closed")
        local_help = attach_titled(local, run, CTRL_X + "?", ATTACH_HELP_TITLE, "local help")
        run.pane("local-help", local_help)
    attach_seed_room_item(run)

    # 2. The daemon, the writer, the read-only mirror.
    with attach_daemon(run) as (daemon, socket_path):
        run.beat(f"daemon start --foreground answering on a {len(os.fsencode(socket_path))}-byte socket path")
        writer = run.tui(env=attach_env(run, "rustain.log"))
        writer.fresh = False
        writer.extra_args = ["daemon", "attach"]
        mirror = run.tui(env=attach_env(run, "mirror.log"))
        mirror.fresh = False
        mirror.extra_args = ["daemon", "attach"]
        with writer:
            screen = settle(writer, lambda s: attach_status_mode(s) == "writer" and chrome(s, rows) is not None
                            and attach_composer(s) == "", "writer attached")
            run.pane("writer-attached", screen)
            with mirror:
                screen = settle(mirror, lambda s: attach_status_mode(s) == "readonly"
                                and chrome(s, rows) is not None, "mirror read-only")
                run.pane("mirror-attached", screen)

                # 3. The writer's Ctrl+X menu and the two system frames, titled as local.
                writer.send(CTRL_X)
                screen = settle(writer, lambda s: attach_menu_ok(s, rows, detach=True), "writer leader menu")
                run.pane("writer-menu", screen)
                writer.send(ESC)
                settle(writer, lambda s: attach_menu_rows(s) is None and chrome(s, rows) is not None
                       and attach_composer(s) == "", "writer menu closed")
                writer_log = attach_titled(writer, run, CTRL_X + "n", ATTACH_LOG_TITLE, "writer notifications")
                assert attach_same_window(local_log, writer_log, ATTACH_LOG_TITLE), (local_log, writer_log)
                run.pane("writer-log", writer_log)
                attach_close(writer, run, ATTACH_LOG_TITLE, "writer notifications closed")
                writer_help = attach_titled(writer, run, CTRL_X + "?", ATTACH_HELP_TITLE, "writer help")
                assert attach_same_window(local_help, writer_help, ATTACH_HELP_TITLE), (local_help, writer_help)
                run.pane("writer-help", writer_help)
                attach_close(writer, run, ATTACH_HELP_TITLE, "writer help closed")
                run.beat("writer: menu shows Detach; Ctrl+X n and Ctrl+X ? paint the local titles in the local windows")

                # 4. A Write request: the writer's decision frame, the mirror never the rail.
                request = "J21-ATTACH-WRITE please create the file"
                writer.send(request)
                writer.wait_until(lambda s: attach_composer(s) == request, "writer request typed", 15.0)
                writer.send(ENTER)
                wait_frame(writer, ATTACH_PERMISSION_TITLE, ATTACH_PERMISSION_RAIL, timeout=60)
                frame = settle(writer, lambda s: attach_rail_ok(s, cols, rows), "writer permission frame")
                run.pane("writer-frame", frame)
                before = attach_inner(frame, ATTACH_PERMISSION_TITLE)
                pending = settle(mirror, lambda s: "Streaming" in harness.status_row(s) and attach_mirror_quiet(s),
                                 "mirror shows the turn in flight and no decision")
                run.pane("mirror-pending", pending)
                chip = attach_mirror_chip(pending)
                run.beat(f"mirror while the writer's Write request is pending: read-only status, no frame, no rail; chip: {chip!r}")
                mirror.send("y")
                typed = settle(mirror, lambda s: attach_composer(s) == "y" and attach_mirror_quiet(s),
                               "mirror key lands in its composer and decides nothing")
                run.pane("mirror-after-y", typed)
                mirror.send(BACKSPACE)
                settle(mirror, lambda s: attach_composer(s) == "" and attach_mirror_quiet(s), "mirror composer cleared")
                again = settle(writer, lambda s: attach_rail_ok(s, cols, rows), "writer frame unchanged by the mirror key")
                assert attach_inner(again, ATTACH_PERMISSION_TITLE) == before, "the mirror key changed the writer's frame"

                # 5. The writer denies; the turn continues; the file never appears.
                writer.send("n")
                screen = settle(writer, lambda s: "Decision:" not in s and attach_status_mode(s) == "writer"
                                and "Ready" in harness.status_row(s) and attach_composer(s) == ""
                                and chrome(s, rows) is not None, "writer denied; turn finished", timeout=60)
                run.pane("writer-denied", screen)
                turns = [row for row in run.stub.rows() if "turn" in row]
                assert [row["turn"] for row in turns] == [1, 2], turns
                assert turns[1].get("tool_results") == 1, turns
                assert not (run.ws / "j21-attach-out.txt").exists(), "the denied Write ran"
                settle(mirror, lambda s: attach_mirror_quiet(s) and attach_composer(s) == "", "mirror still quiet")
                run.beat("writer denied the Write once: two provider turns, no file, the mirror never painted a decision")

                # 6. /team view <id>: the S-THREAD writer rail against the read-only footer.
                opened = attach_open_thread(writer, run, "writer", "writer thread frame")
                run.pane("writer-thread", opened)
                attach_close(writer, run, ATTACH_THREAD_TITLE, "writer thread closed")
                observed = attach_open_thread(mirror, run, "readonly", "mirror thread frame")
                run.pane("mirror-thread", observed)
                attach_close(mirror, run, ATTACH_THREAD_TITLE, "mirror thread closed")
                run.beat("/team view: writer rail [d] [a] [u]; read-only footer and no mutation key on the mirror")

                # 7. Ctrl+X d detaches the writer; the daemon and the mirror stay.
                writer.send(CTRL_X + "d")
                attach_wait_exit(writer, "writer detach")
                assert writer.child.exitstatus == 0, (writer.child.exitstatus, writer.child.signalstatus)
                assert daemon.poll() is None, "the daemon stopped with the writer"
                screen = settle(mirror, lambda s: attach_status_mode(s) == "readonly" and attach_composer(s) == ""
                                and chrome(s, rows) is not None, "mirror after the writer detached")
                run.pane("mirror-after-detach", screen)
                assert attach_socket_ready(socket_path, time.monotonic() + 10), "the daemon socket stopped answering"
                mirror.send(CTRL_X + "d")
                attach_wait_exit(mirror, "mirror detach")
                assert mirror.child.exitstatus == 0, (mirror.child.exitstatus, mirror.child.signalstatus)
                run.beat("Ctrl+X d: the writer client exited 0, the daemon kept running; the mirror detached the same way")
    run.not_proven = "\n".join([
        "- Behaviour is owned by Rust, not proven here: the attached key front door and leader rows "
        "(src/infrastructure/runtime/attach_keys.rs: story21_3_attached_leader_runs_every_registry_row, "
        "story21_2_attached_menu_and_quit_frames_paint_complete_at_60_80_130, story21_6_attached_mirror_never_paints_a_decision), "
        "the attached loop (src/infrastructure/runtime/attach_loop.rs: story21_4_attached_scroll_is_the_shared_view_state, "
        "story21_7_attached_host_paints_the_strip_above_the_status_row), the daemon approval gate "
        "(src/adapters/daemon/server.rs, approval_lifecycle_tests.rs) and tests/conformance_21_8_thread_inspector/.",
        "- The read-only mirror's waiting chip (`<n> approval(s) waiting for the writer`, `Writer decisions <n>`) is seeded "
        "from the attach snapshot, which excludes an approval already issued to the current writer "
        "(retained_approval_count): this leg asserts only that the mirror never paints the decision frame, the rail or "
        "`Needs you`, and records whichever chip it painted; the chip text itself is owned by "
        "attach_keys.rs story21_6_attached_mirror_never_paints_a_decision and attach_loop.rs seed_mirror_waiting.",
        "- One provider turn per leg; the scene's prose is not evidence (the proof is what the binary painted, wrote and asked).",
        "- Not proven: a second writer, writer takeover, the a2a/p2p daemon paths, daemon restart, the plain line client, "
        "colour or style, a real terminal emulator, latency, a real provider, the default build's feature set.",
    ])


# ── attach family: offline control (K10) ─────────────────────────────────────

ATTACH_SYNTH_STATUS = {"writer": " (daemon) [F] │ attached │ Normal │ Ready │ ^X d detach",
                       "readonly": " (daemon) [F] │ read-only │ Normal │ Ready │ ^X menu"}


def attach_synth(cols: int, rows: int, title: str | None, body: list[str] | None, *, mode: str = "writer",
                 strip: str = "^X l Messages") -> str:
    """A whole synthetic attached screen: ``synth_frame`` with the attached status row and strip."""
    lines = synth_frame(cols, rows, title, body, strip=True).split("\n")
    lines[rows - 5] = strip.ljust(cols)
    lines[rows - 4] = ATTACH_SYNTH_STATUS[mode].ljust(cols)
    return "\n".join(lines)


def attach_synth_rows(cols: int, rows: int, head: list[str], tail: list[str]) -> list[str]:
    """Body rows of a synthetic frame: ``head`` on top, ``tail`` on the last inner rows."""
    inner = (rows - 5) - 2 - (1 if cols >= 100 else 0)  # a centred frame starts one row down
    return head + [""] * max(0, inner - len(head) - len(tail)) + tail


def attach_synth_menu(screen: str, rows: int, items: str) -> str:
    """Paint a Ctrl+X leader menu box over the Stage rows just above the strip."""
    lines = screen.split("\n")
    cols = len(lines[0])
    top = rows - 8
    lines[top] = ("┌ Ctrl+X " + "─" * (cols - 10) + "┐")
    lines[top + 1] = "│" + (" " + items).ljust(cols - 2)[:cols - 2] + "│"
    lines[top + 2] = "└" + "─" * (cols - 2) + "┘"
    return "\n".join(lines)


def attach_label_bottom(screen: str, label: str = "j/k scroll  ? close  [/] Search") -> str:
    """Rewrite the first closing border (a frame's) to carry a footer label, as the Help frame does."""
    lines = screen.split("\n")
    index = next(i for i, line in enumerate(lines) if "└" in line and "┘" in line)
    line = lines[index]
    left = line.index("└")
    right = line.index("┘")
    lines[index] = line[:left] + "└ " + label + " " + "─" * (right - left - len(label) - 3) + "┘" + line[right + 1:]
    return "\n".join(lines)


def attach_wrap(text: str, width: int) -> list[str]:
    """Greedy word wrap, one leading blank per row (the frame's own inner padding)."""
    out: list[str] = []
    for word in text.split():
        if out and len(out[-1]) + 1 + len(word) <= width:
            out[-1] += " " + word
        else:
            out.append(word)
    return [" " + row for row in out]


def attach_synth_thread(cols: int, rows: int, mode: str, *, keys: tuple = (), footer: str = "",
                        hard: bool = False) -> str:
    """A synthetic ``Thread · <token>`` frame: the pinned header, a body row, the wrapped footer and rail;
    ``hard`` wraps the rail row at the frame width even inside a token, as the product does."""
    width = cols - 4 if cols < 100 else int(cols * 0.9) - 4
    rail = "  ".join((*keys, "[PgUp/PgDn] Scroll", "[Esc] Close"))
    rail_rows = ["  " + rail[index:index + width - 1] for index in range(0, len(rail), width - 1)] if hard \
        else attach_wrap(rail, width)
    tail = attach_wrap(footer, width) + rail_rows
    head = [f"item actions · {ATTACH_ITEM}", ATTACH_TOKEN, "J21-ATTACH-BODY first row"]
    return attach_synth(cols, rows, ATTACH_THREAD_TITLE, attach_synth_rows(cols, rows, head, tail), mode=mode)


def test_j21_predicate_attach():
    """K10: attached parity: the same titles in the same windows on both hosts, the writer's rail
    against the mirror's none, the Detach menu item, the read-only footer. MP10 (the mirror paints
    the decision rail) and the torn or crossed variants are rejected at 60x16, 80x24 and 130x30."""
    crumb = "Needs you › Permission Write | 1 pending"
    for cols, rows in SYNTH_VIEWPORTS:
        # the writer's Decision: Permission frame: accepted whole; MP10 and torn variants rejected
        body = attach_synth_rows(cols, rows, [crumb, "file_path: j21-attach-out.txt"], list(ATTACH_PERMISSION_RAIL))
        writer = attach_synth(cols, rows, ATTACH_PERMISSION_TITLE, body)
        assert attach_rail_ok(writer, cols, rows), (cols, rows)
        assert attach_status_mode(writer) == "writer"
        swapped = attach_synth(cols, rows, ATTACH_PERMISSION_TITLE,
                               attach_synth_rows(cols, rows, [crumb, "file_path: j21-attach-out.txt"],
                                                 list(reversed(ATTACH_PERMISSION_RAIL))))
        assert not attach_rail_ok(swapped, cols, rows)
        half = attach_synth(cols, rows, ATTACH_PERMISSION_TITLE,
                            attach_synth_rows(cols, rows, [crumb, "file_path: j21-attach-out.txt"],
                                              [ATTACH_PERMISSION_RAIL[0]]))
        assert not attach_rail_ok(half, cols, rows)
        assert not attach_rail_ok(attach_synth(cols, rows, ATTACH_PERMISSION_TITLE, body).replace("┘", " ", 1), cols, rows)
        local_status = attach_synth(cols, rows, ATTACH_PERMISSION_TITLE, body).replace("│ attached │", "│ Normal   │")
        assert attach_status_mode(local_status) is None

        # the read-only mirror: quiet while a request waits; MP10 paints the frame and rail on it
        quiet = attach_synth(cols, rows, None, None, mode="readonly")
        assert attach_mirror_quiet(quiet) and attach_mirror_chip(quiet) is None
        chip_strip = attach_synth(cols, rows, None, None, mode="readonly", strip="Writer decisions 1")
        assert attach_mirror_quiet(chip_strip) and attach_mirror_chip(chip_strip) == "Writer decisions 1"
        chip_status = attach_synth(cols, rows, None, None, mode="readonly").replace(
            "Ready │", "1 approval(s) waiting for the writer │")
        assert attach_mirror_quiet(chip_status) and attach_mirror_chip(chip_status) == "1 approval(s) waiting for the writer"
        mp10 = attach_synth(cols, rows, ATTACH_PERMISSION_TITLE, body, mode="readonly")  # the mirror paints the rail
        assert not attach_mirror_quiet(mp10)
        rail_only = attach_synth(cols, rows, "Chat", ["row", ATTACH_PERMISSION_RAIL[0]], mode="readonly")
        assert not attach_mirror_quiet(rail_only)
        assert not attach_mirror_quiet(attach_synth(cols, rows, None, None, mode="readonly").replace(
            "Ready │", "Needs you (1) ^X q │"))
        assert not attach_mirror_quiet(attach_synth(cols, rows, None, None, mode="readonly", strip="Decisions 1"))
        assert not attach_mirror_quiet(attach_synth(cols, rows, None, None, mode="writer"))

        # the same titles on both hosts in the same window; another title or another window is rejected
        for title in (ATTACH_LOG_TITLE, ATTACH_HELP_TITLE):
            local = synth_frame(cols, rows, title, ["row"])
            attached = attach_synth(cols, rows, title, ["row"])
            if title == ATTACH_HELP_TITLE:  # the Help frame's closing border names its keys
                local, attached = attach_label_bottom(local), attach_label_bottom(attached)
                assert attach_window(local, title) is not None and frame_window(local, title) is None
            assert attach_titled_ok(local, title, cols, rows) and attach_titled_ok(attached, title, cols, rows)
            assert attach_same_window(local, attached, title)
            assert not attach_same_window(local, attach_synth(cols, rows, "System · Quit", ["row"]), title)
            shifted = synth_frame(cols, rows, title, ["row"], strip=False)
            assert not attach_same_window(local, shifted, title)
            assert not attach_titled_ok(attached.replace("└", " ", 1), title, cols, rows)

        # the leader menu: the attached host carries Detach, the local host does not
        base = attach_synth(cols, rows, None, None)
        items = "[D] Detach  [N] Notifications  [?] Help"
        assert attach_menu_ok(attach_synth_menu(base, rows, items), rows, detach=True)
        assert not attach_menu_ok(attach_synth_menu(base, rows, "[N] Notifications  [?] Help"), rows, detach=True)
        assert attach_menu_ok(attach_synth_menu(base, rows, "[N] Notifications  [?] Help"), rows, detach=False)
        assert not attach_menu_ok(attach_synth_menu(base, rows, items), rows, detach=False)
        torn = attach_synth_menu(base, rows, items).replace("┘", " ", 1)
        assert not attach_menu_ok(torn, rows, detach=True) and attach_menu_rows(torn) is None

        # S-THREAD: the writer rail against the read-only footer (the footer wraps at the frame width)
        writer_frame = attach_synth_thread(cols, rows, "writer", keys=ATTACH_WRITER_KEYS)
        assert attach_thread_ok(writer_frame, "writer", cols, rows), (cols, rows)
        torn_token = attach_synth_thread(cols, rows, "writer", keys=ATTACH_WRITER_KEYS, hard=True)
        assert attach_thread_ok(torn_token, "writer", cols, rows), (cols, rows)  # `[E` / `sc] Close`
        hard_readonly = attach_synth_thread(cols, rows, "readonly", footer=ATTACH_READONLY_FOOTER, hard=True)
        assert attach_thread_ok(hard_readonly, "readonly", cols, rows), (cols, rows)
        assert not attach_thread_ok(writer_frame, "readonly", cols, rows)
        assert not attach_thread_ok(attach_synth_thread(cols, rows, "writer"), "writer", cols, rows)  # rail keys gone
        local_footer = attach_synth_thread(cols, rows, "writer", keys=ATTACH_WRITER_KEYS, footer=ATTACH_LOCAL_FOOTER)
        assert not attach_thread_ok(local_footer, "writer", cols, rows)  # the LocalOnly footer is not the writer's
        assert not attach_thread_ok(writer_frame.replace("└", " ", 1), "writer", cols, rows)
        readonly_frame = attach_synth_thread(cols, rows, "readonly", footer=ATTACH_READONLY_FOOTER)
        assert attach_thread_ok(readonly_frame, "readonly", cols, rows), (cols, rows)
        assert not attach_thread_ok(readonly_frame, "writer", cols, rows)
        leaked = attach_synth_thread(cols, rows, "readonly", keys=("[d] Discuss",), footer=ATTACH_READONLY_FOOTER)
        assert not attach_thread_ok(leaked, "readonly", cols, rows)  # a mutation key on the mirror
        assert not attach_thread_ok(attach_synth_thread(cols, rows, "readonly"), "readonly", cols, rows)  # no footer
        assert not attach_thread_ok(readonly_frame.replace("[Esc] Close", "[Esc] Clos"), "readonly", cols, rows)


# ── Leg `onboard` (K11): the Story 21.14 tour, read natively at every viewport ──
# Copy comes from the landed code (`src/adapters/tui/tour/copy.rs`: card_title, rail, coach_instruction,
# complete_notice, closed_notice) and the core leg A (`tests_tui/test_story_21_14_onboarding.py`).

ONBOARD_OFFER_RAIL = "[Enter] Continue  [Esc] Skip tour"
ONBOARD_STEP_RAIL = "[Enter] Continue  [n] Skip step  [Esc] Skip tour"
ONBOARD_COMPLETE = "Tour complete. Help is always one key away: Ctrl+X, ?."
ONBOARD_CLOSED = "Tour closed. Run /tour to take it again."
ONBOARD_COACH = {4: "Open Mission Control with Ctrl+X, S", 5: "Open help with Ctrl+X, ?"}
ONBOARD_IDLE_HINT = "^X s Mission Control"
ONBOARD_OFFER_ARGS = ["-c", "onboarding.offer_tour=true"]
ONBOARD_TEAM_ARGS = ["-c", "onboarding.offer_tour=true", "-c", "onboarding.team=true"]
ONBOARD_TITLE = re.compile(r"┌ ?Tour(?: · [1-5] of 5)? ?─*┐")
ONBOARD_HELP_TITLE = re.compile(r"┌ ?Help — Rustain Keybindings ?─*┐")
ONBOARD_MESSAGE = "J21-ONBOARD-MSG-1"
ONBOARD_NO_OFFER_POLLS = 30


def onboard_title_rows(screen: str) -> list[int]:
    """Rows painting a ``┌Tour…┐`` title: the offer card, a step card, or a stacked second one."""
    return [index for index, row in enumerate(screen.split("\n")) if ONBOARD_TITLE.search(row)]


def onboard_chrome_ok(screen: str, rows: int, mode: str = "Normal", strip: bool = True) -> bool:
    """The whole bottom chrome: a closed composer, the status row (permission mode named) and, directly
    above it, the idle strip hint (the reserved strip row stays painted under a card)."""
    if chrome(screen, rows) is None or mode not in harness.status_row(screen):
        return False
    return (ONBOARD_IDLE_HINT in harness.strip_row(screen)) if strip else True


def onboard_card_ok(screen: str, title: str, rail: str, cols: int, rows: int) -> bool:
    """The card: a whole frame whose title row reads exactly ``┌<title> ──┐`` (``Tour`` is not
    ``Tour · 1 of 5``), a body row, the rail as the last row inside the frame, the one Tour title on
    screen, full width below 100 columns / centred at 100+, and the chrome underneath."""
    window = frame_window(screen, title)
    if window is None or onboard_title_rows(screen) != [window.top]:
        return False
    cells = grid(screen)
    head = "".join(cells[window.top][window.left:window.right + 1])
    if re.fullmatch(r"┌ ?" + re.escape(title) + r" ?─*┐", head) is None:
        return False
    inner = ["".join(cells[index][window.left + 1:window.right]).strip()
             for index in range(window.top + 1, window.bottom)]
    if len(inner) < 3 or not inner[0] or inner[-1] != rail:
        return False
    return geometry_ok(screen, title, cols, rows) and onboard_chrome_ok(screen, rows)


def onboard_coach_ok(screen: str, number: int, rows: int) -> bool:
    """The coach row: the chat pane's top row (screen row 0, the Stage's first row) reads
    ``Tour N/5 · <instruction>`` (the trailing ``· /tour`` when it fits), no card is up and the
    chrome is whole; no other row carries a coach line."""
    lines = screen.split("\n")
    pattern = rf"Tour {number}/5 · {re.escape(ONBOARD_COACH[number])}(?: · /tour)?(?:\s{{2,}}\S.*)?"
    if re.fullmatch(pattern, lines[0].rstrip()) is None:
        return False
    if any(re.match(r"\s*Tour \d/5 · ", row) for row in lines[1:]):
        return False
    return not onboard_title_rows(screen) and onboard_chrome_ok(screen, rows)


def onboard_hub_ok(screen: str, cols: int, rows: int) -> bool:
    """The real action of the armed step: ``Mission Control · Agents`` whole, on the Agents tab with its
    empty roster row, no Tour card over or beside it."""
    title = "Mission Control · Agents"
    if not (settled_ok(screen, title, rows) and geometry_ok(screen, title, cols, rows)):
        return False
    window = frame_window(screen, title)
    cells = grid(screen)
    inside = ["".join(cells[index][window.left + 1:window.right]) for index in range(window.top + 1, window.bottom)]
    return (not onboard_title_rows(screen) and onboard_chrome_ok(screen, rows)
            and any("▸Agents" in row for row in inside) and any("No agents active." in row for row in inside))


def onboard_help_ok(screen: str, cols: int, rows: int) -> bool:
    """The Help frame (its closing border carries the scroll hint, so the shared ``frame_window`` does not
    apply): title row, continuous ``│`` edges, a closing ``└…┘`` at the same columns, geometry by
    viewport, above the strip, no Tour card."""
    cells = grid(screen)
    bar = chrome(screen, rows)
    if bar is None or onboard_title_rows(screen):
        return False
    for top, row in enumerate(cells):
        text = "".join(row)
        match = ONBOARD_HELP_TITLE.search(text)
        if match is None:
            continue
        left = sum(2 if wcwidth(c) == 2 else 1 for c in text[:match.start()])
        right = left + sum(2 if wcwidth(c) == 2 else 1 for c in match.group(0)) - 1
        for index in range(top + 1, len(cells)):
            line = cells[index]
            edge_l = line[left] if left < len(line) else " "
            edge_r = line[right] if right < len(line) else " "
            if edge_l == "└" and edge_r == "┘":
                bottom = index
                break
            if edge_l != "│" or edge_r != "│":
                return False
        else:
            return False
        if bottom >= (bar.strip if bar.strip is not None else bar.status) or bottom < top + 3:
            return False
        if cols < 100:
            geometry = left == 0 and right == cols - 1
        else:
            geometry = left > 0 and abs(left - (cols - 1 - right)) <= 1
        return geometry and onboard_chrome_ok(screen, rows)
    return False


def onboard_toast_ok(screen: str, severity: str, text: str, cols: int) -> bool:
    """The newest toast (one borderless row below 100 columns, three bordered rows at 100+) reads
    ``<Severity> <text>``; at 60 columns the painted headline may be cut with ``…`` after at least the
    first sentence of the message."""
    height = 3 if cols >= 100 else 1
    head = text.split(". ")[0] + "."
    pattern = re.compile(rf"(?<![A-Za-z0-9]){severity} (.*?)(?: ×\d+)?\s*│?$")
    for row in harness.toast_rows(screen, rows=height):
        match = pattern.search(row.rstrip())
        if match is None:
            continue
        painted = match.group(1)
        if painted == text or (painted.endswith("…") and len(painted) > len(head)
                               and text.startswith(painted[:-1])):
            return True
    return False


def onboard_no_offer_ok(screen: str, rows: int, mode: str = "Normal") -> bool:
    """MP11: no Tour card on screen (the offer never re-appears) over a whole chrome."""
    return not onboard_title_rows(screen) and onboard_chrome_ok(screen, rows, mode)


def onboard_team_ok(screen: str, rows: int) -> bool:
    """MP12: the team start: ``PLAN`` is the status row's permission-mode segment and no Tour card paints."""
    return harness.mode_segment(screen) == "PLAN" and onboard_no_offer_ok(screen, rows, "PLAN")


def onboard_idle_after_turn_ok(screen: str, rows: int) -> bool:
    """A scene turn finished: the product's token counter (``↑``) and ``Ready`` on the status row, our own
    message echoed, no Tour card (the tour is complete)."""
    status = harness.status_row(screen)
    return ("↑" in status and "Ready" in status and ONBOARD_MESSAGE in screen
            and onboard_no_offer_ok(screen, rows))


@leg("onboard")
def run_onboard(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    env = {"RUSTAIN_CLIPBOARD": "file"}
    with run.tui(extra_args=ONBOARD_OFFER_ARGS, env=env, notification_ttl_secs=30) as tui:
        run.beat("-c onboarding.offer_tour=true: first launch offers the tour")
        run.pane("offer", settle(tui, lambda s: onboard_card_ok(s, "Tour", ONBOARD_OFFER_RAIL, cols, rows),
                                 "the offer card with its rail", timeout=20))
        tui.send(ENTER)
        run.pane("step-1", settle(tui, lambda s: onboard_card_ok(s, "Tour · 1 of 5", ONBOARD_STEP_RAIL, cols, rows),
                                  "step 1 card", timeout=15))
        for number in (2, 3, 4):
            tui.send("n")
            card = settle(tui, lambda s, n=number: onboard_card_ok(s, f"Tour · {n} of 5", ONBOARD_STEP_RAIL, cols, rows),
                          f"step {number} card", timeout=15)
        run.pane("step-4", card)
        tui.send(ENTER)
        run.pane("coach-4", settle(tui, lambda s: onboard_coach_ok(s, 4, rows), "step 4 armed: coach row", timeout=15))
        tui.send(CTRL_X + "s")
        run.pane("hub", settle(tui, lambda s: onboard_hub_ok(s, cols, rows), "Mission Control · Agents", timeout=20))
        tui.send(ESC)
        run.pane("step-5", settle(tui, lambda s: onboard_card_ok(s, "Tour · 5 of 5", ONBOARD_STEP_RAIL, cols, rows),
                                  "step 5 card on the next due tick", timeout=20))
        tui.send(ENTER)
        run.pane("coach-5", settle(tui, lambda s: onboard_coach_ok(s, 5, rows), "step 5 armed: coach row", timeout=15))
        tui.send(CTRL_X + "?")
        run.pane("help", settle(tui, lambda s: onboard_help_ok(s, cols, rows), "the Help frame", timeout=15))
        tui.send(ESC)
        run.pane("complete", settle(
            tui, lambda s: onboard_toast_ok(s, "Success", ONBOARD_COMPLETE, cols) and onboard_no_offer_ok(s, rows),
            "the Tour complete toast", timeout=15))
        # The one scene turn goes here: no card is up, the tour is over, nothing it did is in flight.
        tui.send_message(ONBOARD_MESSAGE)
        run.pane("turn", settle(tui, lambda s: onboard_idle_after_turn_ok(s, rows), "the scene turn answered", timeout=30))
        # `/tour ` (trailing space: the slash popup does not take the Enter) restarts at step 1.
        tui.send_message("/tour ")
        run.pane("retour", settle(tui, lambda s: onboard_card_ok(s, "Tour · 1 of 5", ONBOARD_STEP_RAIL, cols, rows),
                                  "/tour: step 1 card", timeout=15))
        tui.send(ESC)
        run.pane("closed", settle(
            tui, lambda s: onboard_toast_ok(s, "Info", ONBOARD_CLOSED, cols) and onboard_no_offer_ok(s, rows),
            "the Tour closed toast", timeout=15))
    state = run.ws / ".rustain" / "state.toml"
    text = state.read_text(encoding="utf-8")
    assert re.search(r"^tour_done = true$", text, re.M), text
    assert state.stat().st_mode & 0o777 == 0o600, oct(state.stat().st_mode)
    run.beat("state.toml in the config dir holds tour_done = true (owner-only)")
    with run.tui(extra_args=ONBOARD_TEAM_ARGS, env=env) as tui:
        run.beat("second process, same config dir: offer_tour still on, onboarding.team=true")
        assert tui.wait_for_screen("Type a message to start.", timeout=20), tui.get_screen_text()
        screen = settle(tui, lambda s: onboard_team_ok(s, rows), "PLAN on the status row", timeout=20)
        for _ in range(ONBOARD_NO_OFFER_POLLS):
            screen = tui.get_screen_text()
            assert not onboard_title_rows(screen), screen
        assert onboard_team_ok(screen, rows), screen
        run.pane("team", screen)
    run.not_proven = (
        "Steps 1-3 are skipped with `n` and the only provider turn is sent after the tour (it completes\n"
        "no step); that a sent message, an opened tool inspector or an answered decision completes its\n"
        "step, the typing grace, a failed state save and the modifiers of the highlighted coach row are\n"
        "not asserted here. The coach row and cards are read by text and geometry only; colour is not\n"
        "evidence. The offer's first-launch rule, the step machine, persistence, the team start mode and\n"
        "the coach line are owned by Rust: tests/conformance_21_14_onboarding/k1_offer.rs,\n"
        "k3_steps.rs, k5_persist.rs, k6_team.rs (initial_permission_mode :115, startup seeding :170),\n"
        "k7_coach.rs; the core leg A is tests_tui/test_story_21_14_onboarding.py. The monitor column\n"
        "beside the cards (130 columns with the monitor on) is not captured here.")


# ── Offline control (K11): the real frame shapes, no binary ──────────────────


def _onboard_rail_card(cols: int, rows: int, title: str, rail: str) -> str:
    """A synthetic card whose body row is prose and whose last inner row is ``rail``."""
    screen = synth_frame(cols, rows, title, ["prose row"])
    window = frame_window(screen, title)
    assert window is not None
    lines = screen.split("\n")
    inner = window.right - window.left - 1
    row = lines[window.bottom - 1]
    lines[window.bottom - 1] = row[:window.left] + "│" + rail.ljust(inner)[:inner] + "│" + row[window.right + 1:]
    return "\n".join(lines)


def _onboard_toast_frame(cols: int, rows: int, severity: str, text: str, older: tuple[str, str] | None = None) -> str:
    """A synthetic frame with a toast the way the product paints it: one right-aligned row below 100
    columns, a bordered three-row box at 100+ (the newest adjacent to the strip row); ``older`` is a
    (severity, text) toast stacked above it."""
    base = synth_frame(cols, rows, None)
    lines = base.split("\n")
    region = rows - 5
    toasts = ([older] if older else []) + [(severity, text)]
    cursor = region
    for sev, body in reversed(toasts):
        label = f"{sev} {body}"
        if cols >= 100:
            width = len(label) + 2
            lines[cursor - 1] = ("└" + "─" * (width - 2) + "┘").rjust(cols)
            lines[cursor - 2] = ("│" + label + "│").rjust(cols)
            lines[cursor - 3] = ("┌" + "─" * (width - 2) + "┐").rjust(cols)
            cursor -= 3
        else:
            lines[cursor - 1] = (label if len(label) <= cols else label[:cols - 1] + "…").rjust(cols)
            cursor -= 1
    return "\n".join(lines)


def test_j21_predicate_onboard():
    """K11: the offer card, step card, coach row and closed toast are accepted whole at 60x16, 80x24 and
    130x30; MP11 (the offer re-appears after completion) and MP12 (the team start is not PLAN) are
    rejected, with torn/mutated variants of each shape."""
    for cols, rows in SYNTH_VIEWPORTS:
        # the first-launch offer card with its rail (the positive control) and its mutants
        offer = _onboard_rail_card(cols, rows, "Tour", ONBOARD_OFFER_RAIL)
        assert onboard_card_ok(offer, "Tour", ONBOARD_OFFER_RAIL, cols, rows), offer
        assert not onboard_card_ok(offer, "Tour · 1 of 5", ONBOARD_STEP_RAIL, cols, rows)  # the offer is not step 1
        skip = _onboard_rail_card(cols, rows, "Tour", ONBOARD_STEP_RAIL)
        assert not onboard_card_ok(skip, "Tour", ONBOARD_OFFER_RAIL, cols, rows)  # the offer carries no [n] Skip step
        assert not onboard_card_ok(offer, "Tour", ONBOARD_STEP_RAIL, cols, rows)
        window = frame_window(offer, "Tour")
        lines = offer.split("\n")
        torn = list(lines)
        torn[window.bottom] = " " * cols
        assert not onboard_card_ok("\n".join(torn), "Tour", ONBOARD_OFFER_RAIL, cols, rows)
        no_rail = _onboard_rail_card(cols, rows, "Tour", "")
        assert not onboard_card_ok(no_rail, "Tour", ONBOARD_OFFER_RAIL, cols, rows)
        no_strip = synth_frame(cols, rows, "Tour", ["prose row"], strip=False)
        assert not onboard_card_ok(no_strip, "Tour", ONBOARD_OFFER_RAIL, cols, rows)
        wrong_shape = synth_frame(cols, rows, "Tour", ["prose row"], centred=not (cols >= 100))
        assert not geometry_ok(wrong_shape, "Tour", cols, rows)

        # a step card
        for number in (1, 4, 5):
            title = f"Tour · {number} of 5"
            step = _onboard_rail_card(cols, rows, title, ONBOARD_STEP_RAIL)
            assert onboard_card_ok(step, title, ONBOARD_STEP_RAIL, cols, rows), step
            other = f"Tour · {number % 5 + 1} of 5"
            assert not onboard_card_ok(step, other, ONBOARD_STEP_RAIL, cols, rows)
            assert not onboard_card_ok(step, title, ONBOARD_OFFER_RAIL, cols, rows)
        step4 = _onboard_rail_card(cols, rows, "Tour · 4 of 5", ONBOARD_STEP_RAIL)
        stacked = step4.split("\n")
        stacked[1] = offer.split("\n")[window.top]  # a second Tour title painted on another row
        assert not onboard_card_ok("\n".join(stacked), "Tour · 4 of 5", ONBOARD_STEP_RAIL, cols, rows)

        # the coach row on the chat pane's top row
        for number, instruction in ONBOARD_COACH.items():
            plain = synth_frame(cols, rows, None).split("\n")
            coach = list(plain)
            coach[0] = f"Tour {number}/5 · {instruction} · /tour".ljust(cols)
            assert onboard_coach_ok("\n".join(coach), number, rows), "\n".join(coach)
            assert not onboard_coach_ok("\n".join(coach), 9 - number, rows)  # the other step's number
            lower = list(plain)
            lower[1] = coach[0]
            assert not onboard_coach_ok("\n".join(lower), number, rows)  # not the top row
            twice = list(coach)
            twice[2] = coach[0]
            assert not onboard_coach_ok("\n".join(twice), number, rows)
            bare = list(plain)
            bare[0] = f"Tour {number}/5".ljust(cols)
            assert not onboard_coach_ok("\n".join(bare), number, rows)  # no instruction
            with_card = list(coach)
            with_card[3] = ("┌Tour · 4 of 5" + "─" * cols)[:cols - 1] + "┐"
            assert not onboard_coach_ok("\n".join(with_card), number, rows)  # the card is still up

        # the Success toast (cut with an ellipsis at 60) and the Info toast closing the tour
        complete = _onboard_toast_frame(cols, rows, "Success", ONBOARD_COMPLETE)
        assert onboard_toast_ok(complete, "Success", ONBOARD_COMPLETE, cols), complete
        assert not onboard_toast_ok(complete, "Warning", ONBOARD_COMPLETE, cols)  # another severity
        assert not onboard_toast_ok(complete, "Success", ONBOARD_CLOSED, cols)
        cut = ONBOARD_COMPLETE[:40] + "…"
        assert onboard_toast_ok(_onboard_toast_frame(cols, rows, "Success", cut), "Success", ONBOARD_COMPLETE, cols)
        assert not onboard_toast_ok(_onboard_toast_frame(cols, rows, "Success", "Tour…"), "Success", ONBOARD_COMPLETE, cols)
        assert not onboard_toast_ok(_onboard_toast_frame(cols, rows, "Success", ONBOARD_COMPLETE[:40] + "X"),
                                    "Success", ONBOARD_COMPLETE, cols)  # not a prefix with an ellipsis
        closed = _onboard_toast_frame(cols, rows, "Info", ONBOARD_CLOSED, older=("Success", ONBOARD_COMPLETE))
        assert onboard_toast_ok(closed, "Info", ONBOARD_CLOSED, cols), closed
        assert not onboard_toast_ok(closed, "Success", ONBOARD_COMPLETE, cols)  # only the newest toast counts
        elsewhere = synth_frame(cols, rows, None, toast="")
        stage = elsewhere.split("\n")
        stage[1] = f"Info {ONBOARD_CLOSED}".rjust(cols)  # painted in the Stage, not on the toast rows
        assert not onboard_toast_ok("\n".join(stage), "Info", ONBOARD_CLOSED, cols)

        # MP11: no re-offer after completion
        idle = synth_frame(cols, rows, None)
        assert onboard_no_offer_ok(idle, rows), idle
        assert not onboard_no_offer_ok(offer, rows)  # the offer card re-appeared
        reoffer = idle.split("\n")
        reoffer[0] = "┌Tour" + "─" * (cols - 6) + "┐"
        assert not onboard_no_offer_ok("\n".join(reoffer), rows)
        assert not onboard_no_offer_ok(synth_frame(cols, rows, None, strip=False), rows)  # a torn chrome

        # MP12: the team start is PLAN
        plan = idle.replace("│ Normal │", "│ PLAN │")
        assert onboard_team_ok(plan, rows), plan
        assert not onboard_team_ok(idle, rows)  # Normal: the team switch was ignored
        assert not onboard_team_ok(idle.replace("│ Normal │", "│ AUTOEDIT │"), rows)
        replan = plan.split("\n")
        replan[0] = "┌Tour" + "─" * (cols - 6) + "┐"
        assert not onboard_team_ok("\n".join(replan), rows)  # PLAN with the offer painted


# ── The decide leg (Story 21.10, A7 row `decide`) ────────────────────────────
#
# One scene, two phases on one process:
#   phase 1  `Write` request -> `Decision: Permission`; Esc defers (`Needs you (1) ^X q`);
#            `Ctrl+X q` lists it; Enter re-opens it; `n` denies.
#   phase 2  one turn proposes a plan (`propose_plan`) AND asks for a `Write` -> two decisions wait
#            (`Needs you (2)`), listed by `Ctrl+X q`; the plan card is approved, the write denied.
# Every decision key follows a frame that stayed complete and unchanged for 600 ms (DecideGate).

DECIDE_PERMISSION_RAIL = ("[y] Allow", "[s] Session", "[a] Always", "[n] Deny", "[f] Deny + feedback", "[r] Review")
DECIDE_PLAN_RAIL = ("[y] Approve", "[e] Edit", "[n] Reject", "[Esc] Later")
DECIDE_APPROVAL_RAIL = ("[y] Approve", "[a] Approve + auto-edit", "[n] Reject", "[e] Edit", "[Esc] Later")
CTRL_P_KEY = "\x10"
DECIDE_LIST_RAIL = ("[Enter] Open", "[Esc] Back", "[Up/Down] Select")
DECIDE_STABLE_SECS = 0.6
DECIDE_PASTE = "\x1b[200~{}\x1b[201~\r"


def decide_rail_in_order(rail_text: str, tokens) -> bool:
    """K8: every rail token is present in the given order, consecutive, with only blanks between them
    and nothing else in the rail text (a swapped, missing, duplicated or foreign token fails)."""
    position = 0
    for token in tokens:
        found = rail_text.find(token, position)
        if found < 0 or rail_text[position:found].strip():
            return False
        position = found + len(token)
    return not rail_text[position:].strip()


def decide_window_rows(screen: str, title: str):
    """The window titled ``title`` and its interior rows read by the window's own columns (a centred
    window at 130 columns has Stage text beside it); None while the window is not whole."""
    window = frame_window(screen, title)
    if window is None:
        return None
    cells = grid(screen)
    rows = ["".join(cells[index][window.left + 1:window.right]).strip()
            for index in range(window.top + 1, window.bottom)]
    return window, rows


def decide_frame(screen: str, kind: str, crumb: str, rail, cols: int, rows: int):
    """The whole ``Decision: <kind>`` frame, or None: its geometry (full sheet below 100 columns,
    centred above, status and strip uncovered), the ``Needs you › <crumb> | N pending`` row first and the
    rail tokens in order behind it. Returns ``(window, content rows)``."""
    title = f"Decision: {kind}"
    if not geometry_ok(screen, title, cols, rows):
        return None
    got = decide_window_rows(screen, title)
    if got is None:
        return None
    window, interior = got
    content = [row for row in interior if row]
    if not content or not re.fullmatch(re.escape(f"Needs you › {crumb}") + r" \| \d+ pending", content[0]):
        return None
    rail_rows = []
    for row in content[1:]:
        if rail_rows or row.startswith("["):
            rail_rows.append(row)
    if not decide_rail_in_order(" ".join(rail_rows), rail):
        return None
    return window, tuple(content)


def decide_list(screen: str, entries, cols: int, rows: int):
    """The whole ``Needs you`` list: ``Needs you › N waiting`` first, one row per entry (in the given
    order, each starting with its kind and subject, marker aside), the list rail last. Returns
    ``(window, content rows, selected index)`` or None."""
    if not geometry_ok(screen, "Needs you", cols, rows):
        return None
    got = decide_window_rows(screen, "Needs you")
    if got is None:
        return None
    window, interior = got
    content = [row for row in interior if row]
    if len(content) != len(entries) + 2 or content[0] != f"Needs you › {len(entries)} waiting":
        return None
    if not decide_rail_in_order(content[-1], DECIDE_LIST_RAIL):
        return None
    selected = None
    for index, (row, entry) in enumerate(zip(content[1:-1], entries)):
        marked = row.startswith("▸ ")
        text = row[2:] if row[:2] in ("▸ ", "  ") else row
        if not text.startswith(entry):
            return None
        if marked:
            if selected is not None:
                return None
            selected = index
    if selected is None:
        return None
    return window, tuple(content), selected


def decide_cue(screen: str, waiting: int) -> bool:
    """The status cue ``Needs you (N) ^X q`` on the status row, no decision frame or list open."""
    status = harness.status_row(screen)
    return f"Needs you ({waiting}) ^X q" in status and "Decision:" not in screen and "┌ Needs you ─" not in screen


class DecideGate:
    """MD9 gate: a decision key may be sent only on a value (a complete frame) that was seen, whole, at
    least ``stable_for`` seconds ago and has not changed since. ``feed`` takes the observed frame (None
    when torn or absent) and the clock; it returns True when the key may be sent."""

    def __init__(self, stable_for: float = DECIDE_STABLE_SECS) -> None:
        self.stable_for = stable_for
        self.value = None
        self.since = 0.0

    def feed(self, value, now: float) -> bool:
        if value is None:
            self.value = None
            return False
        if value != self.value:
            self.value, self.since = value, now
            return False
        return now - self.since >= self.stable_for


def decide_wait(tui, observe, label: str, timeout: float = 45.0, clock=time.monotonic) -> str:
    """Wait until ``observe(screen)`` is a complete value that stayed unchanged for the gate's window;
    return that screen. Paced by the PTY read, bounded by ``timeout``."""
    gate = DecideGate()
    deadline = clock() + timeout
    while True:
        screen = tui.get_screen_text()
        now = clock()
        if gate.feed(observe(screen), now):
            return screen
        assert now < deadline, f"stable {label}:\n{screen}"


def decide_answer(tui, observe, key: str, label: str, timeout: float = 45.0, clock=time.monotonic,
                  precheck=None) -> str:
    """THE decision-key sender (MD9): wait for ``observe`` to be a complete frame unchanged for the gate's
    window, run ``precheck`` on that screen, then send ``key``. Every decision answer of the leg goes
    through here, and ``test_j21_predicate_rail`` drives this very function against a fake PTY and clock."""
    screen = decide_wait(tui, observe, label, timeout, clock)
    if precheck is not None:
        precheck(screen)
    tui.send(key)
    return screen


def decide_ready(screen: str, rows: int) -> bool:
    bar = chrome(screen, rows)
    return bar is not None and "Ready" in harness.status_row(screen) and "Decision:" not in screen


def _no_file(run: "J21Run", name: str, message: str) -> None:
    assert not (run.ws / name).exists(), message


def decide_quiet(screen: str, rows: int) -> bool:
    """The chrome is whole and no decision frame is open (the status row's ``Ready`` word is clipped at
    60 columns once the PLAN segment and the plan-reminder cue are painted, so it is not asked for)."""
    return chrome(screen, rows) is not None and "Decision:" not in screen


def decide_rejected(screen: str) -> bool:
    """The product's own rejection message after ``n`` on the plan approval, still in Plan mode."""
    return ("Plan rejected. Please revise the plan based on the user's feedback." in " ".join(screen.split())
            and "Decision:" not in screen and harness.mode_segment(screen) == "PLAN")


def decide_select(tui, run: "J21Run", kind: str, entries, timeout: float = 20.0) -> None:
    """In the settled ``Needs you`` list, move the selection to the entry that starts with ``kind`` and
    open it with Enter. Each move is a key sent on a settled list; the loop is bounded."""
    deadline = time.monotonic() + timeout
    index = [entry.split(" ·")[0] for entry in entries].index(kind)
    while True:
        screen = decide_wait(tui, lambda s: decide_list(s, entries, run.cols, run.rows), "needs-you list")
        selected = decide_list(screen, entries, run.cols, run.rows)[2]
        if selected == index:
            tui.send(ENTER)
            return
        tui.send("\x1b[B" if selected < index else "\x1b[A")
        moved = lambda s, was=selected: (decide_list(s, entries, run.cols, run.rows) or (0, 0, was))[2] != was
        tui.wait_until(moved, "list selection move", 10)
        assert time.monotonic() < deadline, f"selecting {kind}:\n{screen}"


def decide_denied(screen: str, path: str) -> bool:
    """The product's own transcript of a denied Write: the failed tool row and the denial line."""
    lines = [line.strip().lstrip("│┃ ").strip() for line in screen.split("\n")]
    return any(line.startswith("✗ Write " + path) for line in lines) and "Permission denied by user" in screen


def decide_turn(run: "J21Run", turn: int, text: str) -> bool:
    """The scene provider's request log (what the product sent) holds scene turn ``turn`` whose last
    user text contains ``text``."""
    return any(row.get("turn") == turn and text in row.get("last_user", "") for row in run.stub.rows())


@leg("decide")
def run_decide(run: J21Run) -> None:
    cols, rows = run.cols, run.rows
    perm_frame = lambda s: decide_frame(s, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
    plan_frame = lambda s: decide_frame(s, "Plan card", "Plan card J21-DECIDE-PLAN", DECIDE_PLAN_RAIL, cols, rows)
    one = ("Permission · Write",)
    two = ("Plan card · J21-DECIDE-PLAN", "Permission · Write")
    run.not_proven = (
        "Behaviour truth is owned by Rust, not re-proven here: the decision queue, its arrival and "
        "ordering rules, the frame and rail generator and the inert window "
        "(tests/conformance_21_6_decision_queue/k1_queue.rs, k2_arrival.rs, k3_frame.rs, k4_permission.rs, "
        "k6_list.rs, k8_invariance.rs), and the registry rows behind the decision keys "
        "(tests/conformance_21_3_keymap/k11_decisions.rs:523); the core 21_6 legs B/C and the core 20_9a plan "
        "card (test_story_20_9a_ephemeral_rows.py:313-339) carry the native precedent.\n"
        "Not exercised here: Session/Always allow, Deny + feedback, Review, the question, fork, consent and "
        "recipient decisions, the attached mirror (core 21_6 leg B; the attach leg), and the Approve and "
        "Approve + auto-edit outcomes of `Decision: Plan approval` (only Reject is pressed; Rust owns the outcomes). "
        "`Decision: Plan approval` is raised by `exit_plan_mode` in the Plan permission mode (Ctrl+P `mode: plan`, "
        "one scene turn) and reached through the `Ctrl+X q` list; the rejection is asserted as the product's own "
        "`Plan rejected. Please revise…` message with no further provider turn. `Decision: Plan card` "
        "(`propose_plan`) is the separate plan-card kind (tests/conformance_21_6_decision_queue/ cards, "
        "src/adapters/tui/decision_queue/plan.rs); its task execution after Approve is asserted only as "
        "the product's own `Now executing task 1: …` turn in the request log, not its outcome. The served text is not evidence; the product's painted frames, "
        "rows, files and request log are.")
    with run.tui() as tui:
        screen = decide_wait(tui, lambda s: decide_ready(s, rows) or None, "launch")
        run.beat("launched; empty allow-list")

        # phase 1: a Write request -> Decision: Permission
        tui.send(DECIDE_PASTE.format("J21-DECIDE-MSG-1"))
        screen = decide_answer(tui, perm_frame, ESC, "Decision: Permission",  # Esc = Later
                               precheck=lambda s: _no_file(run, "j21-decide-one.txt", "the write ran before any decision"))
        run.pane("01-permission-frame", screen)
        run.beat("Decision: Permission settled, rail in order behind Needs you › Permission; Esc deferred it")
        screen = decide_wait(tui, lambda s: True if decide_cue(s, 1) else None, "Needs you (1) cue")
        assert not (run.ws / "j21-decide-one.txt").exists()
        run.pane("02-deferred-cue", screen)
        run.beat("Esc deferred: Needs you (1) ^X q")

        tui.send(CTRL_X + "q")
        screen = decide_wait(tui, lambda s: decide_list(s, one, cols, rows), "Needs you list (1)")
        run.pane("03-list-one", screen)
        decide_select(tui, run, "Permission", one)
        screen = decide_answer(tui, perm_frame, "n", "Decision: Permission re-opened")
        run.pane("04-permission-reopened", screen)
        screen = decide_wait(tui, lambda s: True if decide_denied(s, "j21-decide-one.txt")
                             and decide_ready(s, rows) else None, "first write denied")
        assert not (run.ws / "j21-decide-one.txt").exists(), "a denied write created its file"
        run.pane("05-first-denied", screen)
        run.beat("n denied the write; no file")
        approval_frame = lambda s: decide_frame(s, "Plan approval", "Plan approval plan", DECIDE_APPROVAL_RAIL, cols, rows)
        # phase 2: a plan card and a second Write wait together
        tui.send(DECIDE_PASTE.format("J21-DECIDE-MSG-2"))
        screen = decide_wait(tui, lambda s: True if plan_frame(s) or decide_cue(s, 2) else None,
                             "plan card frame or the Needs you (2) cue")
        if plan_frame(screen):
            screen = decide_answer(tui, plan_frame, ESC, "plan card frame")
            run.pane("06-plan-card-frame", screen)
            screen = decide_wait(tui, lambda s: True if decide_cue(s, 2) else None, "Needs you (2) cue")
        run.pane("07-two-waiting-cue", screen)
        run.beat("two decisions wait: Needs you (2) ^X q")

        tui.send(CTRL_X + "q")
        screen = decide_wait(tui, lambda s: decide_list(s, two, cols, rows), "Needs you list (2)")
        run.pane("08-list-both", screen)
        decide_select(tui, run, "Plan card", two)
        screen = decide_answer(tui, plan_frame, "y", "Decision: Plan card")  # Approve
        run.pane("09-plan-card-open", screen)
        screen = decide_wait(tui, lambda s: True if decide_cue(s, 1) else None, "Needs you (1) after Approve")
        run.pane("10-plan-approved-cue", screen)
        run.beat("plan card approved; the write still waits")

        tui.send(CTRL_X + "q")
        screen = decide_wait(tui, lambda s: decide_list(s, one, cols, rows), "Needs you list (1) again")
        decide_select(tui, run, "Permission", one)
        screen = decide_answer(tui, perm_frame, "n", "Decision: Permission (second)",
                               precheck=lambda s: _no_file(run, "j21-decide-two.txt", "the write ran before any decision"))
        run.pane("11-permission-second", screen)
        screen = decide_wait(tui, lambda s: True if decide_turn(run, 4, "Permission denied by user")
                             and decide_turn(run, 5, "Now executing task 1: J21-DECIDE-TASK-ONE")
                             and decide_ready(s, rows) else None, "second write denied, approved plan's task turn")
        assert not (run.ws / "j21-decide-two.txt").exists(), "a denied write created its file"
        run.pane("12-second-denied-task-turn", screen)
        run.beat("n denied the second write (no file); the approved plan's first task started its own turn")

        # phase 3 (review D01): the specified `Decision: Plan approval`, raised by `exit_plan_mode` in Plan mode.
        tui.send(CTRL_P_KEY)
        decide_wait(tui, lambda s: True if "Command Palette" in s else None, "the command palette")
        for char in "mode: plan":
            tui.send(char)
            tui.get_screen_text()
        tui.send(ENTER)
        screen = decide_wait(tui, lambda s: True if harness.mode_segment(s) == "PLAN" and decide_quiet(s, rows) else None,
                             "PLAN on the status row")
        run.pane("13-plan-mode", screen)
        run.beat("Ctrl+P `mode: plan`: the status row reads PLAN")
        tui.send(DECIDE_PASTE.format("J21-DECIDE-MSG-3"))
        screen = decide_wait(tui, lambda s: True if re.search(r"(?:Decisions|D) 1\b", harness.strip_row(s)) and decide_quiet(s, rows)
                             else None, "the plan approval queued (strip: Decisions 1)")
        run.pane("14-approval-queued", screen)
        tui.send(CTRL_X + "q")
        approval = ("Plan approval",)
        screen = decide_wait(tui, lambda s: decide_list(s, approval, cols, rows), "Needs you list (plan approval)")
        run.pane("15-list-approval", screen)
        decide_select(tui, run, "Plan approval", approval)
        screen = decide_answer(tui, approval_frame, "n", "Decision: Plan approval")  # Reject
        assert harness.mode_segment(screen) == "PLAN", screen
        run.pane("16-plan-approval-frame", screen)
        run.beat("exit_plan_mode in Plan mode: Decision: Plan approval settled, rail in order behind Needs you › Plan approval; n rejected it")
        screen = decide_wait(tui, lambda s: True if decide_rejected(s) and decide_quiet(s, rows) else None,
                             "plan rejected, the product's own revise message")
        assert len([row for row in run.stub.rows() if "turn" in row]) == 7, run.stub.rows()  # the rejection submits no turn
        run.pane("17-plan-rejected", screen)
        run.beat("n rejected the plan approval: the product painted its own `Plan rejected. Please revise…` message in PLAN mode")


# ── Offline control: decision rail exactness (K8) ────────────────────────────


def decide_synth_permission(cols: int, rows: int, rail_rows=None, crumb="Needs you › Permission Write | 1 pending"):
    rail_rows = rail_rows or ["[y] Allow  [s] Session  [a] Always", "[n] Deny  [f] Deny + feedback  [r] Review"]
    body = [crumb, "Write", "│ file_path: j21-decide-one.txt", "│ content: J21-DECIDE-BODY-ONE", *rail_rows]
    return synth_frame(cols, rows, "Decision: Permission", body)


def test_j21_predicate_rail():
    """K8: the whole permission frame (and the plan card, and the list) is accepted at each viewport; a rail
    with two tokens swapped (MP7), a missing token, a torn border and a wrong crumb are rejected; a decision
    key is gated on 600 ms of an unchanged complete frame (MD9)."""
    for cols, rows in SYNTH_VIEWPORTS:
        whole = decide_synth_permission(cols, rows)
        assert decide_frame(whole, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
        # MP7: two rail tokens swapped, on one row and across the two rows
        within = decide_synth_permission(cols, rows, ["[s] Session  [y] Allow  [a] Always",
                                                      "[n] Deny  [f] Deny + feedback  [r] Review"])
        across = decide_synth_permission(cols, rows, ["[y] Allow  [s] Session  [n] Deny",
                                                      "[a] Always  [f] Deny + feedback  [r] Review"])
        for torn in (within, across):
            assert not decide_frame(torn, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
        missing = decide_synth_permission(cols, rows, ["[y] Allow  [s] Session  [a] Always", "[n] Deny  [r] Review"])
        assert not decide_frame(missing, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
        foreign = decide_synth_permission(cols, rows, ["[y] Allow  [s] Session  [a] Always  [x] Extra",
                                                       "[n] Deny  [f] Deny + feedback  [r] Review"])
        assert not decide_frame(foreign, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
        wrong_crumb = decide_synth_permission(cols, rows, crumb="Needs you › Permission Bash | 1 pending")
        assert not decide_frame(wrong_crumb, "Permission", "Permission Write", DECIDE_PERMISSION_RAIL, cols, rows)
        # a closing border missing: the frame is torn
        lines = whole.split("\n")
        window = frame_window(whole, "Decision: Permission")
        assert window is not None
        torn_bottom = [line for line in lines]
        row = torn_bottom[window.bottom]
        torn_bottom[window.bottom] = row[:window.left] + " " * (window.right - window.left + 1) + row[window.right + 1:]
        assert not decide_frame("\n".join(torn_bottom), "Permission", "Permission Write",
                                DECIDE_PERMISSION_RAIL, cols, rows)
        # the plan card and the list under the same predicates
        plan = synth_frame(cols, rows, "Decision: Plan card", [
            "Needs you › Plan card J21-DECIDE-PLAN | 2 pending", "J21-DECIDE-PLAN", "1. J21-DECIDE-TASK-ONE",
            "[y] Approve  [e] Edit  [n] Reject  [Esc] Later"])
        assert decide_frame(plan, "Plan card", "Plan card J21-DECIDE-PLAN", DECIDE_PLAN_RAIL, cols, rows)
        swapped = plan.replace("[e] Edit  [n] Reject", "[n] Reject  [e] Edit")
        assert not decide_frame(swapped, "Plan card", "Plan card J21-DECIDE-PLAN", DECIDE_PLAN_RAIL, cols, rows)
        both = ("Plan card · J21-DECIDE-PLAN", "Permission · Write")
        listing = synth_frame(cols, rows, "Needs you", [
            "Needs you › 2 waiting", "  Plan card · J21-DECIDE-PLAN · abc · 8s · deferred",
            "▸ Permission · Write · — · 8s", "[Enter] Open  [Esc] Back  [Up/Down] Select"])
        got = decide_list(listing, both, cols, rows)
        assert got and got[2] == 1
        assert not decide_list(listing.replace("[Esc] Back  [Up/Down] Select", "[Up/Down] Select  [Esc] Back"),
                               both, cols, rows)
        assert not decide_list(listing, both[::-1], cols, rows)
        # a status cue is not a frame, and a frame is not a cue
        cued = synth_frame(cols, rows, None).replace("Normal", "Normal │ Needs you (2) ^X q")
        assert decide_cue(cued, 2) and not decide_cue(cued, 1) and not decide_cue(whole, 2)
    # the pure rail predicate on its own
    assert decide_rail_in_order("[a] A  [b] B", ("[a] A", "[b] B"))
    assert not decide_rail_in_order("[b] B  [a] A", ("[a] A", "[b] B"))
    assert not decide_rail_in_order("[a] A  [b] B", ("[a] A", "[b] B", "[c] C"))
    assert not decide_rail_in_order("[a] A  x  [b] B", ("[a] A", "[b] B"))
    # MD9: the key gate. A frame seen for the first time is never a go; a changed frame restarts the clock.
    frame_a, frame_b = ("w", ("a",)), ("w", ("b",))
    gate = DecideGate()
    assert not gate.feed(frame_a, 10.0)  # first sight
    assert not gate.feed(frame_a, 10.3)
    assert gate.feed(frame_a, 10.7)
    gate = DecideGate()
    assert not gate.feed(frame_a, 0.0) and not gate.feed(frame_a, 0.5)
    assert not gate.feed(frame_b, 0.9)  # changed inside the window: restarts
    assert not gate.feed(frame_b, 1.4) and gate.feed(frame_b, 1.6)
    gate = DecideGate()
    assert not gate.feed(frame_a, 0.0) and gate.feed(frame_a, 0.7)
    assert not gate.feed(None, 0.8) and not gate.feed(frame_a, 0.9)  # torn then whole again: restarts
    assert not DecideGate().feed(frame_a, 5.0)  # a sender with no gate delay (MD9) would fail here

    # MD9 on the ACTUAL sender: `decide_answer` against a fake PTY and clock. The key is sent only after a
    # complete frame stayed unchanged for the gate's window; a sender that bypasses the wait (the mutant) is
    # caught by the same check.
    class FakePty:
        def __init__(self, script):
            self.script, self.now, self.sent, self.reads = list(script), 100.0, [], 0

        def clock(self):
            return self.now

        def get_screen_text(self):
            self.now += 0.2  # one PTY read
            screen = self.script[min(self.reads, len(self.script) - 1)]
            self.reads += 1
            return screen

        def send(self, key):
            self.sent.append((key, self.now))

    observe = lambda screen: screen if screen.startswith("FRAME") else None  # noqa: E731
    script = ["torn", "FRAME a", "FRAME a", "FRAME b"] + ["FRAME b"] * 8  # b first seen on read 4 (t = 100.8)
    pty = FakePty(script)
    seen_b = 100.0 + 0.2 * 4
    decide_answer(pty, observe, "n", "fake frame", clock=pty.clock)
    assert [key for key, _ in pty.sent] == ["n"]
    assert pty.sent[0][1] - seen_b >= DECIDE_STABLE_SECS - 1e-9, pty.sent  # not before 600 ms of the final frame
    assert pty.reads >= 7
    bypass = FakePty(script)  # the mutant: send on first sight, no wait
    bypass.get_screen_text()
    bypass.send("n")
    assert bypass.sent[0][1] - seen_b < DECIDE_STABLE_SECS, "the bypass must violate the window the real sender keeps"
    never = FakePty(["torn"])  # a frame that never completes: no key is ever sent, the wait fails loudly
    with pytest.raises(AssertionError, match="stable never"):
        decide_answer(never, observe, "n", "never", timeout=1.0, clock=never.clock)
    assert never.sent == []
    checked = []
    pty = FakePty(["FRAME c"] * 8)
    decide_answer(pty, observe, "y", "c", clock=pty.clock, precheck=lambda screen: checked.append((screen, list(pty.sent))))
    assert checked == [("FRAME c", [])] and [key for key, _ in pty.sent] == ["y"]  # precheck runs before the key

    # the Plan approval frame (review D01): one rail row behind its crumb, geometry per viewport
    for cols, rows in SYNTH_VIEWPORTS:
        approval = synth_frame(cols, rows, "Decision: Plan approval", [
            "Needs you › Plan approval plan | 1 pending", "Plan Approval — plan.md", "Summary: J21-DECIDE-PLAN-APPROVAL summary", "",
            "[y] Approve  [a] Approve + auto-edit", "[n] Reject  [e] Edit  [Esc] Later"])
        assert decide_frame(approval, "Plan approval", "Plan approval plan", DECIDE_APPROVAL_RAIL, cols, rows)
        swapped = approval.replace("[n] Reject  [e] Edit", "[e] Edit  [n] Reject")  # swapped within the second row
        assert not decide_frame(swapped, "Plan approval", "Plan approval plan", DECIDE_APPROVAL_RAIL, cols, rows)
        assert not decide_frame(approval, "Plan card", "Plan approval plan", DECIDE_PLAN_RAIL, cols, rows)  # not the plan card
    reject = synth_frame(80, 24, None).replace("│ Normal │", "│ PLAN │")
    assert not decide_rejected(reject)
    lines = reject.split("\n")
    lines[3] = "Plan rejected. Please revise the plan based on the user's feedback."
    assert decide_rejected("\n".join(lines))
    assert not decide_rejected("\n".join(lines).replace("│ PLAN │", "│ Normal │"))


# ── The campaign test ────────────────────────────────────────────────────────


@pytest.mark.parametrize("leg_name", LEGS)
def test_j21_leg(leg_name, monkeypatch):
    path = run_leg(leg_name, monkeypatch)
    assert path.name.startswith("journey-J21-")
