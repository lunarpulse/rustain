#!/usr/bin/env python3
"""Run the real named capture fixture and harness start without opening a PTY."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


PRODUCT = Path(__file__).resolve().parents[2]
ACT1 = "test_story_19_21_act1_distribution.py"
MODES = "test_story_19_21b_standalone_modes.py"
OTHER = "test_other.py"
CRASH = f"{ACT1}::test_act1_daemon_crash_restart"
MODE = f"{MODES}::test_standalone_auto_mode_still_requires_human_send"
EVIDENCE = "RUSTAIN_NAMED_PTY_EVIDENCE"

SUPPORT = r'''
from pathlib import Path

import harness
import pytest

ORIGINAL_START = harness.RustainTUI.start


class Child:
    def __init__(self, size, alive):
        self.size = size
        self.alive = alive
        self.reads = 0
        self.closed = False

    def getwinsize(self):
        self.reads += 1
        return self.size

    def isalive(self):
        return self.alive

    def sendcontrol(self, char):
        assert char == "c"

    def close(self, *, force):
        assert force
        self.closed = True
        self.alive = False


def exercise(tmp_path, monkeypatch, size, *, count=1, scenario="normal"):
    named = size == (24, 80)
    assert (harness.RustainTUI.start is not ORIGINAL_START) == named
    requests, children = [], []

    def spawn(*args, dimensions, **kwargs):
        if scenario == "spawn-error":
            raise RuntimeError("control failed before a child was created")
        requests.append(dimensions)
        size_seen = (30, 130) if scenario == "wrong-size" else dimensions
        child = Child(size_seen, alive=scenario in {"wrong-size", "startup-error"})
        children.append(child)
        return child

    def ready(*args, **kwargs):
        if scenario == "startup-error":
            raise RuntimeError("control failed after a child was created")
        return True

    monkeypatch.setattr(harness.pexpect, "spawn", spawn)
    monkeypatch.setattr(harness.RustainTUI, "wait_for_screen", ready)
    # Remove the stop() delay without replacing its cleanup machinery.
    if scenario == "startup-error":
        monkeypatch.setattr(harness.time, "sleep", lambda seconds: None)

    for _ in range(count):
        tui = harness.RustainTUI(build=False, binary=Path(__file__), workspace=tmp_path)
        if scenario == "wrong-size":
            with pytest.raises(AssertionError, match=r"expected PTY size \(24, 80\), got \(30, 130\)"):
                with tui:
                    pytest.fail("unverified child escaped start()")
            assert children[-1].closed
        else:
            try:
                with tui:
                    pass
            finally:
                if scenario == "startup-error":
                    assert children[-1].closed
                    assert children[-1].reads == 1
        assert harness.pexpect.spawn is spawn

    assert requests == [size] * count
    assert [child.reads for child in children] == [int(named)] * count
'''

ACT1_CASES = '''
import os
import pytest
from geometry_control import exercise


def test_act1_daemon_crash_restart(tmp_path, monkeypatch):
    scenario = os.environ["GEOMETRY_CONTROL_SCENARIO"]
    count = 2 if scenario == "missing-spawn" else 3
    exercise(tmp_path, monkeypatch, (24, 80), count=count, scenario=scenario)


@pytest.mark.parametrize("viewport", ["80x24"])
def test_act1_attached_distribution(tmp_path, monkeypatch, viewport):
    exercise(tmp_path, monkeypatch, (30, 130))
'''

MODE_CASES = '''
import os
import pytest
from geometry_control import exercise


@pytest.mark.parametrize("mode", ["yolo", "autoedit", "yolo-extra"])
def test_standalone_auto_mode_still_requires_human_send(tmp_path, monkeypatch, mode):
    size = (30, 130) if mode == "yolo-extra" else (24, 80)
    exercise(tmp_path, monkeypatch, size, scenario=os.environ["GEOMETRY_CONTROL_SCENARIO"])


class TestSibling:
    @pytest.mark.parametrize("mode", ["yolo"])
    def test_standalone_auto_mode_still_requires_human_send(self, tmp_path, monkeypatch, mode):
        exercise(tmp_path, monkeypatch, (30, 130))
'''

OTHER_CASES = '''
from geometry_control import exercise


def test_act1_daemon_crash_restart(tmp_path, monkeypatch):
    exercise(tmp_path, monkeypatch, (30, 130))
'''


def row(nodeid, spawn, rows=24, columns=80):
    return {"nodeid": nodeid, "spawn": spawn, "rows": rows, "columns": columns}


class NamedCaptureGeometry(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.root = Path(work.name)
        self.tui = self.root / "tests_tui"
        self.tui.mkdir()
        for name in ("conftest.py", "harness.py", "keys.py", "lane_guard.py"):
            shutil.copyfile(PRODUCT / "tests_tui" / name, self.tui / name)
        (self.tui / "pytest.ini").write_text("[pytest]\nmarkers = native_nightly\n", encoding="utf-8")
        (self.tui / "lanes.json").write_text(json.dumps({
            "modules": dict.fromkeys((ACT1, MODES, OTHER), "native_nightly"),
        }), encoding="utf-8")
        for name, source in (("geometry_control.py", SUPPORT), (ACT1, ACT1_CASES),
                             (MODES, MODE_CASES), (OTHER, OTHER_CASES)):
            (self.tui / name).write_text(source, encoding="utf-8")

    def run_cases(self, root, *cases, scenario="normal", export=True):
        env = os.environ.copy()
        for key in ("PYTEST_ADDOPTS", "PYTEST_PLUGINS", "PYTHONPATH", EVIDENCE):
            env.pop(key, None)
        evidence = self.root / f"{scenario}-{root.name}-{export}.jsonl"
        if export:
            env[EVIDENCE] = str(evidence)
        env["RUSTAIN_TUI_BINARY"] = str(self.tui / "geometry_control.py")
        env["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = "1"
        env["GEOMETRY_CONTROL_SCENARIO"] = scenario
        prefix = "tests_tui/" if root == self.root else ""
        result = subprocess.run(
            [sys.executable, "-m", "pytest", "-q", "--rootdir", str(root),
             "-c", str(self.tui / "pytest.ini"), *(prefix + case for case in cases)],
            cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, timeout=30,
        )
        records = [json.loads(line) for line in evidence.read_text(encoding="utf-8").splitlines()] if evidence.exists() else []
        return result, records

    def test_exact_ids_and_restoration_under_both_pytest_roots(self):
        expected = [row(CRASH, n) for n in (1, 2, 3)] + [row(f"{MODE}[{mode}]", 1) for mode in ("yolo", "autoedit")]
        for root in (self.root, self.tui):
            with self.subTest(root=root):
                result, records = self.run_cases(root, ACT1, MODES, OTHER)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn("7 passed", result.stdout)
                self.assertEqual(records, expected)

    def test_wrong_observed_size_fails_entry_and_closes_child_with_or_without_export(self):
        case = f"{MODE}[yolo]"
        for export in (True, False):
            with self.subTest(export=export):
                result, records = self.run_cases(self.tui, case, scenario="wrong-size", export=export)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(records, [row(case, 1, 30, 130)] if export else [])

    def test_missing_spawn_fails_even_when_case_body_passes(self):
        result, records = self.run_cases(self.tui, CRASH, scenario="missing-spawn")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("expected 3 PTY spawns, observed 2", result.stdout)
        self.assertEqual(records, [row(CRASH, 1), row(CRASH, 2)])

    def test_original_start_errors_survive_without_fabricated_records(self):
        for scenario, moment, spawns in (("spawn-error", "before", 0), ("startup-error", "after", 1)):
            with self.subTest(scenario=scenario):
                result, records = self.run_cases(self.tui, CRASH, scenario=scenario)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(f"RuntimeError: control failed {moment} a child was created", result.stdout)
                self.assertIn(f"expected 3 PTY spawns, observed {spawns}", result.stdout)
                self.assertEqual(records, [row(CRASH, 1)] if spawns else [])


if __name__ == "__main__":
    unittest.main()
