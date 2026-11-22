"""agents-thread: one debug-native 80x24 smoke, Agents `t Open thread` and the frame Follow up.

B is a rustain daemon serving A2A with a notify-and-wait policy for every principal (the 19.18b
recipient shape) and one advertised skill; an attached B client grants A's loopback principal
`Always allow` once (the 19.20 consent setup). A is a standalone TUI whose scripted turn calls
the `a2a__recipient__native-review` delegate tool once; the Permission decision holds the turn
while the Main draft `main draft` is typed, so the draft is in the composer before the turn ends.

One journey (AC2, AC3, AC4, AC6-AC10), all from the Agents row: `Ctrl+X s`, the `a2a-peer` row
selected, `[t] Open thread` painted directly after `[Enter] Open`; `t` opens
`Thread · sent · recipient` on B's journaled item for the delegation with zero provider
requests, zero new B items and zero new A dispatch rows. The delegation's dispatch row carries
the hash of the delegated text, so the item is this host's own turn (`you <item>`) and the
frame offers `[f] Follow up`: `f` paints `/team followup recipient <item> `, the text is typed,
Enter sends exactly one follow-up, the frame logs `delivered`, B journals exactly one new item
with `in_reply_to` = the item and A exactly one dispatch row. `Esc` returns to
`Mission Control · Agents` with the row selected; `Esc` restores the composer bytes and caret
exactly as before `Ctrl+X s` (the Main draft was never touched by the field).

Waits are structural (PTY-paced screen predicates and journal predicates). Evidence goes to
`RUSTAIN_CAPTURE_EVIDENCE_DIR` (else the system temp dir); the integrator copies a passing run
to `_bmad-output/implementation-artifacts/agents-thread-smoke/`.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import traceback
import urllib.error
import urllib.request
import time

import pytest
import harness
from fixtures.scene_provider import SceneStub
from keys import ENTER, ESC
from decision_frame_helpers import _frame as _decision_frame
from decision_frame_helpers import defer_frame, open_decision, wait_frame
from test_story_10_4_subagent_panel import _send_chord_ctrl_x_s
from test_story_19_18b_input_required_smoke import _tui, _world
from harness import sha256_file, stop_process_group
from test_story_19_21_act1_distribution import _command, _events, _wait

PEER = "recipient"
SKILL = "native-review"
TOOL = f"a2a__{PEER}__{SKILL}"
PROMPT = "DELEGATE-AGENTS-THREAD"
SETTLED = "DELEGATION-SETTLED"
DELEGATION = "agents thread delegation"
DRAFT = "main draft"
FOLLOW = "follow-up from agents"
MC = "Mission Control · Agents"
THREAD = f"Thread · sent · {PEER}"
USAGE = {"input_tokens": 10, "output_tokens": 10}


def _frame(screen, title):
    """Inner rows of the frame whose title row names `title`, or None while it is torn."""
    rows = screen.split("\n")
    for top, row in enumerate(rows):
        at = row.find(title)
        left, right = row.rfind("┌", 0, max(at, 0)), row.find("┐", max(at, 0))
        if at < 0 or left < 0 or right < 0:
            continue
        for end in range(top + 1, len(rows)):
            if len(rows[end]) > right and rows[end][left] == "└" and rows[end][right] == "┘":
                return [inner[left + 1:right] for inner in rows[top + 1:end]]
        return None
    return None


def _text(inner):
    return " ".join(" ".join(inner or []).replace("│", " ").split())


def _flat(inner):
    """The frame's text with border glyphs and ALL whitespace removed: at 80 columns a receipt
    or notice line wraps mid-word (`… · ● recipient d` / `elivered`), so multi-word matches
    compare against `_squash(expected)` here, never against painted rows."""
    return _squash("".join(inner or []).translate(_BORDER))


_BORDER = str.maketrans("", "", "│┌┐└┘─")


def _squash(text):
    return "".join(text.split())


def _mc_top(screen):
    return THREAD not in screen and _frame(screen, MC) is not None


def _peer_selected(inner):
    return any(re.search(r"▸ .*a2a-peer", row) for row in inner or [])


def _composer(tui, screen):
    """(first composer row, caret cell relative to the composer box's top-left corner, caret
    hidden) of the screen `tui` just painted; a paint without a composer reads as a hidden
    caret on an empty row. Relative, so a Stage that grew between two paints compares equal."""
    rows = screen.split("\n")
    top = harness.composer_top(rows)
    if top is None or top + 1 >= len(rows):
        return "", None, True
    cursor = tui._screen.cursor
    return rows[top + 1].rstrip(), (cursor.x, cursor.y - top), cursor.hidden


def _caret_in_composer(tui, screen):
    """The painted caret is visible and sits inside the composer box (strictly between its
    borders). A paint caught mid-frame leaves the terminal cursor wherever the renderer last
    drew (a spinner or notice cell above the composer) — never a caret to capture or compare."""
    rows = screen.split("\n")
    top = harness.composer_top(rows)
    bottom = max((i for i, line in enumerate(rows) if line.strip().startswith("└")), default=None)
    if top is None or bottom is None:
        return False
    cursor = tui._screen.cursor
    width = len(rows[top + 1])
    return not cursor.hidden and top < cursor.y < bottom and 0 < cursor.x < width - 1


def _field(screen, prompt):
    """The text right of the follow-up field's prompt inside the thread frame, or None."""
    for row in _frame(screen, THREAD) or []:
        if prompt in row:
            return row.split(prompt, 1)[1].strip(" │")
    return None


def _drain(tuis):
    for tui in tuis:
        tui.get_screen_text()


def _until(tui, predicate, label, others=(), timeout=30):
    """Wait for `predicate(screen)` on `tui`, draining the other clients' PTYs meanwhile."""
    def poll():
        _drain(others)
        screen = tui.get_screen_text()
        return screen if predicate(screen) else None
    return _wait(poll, lambda: f"{label}:\n{tui.get_screen_text()}", timeout)


def _journal(predicate, label, tuis, timeout=30):
    """Wait for a journal fact (`predicate()` truthy, returned), paced by both clients' PTYs."""
    def poll():
        _drain(tuis)
        return predicate()
    return _wait(poll, lambda: label + ":\n" + "\n---\n".join(t.get_screen_text() for t in tuis), timeout)


def _received(workspace):
    return _events(workspace, "recipient_item_received")


def _counts(stub, a_ws, b_ws):
    rooms = sorted((b_ws / ".rustain/rooms").glob("*.jsonl"))
    return {
        "provider_turns": len([row for row in stub.rows() if "turn" in row and "kind" not in row]),
        "b_received": len(_received(b_ws)),
        "b_journal_rows": sum(len([line for line in path.read_text().splitlines() if line.strip()])
                              for path in rooms),
        "a_dispatched": len(_events(a_ws, "remote_envelope_dispatched")),
    }


def _daemon(stack, binary, workspace, env, evidence):
    reservation = socket.socket()
    reservation.bind(("127.0.0.1", 0))
    port = reservation.getsockname()[1]
    reservation.close()
    endpoint = f"http://127.0.0.1:{port}/"
    logfile = evidence / "recipient.daemon.log"
    log = stack.enter_context(logfile.open("w"))
    process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground"],
                               cwd=workspace, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    stack.callback(stop_process_group, process)

    def ready():
        assert process.poll() is None, logfile.read_text()
        status = subprocess.run([str(binary), "daemon", "status"], cwd=workspace, env=env,
                                capture_output=True, text=True, timeout=5)
        try:
            with urllib.request.urlopen(endpoint + ".well-known/agent-card.json", timeout=1) as response:
                card = json.load(response)
        except (OSError, urllib.error.URLError, ValueError):
            return None
        running = status.returncode == 0 and "running" in (status.stdout + status.stderr).lower()
        return card if running and any(s.get("id") == SKILL for s in card.get("skills", [])) else None

    card = _wait(ready, logfile.read_text, timeout=45)
    (evidence / "recipient-agent-card.json").write_text(json.dumps(card, indent=2) + "\n")
    return endpoint


def _catalog_ready(evidence):
    return any("A2A AgentCard catalog changed" in line and f"peer_id={PEER}" in line
               for path in evidence.glob("operator.runtime.log*")
               for line in path.read_text(errors="replace").splitlines())


def _reopen_permission(tui, others, timeout=60):
    """Present the deferred Permission decision again from the Needs-you queue.

    `open_decision` asks with Ctrl+X q / Enter only after the whole screen has been unchanged
    for 600 ms; while the deferred tool call waits, the Stage's tool spinner and elapsed time
    repaint every frame, so that gate never opens (first native run). As in the 19.18b
    outbound-permission smoke, ask once the `Needs you (` chip is painted and no frame is up,
    then Enter on the selected Permission row of the list; a key dropped while the list or
    frame is settling is asked again after 2 s. The frame is then waited out structurally
    (`wait_frame`: unchanged frame rows for 600 ms, its inert window)."""
    title, rail = "Decision: Permission", ("[y] Allow",)
    asked = entered = None

    def presented():
        nonlocal asked, entered
        _drain(others)
        screen = tui.get_screen_text()
        if _decision_frame(screen, title, rail):
            return screen
        now = time.monotonic()
        if _decision_frame(screen, "Needs you", ("[Enter] Open",)):
            selected = any(re.match(r"[▸▶] Permission ·", row.strip().lstrip("│ "))
                           for row in screen.splitlines())
            if selected and (entered is None or now - entered >= 2.0):
                tui.send("\r")
                entered = now
        elif ("Decision:" not in screen and "Needs you (" in screen
              and (asked is None or now - asked >= 2.0)):
            tui.send("\x18q")
            asked = now
        return None

    _wait(presented, lambda: f"{title} re-presented:\n{tui.get_screen_text()}", timeout)
    return wait_frame(tui, title, rail, timeout)


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "agents-thread-native", "personas": {
        "operator": {"turns": [
            {"expect": PROMPT, "content": [{"type": "tool_use", "name": TOOL, "input": {"message": DELEGATION}}],
             "stop_reason": "tool_use", "usage": USAGE},
            {"expect": "remote agent requires", "content": [{"type": "text", "text": SETTLED}],
             "stop_reason": "end_turn", "usage": USAGE},
        ]}, "recipient": {"turns": []},
    }}) + "\n")
    facts = {}
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "provider-requests.jsonl"))
        b_ws, b_config, b_env = _world(root, PEER, stub, evidence)
        (b_config / "a2a.json").write_text('{"server":{"admission":"allow"},"agents":{}}\n')
        (b_config / "a2a-interaction.toml").write_text(
            '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
        skill = b_ws / ".agents/skills" / SKILL / "SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text(f"---\nname: {SKILL}\ndescription: Native delegation fixture\n---\nReview the message.\n")
        endpoint = _daemon(stack, binary, b_ws, b_env, evidence)
        b = _tui(stack, binary, b_ws, b_env, evidence, "recipient", attached=True)
        a_ws, a_config, a_env = _world(root, "operator", stub, evidence)
        (a_config / "a2a.json").write_text(json.dumps({"agents": {PEER: {"url": endpoint}}}) + "\n")
        (a_config / "config.toml").write_text("[permissions]\nalways_tools = []\n")
        a = _tui(stack, binary, a_ws, a_env, evidence, "operator")
        _until(a, lambda _screen: _catalog_ready(evidence), "B's AgentCard catalog on A", (b,), 45)

        # Consent once, through a typed send (a pending consent would cancel the delegation).
        _command(a, f"/team send {PEER} consent-setup")
        open_decision(b, "Peer consent", ("[a] Always allow",), timeout=60)
        b.send("a")
        _journal(lambda: any(r["content"] == "consent-setup" for r in _received(b_ws)), "consent item", (a, b), 60)

        # The delegation turn, held by its Permission decision while the Main draft is typed.
        known = {row["address"]["item"] for row in _received(b_ws)}
        _command(a, PROMPT)
        open_decision(a, "Permission", ("[y] Allow",), timeout=60)
        defer_frame(a, "Decision: Permission")
        a.send(DRAFT)
        _until(a, lambda s: DRAFT in _composer(a, s)[0], "the Main draft typed", (b,), 15)
        _reopen_permission(a, (b,))
        a.send("y")
        settled = _until(a, lambda s: SETTLED in s and DRAFT in _composer(a, s)[0] and _caret_in_composer(a, s),
                         "the delegation turn settled with the draft kept and the caret in it", (b,), 90)
        base = _composer(a, settled)
        (evidence / "01-stage-before.screen.txt").write_text(settled)
        delegated = _journal(lambda: [r for r in _received(b_ws) if r["address"]["item"] not in known],
                             "the delegated item journaled by B", (a, b))
        assert len(delegated) == 1, delegated
        item = delegated[0]["address"]["item"]

        # The Agents journey: `t Open thread` (AC2, AC3), the frame's Follow up on the same
        # delegation (AC6-AC9), exact return (AC4) and the untouched Main draft (AC10).
        _send_chord_ctrl_x_s(a)
        screen = _until(a, lambda s: _mc_top(s) and "a2a-peer" in _text(_frame(s, MC)), "the delegation row", (b,), 20)
        inner = _frame(screen, MC)
        target = [index for index, row in enumerate(inner) if "a2a-peer" in row]
        assert len(target) == 1, screen
        selected = next((index for index, row in enumerate(inner) if "▸ " in row), target[0])
        if target[0] != selected:
            a.send(("j" if target[0] > selected else "k") * abs(target[0] - selected))
        screen = _until(a, lambda s: _mc_top(s) and _peer_selected(_frame(s, MC))
                        and "[t] Open thread" in _text(_frame(s, MC)), "[t] Open thread painted", (b,), 20)
        assert "[Enter] Open [t] Open thread" in _text(_frame(screen, MC)), screen
        (evidence / "02-agents-offered.screen.txt").write_text(screen)
        before = _counts(stub, a_ws, b_ws)
        a.send("t")
        screen = _until(a, lambda s: _squash(f"thread · {PEER} · {item}") in _flat(_frame(s, THREAD))
                        and _squash(f"you {item}") in _flat(_frame(s, THREAD))
                        and _squash("[f] Follow up") in _flat(_frame(s, THREAD)),
                        "the delegated exchange's frame offering [f] Follow up", (b,), 30)
        (evidence / "03-agents-thread.screen.txt").write_text(screen)
        opened = _counts(stub, a_ws, b_ws)
        for key in ("provider_turns", "b_received", "a_dispatched"):
            assert opened[key] == before[key], (key, before, opened)
        facts.update(item=item, before_open=before, after_open=opened)

        a.send("f")
        prompt = f"/team followup {PEER} {item} "
        screen = _until(a, lambda s: _field(s, prompt) == ""
                        and _squash("[Enter] Send") in _flat(_frame(s, THREAD)),
                        "the follow-up field prompt", (b,), 15)
        (evidence / "04-follow-up-field.screen.txt").write_text(screen)
        a.send(FOLLOW)
        _until(a, lambda s: bool(_field(s, prompt)) and FOLLOW.endswith(_field(s, prompt)), "the typed follow-up", (b,), 15)
        a.send(ENTER)
        delivered = _squash(f"follow-up answering {item} · Send · ● {PEER} delivered")
        screen = _until(a, lambda s: delivered in _flat(_frame(s, THREAD)), "the delivered receipt", (b,), 60)
        assert _flat(_frame(screen, THREAD)).count(delivered) == 1, screen
        (evidence / "05-follow-up-delivered.screen.txt").write_text(screen)
        replies = _journal(lambda: [r for r in _received(b_ws) if r.get("in_reply_to") == item],
                           "B's follow-up item", (a, b))
        sent = _counts(stub, a_ws, b_ws)
        assert len(replies) == 1 and replies[0]["content"] == FOLLOW, replies
        assert sent["b_received"] == opened["b_received"] + 1, (opened, sent)
        assert sent["a_dispatched"] == opened["a_dispatched"] + 1, (opened, sent)
        assert sent["provider_turns"] == opened["provider_turns"], (opened, sent)
        facts.update(after_follow_up=sent, follow_up=replies[0])

        a.send(ESC)
        screen = _until(a, lambda s: _mc_top(s) and _peer_selected(_frame(s, MC)), "Agents with the row selected", (b,), 15)
        (evidence / "06-agents-return.screen.txt").write_text(screen)
        a.send(ESC)
        screen = _until(a, lambda s: MC not in s and _caret_in_composer(a, s), "the Stage restored", (b,), 15)
        assert _composer(a, screen) == base, (base, _composer(a, screen), screen)
        (evidence / "07-stage-after.screen.txt").write_text(screen)
        rows = stub.rows()
        assert not any(row.get("desync") for row in rows), rows
        assert len([row for row in rows if row.get("persona") == "operator" and "turn" in row]) == 2, rows
        assert not any(row.get("persona") == PEER and "turn" in row for row in rows), rows
        (evidence / "facts.json").write_text(json.dumps(facts, indent=2) + "\n")
        (evidence / "assertions.txt").write_text(
            "PASS: debug binary, one 80x24 native smoke, one Agents journey.\n"
            "PASS: [t] Open thread follows [Enter] Open on the selected a2a-peer row.\n"
            "PASS: t opens Thread · sent · recipient on B's item; zero provider turns, B items, A dispatches.\n"
            "PASS: the delegated item is `you` and the same frame offers [f] Follow up.\n"
            "PASS: f prompt /team followup recipient <item>; one follow-up, in_reply_to the item, delivered.\n"
            "PASS: Esc returns to Agents with the row selected; Esc restores the Main draft and caret.\n"
            "PASS: no provider scene desync.\n")


@pytest.mark.a2a_binary
@pytest.mark.timeout(300)
def test_agents_open_thread_and_follow_up_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="agents-thread-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=sha256_file(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="ragents-thread-", dir="/tmp") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert sha256_file(binary) == manifest["binary_sha256"], "binary changed during smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_AGENTS_THREAD_EVIDENCE={evidence}", flush=True)
