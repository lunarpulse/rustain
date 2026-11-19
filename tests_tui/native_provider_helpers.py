"""Shared native provider smoke helpers."""
import hashlib
from html import escape
import json
import re
import time

import harness
import pexpect
import pytest
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


def _legacy_status_row(rows):
    """A8's opaque status cutout is valid only with settled composer chrome."""
    top = harness.composer_top(rows)
    if (not top or not rows[top].startswith("┌") or not rows[top].endswith("┐")
            or not rows[-1].startswith("└") or not rows[-1].endswith("┘")
            or not re.search(r"│ (?:Ready|Streaming\.\.\.)(?:[ │]|$)", rows[top - 1])):
        return None
    return top - 1


def _modal_complete(screen, title="Select Model"):
    """The complete caller-supplied A8 sheet (0, 0, 80, 19), above the idle mission strip row
    (Story 21.7: the strip is directly above the status row) and the chat chrome."""
    rows = screen.split("\n")
    prefix = f"┌ {title} "
    if (len(rows) != 24 or any(len(row) != 80 for row in rows)
            or _legacy_status_row(rows) != 20
            or rows[0] != prefix + "─" * (79 - len(prefix)) + "┐"
            or rows[18] != "└" + "─" * 78 + "┘"
            or rows[19].rstrip() != "^X s Mission Control"
            or not rows[21].startswith("┌ Message")
            or rows[22][0] != "│" or rows[22][-1] != "│"
            or rows[23] != "└" + "─" * 78 + "┘"):
        return False
    if any(row[0] != "│" or row[-1] != "│" for row in rows[1:18]):
        return False
    content = [row[1:-1].strip() for row in rows[1:18] if row[1:-1].strip()]
    footers = {
        "Select Model": r"(?:Enter sign in · ←→ provider|←→ provider|↑↓ navigate · Enter select) · Esc close(?: · / search)?",
        "Log out": r"↑↓ navigate · Enter choose · Esc close",
        "Log out of OpenAI": r"↑↓ navigate · Enter log out · Esc back",
        "OpenAI accounts": r"↑↓ navigate · Enter make active · Esc close",
    }
    footer = footers.get(title, r"(?:↑↓ navigate · Enter select · Esc close|Enter submit(?: · Alt\+C copy link · Alt\+L copy local link)? · Esc cancel)")
    return bool(content and re.fullmatch(footer, content[-1]))


def status_ready(text):
    """A whole `Ready` state; optional key hints must yield before state text."""
    return "Retry" not in text and "Ready" in text
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
