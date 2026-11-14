"""19.29: one debug-native 80x24 smoke — a redirect under a Bash command pattern.

A workspace skill declares ``allowed-tools: Bash(echo:*)``; the operator is in
YOLO (no approval card, so only the permission chain can stop a call). The
scene's model activates the skill, then emits ``echo PROBE > <outside>/probe``
(refused, never reaches the shell) and ``echo OK > inside-19-29.txt`` (admitted).

Assertions are on product-minted text (the request log's tool_result carries the
refusal the binary built), filesystem side effects, and the screen — never on
served prose. The scene is generated at test time so ``<outside>`` is this run's
own sibling tempdir, never ``$HOME`` or a fixed path.
"""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
import time
import traceback
from datetime import datetime, timezone
from pathlib import Path

import pexpect
import pytest

import harness
from fixtures.scene_provider import SceneStub
from fixtures.skills import write_workspace_skill
from harness import RustainTUI

pytestmark = pytest.mark.story_19_29

SKILL_NAME = "redirect-probe"
SKILL_MD = (
    f"---\nname: {SKILL_NAME}\n"
    "description: Probe the redirect boundary under a command pattern.\n"
    "allowed-tools: Bash(echo:*)\n---\n## Protocol\nRun echo only.\n"
)
REFUSAL = "redirect refused by skill"


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _scene(outside: Path) -> dict:
    usage = {"input_tokens": 10, "output_tokens": 10}

    def tool(expect, name, tool_input):
        return {"expect": expect, "stop_reason": "tool_use", "usage": usage,
                "content": [{"type": "tool_use", "name": name, "input": tool_input}]}

    def text(expect, body):
        return {"expect": expect, "stop_reason": "end_turn", "usage": usage,
                "content": [{"type": "text", "text": body}]}

    return {"scene": "19-29-redirect", "personas": {"operator": {"turns": [
        tool("activate the redirect probe", "activate_skill", {"name": SKILL_NAME}),
        text(f"Skill '{SKILL_NAME}' activated", "The probe skill is active."),
        tool("run the two redirects", "Bash",
             {"command": f"echo PROBE > {outside}/probe-19-29"}),
        # The needle is composed by the shell from two quoted fragments, so the
        # joined literal is not a byte of this scene.
        tool(REFUSAL, "Bash",
             {"command": 'echo OK > inside-19-29.txt; echo "TAIL-19""-29"'}),
        text("TAIL-19-29", "Both redirects handled."),
    ]}}}


def _normalized(screen: str) -> str:
    return " ".join(screen.replace("│", " ").split())


# ── Predicates, each read from the product (source cited) ────────────────────
# Trust modal title: src/adapters/tui/widgets/skill_trust_prompt.rs:39.
TRUST_TITLE = "Trust and enable this skill"
# Queued decision cue `Needs you (N) ^X q`: status_bar.rs:468 (reserved at the
# right edge of the status row), count from decision_queue/mod.rs:942.
QUEUED_CUE = "Needs you (1)"
# Idle status while the permission mode is YOLO: the idle "Ready" text
# (status_bar.rs `StatusState::Idle.display_text()`) is REPLACED by the YOLO
# fact `YOLO_FACT` (status_bar.rs:573; pinned by the status_bar test asserting
# the YOLO row contains the fact and not "Ready").
YOLO_IDLE_FACT = "all tools auto-approved"
# Busy statuses: `StatusState::display_text()` — "Executing <tool>...",
# "Streaming...", "Retrying (a/b) in …" (status_bar.rs, test_status_state_display_text).
BUSY_MARKERS = ("Executing", "Streaming...", "Retrying")
# Keys sent inside a decision frame's inert window are dropped:
# decision_queue/mod.rs:113 INERT_AFTER_PAINT = 500 ms.


def is_idle(screen: str) -> bool:
    text = _normalized(screen)
    return YOLO_IDLE_FACT in text and not any(m in text for m in BUSY_MARKERS)


def _wait_idle(observe, tui, timeout: float = 30.0, stable: float = 1.0) -> None:
    deadline = time.monotonic() + timeout
    since = None
    while True:
        screen = observe(tui)
        now = time.monotonic()
        if is_idle(screen):
            since = since or now
            if now - since >= stable:
                return
        else:
            since = None
        assert now < deadline, f"the TUI never settled to the YOLO idle row:\n{screen}"


def _exercise(binary: Path, evidence: Path, scratch: Path, monkeypatch) -> None:
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)

    workspace = scratch / "ws"
    outside = scratch / "out"
    home = scratch / "home"
    for directory in (workspace, outside, home):
        directory.mkdir()
    source = scratch / "skill-src" / SKILL_NAME / "SKILL.md"
    source.parent.mkdir(parents=True)
    source.write_text(SKILL_MD)
    write_workspace_skill(workspace, source)

    scene = evidence / "scene.json"
    scene.write_text(json.dumps(_scene(outside), indent=1) + "\n")
    stub = SceneStub(scene, evidence / "provider-requests.jsonl").start()
    status = 0
    raw = (evidence / "terminal.raw.txt").open("w")
    try:
        # Raw PTY capture AT SPAWN: the factory attaches the log before any read.
        real_spawn = pexpect.spawn

        def spawn_with_log(*args, **kwargs):
            child = real_spawn(*args, **kwargs)
            child.logfile_read = raw
            return child

        config = workspace / ".rustain"
        config.mkdir()
        env = {
            **stub.env("operator", home),
            "NO_COLOR": "1",
            "RUSTAIN_CONFIG_DIR": str(config),
            "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
            "RUSTAIN_LOG_PATH": str(evidence / "runtime.log"),
        }
        monkeypatch.setattr(harness.pexpect, "spawn", spawn_with_log)
        seen_refusal = False
        screens = (evidence / "screens.jsonl").open("w")

        def observe(tui) -> str:
            nonlocal seen_refusal
            screen = tui.get_screen_text()
            screens.write(json.dumps({"screen": screen}) + "\n")
            screens.flush()
            if REFUSAL in _normalized(screen):
                seen_refusal = True
            return screen

        def turn_rows():
            return [row for row in stub.rows() if "turn" in row]

        with RustainTUI(fresh=True, build=False, workspace=workspace, allowed_tools=[],
                        env_overrides=env, timeout=60) as tui:
            (workspace / ".env").unlink(missing_ok=True)
            tui.set_permission_mode("yolo")

            tui.send_message("activate the redirect probe")
            opened = False
            prompt_since = None
            deadline = time.monotonic() + 60
            while not any(row["turn"] >= 2 for row in turn_rows()):
                screen = observe(tui)
                now = time.monotonic()
                if "Trust and enable this skill" in screen:
                    # Keys inside the frame's inert window are dropped (19.7):
                    # answer only once the frame has been stable for a second.
                    if prompt_since is None:
                        prompt_since = now
                    elif now - prompt_since >= 1.0:
                        tui.send("y")
                        prompt_since = now
                else:
                    prompt_since = None
                    if not opened and "Needs you (1)" in screen:
                        # The decision is queued behind the status row (19.7).
                        tui.send("\x18q")
                        tui.send("\r")
                        opened = True
                assert time.monotonic() < deadline, f"activation turn never finished:\n{screen}"
            _wait_idle(observe, tui)

            tui.send_message("run the two redirects")
            deadline = time.monotonic() + 60
            while not any(row["turn"] >= 5 for row in turn_rows()):
                screen = observe(tui)
                assert time.monotonic() < deadline, f"redirect turns never finished:\n{screen}"
            _wait_idle(observe, tui)
            final = observe(tui)
            (evidence / "final.screen.txt").write_text(final)

        probe = outside / "probe-19-29"
        inside = workspace / "inside-19-29.txt"
        assert not probe.exists(), "the refused redirect must never reach the shell"
        assert inside.read_text() == "OK\n", "the in-workspace redirect must run"
        assert seen_refusal, "no observed screen showed the product's refusal text"
        assert not any(row.get("desync") for row in stub.rows()), stub.rows()
        # The refusal travelled back to the model verbatim — product-minted.
        assert REFUSAL in stub.log_path.read_text()
        (evidence / "assertions.txt").write_text(
            "PASS: debug binary, one 80x24 native smoke, YOLO, skill Bash(echo:*).\n"
            "PASS: outside redirect refused; target never created.\n"
            "PASS: in-workspace redirect ran; inside-19-29.txt == 'OK'.\n"
            "PASS: the refusal text was observed on screen and in the request log.\n")
    finally:
        raw.close()
        status = stub.stop()
    assert status == 0, f"scene provider exited {status}"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_redirect_under_a_command_pattern_is_refused_80x24(monkeypatch):
    if not os.environ.get("RUSTAIN_TUI_BINARY"):
        pytest.skip("no native binary: build `cargo build --bin rustain` and set RUSTAIN_TUI_BINARY")
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-29-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="s19-29-", dir="/tmp") as scratch:
            _exercise(binary, evidence, Path(scratch), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_19_29_EVIDENCE={evidence}", flush=True)
