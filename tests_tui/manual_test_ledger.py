"""Manual check: one scripted turn through the real TUI, no provider account.

Starts ``fixtures/scene_provider.py`` on an ephemeral port, drives one turn through
the real binary under pexpect, and prints the usage-ledger rows *this run* minted —
from the harness workspace (``tui.wp/.rustain_data/usage``), never from ``$HOME``.

    python3 tests_tui/manual_test_ledger.py

The ledger row is the journey-receipt shape: ``timestampMs`` proves the row was
minted inside the run, ``tokensOut`` proves the turn actually succeeded (the failure
path writes a row too, with ``tokensOut: 0``).
"""

import json
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub
from harness import RustainTUI

HERE = Path(__file__).resolve().parent
SCENE = HERE / "fixtures" / "scenes" / "smoke.json"
PERSONA = "scene-ci"


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="rustain_ledger_") as tmp:
        stub = SceneStub(SCENE, Path(tmp) / "requests.jsonl").start()
        home = Path(tmp) / "home"
        home.mkdir()
        print(f"scene provider on {stub.url}  scene sha256:{stub.sha256}")
        try:
            with RustainTUI(fresh=True, env_overrides=stub.env(PERSONA, home)) as tui:
                tui.send_message("scene ping")
                # Completion FIRST, then Ready: the screen already says `Ready` at
                # startup and `wait_for_screen` checks before sleeping, so a
                # Ready-only wait can match the pre-turn state and race the ledger
                # write (the CI test waits in this same order).
                if not tui.wait_for_screen("scene pong", timeout=30):
                    print("FAIL: the turn never completed")
                    print(tui.get_screen_text())
                    return 1
                if not tui.wait_for_screen("Ready", timeout=30):
                    print("FAIL: the turn never returned to Ready")
                    print(tui.get_screen_text())
                    return 1
                print("=== screen ===")
                print(tui.get_screen_text())
                # The row is minted at turn end and can trail the Ready render by a
                # moment — poll briefly instead of globbing once.
                deadline = time.monotonic() + 10
                rows: list[Path] = []
                while time.monotonic() < deadline:
                    rows = sorted((tui.wp / ".rustain_data" / "usage").glob("*.jsonl"))
                    if rows and any(path.read_text().strip() for path in rows):
                        break
                    time.sleep(0.25)
                if not rows:
                    print(f"FAIL: no ledger file under {tui.wp / '.rustain_data' / 'usage'}")
                    return 1
                print("=== usage ledger (product-minted) ===")
                count = 0
                for path in rows:
                    for line in path.read_text().splitlines():
                        count += 1
                        entry = json.loads(line)
                        print(
                            f"  {path.name}: timestampMs={entry['timestampMs']} "
                            f"model={entry['model']} tokensIn={entry['usage']['tokensIn']} "
                            f"tokensOut={entry['usage']['tokensOut']}"
                        )
                print("=== request log (what the binary sent) ===")
                for row in stub.rows():
                    print(f"  {json.dumps(row, separators=(',', ':'))}")
        finally:
            status = stub.stop()
            print(f"scene provider exit status: {status}")
        if count < 1:
            print("FAIL: the ledger file exists but holds no rows")
            return 1
        if status != 0:
            print("FAIL: the scene desynced or an unknown path was hit")
            return 1
    print(f"OK — {count} ledger row(s) from this run's workspace")
    return 0


if __name__ == "__main__":
    sys.exit(main())
