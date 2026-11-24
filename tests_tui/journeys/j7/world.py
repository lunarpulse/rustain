"""J7 world: scratch dirs, the J7 provider stub, the loopback Telegram harness and the
daemon child, all launched with an EXPLICIT child-only environment.

Nothing here edits product source or the binary under test. The only things that make
Telegram reachable are child-environment variables (``loopback_telegram.child_env``).
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import loopback_telegram as lt

J7 = Path(__file__).resolve().parent
TOKEN = "123456:J7-FIXTURE-NOT-A-REAL-TOKEN"
PROVIDER_KEY = "j7-dana-key"
PROFILE = "j7-dana"
ALLOWED_CHAT = 101
DENIED_CHAT = 202
TELEGRAM_PROMPT = "J7 Dana: confirm my design review."
TELEGRAM_REPLY = "J7 Telegram reply: design review confirmed."
BRIEFING_PROMPT = "J7 Dana: give my morning briefing."


class World:
    """One isolated run. ``loopback=None`` builds the Telegram harness; tests pass a
    prepared one (e.g. with a different CA) to run controls."""

    def __init__(
        self,
        binary: Path,
        *,
        env_overrides: dict[str, str] | None = None,
        env_remove: tuple[str, ...] = (),
    ) -> None:
        self.binary = Path(binary)
        # Short path: the daemon's Unix socket lives under the data/run dirs.
        self.root = Path(tempfile.mkdtemp(prefix="j7-", dir="/tmp"))
        self.ws = self._mk("ws")
        self.cfg = self._mk("cfg")
        self.data = self._mk("data")
        self.home = self._mk("home")
        self.logs = self._mk("logs")
        self.tg = lt.LoopbackTelegram(TOKEN)
        self.env_overrides = env_overrides or {}
        self.env_remove = env_remove
        self.provider: subprocess.Popen | None = None
        self.provider_url = ""
        self.provider_log = self.logs / "provider-requests.jsonl"
        self.daemon: subprocess.Popen | None = None
        self.audit: lt.EgressAudit | None = None
        self._install_fixtures()

    def _mk(self, name: str) -> Path:
        path = self.root / name
        path.mkdir(mode=0o700)
        return path

    def _install_fixtures(self) -> None:
        (self.cfg / "profiles").mkdir()
        (self.cfg / "personas").mkdir()
        shutil.copyfile(J7 / "profiles" / f"{PROFILE}.toml", self.cfg / "profiles" / f"{PROFILE}.toml")
        shutil.copyfile(J7 / "personas" / f"{PROFILE}.md", self.cfg / "personas" / f"{PROFILE}.md")

    # ── budget fixture (AC5) ─────────────────────────────────────────────────

    def configure_budget(self, limit_usd: float) -> None:
        """User-global config: a finite daily limit and ONE priced model, so a ledger row of
        1,000,000 input tokens is exactly $1.00 (``[pricing.j7-priced-model]`` $1/M in)."""
        (self.cfg / "config.toml").write_text(
            f"[budget]\ndaily_limit_usd = {limit_usd!r}\n\n"
            "[pricing.j7-priced-model]\ninput_per_million = 1.0\noutput_per_million = 0.0\n"
        )

    def seed_spend(self, tokens_in: int = 1_000_000) -> None:
        """Append one priced row dated NOW (current local day) to the real usage ledger the
        daemon reads (`<data>/usage/<session>.jsonl`); the unchanged ledger code prices it."""
        import json

        usage = self.data / "usage"
        usage.mkdir(exist_ok=True)
        row = {
            "timestampMs": int(time.time() * 1000), "sessionId": "j7-seed", "conversationId": "j7-seed",
            "providerId": "j7-fixture", "model": "j7-priced-model", "tier": "flagship", "stepKind": None,
            "escalationReason": "none",
            "usage": {"tokensIn": tokens_in, "tokensOut": 0, "parentCtx": 0},
        }
        (usage / "j7-seed.jsonl").write_text(json.dumps(row) + "\n")

    # ── lifecycle ────────────────────────────────────────────────────────────

    def start_provider(self) -> str:
        persona = self.cfg / "personas" / f"{PROFILE}.md"
        self.provider = subprocess.Popen(
            [sys.executable, str(J7 / "provider_stub.py"), "--scene", str(J7 / "scenes" / "j7-dana.json"),
             "--port", "0", "--log", str(self.provider_log), "--persona-file", str(persona)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        banner = self.provider.stdout.readline().strip()
        if not banner.startswith("listening "):
            raise lt.HarnessError(f"provider stub did not start: {banner!r}")
        self.provider.stdout.readline()  # scene sha256 line
        self.provider_url = banner.split(" ", 1)[1]
        return self.provider_url

    def child_base_env(self) -> dict[str, str]:
        """The explicit allowlist: nothing is inherited except PATH."""
        return {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_DATA_HOME": str(self.home / ".local/share"),
            "XDG_CACHE_HOME": str(self.home / ".cache"),
            "TMPDIR": str(self.root),
            "TERM": "xterm-256color",
            "NO_COLOR": "1",
            "TZ": "UTC",
            "RUSTAIN_CONFIG_DIR": str(self.cfg),
            "RUSTAIN_DATA_DIR": str(self.data),
            "RUSTAIN_LOG_PATH": str(self.logs / "product.log"),
            "ANTHROPIC_API_KEY": PROVIDER_KEY,
            "ANTHROPIC_AUTH_TOKEN": "",
            "ANTHROPIC_BASE_URL": self.provider_url,
            "RUSTAIN_MODELS_DEV_URL": self.provider_url,
            "TELEGRAM_BOT_TOKEN": TOKEN,
        }

    def child_env(self) -> dict[str, str]:
        env = self.tg.env(self.child_base_env())
        env.update(self.env_overrides)
        for name in self.env_remove:
            env.pop(name, None)
        return env

    def start_daemon(self) -> subprocess.Popen:
        self.tg.start()
        self.daemon = subprocess.Popen(
            [str(self.binary), "--profile", PROFILE, "daemon", "start", "--foreground"],
            cwd=self.ws, env=self.child_env(), stdin=subprocess.DEVNULL,
            stdout=open(self.logs / "daemon.out", "wb"), stderr=open(self.logs / "daemon.err", "wb"),
        )
        self.audit = lt.EgressAudit(self.daemon.pid, self.tg.log)
        self.audit.start()
        return self.daemon

    def wait_ready(self, deadline: float = 60.0) -> None:
        pid_file = self.ws / ".rustain" / "daemon.pid"
        end = time.monotonic() + deadline
        while time.monotonic() < end:
            if self.daemon.poll() is not None:
                raise lt.HarnessError(f"daemon exited early ({self.daemon.returncode}): {self.diagnostics()}")
            try:
                text = pid_file.read_text()
            except OSError:
                text = ""
            if f"pid = {self.daemon.pid}" in text:
                return
            time.sleep(0.05)
        raise lt.HarnessError(f"daemon not ready in {deadline}s: {self.diagnostics()}")

    def diagnostics(self) -> str:
        out = []
        for p in sorted(self.logs.glob("daemon.*")) + sorted(self.logs.glob("product.log*")):
            out.append(f"--- {p.name} ---\n{p.read_text(errors='replace')[-2500:]}")
        return "\n".join(out)

    def stop(self) -> None:
        if self.audit:
            self.audit.stop()
        if self.daemon and self.daemon.poll() is None:
            self.daemon.send_signal(signal.SIGTERM)
            try:
                self.daemon.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait()
        if self.provider and self.provider.poll() is None:
            self.provider.terminate()
            try:
                self.provider.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.provider.kill()
        self.tg.stop()
        shutil.rmtree(self.root, ignore_errors=True)

    def __enter__(self) -> "World":
        self.start_provider()
        return self

    def __exit__(self, *exc) -> None:
        self.stop()

    # ── observations ─────────────────────────────────────────────────────────

    def provider_rows(self) -> list[dict]:
        import json

        if not self.provider_log.exists():
            return []
        return [json.loads(line) for line in self.provider_log.read_text().splitlines() if line.strip()]
