"""J7 — controls for the env-scoped loopback Telegram harness and the J7 provider.

Binary-free: these prove the harness itself is a sound, fail-closed instrument
before a journey run trusts it (testability Rules 0-4). They use the stdlib TLS stack
as the client, so they prove the fixture chain validates under strict verification and
that the proxy refuses everything but ``api.telegram.org:443`` — not that any
particular binary honours the proxy variables (the journey's preflight proves that on
the real executable).
"""

from __future__ import annotations

import http.client
import json
import socket
import ssl
import subprocess
import sys
import urllib.parse
from pathlib import Path

import pytest

J7 = Path(__file__).parent / "journeys" / "j7"
sys.path.insert(0, str(J7))

import evidence_guard as eg  # noqa: E402
import loopback_telegram as lt  # noqa: E402
import provider_stub  # noqa: E402

TOKEN = "123456:J7-FIXTURE-NOT-A-REAL-TOKEN"


@pytest.fixture()
def world():
    with lt.LoopbackTelegram(TOKEN) as harness:
        yield harness


def _connect(proxy_port: int, authority: str) -> tuple[socket.socket, bytes]:
    sock = socket.create_connection(("127.0.0.1", proxy_port), timeout=10)
    sock.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode())
    reply = b""
    while b"\r\n\r\n" not in reply:
        chunk = sock.recv(4096)
        if not chunk:
            break
        reply += chunk
    return sock, reply


def _tls_through_proxy(world: "lt.LoopbackTelegram", cafile: Path | None):
    sock, reply = _connect(world.proxy.port, lt.TELEGRAM_AUTHORITY)
    assert reply.startswith(b"HTTP/1.1 200"), reply
    context = ssl.create_default_context(cafile=str(cafile) if cafile else None)
    # Strict verification stays ON: that is the whole claim of the harness.
    assert context.verify_mode == ssl.CERT_REQUIRED and context.check_hostname
    return context.wrap_socket(sock, server_hostname=lt.TELEGRAM_HOST)


def _post(tls: ssl.SSLSocket, token: str, method: str, body: dict) -> tuple[int, dict]:
    conn = http.client.HTTPConnection("api.telegram.org", 443)
    conn.sock = tls
    payload = json.dumps(body).encode()
    conn.request("POST", f"/bot{token}/{method}", payload, {"content-type": "application/json"})
    response = conn.getresponse()
    return response.status, json.loads(response.read())


def test_fixture_chain_validates_under_strict_verification_through_the_proxy(world):
    tls = _tls_through_proxy(world, world.pki.ca_bundle)
    status, body = _post(tls, TOKEN, "GetUpdates", {"offset": 0, "timeout": 30})
    # teloxide-core sends PascalCase method names; the match is case-insensitive.
    assert (status, body) == (200, {"ok": True, "result": []})
    tls.close()
    rows = [r["event"] for r in world.log.rows()]
    assert rows[:3] == ["connect", "tls_handshake_ok", "poll"]


def test_without_the_fixture_root_the_handshake_fails_and_is_recorded(world):
    # Negative control: the fixture root is not in any default store, so the SAME
    # tunnel fails verification — SSL_CERT_FILE is what makes the chain acceptable.
    with pytest.raises(ssl.SSLCertVerificationError):
        _tls_through_proxy(world, None)
    failed = world.log.wait_for(lambda r: r["event"] == "tls_handshake_failed", 5)
    assert failed is not None, world.log.rows()


def test_a_different_root_does_not_validate_the_leaf(world):
    other = lt.Pki()
    try:
        with pytest.raises(ssl.SSLCertVerificationError):
            _tls_through_proxy(world, other.ca_bundle)
    finally:
        other.close()


def test_the_bundle_holds_one_root_and_no_private_key(world):
    text = world.pki.ca_bundle.read_text()
    assert text.count("BEGIN CERTIFICATE") == 1
    assert "PRIVATE KEY" not in text


@pytest.mark.parametrize("authority", ["example.com:443", "api.telegram.org:8443", "127.0.0.1:443", "api.telegram.org.evil.test:443"])
def test_proxy_refuses_every_other_authority_and_records_it(world, authority):
    sock, reply = _connect(world.proxy.port, authority)
    sock.close()
    assert reply.startswith(b"HTTP/1.1 403"), reply
    refused = world.proxy.refused()
    assert [r["target"] for r in refused] == [authority]
    # Never dialled: no tunnel, no handshake at the fake server.
    assert not [r for r in world.log.rows() if r["event"] in ("connect", "tls_handshake_ok")]


def test_proxy_refuses_plain_http_and_does_not_record_the_path(world):
    sock = socket.create_connection(("127.0.0.1", world.proxy.port), timeout=10)
    secret_path = "/bot999:SHOULD-NOT-BE-LOGGED/getMe?x=1"
    sock.sendall(f"GET http://example.com{secret_path} HTTP/1.1\r\nHost: example.com\r\n\r\n".encode())
    reply = sock.recv(4096)
    sock.close()
    assert reply.startswith(b"HTTP/1.1 403")
    refused = world.proxy.refused()
    assert len(refused) == 1 and refused[0]["method"] == "GET"
    assert "SHOULD-NOT-BE-LOGGED" not in json.dumps(world.log.rows())


def test_wrong_token_is_rejected_and_never_logged(world):
    tls = _tls_through_proxy(world, world.pki.ca_bundle)
    status, _ = _post(tls, "999:WRONG-TOKEN-VALUE", "GetUpdates", {"offset": 0})
    assert status == 401
    rows = world.log.rows()
    assert [r for r in rows if r["event"] == "rejected" and r["token_ok"] is False]
    assert "WRONG-TOKEN-VALUE" not in json.dumps(rows) and TOKEN not in json.dumps(rows)


def test_unknown_bot_api_method_is_a_recorded_failure_not_a_silent_ok(world):
    tls = _tls_through_proxy(world, world.pki.ca_bundle)
    status, _ = _post(tls, TOKEN, "GetMe", {})
    assert status == 404
    assert [r["method"] for r in world.log.rows() if r["event"] == "unknown"] == ["getme"]


def test_poll_offset_confirms_updates_exactly_once(world):
    world.api.serve_update(1001, 202, "denied")
    world.api.serve_update(1002, 101, "allowed")
    first = world.api.take_updates(0, 0.05)
    assert [u["update_id"] for u in first] == [1001, 1002]
    # Redelivered until confirmed: offset N confirms (drops) every id below N.
    assert [u["update_id"] for u in world.api.take_updates(1001, 0.05)] == [1001, 1002]
    assert [u["update_id"] for u in world.api.take_updates(1002, 0.05)] == [1002]
    assert world.api.take_updates(1003, 0.05) == []


def test_send_message_row_keeps_exact_text_and_chat(world):
    tls = _tls_through_proxy(world, world.pki.ca_bundle)
    text = "J7 Telegram reply: design review confirmed."
    status, body = _post(tls, TOKEN, "SendMessage", {"chat_id": 101, "text": text})
    assert status == 200 and body["result"]["text"] == text
    (send,) = [r for r in world.log.rows() if r["event"] == "send"]
    assert (send["chat_id"], send["text"]) == (101, text)


def test_child_env_routes_through_the_proxy_and_never_exempts_telegram(world):
    base = {"HOME": "/tmp/x", "ALL_PROXY": "socks5://inherited:1", "TELOXIDE_API_URL": "http://elsewhere"}
    env = world.env(base)
    assert env["HTTPS_PROXY"] == env["https_proxy"] == env["HTTP_PROXY"] == world.proxy.url
    assert env["SSL_CERT_FILE"] == str(world.pki.ca_bundle)
    assert "ALL_PROXY" not in env and "TELOXIDE_API_URL" not in env and "SSL_CERT_DIR" not in env
    assert "telegram" not in env["NO_PROXY"] and "127.0.0.1" in env["NO_PROXY"]
    assert base["ALL_PROXY"], "the caller's mapping is not mutated"


@pytest.mark.parametrize(
    ("hex_addr", "loopback"),
    [
        ("0100007F", True),
        ("0101A8C0", False),
        ("00000000000000000000000001000000", True),
        ("0000000000000000FFFF00000100007F", True),
        ("0000000000000000FFFF00000101A8C0", False),
        ("B80D01200000000000000000FFFF0000", False),
    ],
)
def test_proc_net_address_decoding(hex_addr, loopback):
    assert lt._hex_ip_is_loopback(hex_addr) is loopback


def test_egress_audit_ignores_loopback_and_flags_a_non_loopback_peer():
    import os

    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    client = socket.create_connection(listener.getsockname())
    log = lt.EventLog()
    audit = lt.EgressAudit(os.getpid(), log)
    try:
        audit.sample()
        assert log.rows() == [], "loopback peers are not egress"
        # A connected UDP socket sends no packet: it only records a (documentation
        # range, TEST-NET-1) peer, which the audit must flag.
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            probe.connect(("192.0.2.1", 9))
        except OSError:
            pytest.skip("no route: the kernel refuses a non-loopback UDP connect here")
        audit.sample()
        probe.close()
        flagged = [r for r in log.rows() if r["event"] == "egress_nonloopback"]
        assert len(flagged) == 1 and flagged[0]["table"] == "udp"
    finally:
        client.close()
        listener.close()


# ── The J7 provider: persona must be at the START of decoded `system` ─────────


def test_persona_prefix_predicate_is_exact_and_system_only():
    persona = "# J7 Dana fixture persona\nAnswer briefly."
    assert provider_stub.has_persona_prefix(persona + "\n\nrest", persona)
    assert not provider_stub.has_persona_prefix("preamble\n" + persona, persona)
    assert not provider_stub.has_persona_prefix(None, persona)
    assert provider_stub.decoded_system({"system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}) == "ab"
    assert provider_stub.decoded_system({"messages": [{"role": "user", "content": persona}]}) is None


def _run_stub(directory: Path):
    directory.mkdir()
    persona = directory / "persona.md"
    persona.write_text("# J7 Dana fixture persona\nAnswer briefly.\n")
    log = directory / "requests.jsonl"
    scene = J7 / "scenes" / "j7-dana.json"
    proc = subprocess.Popen(
        [sys.executable, str(J7 / "provider_stub.py"), "--scene", str(scene), "--port", "0",
         "--log", str(log), "--persona-file", str(persona)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    url = proc.stdout.readline().split(" ", 1)[1].strip()
    proc.stdout.readline()
    return proc, url, log


def _turn(url: str, system, user: str) -> None:
    parsed = urllib.parse.urlparse(url)
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=10)
    body = {"model": "m", "stream": True, "max_tokens": 64, "messages": [{"role": "user", "content": user}]}
    if system is not None:
        body["system"] = system
    conn.request("POST", "/v1/messages", json.dumps(body), {"x-api-key": "j7-dana-key", "content-type": "application/json"})
    conn.getresponse().read()
    conn.close()


def _observe(directory: Path, system) -> dict:
    proc, url, log = _run_stub(directory)
    try:
        _turn(url, system, "J7 Dana: confirm my design review.")
    finally:
        proc.terminate()
        proc.wait(timeout=15)
    (row,) = [json.loads(line) for line in log.read_text().splitlines()]
    return row


def test_stub_flags_persona_only_when_it_leads_the_system_field(tmp_path):
    persona = "# J7 Dana fixture persona\nAnswer briefly."
    # Keystone control: persona text only in the USER message, system generic -> False.
    wrong = _observe(tmp_path / "a", "You are generic.\n" + persona)
    assert wrong["turn"] == 1 and wrong["system_has_persona_prefix"] is False
    # The persona at the start of `system` -> True.
    right = _observe(tmp_path / "b", persona + "\n\nrest of the prompt")
    assert right["turn"] == 1 and right["system_has_persona_prefix"] is True
    # Absent system field -> False (a missing persona file cannot pass).
    assert _observe(tmp_path / "c", None)["system_has_persona_prefix"] is False


# ── The J7 fixture profile ────────────────────────────────────────────────────


def test_fixture_profile_is_not_a_preview_child():
    """A child of ``personal-assistant`` inherits ``preview = true``, and a preview
    profile rewrites ``telegram -> terminal`` and ``cron -> none`` (profile_loader.rs
    ``build_selection``): the daemon would then never poll Telegram. The fixture must
    therefore compose the same selection on ``base``."""
    try:
        import tomllib
    except ModuleNotFoundError:  # Python 3.10 (CI)
        import tomli as tomllib

    profile = tomllib.loads((J7 / "profiles" / "j7-dana.toml").read_text())
    assert profile["extends"] == "base" and not profile.get("preview")
    assert profile["persona"] == {"adapter": "personal-assistant", "config": {"file": "j7-dana.md"}}
    assert profile["channels"]["adapter"] == "telegram"
    assert profile["channels"]["config"] == {"allowed_chat_ids": [101]}
    assert "bot_token" not in profile["channels"]["config"]
    assert profile["scheduler"]["adapter"] == "cron"


# ── AC5: budget fixture and the evidence redaction guard ──────────────────────


def test_budget_fixture_prices_the_seeded_row_at_exactly_one_dollar(tmp_path):
    import json
    try:
        import tomllib
    except ModuleNotFoundError:  # Python 3.10 (CI)
        import tomli as tomllib

    import world as j7world

    w = object.__new__(j7world.World)  # fixture files only: no processes, no PKI
    w.cfg, w.data = tmp_path / "cfg", tmp_path / "data"
    w.cfg.mkdir()
    w.data.mkdir()
    w.configure_budget(1.0)
    w.seed_spend()
    config = tomllib.loads((w.cfg / "config.toml").read_text())
    assert config["budget"] == {"daily_limit_usd": 1.0}
    price = config["pricing"]["j7-priced-model"]
    (row,) = [json.loads(line) for line in (w.data / "usage" / "j7-seed.jsonl").read_text().splitlines()]
    assert row["model"] == "j7-priced-model" and row["usage"]["tokensOut"] == 0
    cost = row["usage"]["tokensIn"] * price["input_per_million"] / 1_000_000
    assert cost == 1.0
    # Dated now (the current local day), so the daemon's day-start read includes it.
    import time

    assert abs(row["timestampMs"] / 1000 - time.time()) < 60


@pytest.mark.parametrize(
    "text",
    ["leaked SYNTHETIC-CANARY-CREDENTIAL-0001 here", "POST /bot1:x/GetUpdates", "X-Api-Key: v", "Authorization: Bearer v"],
)
def test_evidence_guard_refuses_synthetic_canaries_and_writes_nothing(tmp_path, text):
    target = tmp_path / "evidence.txt"
    with pytest.raises(eg.RedactionError):
        eg.write_checked(target, text, ("SYNTHETIC-CANARY-CREDENTIAL-0001",))
    assert not target.exists()


def test_evidence_guard_passes_the_exact_witness_rows(tmp_path):
    target = tmp_path / "evidence.txt"
    text = (
        "[cron: morning-briefing] [cron: skipped — daily budget of $1.00 reached ($1.00 spent today)]\n"
        "Daily budget of $1.00 reached ($1.00 spent today). Raise [budget] daily_limit_usd or wait until tomorrow.\n"
    )
    eg.write_checked(target, text, ("SYNTHETIC-CANARY-CREDENTIAL-0001", "123456:J7-FIXTURE-NOT-A-REAL-TOKEN"))
    assert target.read_text() == text
