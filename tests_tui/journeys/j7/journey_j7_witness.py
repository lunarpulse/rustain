#!/usr/bin/env python3
"""J7 witness — Dana's narrowed journey against an UNCHANGED shipped-profile binary.

    python3 journey_j7_witness.py --binary <release rustain> [--evidence-dir DIR] [--no-attach]

The binary is whatever the caller names (the release-profile, six-feature build for
the D-27 witness). Nothing in the product is changed and no product knob is used:
the child's environment alone sends Telegram's TLS Bot API to a 127.0.0.1 CONNECT
proxy (``HTTPS_PROXY``) whose only tunnel ends at a loopback fake server, and trusts
only a throwaway fixture root (``SSL_CERT_FILE``). See ``loopback_telegram.py``.

Beats (each asserts on harness-observed requests/rows, never on text this script printed):

  1  AC1/AC2  denied chat 202 + allowed chat 101 through the real long poll;
              exactly one completion (persona leads decoded ``system``) and exactly one
              ``sendMessage`` to 101 with the exact reply text;
  2  AC3      one ``[[jobs]]`` entry installed atomically, ``SIGHUP`` reload, the
              scheduler (not this script) fires it at the recorded UTC minute, the
              forward reaches Telegram;
  3  AC4      ``daemon attach`` on an 80x24 PTY shows both results with origin cues;
  4           egress: the proxy log holds ONLY the loopback-bound Telegram tunnel; no
              refusal and no non-loopback peer was recorded.

Controls (separate daemons): wrong root CA -> handshake rejected, zero replies; a
non-loopback dial -> refused by the proxy and recorded; persona file missing -> the
daemon refuses to start.

Exit status 0 only when every beat and control passed. AC5 (budget refusals) and AC6
(tagged-asset provenance) are out of this driver's scope and are stated, not faked.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parents[1]))  # tests_tui (for TerminalScreen)

import evidence_guard as eg  # noqa: E402
import loopback_telegram as lt  # noqa: E402
import world as w  # noqa: E402

REQUIRED_FEATURES = ("telegram", "cron", "a2a", "p2p", "relay-server", "self-update")
BRIEFING_REPLY = "J7 morning briefing: two meetings."
FORWARD_TEXT = f"[cron: morning-briefing] {BRIEFING_REPLY}"
SECRETS = (w.TOKEN, w.PROVIDER_KEY)
BUDGET_REFUSAL = "Daily budget of $1.00 reached ($1.00 spent today). Raise [budget] daily_limit_usd or wait until tomorrow."
CRON_SKIP = "[cron: skipped — daily budget of $1.00 reached ($1.00 spent today)]"


def log(msg: str) -> None:
    print(f"[j7] {msg}", flush=True)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Failure(AssertionError):
    pass


def check(condition: bool, message: str) -> None:
    if not condition:
        raise Failure(message)


def binary_identity(binary: Path) -> dict:
    out = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=30, env={"PATH": os.environ.get("PATH", "")})
    check(out.returncode == 0, f"--version failed: {out.stderr}")
    features_line = next((l for l in out.stdout.splitlines() if l.startswith("features:")), "")
    features = {f.strip() for f in features_line.removeprefix("features:").split(",")}
    missing = [f for f in REQUIRED_FEATURES if f not in features]
    check(not missing, f"binary lacks the shipped features {missing}: {features_line!r}")
    return {"version_text": out.stdout, "sha256": sha256(binary), "bytes": binary.stat().st_size}


def wait_until(predicate, deadline_secs: float, what: str):
    end = time.monotonic() + deadline_secs
    while time.monotonic() < end:
        found = predicate()
        if found:
            return found
        time.sleep(0.05)
    raise Failure(f"timed out after {deadline_secs:.0f}s waiting for {what}")


def sends(world: w.World) -> list[dict]:
    return [r for r in world.tg.log.rows() if r["event"] == "send"]


def completions(world: w.World) -> list[dict]:
    return [r for r in world.provider_rows() if "turn" in r]


# ── AC4 PTY ──────────────────────────────────────────────────────────────────


class AttachPty:
    """``daemon attach`` on an 80x24 PTY with the same explicit child environment."""

    def __init__(self, world: w.World) -> None:
        import pexpect
        import pyte
        from harness import TerminalScreen

        self.screen = TerminalScreen(80, 24)
        self.stream = pyte.Stream(self.screen)
        self.pexpect = pexpect
        self.child = pexpect.spawn(
            str(world.binary), args=["--profile", w.PROFILE, "daemon", "attach"], cwd=str(world.ws),
            env=world.child_env(), encoding="utf-8", dimensions=(24, 80), timeout=30,
        )

    def text(self) -> str:
        while True:
            try:
                data = self.child.read_nonblocking(size=4096, timeout=0.1)
            except (self.pexpect.TIMEOUT, self.pexpect.EOF):
                break
            if data:
                self.stream.feed(data)
            else:
                break
        return "\n".join(self.screen.display)

    def wait_screen(self, needles: list[str], deadline_secs: float) -> str:
        end = time.monotonic() + deadline_secs
        screen = ""
        while time.monotonic() < end:
            screen = self.text()
            if all(n in screen for n in needles):
                return screen
            time.sleep(0.2)
        raise Failure(f"attached screen lacks {needles} after {deadline_secs:.0f}s:\n{screen}")

    def close(self) -> None:
        if self.child.isalive():
            self.child.sendcontrol("c")
            time.sleep(0.3)
            self.child.close(force=True)


def install_cron(world: w.World) -> tuple[dt.datetime, str]:
    """Atomically install the one documented job at a future UTC minute, then SIGHUP-reload."""
    now = dt.datetime.now(dt.timezone.utc)
    fire = (now + dt.timedelta(seconds=60)).replace(second=0, microsecond=0) + dt.timedelta(minutes=1)
    expression = f"{fire.minute} {fire.hour} * * *"
    cron = (
        "[[jobs]]\n"
        'name = "morning-briefing"\n'
        f'schedule = "{expression}"\n'
        f'prompt = "{w.BRIEFING_PROMPT}"\n'
        "forward = true\n"
        "timeout_secs = 600\n"
    )
    tmp = world.cfg / "cron.toml.tmp"
    tmp.write_text(cron)
    os.replace(tmp, world.cfg / "cron.toml")
    log(f"cron `{expression}` (UTC), scheduled epoch {int(fire.timestamp())}; SIGHUP reload")
    count = lambda: sum(p.read_text(errors="replace").count("reloading scheduler") for p in world.logs.glob("product.log*"))  # noqa: E731
    before = count()
    world.daemon.send_signal(signal.SIGHUP)
    wait_until(lambda: count() > before, 15, "the observed SIGHUP reload")
    return fire, expression


# ── beats ────────────────────────────────────────────────────────────────────


def positive(binary: Path, evidence: Path, attach: bool) -> dict:
    record: dict = {}
    with w.World(binary) as world:
        world.start_daemon()
        try:
            world.wait_ready()
            persona_text = (world.cfg / "personas" / f"{w.PROFILE}.md").read_text().splitlines()[0]
            check(persona_text == "# J7 Dana fixture persona", "fixture persona first line changed")

            # Beat 1 — one denied and one allowed update through the REAL long poll.
            log("beat 1: denied chat 202 + allowed chat 101 via the Telegram long poll")
            world.tg.api.serve_update(1001, w.DENIED_CHAT, "J7 denied: do not process this.")
            world.tg.api.serve_update(1002, w.ALLOWED_CHAT, w.TELEGRAM_PROMPT)
            send = wait_until(lambda: next(iter(sends(world)), None), 30, "the reply sendMessage")
            # Consumption barrier: the NEXT poll must carry offset 1003 (both updates confirmed).
            wait_until(lambda: world.tg.log.wait_for(lambda r: r["event"] == "poll" and r["offset"] == 1003, 0.1),
                       30, "poll offset 1003 (both updates consumed)")
            rows = completions(world)
            check(len(rows) == 1, f"expected exactly one completion, got {rows}")
            check(rows[0]["last_user"] == w.TELEGRAM_PROMPT, f"completion saw {rows[0]['last_user']!r}")
            check(rows[0]["system_has_persona_prefix"] is True, "persona file body does not lead the decoded system field")
            check(not any("denied" in json.dumps(r) for r in world.provider_rows()), "denied chat text reached the provider")
            check(len(sends(world)) == 1, f"expected exactly one sendMessage, got {sends(world)}")
            check((send["chat_id"], send["text"]) == (w.ALLOWED_CHAT, w.TELEGRAM_REPLY), f"unexpected send {send}")
            record["telegram"] = {"completions": 1, "send": {"chat_id": send["chat_id"], "text": send["text"]}}

            # Beat 2 — the scheduler fires a documented [[jobs]] entry at a future UTC minute.
            fire, expression = install_cron(world)
            check(len(completions(world)) == 1, "a completion happened before the scheduled minute")
            remaining = fire.timestamp() - time.time() + 90
            wait_until(lambda: len(sends(world)) >= 2 and sends(world)[1], max(remaining, 10), "the scheduled forward")
            check(time.time() >= fire.timestamp() - 1, "the briefing fired before its scheduled minute")
            rows = completions(world)
            check(len(rows) == 2 and rows[1]["last_user"] == w.BRIEFING_PROMPT, f"scheduler completion rows: {rows}")
            check(rows[1]["system_has_persona_prefix"] is True, "persona missing from the cron turn's system field")
            second = sends(world)[1]
            check((second["chat_id"], second["text"]) == (w.ALLOWED_CHAT, FORWARD_TEXT), f"unexpected forward {second}")
            check(len(sends(world)) == 2, "an extra sendMessage was observed")
            record["cron"] = {"expression": expression, "scheduled_epoch": int(fire.timestamp()), "forward": second["text"]}

            # Beat 3 — the authenticated rich attach shows both results with origin cues.
            if attach:
                log("beat 3: daemon attach, 80x24 PTY")
                pty = AttachPty(world)
                try:
                    screen = pty.wait_screen(["J7 Telegram reply", "J7 morning briefing"], 60)
                finally:
                    pty.close()
                record["attach_screen"] = screen
                eg.write_checked(evidence / "attach.screen.txt", screen, SECRETS)
                for cue in ("attached", "[telegram] You:", "[telegram] Assistant:", "[cron] Assistant:", "[cron: morning-briefing]"):
                    check(cue in screen, f"cue {cue!r} missing from the attached screen:\n{screen}")

            # Beat 4 — egress.
            rows = world.tg.log.rows()
            refused = [r for r in rows if r["event"] == "refused"]
            egress = [r for r in rows if r["event"] == "egress_nonloopback"]
            connects = [r for r in rows if r["event"] == "connect"]
            check(not refused, f"the proxy refused traffic in the positive run: {refused}")
            check(not egress, f"non-loopback socket peers were observed: {egress}")
            check(connects and all(r["authority"] == lt.TELEGRAM_AUTHORITY and r["allowed"] for r in connects), f"unexpected tunnels: {connects}")
            check(not [r for r in rows if r["event"] in ("unknown", "rejected", "tls_handshake_failed")], f"harness anomalies: {rows}")
            record["proxy_log"] = rows
            record["egress_samples"] = world.audit.samples if world.audit else 0
        except Exception:
            print(world.diagnostics(), file=sys.stderr)  # local only: never archived
            raise
    return record


def wrong_ca(binary: Path, evidence: Path) -> dict:
    """Control: the binary refuses a leaf the (only) trusted root did not sign."""
    other = lt.Pki()
    try:
        with w.World(binary, env_overrides={"SSL_CERT_FILE": str(other.ca_bundle)}) as world:
            world.start_daemon()
            try:
                world.wait_ready()
                world.tg.api.serve_update(1002, w.ALLOWED_CHAT, w.TELEGRAM_PROMPT)
                failed = wait_until(lambda: world.tg.log.wait_for(lambda r: r["event"] == "tls_handshake_failed", 0.1), 30, "a rejected handshake")
                time.sleep(3)  # the loop retries; no retry may succeed
                rows = world.tg.log.rows()
                check(not [r for r in rows if r["event"] in ("send", "poll")], f"traffic crossed a rejected handshake: {rows}")
                check(not completions(world), "a completion ran behind a rejected handshake")
                check(not [r for r in rows if r["event"] == "refused"], "unexpected refusal")
                return {"handshake_failure": failed["reason"]}
            finally:
                pass
    finally:
        other.close()


def non_loopback(binary: Path, evidence: Path) -> dict:
    """Control: a non-loopback dial from the real binary is refused by the proxy and recorded."""
    target = "j7-control.invalid"
    with w.World(binary, env_overrides={"ANTHROPIC_BASE_URL": f"https://{target}"}) as world:
        world.start_daemon()
        world.wait_ready()
        refused = wait_until(lambda: next(iter(world.tg.proxy.refused()), None), 30, "a recorded refusal")
        check(refused["method"] == "CONNECT" and refused["target"] == f"{target}:443", f"unexpected refusal {refused}")
        check(not [r for r in world.tg.log.rows() if r["event"] == "egress_nonloopback"], "a non-loopback peer was observed")
        return {"refused": refused}


def persona_missing(binary: Path, evidence: Path) -> dict:
    """Control: no persona file -> the daemon fails closed before readiness."""
    with w.World(binary) as world:
        (world.cfg / "personas" / f"{w.PROFILE}.md").unlink()
        world.start_daemon()
        try:
            world.wait_ready(deadline=20)
        except lt.HarnessError as exc:
            check("exited early" in str(exc), f"expected an early exit, got {exc}")
            diag = world.diagnostics()
            check("not found; create it or correct [persona] config.file" in diag, f"persona-file diagnosis absent:\n{diag}")
            check(not completions(world), "a completion request was made")
            return {"exit": world.daemon.returncode, "diagnostic": "persona file … not found; create it or correct [persona] config.file"}
        raise Failure("the daemon became ready without its persona file")


def budget_case(binary: Path, evidence: Path, *, limit: float, refused: bool) -> dict:
    """AC5: priced spend of exactly $1.00 today vs a finite limit, through the real channel
    and scheduler admission paths of the unchanged binary (no provider beyond the scene)."""
    with w.World(binary) as world:
        world.configure_budget(limit)
        world.seed_spend()
        world.start_daemon()
        try:
            world.wait_ready()
            world.tg.api.serve_update(1002, w.ALLOWED_CHAT, w.TELEGRAM_PROMPT)
            send = wait_until(lambda: next(iter(sends(world)), None), 30, "the Telegram answer")
            wait_until(lambda: world.tg.log.wait_for(lambda r: r["event"] == "poll" and r["offset"] == 1003, 0.1), 30, "poll offset 1003")
            if refused:
                check((send["chat_id"], send["text"]) == (w.ALLOWED_CHAT, BUDGET_REFUSAL), f"unexpected refusal {send}")
                check(not completions(world), f"a completion ran for a refused channel turn: {completions(world)}")
            else:
                check((send["chat_id"], send["text"]) == (w.ALLOWED_CHAT, w.TELEGRAM_REPLY), f"unexpected reply {send}")
                check(len(completions(world)) == 1, f"below the limit the completion counter must increment: {completions(world)}")
            fire, expression = install_cron(world)
            wait_until(lambda: len(sends(world)) >= 2 and sends(world)[1], max(fire.timestamp() - time.time() + 90, 10), "the cron forward")
            check(time.time() >= fire.timestamp() - 1, "the job fired before its scheduled minute")
            second = sends(world)[1]
            if refused:
                check(second["text"] == f"[cron: morning-briefing] {CRON_SKIP}", f"unexpected cron marker {second}")
                check(not completions(world), f"a completion ran for a refused cron turn: {completions(world)}")
            else:
                check(second["text"] == FORWARD_TEXT, f"unexpected cron forward {second}")
                check(len(completions(world)) == 2, f"completions: {completions(world)}")
            check(len(sends(world)) == 2, "an extra sendMessage was observed")
            pty = AttachPty(world)
            try:
                screen = pty.wait_screen(["attached", "[cron] Assistant:"], 60)
            finally:
                pty.close()
            if refused:
                check(CRON_SKIP in " ".join(screen.split()), f"skip marker missing from the attached screen:\n{screen}")
                check(w.TELEGRAM_PROMPT not in screen and "[telegram] You:" not in screen, f"a refused turn left a dialogue row:\n{screen}")
            else:
                check("[telegram] You:" in screen and w.TELEGRAM_REPLY in " ".join(screen.split()), f"admitted row missing:\n{screen}")
            check(not [r for r in world.tg.log.rows() if r["event"] in ("refused", "egress_nonloopback", "unknown", "rejected")], "harness anomaly")
            eg.write_checked(evidence / f"budget-{'at' if refused else 'below'}-limit.screen.txt", screen, SECRETS)
            return {"limit": limit, "spent": 1.0, "telegram": send["text"], "cron": second["text"], "completions": len(completions(world))}
        except Exception:
            print(world.diagnostics(), file=sys.stderr)
            raise


def missing_token(binary: Path, evidence: Path) -> dict:
    """AC5: no config/env token -> refuse before PID readiness, zero completion requests."""
    with w.World(binary, env_remove=("TELEGRAM_BOT_TOKEN",)) as world:
        world.start_daemon()
        try:
            world.wait_ready(deadline=20)
        except lt.HarnessError as exc:
            check("exited early" in str(exc), f"expected an early exit, got {exc}")
            diag = world.diagnostics()
            check("bot_token required for telegram adapter" in diag, f"missing-token diagnostic absent:\n{diag}")
            check(not (world.ws / ".rustain" / "daemon.pid").exists() or f"pid = {world.daemon.pid}" not in (world.ws / ".rustain" / "daemon.pid").read_text(), "PID file named the refused daemon")
            check(not completions(world), "a completion request was made")
            return {"exit": world.daemon.returncode, "diagnostic": "bot_token required for telegram adapter"}
        raise Failure("the daemon became ready without a Telegram token")


def evidence_guard_control() -> dict:
    """AC5: the redaction guard refuses synthetic canaries and passes clean text."""
    canary = "SYNTHETIC-CANARY-CREDENTIAL-0001"
    probes = {
        "credential value": f"row says {canary}",
        "request url": "POST /bot123:abc/GetUpdates",
        "auth header": "X-Api-Key: whatever",
    }
    for name, text in probes.items():
        try:
            eg.write_checked(Path("/nonexistent/never-written"), text, (canary,))
        except eg.RedactionError:
            continue
        raise Failure(f"the guard let a {name} through")
    check(not eg.violations("[cron: morning-briefing] J7 morning briefing: two meetings.", (canary,)), "the guard flagged clean text")
    return {"probes": sorted(probes)}


def budget_pair(binary: Path, evidence: Path) -> dict:
    """At-limit and below-limit runs, in parallel (each waits for its own cron minute)."""
    import concurrent.futures as cf

    with cf.ThreadPoolExecutor(2) as pool:
        at = pool.submit(budget_case, binary, evidence, limit=1.0, refused=True)
        below = pool.submit(budget_case, binary, evidence, limit=1.01, refused=False)
        return {"at_limit": at.result(), "below_limit": below.result()}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--no-attach", action="store_true")
    parser.add_argument("--only", help="comma-separated beat names (debugging); the verdict run uses none")
    args = parser.parse_args()

    binary = args.binary.resolve()
    evidence = args.evidence_dir or Path(os.environ.get("TMPDIR", "/tmp")) / f"j7-evidence-{int(time.time())}"
    evidence.mkdir(parents=True, exist_ok=True)
    identity = binary_identity(binary)
    log(f"binary {binary} sha256 {identity['sha256']} ({identity['bytes']} bytes)")
    summary: dict = {"binary": str(binary), **{k: identity[k] for k in ("sha256", "bytes")},
                     "started": dt.datetime.now(dt.timezone.utc).isoformat(), "results": {}}
    failures = 0
    for name, run in (
        ("positive", lambda: positive(binary, evidence, not args.no_attach)),
        ("control_wrong_ca", lambda: wrong_ca(binary, evidence)),
        ("control_non_loopback", lambda: non_loopback(binary, evidence)),
        ("control_persona_missing", lambda: persona_missing(binary, evidence)),
        ("ac5_missing_token", lambda: missing_token(binary, evidence)),
        ("ac5_evidence_guard", evidence_guard_control),
        ("ac5_budget", lambda: budget_pair(binary, evidence)),
    ):
        if args.only and name not in args.only.split(","):
            continue
        try:
            summary["results"][name] = {"pass": True, "detail": run()}
            log(f"PASS {name}")
        except Exception as exc:  # noqa: BLE001 - every failure is a verdict row
            failures += 1
            summary["results"][name] = {"pass": False, "error": f"{type(exc).__name__}: {exc}"}
            log(f"FAIL {name}: {exc}")
    check(sha256(binary) == identity["sha256"], "the binary changed during the run")
    summary["finished"] = dt.datetime.now(dt.timezone.utc).isoformat()
    eg.write_checked(evidence / "j7-witness.json", json.dumps(summary, indent=2) + "\n", SECRETS)
    log(f"evidence: {evidence}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
