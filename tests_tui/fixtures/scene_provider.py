#!/usr/bin/env python3
"""Deterministic scene provider — an Anthropic Messages *and* OpenAI-compat stand-in.

The real `rustain` binary talks to this process over HTTP exactly as it would talk
to Anthropic: `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` and an `ANTHROPIC_API_KEY`
that is *also* the scene key. No provider account, no network, byte-stable replay.

⛔ What the stub says is NOT evidence. What the binary sent, wrote or journaled is.
The product-side evidence this process makes available is its **request log**: every
request the binary made. A gate assertion may rest on the log, the usage ledger, the
session files and screen *state changes* — never on prose the stub was told to serve.

Usage::

    python3 scene_provider.py --scene <file> --port 0 --log <jsonl>

It prints exactly two lines, in this order::

    listening http://127.0.0.1:<port>
    scene sha256:<hex>

…then serves until SIGTERM/SIGINT. Exit status: **0** on a clean scene, **1** if any
desync or unknown path occurred, **2** if the scene file itself is unusable (that
verdict is printed before `listening`, so a driver can tell the two apart).

Endpoints answered (nothing else — any other verb or path is logged
`"kind":"unknown"` + 404, or 400 for an unparseable body, and forces exit 1):

* ``POST /v1/messages`` with a ``stream`` field  → a scene turn, served as SSE.
* ``POST /v1/messages`` without a ``stream`` field **and** with ``max_tokens: 1``
  → the boot ``health_check``, answered 200 and **never consuming a scene turn**.
  A stream-less body of any other shape is out of contract and is answered loud.
* ``POST /v1/messages`` with ``max_tokens: 30``, no tools and the system prompt
  ``"Generate a concise title…"`` → the product's one-shot **title call**
  (``title_trigger = conversation.turns.len() == 2``, `event_loop.rs`), answered with
  a fixed title and **never consuming a scene turn**. It bypasses ``run_turn``, so it
  writes no ledger row — the ledger stays one row per user turn.
* ``GET /v1/models`` (also ``/models``)  → ``200 {"data":[]}`` — the connectivity
  probe (`doctor`/`auth` use it, and a 404 there fills a receipt with WARNs).
  The alias mirrors ``OPENAI_PATHS`` below: the OpenAI adapter's health check is
  ``GET {base_url}/models`` (``openai/mod.rs``), so a ``base_url`` without the
  ``/v1`` suffix probes instead of 404ing into ``kind:"unknown"`` + exit 1.
* ``GET /api.json``   → ``200 {}`` (models.dev; point ``RUSTAIN_MODELS_DEV_URL`` here
  or the default ``models-dev`` feature reaches the real host on a stale cache).
* ``POST /v1/chat/completions`` (also ``/chat/completions``) with ``"stream": true``
  → a scene turn served on the **OpenAI wire** (Story 19.9 A11), for a persona keyed
  by the presented **Bearer** value — and the scheme is policed: a credential
  presented any other way on this endpoint (``x-api-key``, none) is a loud 400 +
  desync row, never a served turn. This is the wire the OpenRouter / OpenAI-compat
  adapter speaks (``POST {base_url}/chat/completions``, ``openai/mod.rs``), and its
  health check is the same ``GET {base_url}/models`` below — so a config-path
  ``[provider.openrouter] base_url = "<stub>/v1"`` boots and streams against this
  process with no product change. **Text turns only** in this cut: a ``tool_calls``
  arm is future work, and a persona declared ``"wire": "openai"`` whose turns script
  a ``tool_use`` block is a load-time error (exit 2).

── Scene file format (pinned; 19.8/19.9/19.10/19.13/19.26 consume it) ────────────

    {"scene": "<name>",
     "personas": {
       "<api-key-value>": {
         "wire": "anthropic" | "openai",        # optional, default "anthropic"
         "turns": [
           {"expect": "<substring of last user text>",
            "content": [ {"type":"text","text":"…"}
                       | {"type":"tool_use","name":"<tool>","input":{…}} ],
            "stop_reason": "end_turn"|"tool_use",
            "usage": {"input_tokens": N, "output_tokens": N}}
         ]}}}

A tool turn ends ``stop_reason: "tool_use"`` and the binary's next POST carries the
``tool_result``; the scene's next turn ``expect``s text from that result or from the
user. Shorthand: a top-level ``"turns"`` is persona ``"*"`` (any key).

``wire`` names which endpoint a persona is served on, and is validated at load:
an ``"openai"`` persona may script text turns only (exit 2 otherwise), and a POST
that reaches the *other* wire's endpoint for a declared persona is a **desync**,
not a silent cross-serve — so a mis-routed capture fails loudly instead of
passing on the wrong credential.

Personas are keyed by **the api-key value the binary presents** — that is how eight
personas share one stub with zero product config: eight data dirs, eight key values,
one process. Turns are consumed **sequentially per persona**. A presented value the
scene does not declare resolves to ``"persona": "<unknown>"``, is a desync, and its
value is **never written to the log** (it may be a real credential).

An ``expect`` mismatch is a **scene desync**: the stub answers a 200 SSE ``error``
event (``{"type":"error","error":{"type":"scene_desync","message":"turn N expected …"}}``),
logs ``"desync": true``, does not advance the turn cursor, and exits 1. A desync is
the product sending something the scene did not script — a real finding, never
auto-advanced past. A capture therefore never has to script the title call, and the
stub's own desync message can never satisfy a later ``expect`` by being rendered by
the product and fed back to the stub.

── Request log format (JSONL, one object per request) ────────────────────────────

    {"n":1,"method":"POST","path":"/v1/messages","persona":"scene-sam",
     "auth":"x-api-key","stream":true,"model":"claude-sonnet-4-6","turn":1,
     "last_user":"<≤120 chars>","tools":12,"tool_results":0,"desync":false}

An OpenAI-wire row carries ``"path":"/v1/chat/completions"``, ``"auth":"bearer"``
and ``"wire":"openai"`` alongside the same ``stream``/``model``/``turn``/
``last_user``/``tools``/``tool_results``/``desync`` fields. Anthropic-wire rows are
byte-identical to 19.7's and carry no ``wire`` field.

Boot calls log ``"kind":"health_check"`` (``stream`` absent, ``max_tokens`` 1),
``"kind":"probe"`` (``GET /v1/models``), ``"kind":"models_dev"`` (``GET /api.json``)
and ``"kind":"title"`` (the product's title call); any other path logs
``"kind":"unknown"`` + 404. A scene-turn row carries ``turn`` and no ``kind``, which
is how a consumer selects the rows that are user turns.

⛔ No header values, no request bodies beyond ``last_user``, no timestamps claimed as
product-minted (the stub's clock is the shell's, not the binary's).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import signal
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ANY_PERSONA = "*"
UNKNOWN_PERSONA = "<unknown>"
LAST_USER_MAX = 120
VALID_STOP_REASONS = ("end_turn", "tool_use")
TITLE_SYSTEM_PREFIX = "Generate a concise title"
TITLE_TEXT = "Scene capture"
WIRE_ANTHROPIC = "anthropic"
WIRE_OPENAI = "openai"
VALID_WIRES = (WIRE_ANTHROPIC, WIRE_OPENAI)


class SceneError(Exception):
    """The scene file is unusable — exit 2 before serving."""


# ── Scene ────────────────────────────────────────────────────────────────────


class Scene:
    """A validated scene: per-persona turn lists plus per-persona cursors."""

    def __init__(self, path: Path) -> None:
        raw = path.read_bytes()
        self.path = path
        self.sha256 = hashlib.sha256(raw).hexdigest()
        try:
            doc = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise SceneError(f"{path}: not valid JSON: {exc}") from exc
        if not isinstance(doc, dict):
            raise SceneError(f"{path}: top level must be an object")

        self.name = doc.get("scene", path.stem)
        personas = doc.get("personas")
        if personas is None:
            turns = doc.get("turns")
            if turns is None:
                raise SceneError(f"{path}: needs a 'personas' object or a top-level 'turns' list")
            personas = {ANY_PERSONA: {"turns": turns}}
        if not isinstance(personas, dict) or not personas:
            raise SceneError(f"{path}: 'personas' must be a non-empty object")

        self.turns: dict[str, list[dict]] = {}
        self.wires: dict[str, str] = {}
        for key, body in personas.items():
            if not isinstance(body, dict) or not isinstance(body.get("turns"), list):
                raise SceneError(f"{path}: persona {key!r} must be an object with a 'turns' list")
            wire = body.get("wire", WIRE_ANTHROPIC)
            if wire not in VALID_WIRES:
                raise SceneError(
                    f"{path}: persona {key!r} declares wire {wire!r}; must be one of {VALID_WIRES}"
                )
            for i, turn in enumerate(body["turns"], start=1):
                _validate_turn(path, key, i, turn)
                # A11: the OpenAI arm serves TEXT turns only in this cut. A
                # tool_calls arm is future work, so a scene that scripts one is
                # an authoring error caught at load — never a turn served with
                # its tool_use silently dropped.
                if wire == WIRE_OPENAI and any(
                    block.get("type") == "tool_use" for block in turn["content"]
                ):
                    raise SceneError(
                        f"{path}: persona {key!r} turn {i} scripts a tool_use block, but this"
                        f" persona is declared on the {WIRE_OPENAI!r} wire, which serves TEXT"
                        " turns only (Story 19.9 A11 — a tool_calls arm is future work)"
                    )
            self.turns[key] = body["turns"]
            self.wires[key] = wire

        self._cursor: dict[str, int] = {key: 0 for key in self.turns}
        self._lock = threading.Lock()

    def resolve_persona(self, presented: str | None) -> str:
        """Map a presented credential to a declared scene key — or `<unknown>`.

        The presented value is returned ONLY when the scene declares it, so a real
        credential can never become a log field (A5).
        """
        if presented is not None and presented in self.turns:
            return presented
        if ANY_PERSONA in self.turns:
            return ANY_PERSONA
        return UNKNOWN_PERSONA

    def take_turn(self, persona: str, haystack: str) -> tuple[int, dict | None, str]:
        """Claim the next turn for `persona`.

        Returns `(turn_number, turn_or_None, reason)`. A `None` turn is a desync and
        the cursor does NOT advance — the product sent something unscripted.
        """
        turns = self.turns.get(persona)
        if turns is None:
            return 0, None, f"persona {persona} is not declared by scene {self.name}"
        with self._lock:
            index = self._cursor[persona]
            number = index + 1
            if index >= len(turns):
                return number, None, (
                    f"turn {number} expected nothing — scene {self.name} scripts "
                    f"{len(turns)} turn(s) for this persona"
                )
            turn = turns[index]
            expect = turn["expect"]
            if expect not in haystack:
                return number, None, (
                    f"turn {number} expected {expect!r} in the last user text, got "
                    f"{_clip(haystack, LAST_USER_MAX)!r}"
                )
            self._cursor[persona] = index + 1
            return number, turn, ""


def _validate_turn(path: Path, persona: str, number: int, turn: object) -> None:
    where = f"{path}: persona {persona!r} turn {number}"
    if not isinstance(turn, dict):
        raise SceneError(f"{where}: must be an object")
    if not isinstance(turn.get("expect"), str) or not turn["expect"]:
        raise SceneError(f"{where}: needs a non-empty 'expect' string")
    content = turn.get("content")
    if not isinstance(content, list) or not content:
        raise SceneError(f"{where}: needs a non-empty 'content' list")
    for block in content:
        if not isinstance(block, dict):
            raise SceneError(f"{where}: every content block must be an object")
        kind = block.get("type")
        if kind == "text":
            if not isinstance(block.get("text"), str):
                raise SceneError(f"{where}: a text block needs a 'text' string")
        elif kind == "tool_use":
            if not isinstance(block.get("name"), str) or not block["name"]:
                raise SceneError(f"{where}: a tool_use block needs a 'name' string")
            if not isinstance(block.get("input"), dict):
                raise SceneError(f"{where}: a tool_use block needs an 'input' object")
        else:
            raise SceneError(f"{where}: unsupported content block type {kind!r}")
    has_tool_use = any(block.get("type") == "tool_use" for block in content)
    if turn.get("stop_reason") not in VALID_STOP_REASONS:
        raise SceneError(f"{where}: 'stop_reason' must be one of {VALID_STOP_REASONS}")
    # A11: a tool turn ends `tool_use`, a text-only turn ends `end_turn`. Serving a
    # tool_use block with `end_turn` makes the runtime take the end-turn branch and
    # never execute the tool — a scene-authoring error, caught at load (exit 2).
    if has_tool_use != (turn.get("stop_reason") == "tool_use"):
        raise SceneError(
            f"{where}: 'stop_reason' must match the content — tool_use blocks need"
            " 'tool_use', text-only turns need 'end_turn'"
        )
    usage = turn.get("usage")
    if not isinstance(usage, dict):
        raise SceneError(f"{where}: needs a 'usage' object")
    for field in ("input_tokens", "output_tokens"):
        value = usage.get(field)
        if not isinstance(value, int) or isinstance(value, bool):
            raise SceneError(f"{where}: usage.{field} must be an integer")
        # The adapter's wire type is u32 (`types.rs` InputUsage/OutputUsage): an
        # out-of-range value is skipped by the real parser and silently zeroes the
        # ledger row — a scene error here, not a silent serving defect.
        if not 0 <= value <= 4_294_967_295:
            raise SceneError(
                f"{where}: usage.{field} is outside the adapter's u32 wire range"
            )


# ── Request log ──────────────────────────────────────────────────────────────


class RequestLog:
    """Append-only JSONL record of what the *product* did. No header values, ever."""

    def __init__(self, path: Path) -> None:
        self._handle = path.open("a", encoding="utf-8")
        self._lock = threading.Lock()
        self._n = 0

    def write(self, row: dict) -> None:
        with self._lock:
            self._n += 1
            ordered = {"n": self._n}
            ordered.update(row)
            self._handle.write(json.dumps(ordered, separators=(",", ":")) + "\n")
            self._handle.flush()

    def close(self) -> None:
        with self._lock:
            self._handle.close()


# ── SSE ──────────────────────────────────────────────────────────────────────


def _event(payload: dict) -> bytes:
    """One SSE frame. The adapter dispatches on the JSON `type`, not the event name,
    but the name is emitted anyway because every real capture carries it."""
    return (
        f"event: {payload['type']}\n"
        f"data: {json.dumps(payload, separators=(',', ':'))}\n\n"
    ).encode()


def _turn_sse(scene: Scene, persona: str, number: int, turn: dict) -> bytes:
    """Render a scene turn as the SSE byte stream the Anthropic adapter parses.

    Shape taken from the adapter's own fixtures (`tests/anthropic_streaming.rs` for a
    text turn, `src/adapters/anthropic/stream.rs` for tool_use): `usage` rides in both
    `message_start` and `message_delta`, because only the LAST Usage chunk survives
    into the ledger row and a journey gate asserts `tokensOut` on it.
    """
    usage = turn["usage"]
    out = bytearray()
    out += _event({"type": "message_start", "message": {"usage": {"input_tokens": usage["input_tokens"]}}})
    for index, block in enumerate(turn["content"]):
        if block["type"] == "text":
            out += _event(
                {
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""},
                }
            )
            out += _event(
                {
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": block["text"]},
                }
            )
        else:
            tool_id = block.get("id") or f"toolu_{scene.name}_{persona}_{number}_{index}"
            out += _event(
                {
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "tool_use", "id": tool_id, "name": block["name"]},
                }
            )
            # Two fragments, like the adapter's fixture: tool input is assembled from
            # `partial_json` and parsed at content_block_stop — never read from
            # `content_block_start.input`.
            payload = json.dumps(block["input"], separators=(",", ":"))
            cut = max(1, len(payload) // 2)
            for fragment in (payload[:cut], payload[cut:]):
                out += _event(
                    {
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": fragment},
                    }
                )
        out += _event({"type": "content_block_stop", "index": index})
    out += _event(
        {
            "type": "message_delta",
            "delta": {"stop_reason": turn["stop_reason"]},
            "usage": {"output_tokens": usage["output_tokens"]},
        }
    )
    out += _event({"type": "message_stop"})
    return bytes(out)


def _desync_sse(reason: str) -> bytes:
    return _event({"type": "error", "error": {"type": "scene_desync", "message": reason}})


# ── SSE, OpenAI wire (Story 19.9 A11) ────────────────────────────────────────


def _openai_frame(payload: dict) -> bytes:
    """One OpenAI-style SSE frame. The OpenAI adapter carries no `event:` name —
    `SseLineBuffer` synthesises `"message"` — and dispatches purely on the JSON
    body, so only `data:` is emitted here."""
    return f"data: {json.dumps(payload, separators=(',', ':'))}\n\n".encode()


def _openai_turn_sse(scene: Scene, persona: str, number: int, turn: dict, model: str) -> bytes:
    """Render a TEXT scene turn as the SSE stream `OpenAiStreamTransformer` parses.

    Shape pinned from the adapter, not from docs (Story 19.9 T0.3(9)):

    * ``object`` is a REQUIRED field of ``OpenAiStreamEvent`` (`openai/types.rs`) —
      a chunk without it fails deserialization, the transformer logs a parse warning
      and returns no chunks, and the turn never completes.
    * ``choices[].delta.content`` becomes ``StreamChunk::Text``; ``finish_reason:
      "stop"`` becomes ``StreamChunk::TurnComplete`` (`openai/stream.rs`).
    * ``usage`` rides a FINAL chunk with ``choices: []`` — the shape the adapter's
      own comment names for ``stream_options.include_usage = true``, which
      `OpenAiRequest::from` always sets. It becomes ``StreamChunk::Usage``, and
      `run_turn` drains the whole stream before minting the ledger row, so a usage
      chunk after ``TurnComplete`` still lands in ``tokensOut``.
    * ``data: [DONE]`` is skipped by the transformer and is emitted last because
      every real capture carries it.
    """
    usage = turn["usage"]
    text = "".join(block["text"] for block in turn["content"] if block["type"] == "text")
    chunk_id = f"scene-{scene.name}-{persona}-{number}"

    def envelope(choices: list, extra: dict | None = None) -> dict:
        payload = {
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": choices,
        }
        if extra:
            payload.update(extra)
        return payload

    out = bytearray()
    out += _openai_frame(
        envelope([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}])
    )
    out += _openai_frame(
        envelope([{"index": 0, "delta": {"content": text}, "finish_reason": None}])
    )
    out += _openai_frame(envelope([{"index": 0, "delta": {}, "finish_reason": "stop"}]))
    out += _openai_frame(
        envelope(
            [],
            {
                "usage": {
                    "prompt_tokens": usage["input_tokens"],
                    "completion_tokens": usage["output_tokens"],
                    "total_tokens": usage["input_tokens"] + usage["output_tokens"],
                }
            },
        )
    )
    out += b"data: [DONE]\n\n"
    return bytes(out)


def _last_user_openai(body: dict) -> str:
    """Text of the last user-role message on the OpenAI wire.

    ``messages[].content`` is a plain string in the common case and a list of
    ``{"type":"text","text":…}`` parts in the multimodal case (`openai/types.rs`);
    both are searched so an ``expect`` cannot depend on which one the adapter chose.
    """
    messages = body.get("messages")
    if not isinstance(messages, list):
        return ""
    for message in reversed(messages):
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            return "\n".join(
                block["text"]
                for block in content
                if isinstance(block, dict)
                and block.get("type") == "text"
                and isinstance(block.get("text"), str)
            )
        return ""
    return ""


def _title_sse() -> bytes:
    """A minimal end_turn stream for the product's title call."""
    out = bytearray()
    out += _event({"type": "message_start", "message": {"usage": {"input_tokens": 0}}})
    out += _event(
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}
    )
    out += _event(
        {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": TITLE_TEXT},
        }
    )
    out += _event({"type": "content_block_stop", "index": 0})
    out += _event(
        {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 2},
        }
    )
    out += _event({"type": "message_stop"})
    return bytes(out)


def _is_title_call(body: dict) -> bool:
    """True for the product's title request, and for nothing else.

    All three conditions are the product's own constants (`generate_title` in
    `event_loop.rs`): a 30-token budget, no tools, and that system prompt. If the
    product ever changes them the request stops being classified and desyncs
    loudly — which is the failure direction a stub is allowed to have.
    """
    return (
        body.get("max_tokens") == 30
        and not body.get("tools")
        and str(body.get("system", "")).startswith(TITLE_SYSTEM_PREFIX)
    )


# ── Request inspection ───────────────────────────────────────────────────────


def _clip(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[:limit]


def _auth(headers) -> tuple[str, str | None]:
    """Return `(scheme, presented_value)`. The value never leaves this process except
    as a persona name the scene itself declares."""
    api_key = headers.get("x-api-key")
    if api_key:
        return "x-api-key", api_key
    authorization = headers.get("authorization")
    if authorization:
        token = authorization[7:] if authorization[:7].lower() == "bearer " else authorization
        return "bearer", token
    return "none", None


def _last_user(body: dict) -> tuple[str, int]:
    """Text of the last user-role message, plus its tool_result count.

    A tool-result follow-up carries no text block at all (the adapter builds it from
    `tool_results` only), so the searched text is the message's text blocks AND its
    tool_result contents — which is what "expects text from that result" means.
    """
    messages = body.get("messages")
    if not isinstance(messages, list):
        return "", 0
    for message in reversed(messages):
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        parts: list[str] = []
        tool_results = 0
        content = message.get("content")
        if isinstance(content, str):
            parts.append(content)
        elif isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "text" and isinstance(block.get("text"), str):
                    parts.append(block["text"])
                elif block.get("type") == "tool_result":
                    tool_results += 1
                    if isinstance(block.get("content"), str):
                        parts.append(block["content"])
        return "\n".join(parts), tool_results
    return "", 0


# ── Server ───────────────────────────────────────────────────────────────────


class SceneHandler(BaseHTTPRequestHandler):
    # HTTP/1.0: the body is close-delimited, so there is no chunked framing to get
    # wrong on an SSE response.
    protocol_version = "HTTP/1.0"

    scene: Scene
    log: RequestLog
    state: "ServerState"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003 - stdlib hook
        """Silence the stdlib access log: this process prints exactly two lines."""

    # ── helpers ──────────────────────────────────────────────────────────
    def _respond(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.end_headers()
        self.wfile.write(body)

    def _respond_or_mark(self, status: int, content_type: str, body: bytes) -> None:
        """Deliver a scripted response. A client that vanished mid-turn is a FAILED
        delivery, not a clean one: the turn was consumed and never received, so the
        run must not exit 0 over it."""
        try:
            self._respond(status, content_type, body)
        except (BrokenPipeError, ConnectionResetError):
            self.state.mark_failure()

    def _read_body(self) -> dict | None:
        """The body as an object, ``{}`` when empty, ``None`` when unparseable — an
        unparseable body is nothing the product sends and must stay loud."""
        length = int(self.headers.get("content-length") or 0)
        if length <= 0:
            return {}
        try:
            body = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return None
        return body if isinstance(body, dict) else None

    def _unknown(self) -> None:
        self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
        self.state.mark_failure()
        self._respond(404, "application/json", b'{"error":"unknown path"}')

    def _verb_not_allowed(self) -> None:
        """AC2(b): any verb outside GET/POST is as loud as an unknown path. The
        stdlib default is a silent 501 that logs nothing and marks nothing."""
        self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
        self.state.mark_failure()
        self.send_response(404)
        self.send_header("content-type", "application/json")
        if self.command == "HEAD":
            self.send_header("content-length", "0")
            self.end_headers()
            return
        self.end_headers()
        self.wfile.write(b'{"error":"unknown path"}')

    do_HEAD = _verb_not_allowed
    do_PUT = _verb_not_allowed
    do_PATCH = _verb_not_allowed
    do_DELETE = _verb_not_allowed
    do_OPTIONS = _verb_not_allowed

    # ── verbs ────────────────────────────────────────────────────────────
    def do_GET(self) -> None:  # noqa: N802 - stdlib hook
        if self.path in self.PROBE_PATHS:
            self.log.write({"method": "GET", "path": self.path, "kind": "probe"})
            self._respond(200, "application/json", b'{"data":[]}')
        elif self.path == "/api.json":
            self.log.write({"method": "GET", "path": self.path, "kind": "models_dev"})
            self._respond(200, "application/json", b"{}")
        else:
            self._unknown()

    OPENAI_PATHS = ("/v1/chat/completions", "/chat/completions")
    # Mirrors OPENAI_PATHS: the adapter's health check is `GET {base_url}/models`
    # (openai/mod.rs), so a config `base_url` without the `/v1` suffix — exactly
    # the shape the `/chat/completions` alias invites — probes instead of 404ing
    # into `kind:"unknown"` + stub exit 1 (review finding 2026-08-29).
    PROBE_PATHS = ("/v1/models", "/models")

    def do_POST(self) -> None:  # noqa: N802 - stdlib hook
        if self.path in self.OPENAI_PATHS:
            self._openai_completions()
            return
        if self.path != "/v1/messages":
            self._unknown()
            return

        body = self._read_body()
        if body is None:
            # Not JSON, or not an object: nothing the product ever sends. Loud —
            # it must not collapse into the stream-less health-check arm.
            self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
            self.state.mark_failure()
            self._respond(400, "application/json", b'{"error":"unparseable body"}')
            return
        scheme, presented = _auth(self.headers)

        if "stream" not in body and body.get("max_tokens") == 1:
            # The boot health check, pinned by its full shape (A2): no `stream`
            # field AND the 1-token budget. Any 2xx passes; no turn is consumed.
            self.log.write(
                {
                    "method": "POST",
                    "path": self.path,
                    "kind": "health_check",
                    "persona": self.scene.resolve_persona(presented),
                    "auth": scheme,
                    "model": body.get("model", ""),
                }
            )
            self._respond(
                200,
                "application/json",
                json.dumps(
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1},
                    },
                    separators=(",", ":"),
                ).encode(),
            )
            return

        if "stream" not in body:
            # Stream-less but not the pinned health-check shape: out of contract.
            # A real turn always carries `stream: true` (`types.rs`), so this arm
            # is reached only by contract drift — which must be loud, not swallowed.
            self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
            self.state.mark_failure()
            self._respond(404, "application/json", b'{"error":"unknown request"}')
            return

        if _is_title_call(body):
            # The product's one-shot title call — `title_trigger =
            # conversation.turns.len() == 2` in event_loop.rs — is a REAL provider
            # request that bypasses run_turn (so it writes no ledger row) and whose
            # prompt is "User: …\n\nAssistant: …", i.e. the conversation fed back.
            # It must not consume a scene turn: otherwise every capture with two user
            # turns desyncs, and an error message quoting an `expect` could satisfy a
            # later turn through the product's own rendering — the echo class, on the
            # provider side. Answered with the stub's own prose, which is never
            # evidence and never asserted.
            self.log.write(
                {
                    "method": "POST",
                    "path": self.path,
                    "kind": "title",
                    "persona": self.scene.resolve_persona(presented),
                    "auth": scheme,
                    "model": body.get("model", ""),
                }
            )
            self._respond(200, "text/event-stream", _title_sse())
            return

        persona = self.scene.resolve_persona(presented)
        if persona != UNKNOWN_PERSONA and self.scene.wires[persona] != WIRE_ANTHROPIC:
            # A declared persona reaching the WRONG wire is a mis-route, not a
            # cross-serve: the credential and the endpoint disagree about which
            # provider the product thinks it is talking to. Loud (A11).
            self._mismatched_wire(persona, scheme, body, WIRE_ANTHROPIC)
            return

        haystack, tool_results = _last_user(body)
        if persona == UNKNOWN_PERSONA:
            number, turn, reason = 0, None, (
                "the presented credential is not a scene key — "
                f"scene {self.scene.name} declares {len(self.scene.turns)} persona(s)"
            )
        else:
            number, turn, reason = self.scene.take_turn(persona, haystack)

        self.log.write(
            {
                "method": "POST",
                "path": self.path,
                "persona": persona,
                "auth": scheme,
                "stream": bool(body.get("stream")),
                "model": body.get("model", ""),
                "turn": number,
                "last_user": _clip(haystack, LAST_USER_MAX),
                "tools": len(body.get("tools") or []),
                "tool_results": tool_results,
                "desync": turn is None,
            }
        )

        if turn is None:
            self.state.mark_failure()
            self._respond_or_mark(200, "text/event-stream", _desync_sse(reason))
            return
        self._respond_or_mark(200, "text/event-stream", _turn_sse(self.scene, persona, number, turn))

    # ── the OpenAI-compat wire (Story 19.9 A11) ──────────────────────────
    def _openai_completions(self) -> None:
        """``POST /v1/chat/completions`` — one scene turn on the OpenAI wire.

        Persona is keyed by the presented **Bearer** value, exactly as the
        Anthropic arm keys on the presented `x-api-key` — and the SCHEME is
        policed: the adapter presents `Authorization: Bearer` and nothing else
        (pinned by the committed J2 receipt row and the AC8 wire-arm control),
        so a credential in any other header, or none, is contract drift. An
        undeclared value still resolves to `<unknown>`, desyncs, and is never
        written to the log.
        """
        body = self._read_body()
        if body is None:
            self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
            self.state.mark_failure()
            self._respond(400, "application/json", b'{"error":"unparseable body"}')
            return
        scheme, presented = _auth(self.headers)

        if scheme != "bearer":
            # Wrong SCHEME on the right endpoint (review finding 2026-08-29):
            # `x-api-key` is the Anthropic arm's credential shape, so serving
            # it here would let one wire's key impersonate the other's. The
            # row records the scheme only — never the presented value, which
            # may be a real credential.
            self.log.write(
                {
                    "method": "POST",
                    "path": self.path,
                    "auth": scheme,
                    "wire": WIRE_OPENAI,
                    "stream": bool(body.get("stream")),
                    "model": str(body.get("model", "")),
                    "turn": 0,
                    "last_user": "",
                    "tools": len(body.get("tools") or []),
                    "tool_results": 0,
                    "desync": True,
                }
            )
            self.state.mark_failure()
            self._respond(
                400,
                "application/json",
                json.dumps(
                    {
                        "error": {
                            "type": "scene_auth_scheme",
                            "message": (
                                f"{self.path} keys the persona on a Bearer "
                                f"credential; this request presented the "
                                f"{scheme!r} scheme instead"
                            ),
                        }
                    },
                    separators=(",", ":"),
                ).encode(),
            )
            return

        if not body.get("stream"):
            # The adapter always streams (`OpenAiRequest::from` sets `stream:
            # true`), so a stream-less body is contract drift, not a health
            # check — the OpenAI health check is `GET {base_url}/models`.
            self.log.write({"method": self.command, "path": self.path, "kind": "unknown"})
            self.state.mark_failure()
            self._respond(404, "application/json", b'{"error":"unknown request"}')
            return

        persona = self.scene.resolve_persona(presented)
        if persona != UNKNOWN_PERSONA and self.scene.wires[persona] != WIRE_OPENAI:
            self._mismatched_wire(persona, scheme, body, WIRE_OPENAI)
            return

        haystack = _last_user_openai(body)
        if persona == UNKNOWN_PERSONA:
            number, turn, reason = 0, None, (
                "the presented bearer credential is not a scene key — "
                f"scene {self.scene.name} declares {len(self.scene.turns)} persona(s)"
            )
        else:
            number, turn, reason = self.scene.take_turn(persona, haystack)

        model = str(body.get("model", ""))
        self.log.write(
            {
                "method": "POST",
                "path": self.path,
                "persona": persona,
                "auth": scheme,
                "wire": WIRE_OPENAI,
                "stream": bool(body.get("stream")),
                "model": model,
                "turn": number,
                "last_user": _clip(haystack, LAST_USER_MAX),
                "tools": len(body.get("tools") or []),
                "tool_results": 0,
                "desync": turn is None,
            }
        )

        if turn is None:
            # There is no error frame in the OpenAI stream contract, so a desync
            # is a hard 400: `stream_completion` short-circuits on
            # `!status.is_success()` and the product paints the error instead of
            # hanging on a stream that will never complete.
            self.state.mark_failure()
            self._respond(
                400,
                "application/json",
                json.dumps(
                    {"error": {"type": "scene_desync", "message": reason}},
                    separators=(",", ":"),
                ).encode(),
            )
            return
        self._respond_or_mark(
            200,
            "text/event-stream",
            _openai_turn_sse(self.scene, persona, number, turn, model),
        )

    def _mismatched_wire(self, persona: str, scheme: str, body: dict, reached: str) -> None:
        """A declared persona arrived on the wrong endpoint — log it as a desync
        without consuming a turn, and answer loud on the wire it reached."""
        declared = self.scene.wires[persona]
        reason = (
            f"persona {persona} is declared on the {declared!r} wire but this request "
            f"reached the {reached!r} endpoint {self.path}"
        )
        row = {
            "method": "POST",
            "path": self.path,
            "persona": persona,
            "auth": scheme,
            "stream": bool(body.get("stream")),
            "model": str(body.get("model", "")),
            "turn": 0,
            "last_user": "",
            "tools": len(body.get("tools") or []),
            "tool_results": 0,
            "desync": True,
        }
        if reached == WIRE_OPENAI:
            row["wire"] = WIRE_OPENAI
        self.log.write(row)
        self.state.mark_failure()
        if reached == WIRE_OPENAI:
            self._respond(
                400,
                "application/json",
                json.dumps(
                    {"error": {"type": "scene_wire_mismatch", "message": reason}},
                    separators=(",", ":"),
                ).encode(),
            )
        else:
            self._respond_or_mark(200, "text/event-stream", _desync_sse(reason))


class ServerState:
    """Exit-status ledger: 0 clean, 1 once any desync or unknown path happened."""

    def __init__(self) -> None:
        self._failed = False
        self._lock = threading.Lock()

    def mark_failure(self) -> None:
        with self._lock:
            self._failed = True

    @property
    def exit_status(self) -> int:
        with self._lock:
            return 1 if self._failed else 0


def serve(scene: Scene, log: RequestLog, port: int, host: str = "127.0.0.1") -> int:
    state = ServerState()
    handler = type(
        "BoundSceneHandler",
        (SceneHandler,),
        {"scene": scene, "log": log, "state": state},
    )
    server = ThreadingHTTPServer((host, port), handler)
    server.daemon_threads = True

    def shutdown(_signum, _frame) -> None:
        # server.shutdown() blocks until serve_forever() returns, so it cannot run on
        # the thread that is inside serve_forever().
        threading.Thread(target=server.shutdown, daemon=True).start()

    # Handlers BEFORE the banner: the banner is the readiness boundary SceneStub
    # waits on, and a signal landing in between would kill the child with the
    # default disposition (-15) instead of the documented 0/1 exit status.
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    print(f"listening http://{host}:{server.server_address[1]}", flush=True)
    print(f"scene sha256:{scene.sha256}", flush=True)
    try:
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()
        log.close()
    return state.exit_status


# ── Launcher (imported by the pytest fixture and by manual_test_ledger.py) ────


class SceneStub:
    """Run this module as a child process and expose its port, hash and log.

    Deliberately importable without pytest, so a plain demo driver can use the
    same launcher as the CI test and neither invents its own.
    """

    def __init__(self, scene: Path, log: Path, port: int = 0) -> None:
        self.scene_path = Path(scene)
        self.log_path = Path(log)
        self.requested_port = port
        self.port = 0
        self.url = ""
        self.sha256 = ""
        self._proc: "subprocess.Popen[str] | None" = None

    def start(self) -> "SceneStub":
        self._proc = subprocess.Popen(  # noqa: S603 - fixed argv, no shell
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "--scene",
                str(self.scene_path),
                "--port",
                str(self.requested_port),
                "--log",
                str(self.log_path),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        banner = self._proc.stdout.readline() if self._proc.stdout else ""
        if not banner.startswith("listening "):
            raise RuntimeError(
                f"scene provider did not start: {banner!r} {self._stderr()!r}"
            )
        self.url = banner.split(" ", 1)[1].strip()
        self.port = int(self.url.rsplit(":", 1)[1])
        scene_line = self._proc.stdout.readline() if self._proc.stdout else ""
        self.sha256 = scene_line.strip().removeprefix("scene sha256:")
        return self

    def _stderr(self) -> str:
        if self._proc is None or self._proc.stderr is None:
            return ""
        self._proc.kill()
        return self._proc.stderr.read()

    def env(self, persona: str, home: Path) -> dict[str, str]:
        """The env block a driver or fixture MUST set, verbatim.

        ``ANTHROPIC_AUTH_TOKEN`` empty is *unset* (the binary trims empty env vars),
        which matters because ``rustain/.env`` exports a real one and the auth
        precedence is ``AUTH_TOKEN > API_KEY``: without the override the developer's
        bearer token is what reaches this stub.

        ``home`` is REQUIRED and must be a scratch directory, because the
        user-global config layer is ``dirs::home_dir()/.config/rustain/config.toml``
        and ``RUSTAIN_CONFIG_DIR`` does **not** move it (`config_layer_paths` reads
        ``dirs::home_dir()`` directly). Measured on the authoring host: a developer's
        ``[provider.openrouter] enabled = true`` there makes ``app_config.provider``
        non-empty, so ``init_provider_layer`` takes the CONFIG path, ignores
        ``ANTHROPIC_BASE_URL`` entirely, and sends the turn to openrouter.ai with the
        real ``OPENROUTER_API_KEY`` from ``rustain/.env`` — the stub then sees only
        the models.dev fetch. Redirecting ``HOME`` deletes that layer, which is what
        makes ``app_config.provider.is_empty()`` true and the env path (J0's front
        door) the one under test. ``OPENROUTER_API_KEY`` is blanked as well so no
        credential is in the child env at all.
        """
        return {
            "ANTHROPIC_AUTH_TOKEN": "",
            "ANTHROPIC_API_KEY": persona,
            "ANTHROPIC_BASE_URL": self.url,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
            "RUSTAIN_MODELS_DEV_URL": self.url,
            "OPENROUTER_API_KEY": "",
            "HOME": str(home),
            "NO_COLOR": "1",
        }

    def rows(self) -> list[dict]:
        """Every request the *binary* made, in order."""
        if not self.log_path.exists():
            return []
        return [
            json.loads(line)
            for line in self.log_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def stop(self, timeout: float = 15.0) -> int:
        """SIGTERM, wait, return the exit status (0 clean, 1 desync/unknown path)."""
        if self._proc is None:
            return 0
        if self._proc.poll() is None:
            self._proc.terminate()
        try:
            self._proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait(timeout=timeout)
        for pipe in (self._proc.stdout, self._proc.stderr):
            if pipe is not None:
                pipe.close()
        return self._proc.returncode

    def __enter__(self) -> "SceneStub":
        return self.start()

    def __exit__(self, *exc) -> None:
        status = self.stop()
        if exc[0] is None and status != 0:
            raise RuntimeError(f"scene provider exited {status}: {self.rows()}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, add_help=True)
    parser.add_argument("--scene", required=True, type=Path, help="scene JSON file")
    parser.add_argument("--port", type=int, default=0, help="TCP port (0 = kernel-assigned)")
    parser.add_argument("--log", required=True, type=Path, help="request log (JSONL)")
    args = parser.parse_args(argv)

    try:
        scene = Scene(args.scene)
    except (SceneError, OSError) as exc:
        print(f"scene error: {exc}", file=sys.stderr, flush=True)
        return 2
    return serve(scene, RequestLog(args.log), args.port)


if __name__ == "__main__":
    sys.exit(main())
