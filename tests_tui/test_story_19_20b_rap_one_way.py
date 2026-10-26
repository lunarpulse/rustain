"""19.20b Task 6: real RAP ping, consent and attached 80x24 terminal.

Supersedes 19.20a's RAP auto-template smoke: RAP now paints a one-way
notice, not an automatic reply. Only provider boot probes are stubbed;
identities, tickets, admission, QUIC, verdict and terminal are the product.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import traceback

import pexpect
import pytest

import harness
from fixtures.scene_provider import SceneStub
from harness import RustainTUI

NOTICE = "[no reply over RAP]"
FORBIDDEN = "Received. Reviewing."


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _stop(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
    process.wait(timeout=5)


class _StructuralTUI(RustainTUI):
    """Use the shared spawn/config harness without its legacy sleep waits."""

    def _sync_screen(self):
        # Block on actual PTY output, not on a fixed post-keystroke delay.
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "attached TUI exited"
            return
        except pexpect.EOF:
            pytest.fail("attached TUI exited before the expected screen")
        self._stream.feed(data)
        screen = "\n".join(self._screen.display)
        # Check every observed paint, including startup/consent/intermediate
        # screens, and normalize wrapping before testing the forbidden text.
        assert "".join(FORBIDDEN.split()) not in "".join(screen.split()), screen
        if hasattr(self, "screens"):
            self.screens.write(json.dumps({"screen": screen}) + "\n")
            self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if text in self.get_screen_text():
                return True
        # Raising also prevents start()'s old timeout-to-sleep fallback.
        raise AssertionError(f"Screen did not show {text!r}:\n{self.get_screen_text()}")

    def stop(self):
        if self._child is not None:
            if self.child.isalive():
                self.child.sendcontrol("c")
                try:
                    self.child.expect(pexpect.EOF, timeout=5)
                except pexpect.TIMEOUT:
                    self.child.terminate(force=True)
            self.child.close(force=True)
        if self._tmpdir:
            self._tmpdir.cleanup()
            self._tmpdir = None


def _command(binary, world, args, evidence, name):
    ws, env = world
    result = subprocess.run([str(binary), *args], cwd=ws, env=env,
                            capture_output=True, text=True, timeout=15)
    (evidence / (name + ".txt")).write_text(result.stdout + result.stderr)
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def _start_daemon(stack, binary, world, evidence, alias):
    ws, env = world
    log_path = evidence / (alias + ".daemon.log")
    log = stack.enter_context(log_path.open("w"))
    process = subprocess.Popen([str(binary), "daemon", "start", "--foreground"],
                               cwd=ws, env=env, stdout=log,
                               stderr=subprocess.STDOUT, start_new_session=True)
    stack.callback(_stop, process)
    deadline = time.monotonic() + 45
    # The CLI status request is a structural readiness probe. In particular,
    # an existing socket alone is not proof that the foreground daemon is up.
    while True:
        assert process.poll() is None, log_path.read_text()
        status = subprocess.run([str(binary), "daemon", "status"], cwd=ws, env=env,
                                capture_output=True, text=True, timeout=5)
        reach = ws / ".rustain/p2p-reach.json"
        if (status.returncode == 0 and "running" in (status.stdout + status.stderr).lower()
                and reach.is_file() and json.loads(reach.read_text()).get("self")):
            (evidence / (alias + ".status.txt")).write_text(status.stdout + status.stderr)
            return process
        assert time.monotonic() < deadline, log_path.read_text() + status.stdout + status.stderr


def _import_ticket(stack, binary, world, alias, ticket, evidence):
    ws, env = world
    child = pexpect.spawn(str(binary),
                          ["peer", "add", alias, ticket, "--allow-local-addresses"],
                          cwd=str(ws), env=env, encoding="utf-8", timeout=20)
    stack.callback(child.close, force=True)
    child.delaybeforesend = 0
    child.logfile_read = stack.enter_context((evidence / ("add-" + alias + ".txt")).open("w"))
    child.expect_exact("[y] Confirm and pin")
    child.sendline("y")
    child.expect_exact("Pinned " + alias)
    child.expect(pexpect.EOF)
    child.close()
    assert child.exitstatus == 0, (child.exitstatus, child.signalstatus)


def _exercise(binary, evidence, root, monkeypatch, after_delivery=None):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-20b-native", "personas": {
        "sender": {"turns": []}, "answerer": {"turns": []}}}) + "\n")
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds = {}
        for alias in ("sender", "answerer"):
            ws = root / alias
            config = ws / ".rustain"
            config.mkdir(parents=True)
            (ws / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            (config / "p2p.json").write_text(json.dumps({"listen": True, "agents": {}}) + "\n")
            (config / "relay.json").write_text('{"mode":"disabled"}\n')
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-auto"\nnotification = "immediate"\n')
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(config),
                   "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            worlds[alias] = (ws, env)
            _start_daemon(stack, binary, worlds[alias], evidence, alias)

        tickets = {}
        for alias, world in worlds.items():
            output = _command(binary, world, ["peer", "invite", "--name", alias],
                              evidence, alias + ".invite")
            tickets[alias] = next(line.strip() for line in output.splitlines()
                                  if line.startswith("rustain-peer1."))
            assert "carries no network address" not in output, output
        for local, remote in (("sender", "answerer"), ("answerer", "sender")):
            _import_ticket(stack, binary, worlds[local], remote, tickets[remote], evidence)
            config = worlds[local][0] / ".rustain"
            for filename in ("p2p.json", "p2p-reach.json", "a2a-interaction.toml"):
                (evidence / (local + "." + filename)).write_bytes((config / filename).read_bytes())

        ws, env = worlds["answerer"]
        # Register cleanup before start so failed startup also closes the PTY.
        tui = _StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                             env_overrides=env, extra_args=["daemon", "attach"])
        tui.screens = stack.enter_context((evidence / "answerer.screens.jsonl").open("w"))
        terminal_log = stack.enter_context((evidence / "answerer.terminal.txt").open("w"))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = terminal_log
        (evidence / "ready.screen.txt").write_text(tui.get_screen_text())

        sender_ws, sender_env = worlds["sender"]
        ping_log = stack.enter_context((evidence / "sender.ping.txt").open("w"))
        ping = subprocess.Popen([str(binary), "peer", "ping", "answerer"],
                                cwd=sender_ws, env=sender_env, stdout=ping_log,
                                stderr=subprocess.STDOUT, start_new_session=True)
        stack.callback(_stop, ping)
        deadline = time.monotonic() + 60
        consent_answered = False
        while True:
            screen = tui.get_screen_text()
            if NOTICE in screen:
                break
            if "[y] Allow once" in screen and not consent_answered:
                (evidence / "consent.screen.txt").write_text(screen)
                tui.send("y")
                consent_answered = True
            assert ping.poll() in (None, 0), (evidence / "sender.ping.txt").read_text()
            assert time.monotonic() < deadline, screen + (evidence / "sender.ping.txt").read_text()
        assert ping.wait(timeout=15) == 0, (evidence / "sender.ping.txt").read_text()
        verdict = (evidence / "sender.ping.txt").read_text()
        assert "— accepted." in verdict, verdict
        final = tui.get_screen_text()
        assert NOTICE in final, final
        assert "".join(FORBIDDEN.split()) not in "".join(final.split()), final
        (evidence / "one-way.screen.txt").write_text(final)
        assert not any(row.get("turn") or row.get("desync") or row.get("kind") == "desync"
                       for row in stub.rows()), stub.rows()
        (evidence / "assertions.txt").write_text(
            "PASS: real reciprocal tickets/rosters and foreground p2p listeners.\n"
            "PASS: sender peer ping returned accepted.\n"
            "PASS: attached 80x24 screen shows [no reply over RAP].\n"
            "PASS: no observed screen contains Received. Reviewing.; no provider turn.\n"
            f"Consent card answered: {consent_answered}\n")
        if after_delivery is not None:
            after_delivery(tui, evidence)


@pytest.mark.a2a_binary
@pytest.mark.timeout(240)
def test_rap_ping_is_one_way_on_attached_80x24_terminal(monkeypatch):
    root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-20b-", dir=root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "result": "RUNNING", "viewport": [80, 24],
                "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file(), binary
        assert "release" not in binary.parts, "this smoke requires a debug a2a,p2p binary"
        manifest.update({"binary": str(binary), "binary_sha256": _sha256(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        # Short scratch paths keep the real AF_UNIX attach sockets below SUN_LEN.
        with tempfile.TemporaryDirectory(prefix="r20b-", dir="/tmp") as workspace:
            _exercise(binary, evidence, Path(workspace), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during capture"
    except BaseException:
        manifest.update({"result": "FAIL", "failure": traceback.format_exc()})
        (evidence / "assertions.txt").write_text("FAIL: native smoke did not complete.\n")
        raise
    else:
        manifest["result"] = "PASS"
    finally:
        manifest["finished_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "smoke-binary.txt").write_text(json.dumps({
            key: manifest.get(key) for key in ("binary", "binary_sha256", "binary_bytes")
        }, indent=2) + "\n")
        (evidence / "smoke-result.txt").write_text(manifest["result"] + "\n" + manifest.get("failure", ""))
        manifest["captures"] = {
            path.name: {"sha256": _sha256(path), "bytes": path.stat().st_size}
            for path in sorted(evidence.iterdir()) if path.is_file() and path != identity}
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
