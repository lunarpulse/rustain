"""Credential-gated discovery: real debug-native 80x24, home auth, loopback only."""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
from html import escape
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time
import traceback
from urllib.parse import urlsplit

import harness
import pexpect
import pytest

LIVE_IDS = ("account/native-chat-a", "account/native-chat-b")
KEYS = {"openrouter": "sk-native-openrouter-account", "deepseek": "sk-native-rejected-deepseek"}
PROVIDER_ENV = {
    "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "OPENROUTER_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS",
    "DEEPSEEK_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY", "XAI_API_KEY",
    "GROQ_API_KEY", "MISTRAL_API_KEY", "ZAI_API_KEY", "VOYAGE_API_KEY",
    "DEEPINFRA_API_KEY", "TOGETHER_API_KEY", "RUSTAIN_EMBEDDING_API_KEY",
    "ANTHROPIC_BASE_URL", "OPENAI_BASE_URL", "OPENROUTER_BASE_URL",
    "GOOGLE_BASE_URL", "GEMINI_BASE_URL", "DEEPSEEK_BASE_URL", "MOONSHOT_BASE_URL",
    "OLLAMA_HOST", "OLLAMA_BASE_URL", "RUSTAIN_TEST_AUTH_ORIGIN",
}


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _sensitive_env(name):
    return (name in PROVIDER_ENV or bool(re.search(
        r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY)$", name))
        or name.startswith(("RUSTAIN_TEST_", "RUSTAIN_SCENE_")))


class _NativeTUI(harness.RustainTUI):
    """Shared native spawn, with PTY-driven waits instead of legacy sleeps."""

    def _sync_screen(self):
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "native TUI exited"
            return
        except pexpect.EOF:
            pytest.fail("native TUI exited before the expected screen")
        self._stream.feed(data)
        self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
        self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        _wait(self, lambda screen: text in screen, repr(text), timeout)
        return True  # Never enter the shared harness's timeout-to-sleep fallback.

    def stop(self):
        if self._child is not None:
            if self.child.isalive():
                self.child.sendcontrol("c")
                try:
                    self.child.expect(pexpect.EOF, timeout=5)
                except pexpect.TIMEOUT:
                    self.child.terminate(force=True)
            self.child.close(force=True)


def _wait(tui, predicate, label, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


def _plain(screen):
    return " ".join(screen.replace("│", " ").replace("┆", " ").split())


def _modal_complete(screen):
    """True only once the selector's whole border is painted: a capture taken
    mid-redraw (a missing right edge or bottom corner) is never accepted."""
    rows = screen.split("\n")
    top = next((y for y, row in enumerate(rows) if "┌ Select Model" in row), None)
    if top is None:
        return False
    left = rows[top].index("┌ Select Model")
    right = rows[top].rfind("┐")
    if right <= left:
        return False
    bottom = next((y for y in range(top + 1, len(rows))
                   if len(rows[y]) > right and rows[y][left] == "└"), None)
    if bottom is None or rows[bottom][right] != "┘" or set(rows[bottom][left + 1:right]) != {"─"}:
        return False
    return all(len(rows[y]) > right and rows[y][left] == "│" and rows[y][right] == "│"
               for y in range(top + 1, bottom))


def _chrome_complete(screen):
    """The full 80x24 chat chrome with no selector remnant still being erased."""
    rows = screen.split("\n")
    return (len(rows) == 24 and "Select Model" not in screen
            and rows[21].startswith("┌ Message") and rows[21].rstrip().endswith("┐")
            and "└" not in rows[21] and "┘" not in rows[21]
            and rows[22].startswith("│") and rows[22].rstrip().endswith("│")
            and rows[23].startswith("└") and rows[23].rstrip().endswith("┘")
            and set(rows[23].rstrip()[1:-1]) == {"─"}
            and not any(ch in row for row in rows[2:20] for ch in "│┌┐└┘"))


def _capture(tui, evidence, name, screen):
    """Persist the actual full PTY grid, not a reconstructed UI mock."""
    rows = list(tui._screen.display)
    assert len(rows) == 24 and tui._screen.columns == 80
    assert screen == "\n".join(rows)
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")
    cells = [[vars(tui._screen.buffer[y][x]).copy()
              if hasattr(tui._screen.buffer[y][x], "__dict__")
              else tui._screen.buffer[y][x]._asdict()
              for x in range(80)] for y in range(24)]
    (evidence / f"{name}.state.json").write_text(json.dumps({
        "columns": 80, "rows": 24, "grid": rows, "cells": cells,
        "cursor": {"x": tui._screen.cursor.x, "y": tui._screen.cursor.y,
                   "hidden": tui._screen.cursor.hidden}}, indent=2) + "\n")
    # A directly viewable terminal transcription: every character comes from pyte.
    lines = [f'<text x="8" y="{22 + y * 20}" xml:space="preserve">{escape(row)}</text>'
             for y, row in enumerate(rows)]
    (evidence / f"{name}.svg").write_text(
        '<svg xmlns="http://www.w3.org/2000/svg" width="976" height="496" viewBox="0 0 976 496">'
        '<rect width="976" height="496" fill="#101418"/>'
        '<g fill="#e8edf2" font-family="monospace" font-size="20">'
        + "".join(lines) + '</g></svg>\n')


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    requests = []
    arrivals = set()
    request_lock = threading.Lock()
    # The models.dev response is held until the selector is open on the locked
    # Anthropic column, so its remerge must update an already-open picker.
    models_dev_release = threading.Event()
    catalog = {"anthropic": {"models": {"claude-sonnet-9": {
        "id": "claude-sonnet-9", "name": "Claude Sonnet 9", "limit": {"context": 900000},
        "tool_call": True, "reasoning": True, "modalities": {"input": ["text", "image"]}}}}}

    class Discovery(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            path = urlsplit(self.path).path
            with request_lock:
                arrivals.add(path)
            if path == "/api.json":
                models_dev_release.wait(timeout=60)
            if path == "/or/v1/models":
                status, payload = 200, {"data": [
                    {"id": model, "supported_parameters": ["tools"]} for model in LIVE_IDS
                ]}
                expected_auth = "Bearer " + KEYS["openrouter"]
            elif path == "/or/v1/auth/key":
                status, payload = 200, {}
                expected_auth = "Bearer " + KEYS["openrouter"]
            elif path == "/ds/v1/models":
                status, payload = 401, {"error": {"message": "credential rejected"}}
                expected_auth = "Bearer " + KEYS["deepseek"]
            elif path == "/api.json":
                status, payload, expected_auth = 200, catalog, None
            else:
                status, payload, expected_auth = 404, {"error": "unexpected route"}, None
            authorized = self.headers.get("Authorization") == expected_auth
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            with request_lock:
                requests.append({"method": "GET", "path": self.path, "status": status,
                                 "expected_credential_received": authorized})

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Discovery)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def stop_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive(), "loopback server failed to stop"

        stack.callback(stop_server)
        # Runs before stop_server (LIFO): never leave a handler parked on the hold.
        stack.callback(models_dev_release.set)
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        provider_urls = {"deepseek": origin + "/ds/v1", "openrouter": origin + "/or/v1"}
        config_text = ""
        for provider, model in (("deepseek", "deepseek-chat"), ("openrouter", LIVE_IDS[0])):
            config_text += (f'[provider.{provider}]\nprovider_id = "{provider}"\nkind = "{provider}"\n'
                            f'model_id = "{model}"\napi_key_env = "{provider.upper()}_API_KEY"\n'
                            f'base_url = "{provider_urls[provider]}"\nenabled = true\n'
                            'discover_models = true\nmodel_filter = ["*"]\ncache_ttl_seconds = 3600\n\n')
        (config / "config.toml").write_text(config_text)
        (data / "auth.json").write_text(json.dumps({"version": 2, "next_id": 3, "providers": {
            provider: [{"id": index, "rev": 1, "origin": "stored", "type": "api_key",
                        "api_key": KEYS[provider], "last_validated": None, "disabled_cause": None}]
            for index, provider in enumerate(("deepseek", "openrouter"), 1)}}) + "\n")
        # Include .env names, not merely os.environ: overrides are applied last by start().
        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {name for name in loaded if _sensitive_env(name)}) - {"NO_PROXY"}
        env = {name: "" for name in cleared}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
                   RUSTAIN_DATA_DIR=str(data), RUSTAIN_LOG_PATH=str(evidence / "native.runtime.log"),
                   RUSTAIN_MODELS_DEV_URL=origin, TERM="xterm-256color", NO_PROXY="127.0.0.1,localhost")
        original_spawn = harness.pexpect.spawn

        def safe_spawn(command, *args, **kwargs):
            # This guard runs after the harness merges its .env, before native code runs.
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert Path(kwargs["cwd"]) == workspace
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(name) == "" for name in cleared)
            assert actual["RUSTAIN_CONFIG_DIR"] == str(config)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            assert actual["RUSTAIN_MODELS_DEV_URL"] == origin
            for url in [*provider_urls.values(), actual["RUSTAIN_MODELS_DEV_URL"]]:
                parsed = urlsplit(url)
                assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1"
                assert parsed.port == server.server_address[1]
            project_config = (workspace / ".rustain/config.toml").read_text()
            assert "[provider." not in project_config
            # Prove these are the binary's effective endpoints, not merely values
            # in a file it might ignore. This offline command exits before providers.
            probe = subprocess.run([str(binary), "config", "show", "--json"],
                                   cwd=workspace, env=actual, capture_output=True,
                                   text=True, timeout=30, check=False)
            assert probe.returncode == 0, probe.stderr
            effective = json.loads(probe.stdout)["provider"]
            assert set(effective) == set(provider_urls), effective
            for provider, url in provider_urls.items():
                assert effective[provider]["base_url"] == url
            (evidence / "effective-config.json").write_text(probe.stdout)
            (evidence / "network-safety.json").write_text(json.dumps({
                "binary": str(binary), "cwd_has_provider_config": False,
                "provider_urls": provider_urls, "models_dev_url": origin + "/api.json",
                "cleared_environment_names": sorted(cleared), "instrumentation_required": False,
                "guard": "checked final spawn environment before native process creation"}, indent=2) + "\n")
            kwargs["logfile"] = terminal
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = stack.enter_context((evidence / "native.screens.jsonl").open("w"))
        terminal = stack.enter_context((evidence / "native.terminal.txt").open("w"))
        stack.callback(tui.stop)
        try:
            tui.start()
            tui.child.delaybeforesend = 0

            def ready(screen):
                with request_lock:
                    paths = {urlsplit(row["path"]).path for row in requests}
                    held = "/api.json" in arrivals
                return (_chrome_complete(screen) and "Ready" in screen
                        and "openrouter" in screen.lower()
                        and {"/or/v1/models", "/ds/v1/models"} <= paths and held)

            screen = _wait(tui, ready, "Ready, live discovery receipts and the held models.dev request")
            assert "openrouter" in screen.lower(), screen
            _capture(tui, evidence, "01-ready-openrouter", screen)
            tui.send("\x18")
            tui.send("m")
            screen = _wait(tui, lambda value: _modal_complete(value) and "OpenRouter" in value
                           and all(model in value for model in LIVE_IDS),
                           "complete OpenRouter live account roster frame")
            _capture(tui, evidence, "02-openrouter-live", screen)
            tui.send("\x1b[D")
            screen = _wait(tui, lambda value: _modal_complete(value) and "DeepSeek" in value
                           and "sign in again" in value
                           and "requires authentication" in _plain(value)
                           and "Set DEEPSEEK_API_KEY or run `rustain auth login deepseek`" in _plain(value),
                           "complete DeepSeek rejected-credential frame with its actionable hint")
            _capture(tui, evidence, "03-deepseek-rejected", screen)
            tui.send("\x1b[C")
            _wait(tui, lambda value: _modal_complete(value) and "OpenRouter" in value
                  and all(model in value for model in LIVE_IDS), "return to OpenRouter")
            tui.send("\x1b[C")

            def locked(count, extra):
                def predicate(value):
                    text = _plain(value)
                    return _modal_complete(value) and extra(text) and all(part in text for part in (
                        "Anthropic", "not signed in", "has no credentials configured",
                        "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "rustain auth login anthropic",
                        f"{count} models in catalog"))
                return predicate

            screen = _wait(tui, locked(4, lambda text: "claude-sonnet-9" not in text),
                           "locked Anthropic public seed while models.dev is still held")
            _capture(tui, evidence, "04a-anthropic-locked-before-models-dev", screen)
            models_dev_release.set()
            with_metadata = locked(5, lambda text: "claude-sonnet-9" in text)
            screen = _wait(tui, with_metadata,
                           "open selector rebuilt by the models.dev remerge refresh event")
            _capture(tui, evidence, "04-anthropic-locked", screen)
            tui.send("\r")
            # Locked Enter opens the sign-in chooser; cancelling must restore
            # the same selector without selecting a model.
            _wait(tui, lambda value: "Sign in to Anthropic" in value
                  and "Choose a login method" in value, "locked Enter opens sign-in")
            tui.send("\x1b")
            _wait(tui, with_metadata, "cancel sign-in restores locked selector")
            tui.send("\x1b[D")
            _wait(tui, lambda value: _modal_complete(value) and "OpenRouter" in value
                  and all(model in value for model in LIVE_IDS),
                  "provider navigation after locked Enter remains in the selector")
            tui.send("\x1b[C")
            screen = _wait(tui, with_metadata, "locked Enter keeps selector open with no selection")
            assert not any("▶" in line or "▸" in line for line in screen.splitlines() if "claude-" in line), screen
            _capture(tui, evidence, "05-locked-enter-no-selection", screen)
            tui.send("\x1b")
            screen = _wait(tui, lambda value: _chrome_complete(value) and "Ready" in value
                           and "openrouter" in value.lower()
                           and "models in catalog" not in value and "not signed in" not in value,
                           "Escape preserves active OpenRouter with the selector fully erased")
            _capture(tui, evidence, "06-escape-openrouter-unchanged", screen)
        finally:
            with request_lock:
                observed = list(requests)
            (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        assert observed and all(row["expected_credential_received"] for row in observed), observed
        assert all(urlsplit(row["path"]).path in {
            "/or/v1/models", "/or/v1/auth/key", "/ds/v1/models", "/api.json"
        } for row in observed), observed
        pricing = json.loads((data / "models_pricing.json").read_text())
        extra = next(row for row in pricing["catalog"]["anthropic"] if row["modelId"] == "claude-sonnet-9")
        assert extra["contextWindow"] == 900000 and extra["displayName"] == "Claude Sonnet 9", extra
        assert {"ToolUse", "Thinking", "Vision"} <= set(extra["capabilities"]), extra
        (evidence / "models-dev-extra-metadata.json").write_text(json.dumps(extra, indent=2) + "\n")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(180)
def test_home_credentials_live_rejected_and_locked_providers_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-1c-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r20-1c-") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_20_1C_EVIDENCE={evidence}", flush=True)
