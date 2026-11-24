"""Story C-12 (AC1): `rustain init`'s Messaging channel step on a real debug
binary at 80x24.

Real `init` in a PTY with an isolated HOME, config directory and workspace and
a clean environment whose channel credential variables hold an inert sentinel
that must never appear in output or on disk. Provider sign-in is declined (its
own coverage is unchanged); no credential is typed.

What the binary was built with decides the path, read from its own
`--version` feature list (no marker, no lane change):

- `telegram` and `slack` (e.g. `cargo build --features telegram,cron,slack
  --bin rustain`, passed as `RUSTAIN_TUI_BINARY`) — Dana's path: the menu, a
  pasted conversation ID answered by an index-only corrective line, a corrected
  deduplicated list, the adapter / destination / count review, an explicit
  confirmation and the summary naming environment variables and the exact
  daemon command; a rerun shows the saved channel and Enter keeps it byte for
  byte.
- neither (the default build the per-push lane uses) — the short notice
  replaces the menu, nothing is read and the profile is unchanged.
- only one of them — skipped; the unit tests cover those menus by injection.
"""
import os
from pathlib import Path
import subprocess

import pexpect
import pyte
import pytest

import harness

SENTINEL = "C12-INERT-SENTINEL-NOT-A-TOKEN"
CHANNEL_FEATURES = {"slack", "telegram"}
WORK = (
    "# Dana's assistant\n"
    'name = "work-assistant"\n'
    'description = "Dana\'s work assistant"\n'
    'extends = "coding"\n'
)
IDS = "Slack user IDs (comma-separated; Enter to go back): "


def _built_features(binary, env):
    """The `features:` line of the binary's own `--version` block."""
    out = subprocess.run([str(binary), "--version"], capture_output=True, text=True,
                         env=env, timeout=60, check=True).stdout
    for line in out.splitlines():
        if line.startswith("features: "):
            return {name.strip() for name in line[len("features: "):].split(",")}
    return set()


class _Init:
    """`rustain init` in a PTY: ordered expects on the received bytes, rendered
    through pyte for screen checks."""

    def __init__(self, child):
        self.child = child
        self._screen = pyte.Screen(80, 24)
        self._stream = pyte.Stream(self._screen)

    def expect(self, text, timeout=60):
        self.child.expect_exact(text, timeout=timeout)
        self._stream.feed(self.child.before + self.child.after)

    def send(self, line):
        self.child.send(line + "\r")

    def rows(self):
        return [row.rstrip() for row in self._screen.display]


def _snapshot(root):
    return {path: path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


class _World:
    def __init__(self, root, binary):
        self.binary = binary
        home = root / "home"
        self.config = home / "config"
        self.workspace = root / "workspace"
        for directory in (self.config / "profiles", self.workspace, home / "data", home / "logs"):
            directory.mkdir(parents=True)
        # A clean environment: no inherited credential, profile or config variable.
        self.env = {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / "xdg-config"),
            "XDG_DATA_HOME": str(home / "xdg-data"),
            "RUSTAIN_CONFIG_DIR": str(self.config),
            "RUSTAIN_DATA_DIR": str(home / "data"),
            "RUSTAIN_LOG_PATH": str(home / "logs" / "rustain.log"),
            "TERM": "xterm-256color",
            "TELEGRAM_BOT_TOKEN": SENTINEL,
            "SLACK_APP_TOKEN": SENTINEL,
            "SLACK_BOT_TOKEN": SENTINEL,
        }
        self.profile = self.config / "profiles" / "work-assistant.toml"
        self.profile.write_text(WORK)
        self.evidence = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", root / "evidence"))
        self.evidence.mkdir(parents=True, exist_ok=True)
        self._logs = []

    def spawn(self, name):
        child = pexpect.spawn(str(self.binary), ["--profile", "work-assistant", "init"],
                              cwd=str(self.workspace), env=self.env, dimensions=(24, 80),
                              encoding="utf-8")
        child.delaybeforesend = 0
        # Received bytes only: what the terminal showed, echo included.
        log = (self.evidence / f"{name}.raw.txt").open("w", encoding="utf-8", newline="")
        self._logs.append(log)
        child.logfile_read = log
        return child, _Init(child)

    def capture(self, init, name):
        (self.evidence / f"{name}.screen.txt").write_text("\n".join(init.rows()) + "\n")

    @staticmethod
    def finish(child, init):
        init.expect("Sign in to a model provider now? [y/n] ")
        init.send("n")
        child.expect(pexpect.EOF, timeout=60)
        child.close()
        assert child.exitstatus == 0, child.exitstatus

    def close(self):
        for log in self._logs:
            log.close()


def _dana(world):
    """Slack, corrected IDs, explicit save; then a rerun keeps it."""
    child, init = world.spawn("c12-first")
    try:
        for _ in range(2):  # the existing policy questions, defaults
            init.expect("Selection [1]: ")
            init.send("")
        init.expect("Messaging channel (optional) — profile: work-assistant")
        init.expect("Current channel: none")
        init.expect("  1. Telegram — allowed DM chat IDs")
        init.expect("  2. Slack — allowed app DM user IDs")
        init.expect("  3. Skip [Enter]")
        init.expect("Selection [3]: ")
        init.send("2")
        init.expect(IDS)
        init.send("C012ABCDEF")
        init.expect("Entry 1 must be a Slack user ID, not a channel ID.")
        corrective = [row for row in init.rows() if row.startswith("Entry 1 ")]
        assert corrective == ["Entry 1 must be a Slack user ID, not a channel ID."], init.rows()
        init.expect(IDS)
        init.send("U012ABCDEF, W0345GH ,U012ABCDEF")
        init.expect("  Adapter: Slack (app DMs over Socket Mode)")
        init.expect("  Profile: work-assistant")
        init.expect("  Allowed Slack users: 2")
        init.expect("Save, replacing this profile's channel settings? [y/N] ")
        world.capture(init, "c12-a1-review")
        assert world.profile.read_text() == WORK, "nothing is written before the confirmation"
        init.send("y")
        init.expect("Messaging channel:")
        init.expect("  Slack settings saved (not connected).")
        init.expect("  Profile: work-assistant")
        init.expect("  File: ")
        init.expect("  Before starting the daemon, set SLACK_APP_TOKEN and SLACK_BOT_TOKEN in")
        init.expect("  its environment, then run:")
        init.expect("    rustain --profile work-assistant daemon start")
        init.expect("  A supervised daemon needs these variables in its own environment.")
        init.expect("  restart it with this profile.")
        world.capture(init, "c12-a2-summary")
        # A wrapped path never hides the action line.
        assert "    rustain --profile work-assistant daemon start" in init.rows(), init.rows()
        world.finish(child, init)
    finally:
        child.close(force=True)

    saved = world.profile.read_text()
    assert saved.startswith(WORK), saved
    assert 'adapter = "slack"' in saved, saved
    assert 'allowed_user_ids = ["U012ABCDEF", "W0345GH"]' in saved, saved
    after_save = _snapshot(world.config)

    child, init = world.spawn("c12-rerun")
    try:
        init.expect("Configuration already exists. Overwrite? [y/n] ")
        init.send("n")
        init.expect("Messaging channel (optional) — profile: work-assistant")
        init.expect("Current channel: Slack")
        init.expect("  2. Slack — edit allowed app DM user IDs")
        init.expect("  3. Keep existing [Enter]")
        init.expect("Selection [3]: ")
        init.send("")
        init.expect("  Existing Slack configuration kept.")
        world.capture(init, "c12-a3-rerun")
        world.finish(child, init)
    finally:
        child.close(force=True)
    assert _snapshot(world.config) == after_save


def _without_channel_features(world):
    """The default build: a short notice replaces the menu; nothing is read."""
    child, init = world.spawn("c12-notice")
    try:
        for _ in range(2):
            init.expect("Selection [1]: ")
            init.send("")
        init.expect("Messaging channel (optional) — profile: work-assistant")
        init.expect("Current channel: none")
        init.expect("Telegram and Slack are unavailable in this build (they need the")
        init.expect("telegram or slack feature). Channel settings were not changed.")
        init.expect("Messaging channel:")
        init.expect("  Channel setup skipped.")
        world.capture(init, "c12-notice")
        world.finish(child, init)
    finally:
        child.close(force=True)
    transcript = (world.evidence / "c12-notice.raw.txt").read_text(encoding="utf-8")
    assert "Selection [3]" not in transcript, transcript
    assert world.profile.read_text() == WORK


def test_init_channel_step_80x24(tmp_path):
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file(), binary
    world = _World(tmp_path, binary)
    built = _built_features(binary, world.env) & CHANNEL_FEATURES
    try:
        if built == CHANNEL_FEATURES:
            _dana(world)
        elif not built:
            _without_channel_features(world)
        else:
            pytest.skip(f"binary has only the {', '.join(sorted(built))} channel feature; build "
                        "with both or neither (unit tests cover one-feature menus)")
    finally:
        world.close()
    for path in sorted(tmp_path.rglob("*")):
        if path.is_file():
            assert SENTINEL not in path.read_text(encoding="utf-8", errors="replace"), path
    for capture in world.evidence.glob("c12-*"):
        assert SENTINEL not in capture.read_text(encoding="utf-8", errors="replace"), capture
