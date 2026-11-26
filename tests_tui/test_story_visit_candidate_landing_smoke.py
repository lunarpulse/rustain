"""visit-candidate-landing: one debug-native 80x24 smoke of the Tool Inspector's Visit route.

A scripted provider (no model call, no credential) makes the standalone TUI run one real `task`
whose worker reads one synthetic file. The turn is committed, then the smoke opens the Tool
Inspector on that exact `task` result and checks the binary against what its own `--version`
says it was built with (no marker, no lane change; the same rule as `test_story_c12_init_channel`):

- built with `session-visit` (`cargo build --features session-visit --bin rustain`, passed as
  `RUSTAIN_TUI_BINARY`): the rail offers `[o] Visit source`; `o` opens `Session Visit`, the
  retained worker prose is readable inside it, `Esc` returns to the exact Inspector, and the
  whole route made zero provider requests;
- built without it (the default debug binary of the nightly lane): the rail never offers
  Visit, `o` is inert (the next `Esc` closes the Inspector itself, so nothing opened above it),
  and the same zero-request rule holds. This is the feature-off witness of the default-off row.

Run it once per binary to produce the paired evidence of AC6. The pilot driver
`run_standalone_visit_candidate.py` additionally exercises the off-binary reopen and ordinary
append; this smoke is the lane-registered, self-contained part of that evidence.

Waits are structural (PTY-paced screen predicates), never sleeps.
"""
import json
import os
import subprocess

import harness
from conftest import _write_subagent_profile
from fixtures.agents import write_custom_agent
from fixtures.scene_provider import SceneStub
from keys import ENTER, ESC

SOURCE = "VISIT_SMOKE_SOURCE_7291 synthetic source only"
FINAL = "VISIT_SMOKE_FINAL_1735 plain final worker prose."
MAIN_TOKEN = "MAIN_DONE_4821"
MAIN = f"{MAIN_TOKEN} the synthetic check finished."
FOLD_CUE = "[Enter] Show task results"
RAIL = "[o] Visit source"
USAGE = {"input_tokens": 20, "output_tokens": 8}


def _built_features(binary, home):
    """The `features:` line of the binary's own `--version` block."""
    env = {**os.environ, "HOME": str(home)}
    env.pop("TERM", None)
    out = subprocess.run([str(binary), "--version"], capture_output=True, text=True,
                         env=env, timeout=60, check=True, stdin=subprocess.DEVNULL).stdout
    for line in out.splitlines():
        if line.startswith("features: "):
            return {name.strip() for name in line[len("features: "):].split(",")}
    return set()


def _scene(ws):
    """The scripted turns: Main calls `task`, the worker reads one file and answers, Main ends."""
    source = ws / "synthetic-0.txt"
    source.write_text(SOURCE + "\n")
    prompt = "Load the staged evidence for the visit smoke. Do not make any changes."
    worker = f"Read only {source}."

    def turn(expect, kind, **payload):
        return {"expect": expect, "content": [{"type": kind, **payload}],
                "stop_reason": "tool_use" if kind == "tool_use" else "end_turn", "usage": USAGE}

    turns = [
        turn(prompt, "tool_use", name="task", input={
            "subagent_type": "report-reader", "description": "Same synthetic check", "prompt": worker}),
        turn(worker, "tool_use", name="Read", input={"file_path": str(source)}),
        turn(SOURCE, "text", text=FINAL),
        turn(SOURCE, "text", text=MAIN),
    ]
    path = ws / "scene.json"
    path.write_text(json.dumps({"scene": "visit-candidate-landing-smoke", "turns": turns}))
    return path, prompt


def _committed(ws):
    """The saved conversation holds the Main turn's final prose (the native app wrote it)."""
    directory = ws / ".claude" / "sessions"
    for path in (*directory.glob("*.meta.json"), *directory.glob("*/conversation.json")):
        try:
            saved = json.loads(path.read_text())
        except (OSError, ValueError):
            continue  # a write in flight
        for turn in saved.get("conversation", saved).get("turns", []):
            for part in turn.get("parts", []):
                if part.get("kind") == "prose" and MAIN_TOKEN in part.get("text", ""):
                    return True
    return False


def _in_panel(screen, value):
    """`value` appears inside a framed panel; borders are stripped, whitespace is normalised."""
    text = " ".join(line.strip("│ ") for line in screen.splitlines() if line.lstrip().startswith("│"))
    return " ".join(value.split()) in " ".join(text.split())


def _open_inspector(tui):
    """From the committed turn to the native Inspector on the focused `task` result."""
    tui.send(ESC)
    screen = tui.wait_until(lambda s: tui._screen.cursor.hidden and "[Output]" not in s,
                            "Main Chat owns input")
    if FOLD_CUE in screen:
        tui.send(ENTER)
    elif "[Enter] Inspect" not in screen:
        tui.send("K")
    tui.wait_until(lambda s: "[Enter] Inspect" in s and "[Output]" not in s,
                   "Main focused task result")
    tui.send(ENTER)
    return tui.wait_until(lambda s: "[Output]" in s and "Tool · task #" in s,
                          "Main opens the native Inspector")


def _visit_on(tui, inspector):
    assert RAIL in inspector, f"the feature binary's Inspector must offer Visit:\n{inspector}"
    tui.send("o")
    visit = tui.wait_until(lambda s: "Session Visit" in s, "the exact Visit opens")
    for _ in range(40):
        if _in_panel(visit, FINAL):
            break
        tui.send("j")
        before = visit
        visit = tui.wait_until(lambda s, before=before: "Session Visit" in s and s != before,
                               "scroll Visit to the worker prose")
    else:
        raise AssertionError(f"the retained worker prose never became visible:\n{visit}")
    assert "Working Ground" not in visit and "Engage" not in visit, visit
    tui.send(ESC)
    tui.wait_until(lambda s: "[Output]" in s and "Tool · task #" in s and "Session Visit" not in s,
                   "Back returns to the exact Inspector")


def _visit_off(tui, inspector):
    assert "Visit source" not in inspector, f"the default build must not offer Visit:\n{inspector}"
    tui.send("o")
    # An inert `o` leaves the Inspector on top, so this one Esc closes it; had `o` opened
    # anything above it, Esc would close that instead and the Inspector would remain.
    tui.send(ESC)
    screen = tui.wait_until(lambda s: "[Output]" not in s, "one Esc closes the Inspector")
    assert "Session Visit" not in screen, screen


def test_visit_route_or_its_absence_at_80x24(tmp_path, monkeypatch):
    binary = harness._resolve_binary()[0]
    built = _built_features(binary, tmp_path / "version-home")
    for key in tuple(os.environ):
        if key.startswith(("ANTHROPIC_", "OPENAI_", "GEMINI_")) or key.endswith("_API_KEY"):
            monkeypatch.delenv(key)
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    ws = tmp_path / "ws"
    home, config = ws / "home", ws / ".rustain"
    home.mkdir(parents=True)
    config.mkdir()
    monkeypatch.setattr(harness, "ENV_FILE", ws / "no-credential-file")
    _write_subagent_profile(config, "visit-smoke")
    (config / "config.toml").write_text('[permissions]\nalways_tools = ["task", "Read"]\n')
    write_custom_agent(ws, "report-reader", "Read synthetic assigned file.",
                       "Read only the assigned synthetic file.", allowed_tools=["Read"])
    scene, prompt = _scene(ws)
    stub = SceneStub(scene, ws / "requests.jsonl").start()
    try:
        env = stub.env("visit-candidate-landing-smoke", home)
        env.update(XDG_CONFIG_HOME=str(home / "config"), XDG_DATA_HOME=str(home / "data"),
                   RUSTAIN_PROFILE="visit-smoke", RUST_LOG="warn", NO_PROXY="127.0.0.1,localhost")
        with harness.RustainTUI(binary=binary, build=False, workspace=ws,
                                allowed_tools=["task", "Read"], env_overrides=env) as tui:
            tui.send("\x1b[200~" + prompt + "\x1b[201~")
            tui.send(ENTER)
            tui.wait_until(lambda s: _committed(ws) and "Ready" in harness.status_row(s),
                           "the scripted turn completes and is saved", timeout=170)
            requests = stub.rows()
            assert requests and not any(row.get("desync") for row in requests), requests
            inspector = _open_inspector(tui)
            if "session-visit" in built:
                _visit_on(tui, inspector)
            else:
                _visit_off(tui, inspector)
            assert stub.rows() == requests, "the Inspector/Visit route made provider requests"
    finally:
        stub.stop()
