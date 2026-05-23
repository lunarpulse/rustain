"""Diagnose why live-LLM smokes skip — prints provider-related log lines."""
from __future__ import annotations

import time
import pytest

pytestmark = [pytest.mark.mcp]


def test_dump_provider_status(tui_with_mcp):
    time.sleep(3.0)
    print("\n=== PROVIDER STARTUP LOG ===")
    for ln in tui_with_mcp.log_lines():
        s = ln.lower()
        if any(k in s for k in ["provider", "registr", "anthropic", "router", "health"]):
            print(ln)
    print("=== END ===\n")
    print("=== SCREEN ===")
    print(tui_with_mcp.get_screen_text())
    print("=== END SCREEN ===")
    # Always pass — this is purely informational.
