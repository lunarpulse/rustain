#!/usr/bin/env python3
"""Exercise carried selection with the real checker, without spawning Rust or a PTY."""

from datetime import datetime, timedelta
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


PRODUCT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(PRODUCT / "tests_tui"))
from carried_j10_receipts import CARRIED_PATHS, CHECKER, CarriedReceiptError, load_carried_receipts


class CarriedJ10Receipts(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.outside = Path(temp.name).resolve()
        self.root = self.outside / "product"
        self.root.mkdir()
        self.git_environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.git_environment.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "--quiet", "--template=")
        self.git("commit", "--quiet", "--allow-empty", "-m", "Empty snapshot")
        self.before_receipts = self.git("rev-parse", "HEAD")
        self.copy_product_file(CHECKER)
        for relative in CARRIED_PATHS.values():
            receipt = self.copy_product_file(relative)
            dependencies = re.findall(rb"^(?:scene|source): (\S+) sha256: [0-9a-f]{64}$",
                                      receipt.read_bytes(), re.M)
            self.assertEqual(len(dependencies), 2)
            for dependency in dependencies:
                self.copy_product_file(Path(os.fsdecode(dependency)))
        self.revision = self.snapshot()

    def git(self, *args):
        result = subprocess.run(
            ["git", "-C", str(self.root), "-c", "user.name=Carried Receipt Fixture",
             "-c", "user.email=fixture@example.invalid", *args],
            env=self.git_environment, capture_output=True, text=True, timeout=30, check=True,
        )
        return result.stdout.strip()

    def snapshot(self):
        self.git("add", "--", ".")
        self.git("commit", "--quiet", "-m", "Snapshot receipt fixture")
        return self.git("rev-parse", "HEAD")

    def copy_product_file(self, relative):
        target = self.root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(PRODUCT / relative, target)
        return target

    def assert_checker_passes(self, receipt):
        result = subprocess.run([str(self.root / CHECKER), "--product-root", str(self.root), str(receipt)],
                                cwd=self.outside, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("PASS", result.stdout)

    def new_candidate(self, receipt):
        # Re-date exact captured evidence within the journal window for this isolated
        # fixture only. Fresh runtime evidence is produced separately by the native gates.
        header, body = receipt.read_bytes().split(b"\n", 1)
        previous = datetime.fromisoformat(header.removeprefix(b"recorded-at: ").decode().replace("Z", "+00:00"))
        later = previous + timedelta(seconds=1)
        basename = receipt.name.replace(f"{previous:%Y-%m-%dT%H-%M-%SZ}", f"{later:%Y-%m-%dT%H-%M-%SZ}")
        viewport = receipt.parent.name.split("-", 1)[0]
        parent = receipt.parent.with_name(f"{viewport}-{later:%Y%m%dT%H%M%S%fZ}-{'0' * 32}")
        parent.mkdir()
        fresh = parent / basename
        fresh.write_bytes(f"recorded-at: {later:%Y-%m-%dT%H:%M:%SZ}\n".encode() + body)
        self.assertGreater(fresh.name, receipt.name)
        self.assert_checker_passes(fresh)
        return fresh

    def assert_carried_wins(self, act):
        relative = CARRIED_PATHS[act]
        pinned = self.root / relative
        original = pinned.read_bytes()
        fresh = self.new_candidate(pinned)
        label = re.escape(f"{act} {relative}")
        digest = hashlib.sha256(original).hexdigest()

        selected = load_carried_receipts(self.root, self.revision)[act]
        self.assertEqual((selected.path, selected.sha256), (pinned, digest))

        pinned.unlink()
        with self.assertRaisesRegex(CarriedReceiptError, f"^{label}: missing carried receipt$"):
            load_carried_receipts(self.root, self.revision)

        stale = original.replace(b'"limits": "', b'"limits": "Stale fixture. ', 1)
        self.assertNotEqual(stale, original)
        pinned.write_bytes(stale)
        self.assert_checker_passes(pinned)
        with self.assertRaisesRegex(CarriedReceiptError, f"^{label}: stale carried receipt bytes differ"):
            load_carried_receipts(self.root, self.revision)

        pinned.write_bytes(original)
        self.assertTrue(fresh.is_file())
        self.assertEqual(load_carried_receipts(self.root, self.revision)[act], selected)

    def test_act1_ignores_newer_checker_valid_candidate(self):
        self.assert_carried_wins("act1")

    def test_act2_ignores_newer_checker_valid_candidate(self):
        self.assert_carried_wins("act2")

    def test_requires_tracked_blob_in_requested_commit_not_head_or_index(self):
        with self.assertRaisesRegex(CarriedReceiptError, f"not tracked as a regular blob in {self.before_receipts}"):
            load_carried_receipts(self.root, self.before_receipts)

    def test_rejects_symlinked_exact_file_even_with_identical_bytes(self):
        pinned = self.root / CARRIED_PATHS["act1"]
        target = pinned.rename(self.outside / pinned.name)
        pinned.symlink_to(target)
        self.assert_checker_passes(pinned)
        with self.assertRaisesRegex(CarriedReceiptError, "act1 .*: carried receipt is not a regular nonsymlink file"):
            load_carried_receipts(self.root, self.revision)

    def test_rejects_symlinked_parent_even_within_product(self):
        pinned = self.root / CARRIED_PATHS["act2"]
        target = pinned.parent.rename(self.root / "other-capture")
        pinned.parent.symlink_to(target, target_is_directory=True)
        self.assert_checker_passes(pinned)
        with self.assertRaisesRegex(CarriedReceiptError, "act2 .*: non-directory or symlink component"):
            load_carried_receipts(self.root, self.revision)

    def test_names_and_manifest_columns_do_not_substitute_for_paint(self):
        act1_border = ("└" + "─" * 78 + "┘").encode()
        for act, old, new in (("act1", act1_border, ("└" + "─" * 77 + "┘").encode()),
                              ("act2", b"\x1b[24;1H", b"\x1b[25;1H")):
            with self.subTest(act=act):
                pinned = self.root / CARRIED_PATHS[act]
                original = pinned.read_bytes()
                self.assertIn(old, original)
                pinned.write_bytes(original.replace(old, new))
                try:
                    revision = self.snapshot()
                    self.assert_checker_passes(pinned)
                    with self.assertRaisesRegex(CarriedReceiptError, f"{act} .*: carried receipt does not attest 80x24"):
                        load_carried_receipts(self.root, revision)
                finally:
                    pinned.write_bytes(original)

    def test_tracked_receipt_still_requires_real_checker(self):
        scene = self.root / "tests_tui/journeys/j10/scenes/19-21-act1-v1.json"
        scene.write_bytes(scene.read_bytes() + b"\n")
        with self.assertRaisesRegex(CarriedReceiptError, r"(?s)act1 .*: checker rejected carried receipt.*scene hash mismatch"):
            load_carried_receipts(self.root, self.revision)


if __name__ == "__main__":
    unittest.main()
