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
    """Rows above the toast row, which sits above the status row above the composer."""
    rows = screen_rows(screen)
    top = composer_top(rows)
    assert top is not None and top >= 3, f"no composer on screen:\n{screen}"
    return rows[: top - 2]


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
    stage_h = rows - 5
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
    bottom = [
        toast.ljust(cols),
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
        "# ADR-smoke: thread inspector",
        "[docs](https://e.example/x)",
        "raw esc: \x1b[31mred\x1b[0m",
        "bidi: \u202eevil",
        "[y] Approve",
        "```",
        "c" * 200,
        "```",
        *[f"line {n}" for n in range(1, 61)],
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
