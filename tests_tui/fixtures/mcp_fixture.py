#!/usr/bin/env python3
"""Deterministic stdio MCP fixture server for rustain TUI E2E tests.

Promoted from the project-root ``echo_mcp.py`` (orphaned) into the test tree
and extended with the surfaces Epic 9 acceptance criteria need to assert on:

Tools (returned from ``tools/list``):
  - ``echo``            — round-trip a message; ``read_only_hint=true``
  - ``add``             — numeric arithmetic; ``read_only_hint=true``
  - ``slow_op``         — sleeps ``delay_ms`` ms; used for cancellation tests
  - ``error_op``        — returns ``is_error=true`` content
  - ``file_writer``     — pretends to write; ``read_only_hint=false`` (Elevated)
  - ``large_result``    — returns N KB of payload; for buffering/truncation
  - ``image_content``   — returns an MCP image content block; for placeholder render

Optional ``notifications/tools/list_changed`` is emitted on initialize when the
env var ``FAKE_MCP_EMIT_LIST_CHANGED=1`` is set. The handshake also includes the
``tools.listChanged`` server capability so the client subscribes.

Environment knobs (compatible with the Rust ``fake-mcp-server`` binary):
  - ``FAKE_MCP_DROP_AFTER_MS``    — exit after N ms (drives reconnect tests)
  - ``FAKE_MCP_INIT_DELAY_MS``    — sleep N ms before responding to ``initialize``
  - ``FAKE_MCP_FAIL_INITIALIZE``  — return JSON-RPC error from ``initialize``
  - ``FAKE_MCP_EMIT_LIST_CHANGED``— emit one ``notifications/tools/list_changed``
                                    after handshake to validate cache refresh
  - ``FAKE_MCP_SERVER_NAME``      — override ``serverInfo.name`` (default: ``echo``)

The protocol target is MCP ``2024-11-05`` (matches rustain ``rmcp`` v0.5).
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time


PROTOCOL_VERSION = "2024-11-05"

# ── Tool catalog ────────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "echo",
        "description": "Echo back the input message verbatim.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "Text to echo"},
            },
            "required": ["message"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "add",
        "description": "Add two integers and return the sum.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"},
            },
            "required": ["a", "b"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "slow_op",
        "description": "Sleep for delay_ms milliseconds, then return ok.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "delay_ms": {"type": "integer", "description": "milliseconds to sleep"},
            },
            "required": ["delay_ms"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "error_op",
        "description": "Always returns is_error=true with the provided reason.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "reason": {"type": "string"},
            },
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "file_writer",
        "description": "Simulate a destructive operation (NOT read-only).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["path", "content"],
        },
        "annotations": {"readOnlyHint": False},
    },
    {
        "name": "large_result",
        "description": "Return kb kilobytes of filler text.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kb": {"type": "integer"},
            },
            "required": ["kb"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "image_content",
        "description": "Return an MCP image content block (PNG, 1x1 transparent).",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": True},
    },
]


# ── 1x1 transparent PNG (base64) ────────────────────────────────────────────

PNG_1X1_TRANSPARENT_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVQYV2NgYAAAAAMAAWgmWQ0AAAAASUVORK5CYII="
)


# ── JSON-RPC helpers ────────────────────────────────────────────────────────

def _result(req_id, payload):
    return {"jsonrpc": "2.0", "id": req_id, "result": payload}


def _error(req_id, code, message):
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


def _notification(method, params=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    return msg


def _send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


# ── Tool dispatch ───────────────────────────────────────────────────────────

def _call_tool(name, args):
    if name == "echo":
        text = str(args.get("message", ""))
        return {"content": [{"type": "text", "text": f"echo: {text}"}]}

    if name == "add":
        a = int(args.get("a", 0))
        b = int(args.get("b", 0))
        return {"content": [{"type": "text", "text": str(a + b)}]}

    if name == "slow_op":
        delay_ms = int(args.get("delay_ms", 0))
        time.sleep(delay_ms / 1000.0)
        return {"content": [{"type": "text", "text": f"slept {delay_ms}ms"}]}

    if name == "error_op":
        reason = str(args.get("reason", "unspecified"))
        return {"content": [{"type": "text", "text": f"failure: {reason}"}], "isError": True}

    if name == "file_writer":
        path = str(args.get("path", ""))
        content = str(args.get("content", ""))
        return {"content": [{"type": "text", "text": f"would-write path={path} bytes={len(content)}"}]}

    if name == "large_result":
        kb = max(0, int(args.get("kb", 1)))
        payload = "x" * (kb * 1024)
        return {"content": [{"type": "text", "text": payload}]}

    if name == "image_content":
        return {"content": [{
            "type": "image",
            "data": PNG_1X1_TRANSPARENT_B64,
            "mimeType": "image/png",
        }]}

    return {"content": [{"type": "text", "text": f"unknown tool: {name}"}], "isError": True}


# ── Request handler ─────────────────────────────────────────────────────────

def _handle(request):
    method = request.get("method", "")
    req_id = request.get("id")

    if method == "initialize":
        if os.environ.get("FAKE_MCP_FAIL_INITIALIZE"):
            return _error(req_id, -32603, "fake-mcp-fixture: forced initialize failure")
        init_delay = int(os.environ.get("FAKE_MCP_INIT_DELAY_MS", "0") or "0")
        if init_delay > 0:
            time.sleep(init_delay / 1000.0)
        server_name = os.environ.get("FAKE_MCP_SERVER_NAME", "echo")
        return _result(req_id, {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": server_name, "version": "0.1"},
        })

    if method == "tools/list":
        return _result(req_id, {"tools": TOOLS})

    if method == "tools/call":
        params = request.get("params", {}) or {}
        name = params.get("name", "")
        args = params.get("arguments", {}) or {}
        return _result(req_id, _call_tool(name, args))

    if method == "ping":
        return _result(req_id, {})

    # MCP notifications (no id) — silently accept
    if req_id is None:
        return None

    return _error(req_id, -32601, f"method not found: {method}")


# ── Optional self-termination thread ────────────────────────────────────────

def _maybe_arm_drop_timer():
    drop_after_ms = int(os.environ.get("FAKE_MCP_DROP_AFTER_MS", "0") or "0")
    if drop_after_ms <= 0:
        return

    def _drop():
        time.sleep(drop_after_ms / 1000.0)
        # Hard-exit so the parent's reconnect path engages.
        os._exit(0)

    threading.Thread(target=_drop, daemon=True).start()


def _maybe_emit_list_changed():
    if not os.environ.get("FAKE_MCP_EMIT_LIST_CHANGED"):
        return

    def _emit():
        # Wait briefly for the parent to finish initialize+subscribe.
        time.sleep(0.5)
        _send(_notification("notifications/tools/list_changed"))

    threading.Thread(target=_emit, daemon=True).start()


# ── Main loop ───────────────────────────────────────────────────────────────

def main():
    _maybe_arm_drop_timer()
    _maybe_emit_list_changed()

    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as exc:
            sys.stderr.write(f"parse error: {exc}\n")
            sys.stderr.flush()
            continue
        response = _handle(req)
        if response is not None:
            _send(response)


if __name__ == "__main__":
    main()
