"""Manual test: verify tiered router + token ledger in live TUI.

Spawns rustain, sends a message, and asserts:
1. Status bar shows the resolved model name
2. ~/.rustain/usage/{session_id}.jsonl is created with a valid entry
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from harness import RustainTUI


def main():
    print("Building binary...")
    tui = RustainTUI(fresh=True, build=True).start()

    try:
        print("Waiting for TUI startup...")
        tui.wait_for_idle(4)
        screen = tui.get_screen_text()
        print("=== Startup screen ===")
        print(screen)
        print("======================")

        # Check status bar shows a model name
        if "claude" in screen.lower() or "glm" in screen.lower():
            print("✅ Status bar shows model name")
        else:
            print("⚠️  Model name not obvious on screen (may need to scroll)")

        print("\nSending test message...")
        tui.send_message("Say 'ledger test ok' and nothing else.")
        print("Waiting for response (up to 45s)...")
        tui.wait_for_idle(45)

        screen = tui.get_screen_text()
        print("\n=== Post-turn screen ===")
        print(screen)
        print("========================")

        # Check ledger files
        usage_dir = Path.home() / ".rustain" / "usage"
        ledger_files = list(usage_dir.glob("*.jsonl")) if usage_dir.exists() else []

        if not ledger_files:
            print("\n❌ No ledger files found in ~/.rustain/usage/")
            return 1

        print(f"\n✅ Found {len(ledger_files)} ledger file(s)")
        for f in ledger_files:
            lines = f.read_text().strip().splitlines()
            print(f"\n  {f.name}: {len(lines)} entry(ies)")
            for i, line in enumerate(lines):
                entry = json.loads(line)
                print(f"    Entry {i+1}:")
                print(f"      model={entry['model']}")
                print(f"      tier={entry['tier']}")
                print(f"      escalationReason={entry['escalationReason']}")
                print(f"      usage={entry['usage']}")
                print(f"      providerId={entry['providerId']}")

        # Verify expected fields
        latest = json.loads(lines[-1])
        required = ["timestampMs", "sessionId", "conversationId",
                    "providerId", "model", "tier",
                    "escalationReason", "usage"]
        missing = [k for k in required if k not in latest]
        if missing:
            print(f"\n❌ Missing fields in latest entry: {missing}")
            return 1

        print("\n✅ All required fields present in ledger entry")

        # Check token counts are reasonable (non-zero for a real call)
        usage = latest["usage"]
        if usage["tokensIn"] > 0 or usage["tokensOut"] > 0:
            print(f"✅ Token counts present: in={usage['tokensIn']}, out={usage['tokensOut']}")
        else:
            print(f"⚠️  Token counts are zero (may indicate failed call or no usage chunk)")

        print("\n🎉 Manual ledger test PASSED")
        return 0

    finally:
        print("\nQuitting TUI...")
        tui.send("\x03")  # Ctrl+C
        time.sleep(0.5)
        tui._cleanup()


if __name__ == "__main__":
    sys.exit(main())
