#!/usr/bin/env python3
"""The J7 provider: the shared scene stub plus one decoded-``system`` observation.

AC1 requires the persona file body to be at the START of the decoded request
``system`` field of every admitted beat, not "somewhere in the request" (the shared
stub's ``request_observations`` needle search matches any string, including the user
message, so it cannot carry that claim). ``tests_tui/fixtures/scene_provider.py`` is
hash-bound by the conflict-receipt manifest and must not be edited, so this launcher
imports it unchanged and only decorates two seams in ITS OWN process:

* ``SceneHandler._read_body`` records the decoded ``system`` of the request being read;
* the request log writes one extra field, ``system_has_persona_prefix`` (a boolean,
  never the prompt or the persona text), on rows that carry a ``turn``.

Usage mirrors ``scene_provider.py`` plus ``--persona-file <path>``. Banner and exit
status are the shared stub's (``listening <url>``, ``scene sha256:<hex>``; exit 0 clean,
1 on a desync or unknown path, 2 on a bad scene).
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
import threading
from pathlib import Path

_SHARED = Path(__file__).resolve().parents[2] / "fixtures" / "scene_provider.py"


def _load_shared():
    spec = importlib.util.spec_from_file_location("j7_shared_scene_provider", _SHARED)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def decoded_system(body: dict) -> str | None:
    """The request's ``system`` as text, or ``None`` when absent/unsupported."""
    system = body.get("system")
    if isinstance(system, str):
        return system
    if isinstance(system, list):
        parts = [b.get("text") for b in system if isinstance(b, dict) and isinstance(b.get("text"), str)]
        return "".join(parts) if parts else None
    return None


def has_persona_prefix(system: str | None, persona_body: str) -> bool:
    """Exact: the production-trimmed file body is the START of ``system``."""
    return system is not None and system.startswith(persona_body)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scene", required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--log", required=True)
    parser.add_argument("--persona-file", required=True)
    args = parser.parse_args(argv)

    shared = _load_shared()
    persona_body = Path(args.persona_file).read_text(encoding="utf-8").strip()
    seen = threading.local()

    original_read_body = shared.SceneHandler._read_body

    def read_body(handler):
        body = original_read_body(handler)
        seen.system = decoded_system(body) if isinstance(body, dict) else None
        return body

    shared.SceneHandler._read_body = read_body

    class ObservingLog(shared.RequestLog):
        def write(self, row: dict) -> None:
            if "turn" in row:
                row = {**row, "system_has_persona_prefix": has_persona_prefix(getattr(seen, "system", None), persona_body)}
            super().write(row)

    try:
        scene = shared.Scene(Path(args.scene))
    except (shared.SceneError, OSError) as exc:
        print(f"scene error: {exc}", file=sys.stderr, flush=True)
        return 2
    return shared.serve(scene, ObservingLog(Path(args.log)), args.port)


if __name__ == "__main__":
    raise SystemExit(main())
