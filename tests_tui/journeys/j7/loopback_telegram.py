#!/usr/bin/env python3
"""Env-scoped loopback Telegram harness for the UNCHANGED binary (J7, D-37 ruling 5).

The shipped Telegram adapter builds ``teloxide::Bot::new(token)``: its API origin is
``https://api.telegram.org`` and there is no origin, client or proxy knob in the
product (Task 0, ``telegram.rs:51-84``). This module therefore does not touch the
product. It makes the *child process environment* do the work:

* ``HTTPS_PROXY`` (and ``HTTP_PROXY``) point at :class:`ConnectProxy`, a
  ``127.0.0.1`` CONNECT proxy;
* ``SSL_CERT_FILE`` points at a bundle holding ONE throwaway fixture root, which
  signs the ``api.telegram.org`` leaf served by :class:`FakeBotApi`;
* ``NO_PROXY`` keeps loopback traffic (the local provider stub) off the proxy, and
  never names ``api.telegram.org``.

There is NO certificate-verification relaxation anywhere: the binary validates the
fixture chain exactly as it would validate Telegram's. The proxy tunnels exactly one
authority, ``api.telegram.org:443``, to the fake server on loopback. Every other
CONNECT or plain-HTTP request is **refused with 403 and recorded** — that refusal
is the fail-closed control. A process that ignored the proxy variables would bypass
it; :class:`EgressAudit` samples the child's sockets for non-loopback peers as a
best-effort second net (it cannot see a connection shorter than its poll interval).

Only the Bot API calls the adapter makes are served (``telegram.rs``): ``getUpdates``
(the long poll) and ``sendMessage`` (replies, cron forwards, media refusal). Anything
else is a recorded ``unknown`` and a 404, which a driver treats as a failure.

Nothing here prints or logs the bot token; request rows carry a ``token_ok`` boolean.
"""

from __future__ import annotations

import json
import os
import select
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from socketserver import StreamRequestHandler, ThreadingTCPServer

TELEGRAM_HOST = "api.telegram.org"
TELEGRAM_AUTHORITY = f"{TELEGRAM_HOST}:443"
# The client's request timeout is 17 s (`net::default_reqwest_settings`) while the
# adapter asks Telegram to hold a poll for 30 s: an empty poll is released early so
# the long poll never trips the client timeout, and the loop is not a busy spin.
EMPTY_POLL_HOLD_SECS = 2.0


class HarnessError(RuntimeError):
    """The harness itself cannot be built or is unsafe to run (fail before launch)."""


# ── PKI ──────────────────────────────────────────────────────────────────────


class Pki:
    """A throwaway root CA and an ``api.telegram.org`` leaf, made with ``openssl``.

    Generated per run inside a private directory; the keys never leave it and the
    directory is removed by :meth:`close`. ``ca_bundle`` is the file the child sees
    as ``SSL_CERT_FILE``: the root certificate only, never a key.
    """

    def __init__(self, directory: Path | None = None, *, leaf_host: str = TELEGRAM_HOST) -> None:
        if shutil.which("openssl") is None:
            raise HarnessError("openssl is required to mint the fixture CA (not found on PATH)")
        self._owned = directory is None
        self.dir = Path(directory) if directory else Path(tempfile.mkdtemp(prefix="j7-pki-"))
        self.dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.ca_key = self.dir / "root.key"
        self.ca_bundle = self.dir / "root.pem"
        self.leaf_key = self.dir / "leaf.key"
        self.leaf_chain = self.dir / "leaf.pem"
        self._mint(leaf_host)

    def _openssl(self, *args: str) -> None:
        done = subprocess.run(
            ["openssl", *args], cwd=self.dir, capture_output=True, text=True, timeout=30
        )
        if done.returncode != 0:
            raise HarnessError(f"openssl {args[0]} failed: {done.stderr.strip()[:300]}")

    def _mint(self, leaf_host: str) -> None:
        self._openssl(
            "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
            "-nodes", "-keyout", str(self.ca_key), "-out", str(self.ca_bundle),
            "-days", "2", "-subj", "/CN=J7 loopback fixture root",
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        )
        csr = self.dir / "leaf.csr"
        self._openssl(
            "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
            "-nodes", "-keyout", str(self.leaf_key), "-out", str(csr),
            "-subj", f"/CN={leaf_host}",
        )
        ext = self.dir / "leaf.ext"
        ext.write_text(
            f"subjectAltName=DNS:{leaf_host}\n"
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature\n"
            "extendedKeyUsage=serverAuth\n"
        )
        leaf_cert = self.dir / "leaf.crt"
        self._openssl(
            "x509", "-req", "-in", str(csr), "-CA", str(self.ca_bundle),
            "-CAkey", str(self.ca_key), "-CAcreateserial", "-out", str(leaf_cert),
            "-days", "2", "-extfile", str(ext),
        )
        self.leaf_chain.write_text(leaf_cert.read_text())

    def close(self) -> None:
        if self._owned:
            shutil.rmtree(self.dir, ignore_errors=True)


# ── Recording ────────────────────────────────────────────────────────────────


class EventLog:
    """Ordered, thread-safe event rows. Rows never carry the bot token."""

    def __init__(self) -> None:
        self._rows: list[dict] = []
        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)

    def add(self, row: dict) -> dict:
        with self._cond:
            row = {"n": len(self._rows) + 1, **row}
            self._rows.append(row)
            self._cond.notify_all()
            return row

    def rows(self) -> list[dict]:
        with self._lock:
            return list(self._rows)

    def wait_for(self, predicate, deadline_secs: float):
        """Structural wait: return the first row for which ``predicate`` holds."""
        end = time.monotonic() + deadline_secs
        with self._cond:
            while True:
                for row in self._rows:
                    if predicate(row):
                        return row
                remaining = end - time.monotonic()
                if remaining <= 0:
                    return None
                self._cond.wait(remaining)


# ── Fake Bot API (TLS, loopback) ─────────────────────────────────────────────


def _method_key(path_tail: str) -> str:
    """teloxide-core sends PascalCase method names (``GetUpdates``); Telegram's are
    case-insensitive, so the match is too."""
    return path_tail.lower()


class _BotApiHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    api: "FakeBotApi"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003 - stdlib hook
        """Silent: the harness records structured rows instead."""

    def _json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802 - stdlib hook
        parts = [p for p in self.path.split("?")[0].split("/") if p]
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length > 0 else b""
        if len(parts) != 2 or not parts[0].startswith("bot"):
            self.api.log.add({"event": "unknown", "method": "POST", "path_shape": "unexpected"})
            self._json(404, {"ok": False, "error_code": 404, "description": "Not Found"})
            return
        token_ok = parts[0][3:] == self.api.expected_token
        method = _method_key(parts[1])
        try:
            body = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            body = None
        if not token_ok or not isinstance(body, dict):
            self.api.log.add(
                {"event": "rejected", "method": method, "token_ok": token_ok, "body_json": body is not None}
            )
            self._json(401 if not token_ok else 400,
                       {"ok": False, "error_code": 401 if not token_ok else 400, "description": "Unauthorized"})
            return
        if method == "getupdates":
            self._get_updates(body)
        elif method == "sendmessage":
            self._send_message(body)
        else:
            self.api.log.add({"event": "unknown", "method": method, "token_ok": token_ok})
            self._json(404, {"ok": False, "error_code": 404, "description": "Not Found"})

    def _get_updates(self, body: dict) -> None:
        offset = int(body.get("offset") or 0)
        self.api.log.add({"event": "poll", "offset": offset, "timeout": body.get("timeout")})
        updates = self.api.take_updates(offset, EMPTY_POLL_HOLD_SECS)
        self._json(200, {"ok": True, "result": updates})

    def _send_message(self, body: dict) -> None:
        chat_id = body.get("chat_id")
        text = body.get("text")
        row = self.api.log.add(
            {
                "event": "send",
                "chat_id": chat_id,
                "text": text,
                "parse_mode": body.get("parse_mode"),
            }
        )
        self._json(
            200,
            {
                "ok": True,
                "result": {
                    "message_id": 5000 + row["n"],
                    "date": int(time.time()),
                    "chat": {"id": chat_id, "type": "private", "first_name": "Fixture"},
                    "text": text,
                },
            },
        )


class _TlsHttpServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, api: "FakeBotApi", context: ssl.SSLContext) -> None:
        handler = type("BoundBotApiHandler", (_BotApiHandler,), {"api": api})
        super().__init__(("127.0.0.1", 0), handler)
        self.api = api
        self.context = context

    def finish_request(self, request, client_address) -> None:
        # Handshake on the per-connection thread so a stalled client cannot block
        # accept(), and so a failed handshake is a RECORDED fact (assumption (b)).
        try:
            request.settimeout(10)
            tls = self.context.wrap_socket(request, server_side=True)
            tls.settimeout(None)
        except (ssl.SSLError, OSError) as exc:
            reason = getattr(exc, "reason", None) or type(exc).__name__
            self.api.log.add({"event": "tls_handshake_failed", "reason": str(reason)})
            return
        self.api.log.add({"event": "tls_handshake_ok", "version": tls.version()})
        try:
            self.RequestHandlerClass(tls, client_address, self)
        except (OSError, ssl.SSLError):
            pass


class FakeBotApi:
    """TLS server for the two Bot API calls the adapter makes, on ``127.0.0.1``."""

    def __init__(self, pki: Pki, log: EventLog, expected_token: str) -> None:
        self.log = log
        self.expected_token = expected_token
        self._updates: list[dict] = []
        self._cond = threading.Condition()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(str(pki.leaf_chain), str(pki.leaf_key))
        self._server = _TlsHttpServer(self, context)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)

    def start(self) -> "FakeBotApi":
        self._thread.start()
        return self

    def serve_update(self, update_id: int, chat_id: int, text: str) -> None:
        """Queue one inbound text update; it is served until a poll offset passes it."""
        update = {
            "update_id": update_id,
            "message": {
                "message_id": update_id,
                "date": int(time.time()),
                "chat": {"id": chat_id, "type": "private", "first_name": "Fixture"},
                "from": {"id": chat_id, "is_bot": False, "first_name": "Fixture"},
                "text": text,
            },
        }
        with self._cond:
            self._updates.append(update)
            self._cond.notify_all()

    def take_updates(self, offset: int, hold_secs: float) -> list[dict]:
        """Telegram semantics: an offset confirms (drops) every update below it."""
        end = time.monotonic() + hold_secs
        with self._cond:
            while True:
                self._updates = [u for u in self._updates if u["update_id"] >= offset]
                if self._updates:
                    return list(self._updates)
                remaining = end - time.monotonic()
                if remaining <= 0:
                    return []
                self._cond.wait(remaining)

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


# ── CONNECT proxy (loopback, fail-closed) ────────────────────────────────────


class _ProxyHandler(StreamRequestHandler):
    proxy: "ConnectProxy"

    def handle(self) -> None:
        self.request.settimeout(10)
        try:
            request_line = self.rfile.readline(8192).decode("latin-1").strip()
            headers = []
            while True:
                line = self.rfile.readline(8192)
                if line in (b"\r\n", b"\n", b""):
                    break
                headers.append(line.decode("latin-1").strip())
        except (OSError, UnicodeDecodeError):
            return
        parts = request_line.split()
        method = parts[0].upper() if parts else ""
        target = parts[1] if len(parts) > 1 else ""
        if method == "CONNECT" and target.lower() == TELEGRAM_AUTHORITY:
            self.proxy.log.add({"event": "connect", "authority": TELEGRAM_AUTHORITY, "allowed": True})
            self._tunnel()
            return
        # Fail closed: never dial anything else. The target is recorded (host:port or
        # absolute URL origin — no path/query, so no token-bearing URL is retained).
        origin = target if method == "CONNECT" else target.split("/", 3)[:3]
        if isinstance(origin, list):
            origin = "/".join(origin)
        self.proxy.log.add({"event": "refused", "method": method, "target": origin[:200]})
        try:
            self.wfile.write(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        except OSError:
            pass

    def _tunnel(self) -> None:
        try:
            upstream = socket.create_connection(("127.0.0.1", self.proxy.upstream_port), timeout=5)
        except OSError:
            self.wfile.write(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            return
        with upstream:
            self.wfile.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            self.wfile.flush()
            client = self.request
            client.settimeout(None)
            upstream.settimeout(None)
            pair = (client, upstream)
            while True:
                readable, _, broken = select.select(pair, [], pair, 60)
                if broken or not readable:
                    return
                for sock in readable:
                    try:
                        data = sock.recv(65536)
                    except OSError:
                        return
                    if not data:
                        return
                    other = upstream if sock is client else client
                    try:
                        other.sendall(data)
                    except OSError:
                        return


class ConnectProxy:
    """``127.0.0.1`` HTTP proxy that tunnels ``api.telegram.org:443`` to the fake and
    refuses (403 + a ``refused`` row) everything else."""

    def __init__(self, log: EventLog, upstream_port: int) -> None:
        self.log = log
        self.upstream_port = upstream_port
        handler = type("BoundProxyHandler", (_ProxyHandler,), {"proxy": self})
        self._server = ThreadingTCPServer(("127.0.0.1", 0), handler)
        self._server.daemon_threads = True
        self._server.allow_reuse_address = True
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> "ConnectProxy":
        self._thread.start()
        return self

    def refused(self) -> list[dict]:
        return [r for r in self.log.rows() if r["event"] == "refused"]

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


# ── Egress audit (best effort) ───────────────────────────────────────────────


def _hex_ip_is_loopback(hex_addr: str) -> bool:
    """``/proc/net/tcp*`` prints addresses as 32-bit words in host (little-endian)
    byte order: ``127.0.0.1`` is ``0100007F`` and ``::1`` is
    ``00000000000000000000000001000000``. A v4-mapped v6 address keeps the v4 word
    last, so ``...FFFF0000`` + a ``7F``-ending word is a mapped ``127.x.x.x``."""
    word = hex_addr.upper()
    if len(word) == 8:
        return word.endswith("7F")
    if len(word) == 32:
        return word == "00000000000000000000000001000000" or (
            word[:24] == "0000000000000000FFFF0000" and word.endswith("7F")
        )
    return False


class EgressAudit(threading.Thread):
    """Sample a child's TCP sockets and record any non-loopback peer.

    Best effort, and labelled so: a connection that opens and closes between two
    samples is invisible. It exists to catch a process that ignores the proxy
    variables and dials out directly, which the proxy alone cannot see.
    """

    def __init__(self, pid: int, log: EventLog, interval: float = 0.05) -> None:
        super().__init__(daemon=True)
        self.pid = pid
        self.log = log
        self.interval = interval
        self.samples = 0
        self._halt = threading.Event()
        self._seen: set[tuple[str, str]] = set()

    def _inodes(self) -> set[str]:
        found: set[str] = set()
        fd_dir = Path(f"/proc/{self.pid}/fd")
        try:
            for entry in fd_dir.iterdir():
                try:
                    target = os.readlink(entry)
                except OSError:
                    continue
                if target.startswith("socket:["):
                    found.add(target[8:-1])
        except OSError:
            pass
        return found

    def sample(self) -> None:
        inodes = self._inodes()
        if not inodes:
            return
        for table in ("tcp", "tcp6", "udp", "udp6"):
            try:
                lines = Path(f"/proc/{self.pid}/net/{table}").read_text().splitlines()[1:]
            except OSError:
                continue
            for line in lines:
                cols = line.split()
                if len(cols) < 10 or cols[9] not in inodes:
                    continue
                remote_ip, _, remote_port = cols[2].partition(":")
                if int(remote_port, 16) == 0 and set(remote_ip) <= {"0"}:
                    continue  # listening/unconnected
                if _hex_ip_is_loopback(remote_ip):
                    continue
                key = (table, cols[2])
                if key not in self._seen:
                    self._seen.add(key)
                    self.log.add({"event": "egress_nonloopback", "table": table, "peer": cols[2]})

    def run(self) -> None:
        while not self._halt.is_set():
            self.sample()
            self.samples += 1
            self._halt.wait(self.interval)

    def stop(self) -> None:
        self._halt.set()


# ── Child environment ────────────────────────────────────────────────────────


def child_env(
    *,
    base: dict[str, str],
    proxy_url: str,
    ca_bundle: Path,
) -> dict[str, str]:
    """Add ONLY the loopback-harness variables to an already explicit environment.

    ``base`` must be an allowlisted environment (never ``os.environ``). Lower-case
    twins are set because reqwest honours either spelling and a stale inherited one
    must not win. ``SSL_CERT_DIR`` is removed: rustls-native-certs loads ONLY the
    paths named by ``SSL_CERT_FILE``/``SSL_CERT_DIR`` when either is set, so the
    fixture file is then the only trust input.
    """
    env = dict(base)
    for name in ("HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"):
        env[name] = proxy_url
    for name in ("ALL_PROXY", "all_proxy", "TELOXIDE_PROXY", "TELOXIDE_API_URL"):
        env.pop(name, None)
    env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost,::1"
    env["SSL_CERT_FILE"] = str(ca_bundle)
    env.pop("SSL_CERT_DIR", None)
    return env


class LoopbackTelegram:
    """Everything the harness needs, started and stopped together."""

    def __init__(self, expected_token: str) -> None:
        self.log = EventLog()
        self.pki = Pki()
        self.api = FakeBotApi(self.pki, self.log, expected_token)
        self.proxy = ConnectProxy(self.log, self.api.port)

    def start(self) -> "LoopbackTelegram":
        self.api.start()
        self.proxy.start()
        return self

    def env(self, base: dict[str, str]) -> dict[str, str]:
        return child_env(base=base, proxy_url=self.proxy.url, ca_bundle=self.pki.ca_bundle)

    def stop(self) -> None:
        self.proxy.stop()
        self.api.stop()
        self.pki.close()

    def __enter__(self) -> "LoopbackTelegram":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()
