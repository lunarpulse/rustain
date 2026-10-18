"""Nine native conflict-receipt controls against the product canonical executable.

A current, width-specific h capture is a prerequisite, never a synthetic fixture.
Only diagnostic JSON is exported; private host trees and historical receipts are
never modified or copied into the evidence export.
"""
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

import pytest

from conflict_receipt_manifest import selected_receipts

REPOSITORY = Path(__file__).resolve().parents[1]
GATE = REPOSITORY / ".github/scripts/check-receipt.sh"
SCENE = REPOSITORY / "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json"
SOURCE = REPOSITORY / "tests_tui/test_story_19_21b_conflict_flag.py"
VIEWPORTS = ("80x24", "130x30", "60x16")
LEGS = ("scene", "source", "stamp")


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """The receipt executable launches no Rust binary and needs no cargo build."""


def _sha(data):
    return hashlib.sha256(data).hexdigest()


@pytest.fixture(scope="session")
def current_conflict_receipts():
    binary = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary, "Prerequisite: RUSTAIN_TUI_BINARY must explicitly select the captured binary"
    return selected_receipts(REPOSITORY, Path(binary))


def _verify(receipt, cwd):
    environment = dict(os.environ)
    environment.pop("RUSTAIN_ROOT", None)
    environment.pop("RUSTAIN_RECEIPT_GATE", None)
    return subprocess.run(
        [str(GATE), "--product-root", str(REPOSITORY), str(receipt)],
        cwd=cwd, env=environment, capture_output=True, text=True, timeout=30,
    )


def _assert_valid(result):
    assert result.returncode == 0, (
        "Prerequisite: genuine conflict receipt is not valid\n" + result.stdout + result.stderr)
    assert "PASS" in result.stdout, result.stdout


def _evidence(tmp_path, viewport, leg, phase, receipt, result):
    record = {
        "viewport": viewport, "leg": leg, "phase": phase,
        "receipt": receipt.name, "receipt_sha256": _sha(receipt.read_bytes()),
        "checker_sha256": _sha(GATE.read_bytes()),
        "control_source_sha256": _sha(Path(__file__).read_bytes()),
        "capture_source_sha256": _sha(SOURCE.read_bytes()),
        "scene_sha256": _sha(SCENE.read_bytes()),
        "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr,
    }
    encoded = json.dumps(record, indent=2) + "\n"
    (tmp_path / f"{phase}.json").write_text(encoded)
    if destination := os.environ.get("RUSTAIN_RECEIPT_EVIDENCE_DIR"):
        target = Path(destination) / f"19-21b-{viewport}-{leg}"
        target.mkdir(parents=True, exist_ok=True)
        (target / f"{phase}.json").write_text(encoded)


def _corrupt(original, leg):
    if leg in {"scene", "source"}:
        def changed_digest(match):
            digest = match[2]
            replacement = (b"1" if digest[:1] == b"0" else b"0") + digest[1:]
            return match[1] + replacement
        changed, count = re.subn(
            rb"^(" + leg.encode() + rb": [^\r\n]+ sha256: )([0-9a-f]{64})$",
            changed_digest, original, flags=re.M,
        )
        assert count == 1, f"Prerequisite: {leg} identity header absent or ambiguous"
        return changed, leg + " hash mismatch"
    headers = re.findall(rb"^recorded-at: ([^\r\n]+)$", original, re.M)
    assert len(headers) == 1, "Prerequisite: recorded-at header absent or ambiguous"
    stamp = datetime.fromisoformat(headers[0].decode().replace("Z", "+00:00"))
    changed_stamp = (stamp + timedelta(seconds=1)).astimezone(timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ").encode()
    return original.replace(b"recorded-at: " + headers[0],
                            b"recorded-at: " + changed_stamp, 1), "disagree"


@pytest.mark.a2a_binary
@pytest.mark.parametrize("viewport,leg", [(viewport, leg) for viewport in VIEWPORTS for leg in LEGS],
                         ids=[f"{viewport}-{leg}" for viewport in VIEWPORTS for leg in LEGS])
def test_current_conflict_receipt_rejects_independent_corruption(
        tmp_path, viewport, leg, current_conflict_receipts):
    receipt = current_conflict_receipts[viewport]
    original = receipt.read_bytes()
    valid = _verify(receipt, cwd=tmp_path)
    _evidence(tmp_path, viewport, leg, "positive", receipt, valid)
    _assert_valid(valid)

    changed, expected = _corrupt(original, leg)
    assert changed != original, "Prerequisite: corruption changed no receipt bytes"
    corrupted = tmp_path / receipt.name
    corrupted.write_bytes(changed)
    invalid = _verify(corrupted, cwd=tmp_path)
    _evidence(tmp_path, viewport, leg, "negative", corrupted, invalid)
    assert invalid.returncode != 0, (
        f"M38 {leg}: changed genuine receipt was accepted; expected {expected}\n" + invalid.stdout)
    assert invalid.returncode == 1, invalid.stdout + invalid.stderr
    assert expected in invalid.stdout + invalid.stderr, invalid.stdout + invalid.stderr
    assert receipt.read_bytes() == original, "Original genuine receipt bytes changed"

    corrupted.write_bytes(original)
    assert corrupted.read_bytes() == receipt.read_bytes() == original, (
        "Restored control must be byte-identical to its original genuine receipt")
    restored = _verify(corrupted, cwd=tmp_path)
    _evidence(tmp_path, viewport, leg, "restored", corrupted, restored)
    _assert_valid(restored)
    assert receipt.read_bytes() == original, "Original genuine receipt bytes changed"
