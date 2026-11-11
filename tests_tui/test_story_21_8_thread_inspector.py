"""Story 21.8 slice 8-A — S-THREAD replaces the recipient surface (smoke leg A).

Leg A, 80x24, debug default-feature binary resolved by the harness
(``RUSTAIN_TUI_BINARY`` honoured):

* local TUI: capture the Stage rows; ``/team view ri_smoke_adr`` opens ONE
  ``Thread · architecture_update`` frame (identity row ``item actions · <id>``,
  the LocalOnly footer, no mutation key row, the legacy body with the visible
  ``\\u{1b}`` escape); ``Ctrl+X`` ``n`` opens ``System · Notifications`` over it and
  ``Esc`` returns to the identical frame rows; ``PgDn`` x2 scrolls the body while
  the pinned header stays; ``Esc`` closes the frame: Stage rows equal the capture
  and the composer row is the capture's.
* daemon-attached (``rustain daemon start`` + ``rustain daemon attach``): the
  writer paints the same identity row and a rail with ``[d] Discuss``,
  ``[a] Acknowledge`` and ``[u] Use as context``; ``PgDn`` scrolls; a read-only
  second attach paints the read-only footer and no mutation key.

Leg B exercises local Rendered/Source safety, byte-exact file copy, turn
filtering and the wide-block viewport. Its offline desk checks use synthetic
screens at the same three sizes, including incomplete and forged rows.

Offline desk checks (``test_leg_a_desk_*``) run the very same predicates over
synthetic 60x16, 80x24 and 130x30 screens with torn-frame negatives; they start no
binary, subprocess or scene provider. Every wait is a structural predicate wait
(never a sleep oracle).
"""

from __future__ import annotations

import fcntl
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

import harness  # noqa: E402
from fixtures.scene_provider import SceneStub  # noqa: E402
from harness import NOTIFICATION_LOG_TITLE, RustainTUI, _load_env, _resolve_binary, composer_top  # noqa: E402
from keys import CTRL_X, ENTER, ESC  # noqa: E402

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "smoke.json"
PEER = "12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
ITEM = "ri_smoke_adr"
TOKEN = "architecture_update"
PGDN = "\x1b[6~"

LOCAL_FOOTER = "local view only — replying, acknowledgement and item context require a daemon-attached writer"
READONLY_FOOTER = "read-only — view only; item actions require the attached writer"
MUTATION_KEYS = ("[a] Acknowledge", "[d] Discuss", "[u] Use as context")
WRITER_KEYS = ("[d] Discuss", "[a] Acknowledge", "[u] Use as context")
PAGE_KEYS = ("[p] Earlier", "[l] Newer")
RAIL_SPAN = 6  # inner rows from the bottom that may hold the footer + rail
ESCAPE_LITERAL = "\\u{1b}"


# ── Pure predicates (shared by the live legs and the desk checks) ────────────


def screen_rows(screen: str) -> list[str]:
    return [row.rstrip() for row in screen.split("\n")]


def stage_rows(screen: str) -> list[str]:
    """Rows above the toast row, which sits above the mission strip row, above the status row
    above the composer."""
    rows = screen_rows(screen)
    top = composer_top(rows)
    assert top is not None and top >= 4, f"no composer on screen:\n{screen}"
    return rows[: top - 3]


def composer_row(screen: str) -> str:
    """The composer's first inner row (the row right below its top border)."""
    rows = screen_rows(screen)
    top = composer_top(rows)
    assert top is not None and top + 1 < len(rows), f"no composer on screen:\n{screen}"
    return rows[top + 1]


def normalized(rows: list[str]) -> str:
    return " ".join(" ".join(rows).split())


def frame_inner(screen: str, token: str = TOKEN) -> list[str]:
    """Inner rows (borders removed, right-stripped) of the ``Thread · <token>`` frame.

    Rejects a torn frame: the title row (``┌─ Thread · <token> ─┐``) absent or
    cut, a side border missing on an inner row, or the bottom border missing."""
    rows = screen.split("\n")
    pattern = re.compile(rf"┌[─ ]*Thread · {re.escape(token)}[─ ]*┐")
    top = right = left = None
    for index, row in enumerate(rows):
        found = pattern.search(row)
        if found:
            top, left, right = index, found.start(), found.end() - 1
            break
    assert top is not None, f"title row `Thread · {token}` missing or torn:\n{screen}"
    inner: list[str] = []
    for row in rows[top + 1:]:
        if len(row) > right and row[left] == "└" and row[right] == "┘":
            return inner
        assert len(row) > right and row[left] == "│" and row[right] != " ", (
            f"torn frame row {row!r}:\n{screen}"
        )
        inner.append(row[left + 1:right].rstrip())
    raise AssertionError(f"bottom border missing (torn frame):\n{screen}")


def header_rows(inner: list[str], item: str = ITEM, token: str = TOKEN) -> list[str]:
    """The pinned header: identity row, type token row, optional thread root row."""
    assert inner and re.fullmatch(
        rf"\s*item actions · {re.escape(item)}(?: · peer-tainted)?", inner[0]
    ), f"identity row `item actions · {item}` missing as the first inner row: {inner[:3]}"
    assert len(inner) > 1 and re.fullmatch(
        rf"\s*{re.escape(token)}(?: · peer-tainted)?", inner[1]
    ), f"type token row `{token}` missing as the second inner row: {inner[:3]}"
    count = 3 if len(inner) > 2 and inner[2].strip().startswith("thread root ·") else 2
    return [row.strip() for row in inner[:count]]


def body_rows(inner: list[str], item: str = ITEM, token: str = TOKEN) -> list[str]:
    return inner[len(header_rows(inner, item, token)):]


def rail_text(inner: list[str]) -> str:
    return normalized(inner[-RAIL_SPAN:])


def assert_thread_frame(screen: str, item: str = ITEM, token: str = TOKEN, mode: str = "local") -> list[str]:
    """Whole-frame predicate. ``mode`` is ``local`` (LocalOnly footer), ``writer``
    (attached writer rail) or ``readonly`` (attached observer). Returns the inner rows."""
    inner = frame_inner(screen, token)
    header_rows(inner, item, token)
    rail = rail_text(inner)
    whole = normalized(inner)
    assert "[PgUp/PgDn] Scroll" in rail and "[Esc] Close" in rail, f"scroll/close rail missing: {rail!r}"
    for key in PAGE_KEYS:
        assert key not in whole, f"{key} painted without an earlier/later page: {whole!r}"
    if mode == "local":
        # A footer sentence wraps mid-word at the frame width: compare without whitespace.
        assert LOCAL_FOOTER.replace(" ", "") in rail.replace(" ", ""), f"LocalOnly footer missing: {rail!r}"
        assert READONLY_FOOTER not in whole
        for key in MUTATION_KEYS:
            assert key not in whole, f"mutation key {key} leaked in the local frame: {whole!r}"
    elif mode == "readonly":
        assert READONLY_FOOTER.replace(" ", "") in rail.replace(" ", ""), f"read-only footer missing: {rail!r}"
        assert LOCAL_FOOTER not in whole
        for key in MUTATION_KEYS:
            assert key not in whole, f"mutation key {key} leaked in the read-only frame: {whole!r}"
    elif mode == "writer":
        for key in WRITER_KEYS:
            assert key in rail, f"writer rail item {key} missing: {rail!r}"
        assert LOCAL_FOOTER not in whole and READONLY_FOOTER not in whole
    else:  # pragma: no cover - programming error
        raise ValueError(mode)
    return inner


def assert_escape_body(inner: list[str], item: str = ITEM, token: str = TOKEN) -> None:
    body = body_rows(inner, item, token)
    assert any(ESCAPE_LITERAL in row for row in body), f"no visible {ESCAPE_LITERAL} body row: {body}"


def thread_closed(screen: str, token: str = TOKEN) -> bool:
    return f"Thread · {token}" not in screen and "item actions ·" not in screen


# ── Synthetic screens (what ratatui paints: box characters, side borders) ────

SYNTH_SIZES = [(60, 16), (80, 24), (130, 30)]
SYNTH_BODY = [
    "  ri_smoke_adr · received",
    "      # ADR-smoke: thread inspector",
    "      [docs](https://e.example/x)",
    "      raw esc: \\u{1b}[31mred\\u{1b}[0m",
    "      bidi: \\u{202e}evil",
    "      [y] Approve",
    "      ```",
    "      " + "c" * 40,
    "      ```",
] + [f"      line {n}" for n in range(1, 61)]


def synth_screen(cols: int, rows: int, *, mode: str = "local", frame: bool = True,
                 scroll: int = 0, toast: str = "", footer: str | None = None,
                 rail_extra: str = "", writer_keys: tuple = WRITER_KEYS) -> str:
    off = 0 if cols < 100 else 8
    width = cols - 2 * off
    inner_w = width - 2
    stage_h = rows - 5  # the strip row, the status row and the three composer rows follow
    pad = " " * off

    def boxed(text: str) -> str:
        return pad + "│" + text.ljust(inner_w) + "│"

    if not frame:
        stage = [f"  stage row {n}".ljust(cols) for n in range(stage_h)]
    else:
        footer_text = {"local": LOCAL_FOOTER, "readonly": READONLY_FOOTER, "writer": ""}[mode]
        footer_text = footer_text if footer is None else footer
        footer_rows = [" " + part for part in textwrap.wrap(footer_text, inner_w - 2)] if footer_text else []
        rail = "  ".join(writer_keys) if mode == "writer" else ""
        rail_items = (rail + "  " if rail else "") + (rail_extra + "  " if rail_extra else "")
        rail_items += "[PgUp/PgDn] Scroll  [Esc] Close"
        rail_rows = [" " + part for part in textwrap.wrap(rail_items, inner_w - 2)]
        header = [f"item actions · {ITEM}", TOKEN]
        body_h = stage_h - 2 - len(header) - len(footer_rows) - len(rail_rows)
        assert body_h >= 3, (cols, rows, body_h)
        title = f" Thread · {TOKEN} "
        stage = [pad + "┌─" + title + "─" * (width - 2 - 1 - len(title)) + "┐"]
        stage += [boxed(" " + row) for row in header]
        window = SYNTH_BODY[scroll:scroll + body_h]
        stage += [boxed(row) for row in window + [""] * (body_h - len(window))]
        stage += [boxed(row) for row in footer_rows + rail_rows]
        stage.append(pad + "└" + "─" * inner_w + "┘")
        stage = [row.ljust(cols) for row in stage]
    if toast:
        # a toast is painted over the last Stage row (the one `stage_rows` leaves out)
        stage[-1] = toast.ljust(cols)
    bottom = [
        "^X s Mission Control".ljust(cols),
        "Ready │ model-x".ljust(cols),
        "┌" + "─" * (cols - 2) + "┐",
        "│" + " " * (cols - 2) + "│",
        "└" + "─" * (cols - 2) + "┘",
    ]
    return "\n".join(stage + bottom)


def _cut_row(screen: str, index: int, replacement: str = "") -> str:
    rows = screen.split("\n")
    rows[index] = replacement.ljust(len(rows[index]))
    return "\n".join(rows)


def _row_index(screen: str, needle: str) -> int:
    return next(i for i, row in enumerate(screen.split("\n")) if needle in row)


def _rejected(screen: str, **kwargs) -> bool:
    try:
        assert_thread_frame(screen, **kwargs)
    except AssertionError:
        return True
    return False


# ── Desk checks ──────────────────────────────────────────────────────────────


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
@pytest.mark.parametrize("mode", ["local", "writer", "readonly"])
def test_leg_a_desk_accepts_whole_frames(cols, rows, mode):
    screen = synth_screen(cols, rows, mode=mode)
    inner = assert_thread_frame(screen, mode=mode)
    assert_escape_body(inner)
    assert header_rows(inner) == [f"item actions · {ITEM}", TOKEN]


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_a_desk_rejects_torn_frames(cols, rows):
    local = synth_screen(cols, rows, mode="local")
    assert not _rejected(local, mode="local"), "positive control"
    lines = local.split("\n")
    top = _row_index(local, f"Thread · {TOKEN}")
    # title row cut before its right corner
    cut = lines[top][: lines[top].index("Thread · ") + len("Thread · archit")]
    assert _rejected(_cut_row(local, top, cut), mode="local")
    # title row without its title (plain border)
    assert _rejected(local.replace(f" Thread · {TOKEN} ", "─" * (len(TOKEN) + 10)), mode="local")
    # title of another type
    assert _rejected(local, mode="local", token="unknown type")
    # footer cut: the frame is intact but carries no LocalOnly footer
    assert _rejected(synth_screen(cols, rows, mode="local", footer=""), mode="local")
    # bottom border cut
    frame_bottom = next(i for i, row in enumerate(lines) if i > top and row.lstrip().startswith("└"))
    assert _rejected(_cut_row(local, frame_bottom), mode="local")
    # a side border torn on a body row
    torn = lines[top + 4]
    assert _rejected(_cut_row(local, top + 4, torn.rstrip()[:-1]), mode="local")
    # a mutation key leaks into the LocalOnly rail
    assert _rejected(synth_screen(cols, rows, mode="local", rail_extra="[a] Acknowledge"), mode="local")
    # writer rail without `[u] Use as context`; read-only rail leaking `[d] Discuss`
    writer = synth_screen(cols, rows, mode="writer")
    assert not _rejected(writer, mode="writer"), "positive control"
    assert _rejected(synth_screen(cols, rows, mode="writer", writer_keys=WRITER_KEYS[:2]), mode="writer")
    assert not _rejected(synth_screen(cols, rows, mode="readonly"), mode="readonly"), "positive control"
    assert _rejected(synth_screen(cols, rows, mode="readonly", rail_extra="[d] Discuss"), mode="readonly")
    # a local frame must not pass as an attached writer frame, nor the converse
    assert _rejected(local, mode="writer") and _rejected(writer, mode="local")


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_a_desk_scroll_stage_and_composer_predicates(cols, rows):
    closed = synth_screen(cols, rows, frame=False)
    open_ = synth_screen(cols, rows)
    scrolled = synth_screen(cols, rows, scroll=2)
    first, second = frame_inner(open_), frame_inner(scrolled)
    assert header_rows(first) == header_rows(second), "the header is pinned"
    assert body_rows(first) != body_rows(second), "PgDn changes the body rows"
    assert stage_rows(open_) != stage_rows(closed), "the frame replaces Stage rows while open"
    # a toast on the row above the status never breaks Stage equality ...
    assert stage_rows(synth_screen(cols, rows, frame=False, toast="  Info something")) == stage_rows(closed)
    # ... but a Stage row change does, and a typed composer is not the capture's.
    stale = closed.replace("stage row 0", "stage row X")
    assert stage_rows(stale) != stage_rows(closed)
    # the last Stage row (the frame's bottom border while open) is part of the capture
    for screen in (closed, open_):
        cut = screen.split("\n")
        cut[len(stage_rows(screen)) - 1] = "changed".ljust(cols)
        assert stage_rows("\n".join(cut)) != stage_rows(screen)
    typed = closed.replace("│" + " " * (cols - 2) + "│", "│" + "/team view".ljust(cols - 2) + "│")
    assert composer_row(typed) != composer_row(closed)
    assert thread_closed(closed) and not thread_closed(open_)


# ── Live seeding (the 19.16g pattern, local helper) ──────────────────────────


def _journal(workspace: Path) -> Path:
    rooms = sorted((workspace / ".rustain" / "rooms").glob("*.jsonl"))
    assert len(rooms) == 1, f"exactly one room journal expected: {rooms}"
    return rooms[0]


def _create_journal_with_the_writer(workspace: Path, env: dict[str, str]) -> Path:
    """Run the TUI once: its writer composition creates the room journal."""
    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env):
        pass
    return _journal(workspace)


def _append(journal: Path, payload: dict) -> int:
    """Append one room record under the writers' exclusive `flock`."""
    lock = os.open(journal.with_suffix(".lock"), os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        lines = [line for line in journal.read_text().splitlines() if line.strip()]
        seq = json.loads(lines[-1])["seq"] + 1 if lines else 1
        entry = {
            "schema_version": 1,
            "seq": seq,
            "recorded_at_ms": int(time.time() * 1000),
            "record": {"kind": "room", "payload": payload},
        }
        with open(journal, "a", encoding="utf-8") as out:
            out.write(json.dumps(entry, separators=(",", ":"), ensure_ascii=False) + "\n")
            out.flush()
            os.fsync(out.fileno())
        return seq
    finally:
        os.close(lock)


def smoke_body() -> str:
    return "\n".join([
        "# Heading",
        "",
        "[docs](https://e.example/x)",
        "",
        "raw esc: \x1b[31mred\x1b[0m",
        "",
        "bidi: \u202eevil",
        "",
        "[y] Approve",
        "",
        "viewport prose",
        "",
        "```",
        "0123456789" * 20,
        "```",
        "",
        *[f"line {n}\n" for n in range(1, 61)],
    ])


def received_item(item: str = ITEM, body: str | None = None) -> dict:
    """`RoomEvent::RecipientItemReceived` as the production writer serializes it
    (`unknown` authorship/authorization, absent selection/stories/in_reply_to/
    capability are skipped by serde)."""
    return {
        "event": "recipient_item_received",
        "address": {"principal": {"kind": "a2a_pseudonym", "peer": PEER}, "item": item},
        "task": "smoke-task",
        "alias": "smoke-peer",
        "message_type": TOKEN,
        "content": smoke_body() if body is None else body,
    }


def _seed(workspace: Path, env: dict[str, str]) -> Path:
    journal = _create_journal_with_the_writer(workspace, env)
    _append(journal, received_item())
    return journal


# ── Live helpers ─────────────────────────────────────────────────────────────

_EVIDENCE = {"leg": "", "step": 0}


def _evidence_dir() -> Path | None:
    raw = os.environ.get("RUSTAIN_21_8_EVIDENCE_DIR")
    if not raw:
        return None
    path = Path(raw)
    path.mkdir(parents=True, exist_ok=True)
    return path


def _begin_leg(name: str) -> None:
    _EVIDENCE["leg"], _EVIDENCE["step"] = name, 0


def _save_screen(label: str, screen: str) -> None:
    """Keep one asserted beat as ``<dir>/<test>-<NN>-<label>.txt``; no-op when the env is unset."""
    target = _evidence_dir()
    if target is None or not _EVIDENCE["leg"]:
        return
    _EVIDENCE["step"] += 1
    slug = re.sub(r"[^A-Za-z0-9]+", "-", label).strip("-")[:60] or "step"
    (target / f"{_EVIDENCE['leg']}-{_EVIDENCE['step']:02d}-{slug}.txt").write_text(screen)


def _save_file(label: str, source: Path) -> None:
    """Keep a raw log (the harness exposes none for the PTY; the app/daemon logs are the raw record)."""
    target = _evidence_dir()
    if target is None or not _EVIDENCE["leg"] or not source.is_file():
        return
    shutil.copyfile(source, target / f"{_EVIDENCE['leg']}-{label}.log")


def _save_final(tui: RustainTUI | None, label: str) -> None:
    """Final screen of a client (and its isolated app log) when it is still readable."""
    if tui is None or _evidence_dir() is None:
        return
    try:
        _save_screen(f"final-{label}", tui.get_screen_text())
    except Exception as error:  # the process may already be gone: record that fact
        _save_screen(f"final-{label}", f"<screen unavailable: {error}>")
    try:
        from datetime import date

        _save_file(f"{label}-app", tui.wp / f"rustain.log.{date.today()}")
    except Exception:
        pass


def _settle(tui: RustainTUI, predicate, label: str, timeout: float = 15.0) -> str:
    """Wait for ``predicate(screen)``, then for two consecutive identical reads."""
    deadline = time.monotonic() + timeout
    screen = tui.wait_until(predicate, label, timeout)
    while True:
        again = tui.get_screen_text()
        if again == screen and predicate(again):
            _save_screen(label, again)
            return again
        assert time.monotonic() < deadline, f"{label} never settled:\n{again}"
        screen = again


def _frame_ready(mode: str):
    def predicate(screen: str) -> bool:
        try:
            assert_thread_frame(screen, mode=mode)
        except AssertionError:
            return False
        return True

    return predicate


def _type_and_run(tui: RustainTUI, command: str) -> None:
    tui.send(command)
    tui.wait_until(lambda s: command in s, f"`{command}` typed", 10.0)
    tui.send(ENTER)


def _open_view(tui: RustainTUI, mode: str) -> list[str]:
    _type_and_run(tui, f"/team view {ITEM}")
    screen = _settle(tui, _frame_ready(mode), f"the {mode} Thread frame")
    return assert_thread_frame(screen, mode=mode)


def _scroll(tui: RustainTUI, before: list[str], mode: str) -> list[str]:
    tui.send(PGDN)
    tui.send(PGDN)
    screen = _settle(
        tui,
        lambda s: _frame_ready(mode)(s) and body_rows(frame_inner(s)) != body_rows(before),
        "PgDn x2 scrolling the body",
    )
    after = assert_thread_frame(screen, mode=mode)
    assert header_rows(after) == header_rows(before), "the header never scrolls"
    return after


def _turn_rows(stub: SceneStub) -> list[dict]:
    return [row for row in stub.rows() if "turn" in row]


def _small_terminal(monkeypatch) -> Path:
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    binary = _resolve_binary()[0]
    assert binary.is_file(), f"binary missing: {binary} (build it or set RUSTAIN_TUI_BINARY)"
    return binary


def _try_start(client: RustainTUI) -> bool:
    """Start the attach client; True once it paints an attached screen, False if it died.

    Bounded predicate (child liveness + attached/read-only screen), no fixed settle wait;
    ``get_screen_text`` paces the loop by the PTY read.
    """
    try:
        client.start()
    except Exception:
        return False
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        child = client._child
        if child is None or not child.isalive():
            break
        try:
            screen = client.get_screen_text()
        except Exception:
            break
        if "attached" in screen or "read-only" in screen:
            return True
    client.stop()
    return False


def _wait_for(predicate, timeout: float = 30.0, interval: float = 0.2):
    deadline = time.monotonic() + timeout
    value = predicate()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = predicate()
    return value


@pytest.fixture
def stub(tmp_path):
    instance = SceneStub(SCENE, tmp_path / "requests.jsonl").start()
    try:
        yield instance
    finally:
        assert instance.stop() == 0, instance.rows()


@pytest.fixture
def scene_env(stub, tmp_path) -> dict[str, str]:
    home = tmp_path / "home"
    home.mkdir()
    return stub.env("scene-ci", home)


# ── Leg C: sender refusal and the transparency log ────────────────────────────

def _c_frame(screen: str, filtered: bool = False) -> list[str]:
    """Extract a complete log frame, rejecting partial title, sides or bottom."""
    title = "Mission Control · Messages · filtered" if filtered else "Mission Control · Messages"
    pattern = re.compile(r"┌[─ ]*" + re.escape(title) + r"[─ ]*┐")
    rows = screen.split("\n")
    for index, row in enumerate(rows):
        found = pattern.search(row)
        if found:
            left, right = found.start(), found.end() - 1
            break
    else:
        raise AssertionError("missing or torn log title")
    inner = []
    for row in rows[index + 1:]:
        if len(row) > right and row[left] == "└" and row[right] == "┘":
            return inner
        assert len(row) > right and row[left] == "│" and row[right] == "│", "torn log side"
        inner.append(row[left + 1:right].rstrip())
    raise AssertionError("missing log bottom")


def _c_log_ready(screen: str, *, filtered: bool = False, reminder: bool = False) -> bool:
    try:
        inner = _c_frame(screen, filtered)
        status = harness.status_row(screen)
        assert status and bool(re.search(r"\blog: \d+\+?", status)) == reminder
        assert any(re.fullmatch(r"\s*(?:▶ )?.*\S.*", row) for row in inner)
        assert "showing the 20 most recent" not in screen
        return True
    except AssertionError:
        return False


def _c_timestamp(index: int) -> str:
    from datetime import datetime, timezone

    return datetime.fromtimestamp(1_700_000_000 + index, timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")


def _c_row_visible(inner: list[str], index: int) -> bool:
    # Match an entire painted line, not a substring elsewhere on the screen.
    pattern = r"\s*(?:▶ )?" + re.escape(_c_timestamp(index)) + r" .*\S\s*"
    return any(re.fullmatch(pattern, row) for row in inner)


def _c_warning(screen: str) -> bool:
    return any(
        re.fullmatch(r".*\bWarning\b.*needs the.*a2a.*feature.*", row)
        for row in harness.toast_rows(screen)
    )


def _c_synthetic(cols: int, rows: int, *, filtered: bool = False, reminder: bool = False) -> str:
    title = " Mission Control · Messages · filtered " if filtered else " Mission Control · Messages "
    width = cols - 2
    inner = [f"  {_c_timestamp(0)} ↑ outbound ✗ refused",
             "  — append-only, crash-safe"]
    inner += [""] * (rows - 7 - len(inner))
    return "\n".join([
        "┌─" + title + "─" * (width - 1 - len(title)) + "┐",
        *["│" + row.ljust(width) + "│" for row in inner],
        "└" + "─" * width + "┘",
        "".ljust(cols),
        ("Ready │ model-x" + (" log: 25" if reminder else "")).ljust(cols),
        "┌" + "─" * width + "┐",
        "│" + " " * width + "│",
        "└" + "─" * width + "┘",
    ])


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_c_desk_log_frames_and_reminder(cols, rows):
    unfiltered = _c_synthetic(cols, rows)
    assert _c_log_ready(unfiltered)
    assert _c_row_visible(_c_frame(unfiltered), 0)
    assert not _c_log_ready(unfiltered, reminder=True)
    assert not _c_log_ready(_cut_row(unfiltered, 1))
    assert not _c_log_ready(_cut_row(unfiltered, rows - 6))
    filtered = _c_synthetic(cols, rows, filtered=True, reminder=True)
    assert _c_log_ready(filtered, filtered=True, reminder=True)
    assert not _c_log_ready(filtered, filtered=True)
    assert not _c_log_ready(_cut_row(filtered, 0), filtered=True, reminder=True)
    assert not _c_log_ready(
        filtered.replace("Mission Control · Messages · filtered", "Mission Control · Messages · filtere "),
        filtered=True, reminder=True)
    assert not _c_row_visible(_c_frame(unfiltered), 1)


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_c_desk_warning_and_stage(cols, rows):
    base = synth_screen(cols, rows, frame=False)
    warning = synth_screen(cols, rows, frame=False,
                           toast="Warning needs the `a2a` feature")
    assert _c_warning(warning) and stage_rows(warning) == stage_rows(base)
    assert not _c_warning(base)
    assert not _c_warning(warning.replace("feature", "featur "))
    assert not _c_warning(warning.replace("Warning", "Info   "))


# ── Live legs ────────────────────────────────────────────────────────────────


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_a_local(tmp_path, stub, scene_env, monkeypatch):
    _small_terminal(monkeypatch)
    _begin_leg("test_leg_a_local")
    workspace = tmp_path / "ws"
    workspace.mkdir()
    _seed(workspace, scene_env)

    with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=scene_env) as tui:
        base = _settle(tui, lambda s: composer_top(screen_rows(s)) is not None and "Ready" in s, "the idle Stage")
        base_stage, base_composer = stage_rows(base), composer_row(base)

        inner = _open_view(tui, "local")
        assert_escape_body(inner)
        opened = frame_inner(tui.get_screen_text())

        # Ctrl+X n: S-LOG over the frame; Esc returns to the identical rows.
        tui.send(CTRL_X)
        tui.wait_until(
            lambda s: any(r.strip().startswith("┌") and "Ctrl+X" in r for r in s.split("\n")),
            "the Ctrl+X leader menu", 10.0)
        tui.send("n")
        # Over the thread the S-LOG title is a breadcrumb: `Thread · <type> › System · Notifications`.
        _settle(tui, lambda s: "› System · Notifications" in s, "S-LOG over the Thread frame")
        tui.send(ESC)
        screen = _settle(
            tui,
            lambda s: "System · Notifications" not in s and _frame_ready("local")(s) and frame_inner(s) == opened,
            "the identical Thread rows after S-LOG closes",
        )
        assert frame_inner(screen) == opened

        scrolled = _scroll(tui, opened, "local")
        assert body_rows(scrolled) != body_rows(opened)

        tui.send(ESC)
        closed = _settle(
            tui,
            lambda s: thread_closed(s) and composer_top(screen_rows(s)) is not None
            and stage_rows(s) == base_stage,
            "the Stage restored after Esc",
        )
        assert stage_rows(closed) == base_stage
        assert composer_row(closed) == base_composer, "the frame never writes the composer"
        assert "/team view" not in composer_row(closed)
        _save_final(tui, "client")
    assert _turn_rows(stub) == [], "viewing never reaches the model"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_a_attached(tmp_path, stub, scene_env, monkeypatch):
    binary = _small_terminal(monkeypatch)
    _begin_leg("test_leg_a_attached")
    workspace = tmp_path / "ws"
    workspace.mkdir()
    _seed(workspace, scene_env)

    # A Unix socket path is capped near 108 bytes: keep the daemon's data
    # directory (which holds its socket) short, and share it with the clients.
    data_dir = Path(tempfile.mkdtemp(prefix="r21-8-", dir="/tmp"))
    scene_env = {**scene_env, "RUSTAIN_DATA_DIR": str(data_dir)}
    env = _load_env()
    env["RUSTAIN_CONFIG_DIR"] = str(workspace / ".rustain")
    env["RUSTAIN_LOG_PATH"] = str(workspace / "daemon-run.log")
    env.update(scene_env)
    started = subprocess.run(
        [str(binary), "daemon", "start"],
        cwd=workspace, env=env, capture_output=True, text=True, timeout=60,
    )
    assert started.returncode == 0, (started.stdout, started.stderr)
    writer = reader = None
    try:
        writer = RustainTUI(
            fresh=False, build=False, workspace=workspace,
            env_overrides=scene_env, extra_args=["daemon", "attach"],
        )
        assert _wait_for(lambda: _try_start(writer), timeout=30), "attach never connected"
        assert writer.wait_for_screen("attached", timeout=10), writer.get_screen_text()
        base = _settle(writer, lambda s: composer_top(screen_rows(s)) is not None, "the attached Stage")
        base_composer = composer_row(base)

        opened = _open_view(writer, "writer")
        assert_escape_body(opened)
        scrolled = _scroll(writer, opened, "writer")
        assert body_rows(scrolled) != body_rows(opened)
        writer.send(ESC)
        closed = _settle(writer, thread_closed, "the writer frame closed")
        assert composer_row(closed) == base_composer, "the frame never writes the composer"

        # A second, read-only client: the read-only footer, no mutation key.
        reader = RustainTUI(
            fresh=False, build=False, workspace=workspace,
            env_overrides=scene_env, extra_args=["daemon", "attach"],
        ).start()
        assert reader.wait_for_screen("read-only", timeout=10), reader.get_screen_text()
        read_only = _open_view(reader, "readonly")
        assert_escape_body(read_only)
        reader.send(ESC)
        _settle(reader, thread_closed, "the read-only frame closed")
        assert _turn_rows(stub) == [], "viewing never reaches the model"
    finally:
        _save_final(writer, "writer")
        _save_final(reader, "reader")
        _save_file("daemon-run", workspace / "daemon-run.log")
        for client in (reader, writer):
            if client is not None:
                try:
                    client.send(ESC)
                except Exception:
                    pass
                client.stop()
        subprocess.run(
            [str(binary), "daemon", "stop"],
            cwd=workspace, env=env, capture_output=True, timeout=60,
        )
        shutil.rmtree(data_dir, ignore_errors=True)


# ── Leg B: pure screen predicates and offline desk checks ─────────────────────

LINK_RENDERED = "docs (https://e.example/x)"
LINK_SOURCE = "[docs](https://e.example/x)"


def _b_inner(screen: str) -> list[str]:
    inner = frame_inner(screen)
    header_rows(inner)
    return inner


def _b_row(inner: list[str], text: str, *, gutter: bool = True) -> bool:
    """Match an entire painted row, optionally after a focused-turn marker."""
    prefix = r"(?:▶ )?" + (r" {6,}" if gutter else r" *")
    return any(re.fullmatch(prefix + re.escape(text) + r" *", row) for row in inner)


def _b_rendered(inner: list[str]) -> None:
    assert _b_row(inner, LINK_RENDERED)
    assert _b_row(inner, r"raw esc: \u{1b}[31mred\u{1b}[0m")
    assert _b_row(inner, r"bidi: \u{202e}evil")
    assert _b_row(inner, "[y] Approve")
    assert all(
        "\x1b" not in row and "\u202e" not in row
        and ("[y] Approve" not in row or _b_row([row], "[y] Approve"))
        for row in inner
    )


def _b_source(inner: list[str]) -> None:
    assert _b_row(inner, LINK_SOURCE)
    assert not _b_row(inner, LINK_RENDERED)


def _b_filtered(inner: list[str]) -> None:
    assert _b_row(inner, "ri_filter_40 · received · follow-up to " + ITEM, gutter=False)
    assert _b_row(inner, "line 40")
    assert not any("ri_filter_3 · received" in row for row in inner)
    assert not _b_row(inner, "line 3")


def _b_unfiltered(inner: list[str]) -> None:
    assert _b_row(inner, "ri_filter_3 · received · follow-up to " + ITEM, gutter=False)
    assert _b_row(inner, "line 3")


def _b_block(inner: list[str]) -> tuple[str, str]:
    prose = next(row for row in inner if _b_row([row], "viewport prose"))
    block = next(
        row for row in inner
        if re.fullmatch(r"(?:▶ )? {6,}[│←]?[0-9]{20,}[→│]? *", row)
    )
    return prose, block


def _b_check(screen: str, check) -> bool:
    try:
        check(_b_inner(screen))
        return True
    except (AssertionError, StopIteration):
        return False


def _b_synthetic(cols: int, rows: int, body: list[str]) -> str:
    """A complete frame with a variable body, independent of terminal or process."""
    width = cols - 2
    title = f" Thread · {TOKEN} "
    top = "┌─" + title + "─" * (width - 1 - len(title)) + "┐"
    inner = [f" item actions · {ITEM}", f" {TOKEN}", *body]
    assert len(inner) <= rows - 2
    inner += [""] * (rows - 2 - len(inner))
    return "\n".join([top, *["│" + row.ljust(width) + "│" for row in inner],
                      "└" + "─" * width + "┘"])


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_b_desk_rendered_and_source(cols, rows):
    rendered = _b_synthetic(cols, rows, [
        "      " + LINK_RENDERED,
        r"      raw esc: \u{1b}[31mred\u{1b}[0m",
        r"      bidi: \u{202e}evil",
        "      [y] Approve",
    ])
    assert _b_check(rendered, _b_rendered)
    assert _b_check(_b_synthetic(cols, rows, [
        "▶       " + LINK_RENDERED,
        r"      raw esc: \u{1b}[31mred\u{1b}[0m",
        r"      bidi: \u{202e}evil",
        "      [y] Approve",
    ]), _b_rendered)
    assert not _b_check(rendered.replace(LINK_RENDERED,
                                       "docs (https://e.example/".ljust(len(LINK_RENDERED))), _b_rendered)
    assert not _b_check(_b_synthetic(cols, rows, [
        "      " + LINK_RENDERED,
        r"      raw esc: \u{1b}[31mred\u{1b}[0m",
        r"      bidi: \u{202e}evil",
        " [y] Approve",
    ]), _b_rendered)
    assert not _b_check(_cut_row(rendered, 4), _b_rendered)
    source = _b_synthetic(cols, rows, ["      " + LINK_SOURCE])
    assert _b_check(source, _b_source)
    assert not _b_check(_b_synthetic(cols, rows, ["      " + LINK_SOURCE[:-2]]), _b_source)
    assert not _b_check(_b_synthetic(cols, rows, ["     " + LINK_SOURCE]), _b_source)
    assert not _b_check(_cut_row(source, 3), _b_source)


@pytest.mark.parametrize("cols,rows", SYNTH_SIZES, ids=[f"{c}x{r}" for c, r in SYNTH_SIZES])
def test_leg_b_desk_filter_and_viewport(cols, rows):
    filtered = _b_synthetic(cols, rows, [
        "  ri_filter_40 · received · follow-up to " + ITEM, "      line 40",
    ])
    assert _b_check(filtered, _b_filtered)
    assert not _b_check(filtered.replace("line 40", "line 4"), _b_filtered)
    assert not _b_check(_cut_row(filtered, 3), _b_filtered)
    before = _b_synthetic(cols, rows, ["      viewport prose", "      │012345678901234567890123456789→"])
    after = _b_synthetic(cols, rows, ["      viewport prose", "      ←123456789012345678901234567890→"])
    prose, block = _b_block(_b_inner(before))
    shifted_prose, shifted_block = _b_block(_b_inner(after))
    assert prose == shifted_prose and block != shifted_block
    assert not _b_check(_cut_row(before, 4), _b_block)
    assert not _b_check(_b_synthetic(cols, rows, [
        "     viewport prose", "      │012345678901234567890123456789→",
    ]), _b_block)


def _b_seek(tui: RustainTUI, check, label: str, key: str = "j") -> str:
    """Scroll a row at a time (`key`), waiting for each repaint, until the beat is visible."""
    screen = _settle(tui, lambda s: _b_check(s, lambda inner: None), label + " frame")
    for _ in range(400):
        if _b_check(screen, check):
            _save_screen(label, screen)
            return screen
        old = screen
        tui.send(key)
        screen = _settle(tui, lambda s: s != old and _b_check(s, lambda inner: None),
                         label + " scrolling")
    raise AssertionError(f"{label} not found:\n{screen}")


def test_leg_b_desk_seek_up_reaches_a_header_below_a_long_body(monkeypatch):
    """Replay of `_b_seek` over scripted frames: the follow-up header sits below ~120 body
    rows, so a seek from the end upward (`k`) reaches it; a torn frame is never accepted."""
    header = "  ri_filter_3 · received · follow-up to " + ITEM
    frames = [_b_synthetic(80, 24, ["      line 99", "", "      line 98"]),
              _b_synthetic(80, 24, ["      line 97", "", "      line 96"]),
              _b_synthetic(80, 24, [header, "      line 3"])]
    sent = []

    class Fake:
        def send(self, key):
            sent.append(key)

    queue = list(frames)
    monkeypatch.setattr(sys.modules[__name__], "_settle",
                        lambda tui, predicate, label, timeout=15.0: next(
                            s for s in iter(lambda: queue.pop(0), None) if predicate(s)))
    monkeypatch.setattr(sys.modules[__name__], "_save_screen", lambda label, screen: None)

    def header_3(inner):
        assert _b_row(inner, "ri_filter_3 · received · follow-up to " + ITEM, gutter=False)

    found = _b_seek(Fake(), header_3, "replay", key="k")
    assert header in found and sent == ["k", "k"]
    torn = _cut_row(frames[2], 3)
    assert not _b_check(torn, header_3), "a torn frame is not accepted as the header"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_b(tmp_path, stub, scene_env, monkeypatch):
    _small_terminal(monkeypatch)
    _begin_leg("test_leg_b")
    workspace = tmp_path / "ws"
    workspace.mkdir()
    journal = _seed(workspace, scene_env)
    # Separate follow-up turns expose filtering without pretending it removes
    # individual lines from the pinned opened item.
    for item, body in (("ri_filter_3", "line 3"), ("ri_filter_40", "line 40")):
        event = received_item(item, body)
        event["in_reply_to"] = ITEM
        _append(journal, event)

    with tempfile.TemporaryDirectory(prefix="r21b-", dir="/tmp") as data:
        env = {**scene_env, "RUSTAIN_CLIPBOARD": "file", "RUSTAIN_DATA_DIR": data}
        clipboard = Path(data) / "clipboard.txt"
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui:
            base = _settle(tui, lambda s: composer_top(screen_rows(s)) is not None and "Ready" in s,
                           "the idle Stage")
            base_stage = stage_rows(base)
            _open_view(tui, "local")
            for text in (LINK_RENDERED, r"raw esc: \u{1b}[31mred\u{1b}[0m",
                         r"bidi: \u{202e}evil", "[y] Approve"):
                def row_present(inner, text=text):
                    assert _b_row(inner, text)
                    assert all("\x1b" not in row and "\u202e" not in row for row in inner)
                    if text == "[y] Approve":
                        assert all("[y] Approve" not in row or _b_row([row], text) for row in inner)
                _b_seek(tui, row_present, "rendered " + text)
            tui.send("g")
            _settle(tui, lambda s: _b_check(s, lambda inner: None)
                    and "Context received:" in s, "return to thread top")
            tui.send("v")
            _b_seek(tui, _b_source, "Source literal link")
            tui.send("v")
            def rendered_link(inner):
                assert _b_row(inner, LINK_RENDERED)
            _b_seek(tui, rendered_link, "Rendered link restored")
            tui.send("c")
            # The toast sits on a Stage row that may be cut or wrapped; the file is the proof.
            _settle(tui, lambda s: clipboard.exists()
                    and clipboard.read_bytes() == smoke_body().encode("utf-8"),
                    "file clipboard written")
            assert clipboard.read_bytes() == smoke_body().encode("utf-8")
            tui.send(ESC)
            closed = _settle(tui, lambda s: thread_closed(s) and stage_rows(s) == base_stage,
                             "Stage restored after copy")
            assert stage_rows(closed) == base_stage
            _save_final(tui, "copy-client")

        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui:
            _settle(tui, lambda s: composer_top(screen_rows(s)) is not None and "Ready" in s,
                    "fresh idle Stage")
            _open_view(tui, "local")
            def header_3(inner):
                assert _b_row(inner, "ri_filter_3 · received · follow-up to " + ITEM, gutter=False)

            def header_40(inner):
                assert _b_row(inner, "ri_filter_40 · received · follow-up to " + ITEM, gutter=False)

            # The filter keeps whole turns (the opened item always stays); the follow-up turn
            # `line 3` is found by scrolling, then dropped by the query `line 40`.
            tui.send("G")
            _b_seek(tui, header_3, "unfiltered follow-up", key="k")
            tui.send("g")
            tui.send("/")
            tui.send("line 40")
            _settle(tui, lambda s: "line 40" in s and "Keep filter" in s, "search field")
            tui.send(ENTER)
            _settle(tui, lambda s: "Keep filter" not in s and "/ line 40" in s and "[Esc] Close" in s,
                    "filter kept, rail restored")
            tui.send("G")
            filtered = _b_seek(tui, header_40, "matching follow-up", key="k")
            assert header_rows(_b_inner(filtered)) == [f"item actions · {ITEM}", TOKEN]
            tui.send(ESC)
            # The rail grows back to its full height on the repaint; G before it clamps against the
            # previous (field-open) viewport (attempt 4's race), so wait for the settled frame.
            _settle(tui, lambda s: "/ line 40" not in s and "[Esc] Close" in s, "filter cleared")
            tui.send("G")
            _b_seek(tui, header_3, "cleared filter restores follow-up", key="k")
            tui.send("g")
            _settle(tui, lambda s: _b_check(s, lambda inner: None)
                    and "Context received:" in s, "return to wide-block start")
            _b_seek(tui, _b_block, "wide block and prose")
            tui.send(ENTER)
            viewport = _settle(tui, lambda s: _b_check(s, _b_block) and "Columns" in s and "Leave" in s,
                               "block viewport")
            prose, block = _b_block(_b_inner(viewport))
            tui.send("l")
            shifted = _settle(tui, lambda s: _b_check(s, _b_block)
                              and _b_block(_b_inner(s))[0] == prose
                              and _b_block(_b_inner(s))[1] != block,
                              "only the wide block shifts")
            assert _b_block(_b_inner(shifted))[0] == prose
            tui.send(ESC)
            _settle(tui, _frame_ready("local"), "leave block viewport")
            tui.send(ESC)
            _settle(tui, thread_closed, "leave thread")
            _save_final(tui, "filter-client")
    assert _turn_rows(stub) == [], "viewing never reaches the model"


def _c_seed_log(workspace: Path, env: dict[str, str]) -> None:
    """Create a real writer journal, then append 25 fixture-shaped rows under its lock."""
    journal = _create_journal_with_the_writer(workspace, env)
    lock = os.open(journal.with_suffix(".lock"), os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        lines = [line for line in journal.read_text().splitlines() if line.strip()]
        seq = json.loads(lines[-1])["seq"] + 1 if lines else 1
        with open(journal, "a", encoding="utf-8") as out:
            for index in range(25):
                entry = {
                    "schema_version": 1, "seq": seq + index,
                    "recorded_at_ms": 1_700_000_000_000 + index * 1000,
                    "record": {"kind": "room", "payload": {
                        "event": "remote_envelope_rejected", "peer": PEER,
                        "reason": {"reason": "policy", "detail": f"smoke {index:02d}"},
                        "direction": "inbound", "task": f"smoke-{index:02d}",
                    }},
                }
                out.write(json.dumps(entry, separators=(",", ":")) + "\n")
            out.flush()
            os.fsync(out.fileno())
    finally:
        os.close(lock)


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_c(tmp_path, stub, scene_env, monkeypatch):
    from contextlib import ExitStack

    _small_terminal(monkeypatch)
    _begin_leg("test_leg_c")
    with ExitStack() as stack:
        def launch(name, workspace):
            tui = stack.enter_context(RustainTUI(fresh=True, build=False, workspace=workspace,
                                                env_overrides=scene_env))
            evidence = _evidence_dir()
            if evidence is not None:
                terminal = stack.enter_context((evidence / f"test_leg_c-{name}-terminal.txt").open("w"))
                tui.child.logfile_read = terminal
                stack.callback(setattr, tui.child, "logfile_read", None)
            return tui

        workspace = tmp_path / "refusal"
        workspace.mkdir()
        tui = launch("refusal", workspace)
        base = _settle(tui, lambda s: composer_top(screen_rows(s)) is not None and "Ready" in s,
                       "idle Stage before unavailable thread")
        def calm(rows):
            # Toasts sit on Stage rows (21-8 smoke lesson): compare the Stage without them.
            return ["" if re.match(r"\s+(Warning|Info|Error) ", row) else row for row in rows]

        base_stage = calm(stage_rows(base))
        _type_and_run(tui, "/team thread nobody ri_x")
        warning = _settle(tui, _c_warning, "A2A feature Warning")
        assert calm(stage_rows(warning)) == base_stage
        assert not any(re.search(r"┌[─ ]*Thread · sent", row) for row in screen_rows(warning))
        _save_final(tui, "refusal-client")

        workspace = tmp_path / "log"
        workspace.mkdir()
        _c_seed_log(workspace, scene_env)
        log = launch("log", workspace)
        _settle(log, lambda s: bool(re.search(r"\blog: 25\b", harness.status_row(s))),
                "25 unseen transparency rows")
        _type_and_run(log, "/team log")
        top = _settle(log, lambda s: _c_log_ready(s) and _c_row_visible(_c_frame(s), 0),
                      "unfiltered log presented with oldest row")
        seen = {i for i in range(25) if _c_row_visible(_c_frame(top), i)}
        screen = top
        for _ in range(250):
            if len(seen) == 25:
                break
            old = screen
            log.send("j")
            screen = _settle(log, lambda s: s != old and _c_log_ready(s), "scroll log rows")
            seen.update(i for i in range(25) if _c_row_visible(_c_frame(screen), i))
        assert seen == set(range(25)), f"unreachable log rows: {set(range(25)) - seen}"
        log.send("G")
        end = _settle(log, lambda s: _c_log_ready(s) and _c_row_visible(_c_frame(s), 24)
                      and any(re.fullmatch(r"\s*(?:▶ )?.*append-only.*", r) for r in _c_frame(s)),
                      "last log row and append-only claim")
        assert "showing the 20 most recent" not in end
        log.send(ESC)
        _settle(log, lambda s: "Mission Control · Messages" not in s and composer_top(screen_rows(s)) is not None,
                "log closed")
        _save_final(log, "log-client")

        workspace = tmp_path / "filtered"
        workspace.mkdir()
        _c_seed_log(workspace, scene_env)
        filtered = launch("filtered", workspace)
        _settle(filtered, lambda s: bool(re.search(r"\blog: 25\b", harness.status_row(s))),
                "fresh unseen transparency rows")
        _type_and_run(filtered, "/team log --filter=kind=topic-received")
        _settle(filtered, lambda s: _c_log_ready(s, filtered=True, reminder=True),
                "filtered log keeps reminder")
        _save_final(filtered, "filtered-client")
    assert _turn_rows(stub) == [], "thread and log reads never reach the model"
