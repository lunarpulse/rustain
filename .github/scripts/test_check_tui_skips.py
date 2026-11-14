#!/usr/bin/env python3
"""Deterministic controls for the Python TUI skip allowlist checker (exit codes and key sets only)."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("check_tui_skips", Path(__file__).with_name("check-tui-skips.py"))
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

ALLOWLIST = Path(__file__).resolve().parents[2] / "tests_tui" / "skip-allowlist.json"
REASON = "Owner: tui-5."


def case(classname, name, outcome=None):
    child = ""
    if outcome == "skip":
        child = '<skipped type="pytest.skip" message="why"/>'
    elif outcome == "xfail":
        child = '<skipped type="pytest.xfail" message="why"/>'
    elif outcome == "fail":
        child = '<failure message="boom"/>'
    return f'<testcase classname="{classname}" name="{name}" time="0.1">{child}</testcase>'


class CheckTuiSkips(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)

    def junit(self, *cases, raw=None):
        path = self.dir / "junit.xml"
        body = raw if raw is not None else f'<testsuites><testsuite name="pytest">{"".join(cases)}</testsuite></testsuites>'
        path.write_text(body, encoding="utf-8")
        return path

    def allowlist(self, entries=None, raw=None):
        path = self.dir / "allow.json"
        path.write_text(raw if raw is not None else json.dumps(entries), encoding="utf-8")
        return path

    def run_main(self, junit, allow):
        return checker.main([str(junit), str(allow)])

    def test_k1_unlisted_skip_is_reported_and_exits_1(self):
        junit = self.junit(case("m", "test_a"), case("m", "test_b", "skip"))
        allow = self.allowlist({})
        self.assertEqual(checker.skip_violations(junit, allow), {"unlisted": {"m::test_b"}, "stale": set()})
        self.assertEqual(self.run_main(junit, allow), 1)

    def test_k2_listed_skip_passes(self):
        junit = self.junit(case("m", "test_a"), case("m", "test_b", "skip"))
        allow = self.allowlist({"m::test_b": REASON})
        self.assertEqual(checker.skip_violations(junit, allow), {"unlisted": set(), "stale": set()})
        self.assertEqual(self.run_main(junit, allow), 0)

    def test_k3_stale_entries_fail(self):
        junit = self.junit(case("m", "test_passed"), case("m", "test_other", "skip"))
        allow = self.allowlist({"m::test_passed": REASON, "m::test_never_collected": REASON, "m::test_other": REASON})
        self.assertEqual(
            checker.skip_violations(junit, allow),
            {"unlisted": set(), "stale": {"m::test_passed", "m::test_never_collected"}},
        )
        self.assertEqual(self.run_main(junit, allow), 1)

    def test_k4_unusable_junit_exits_2(self):
        allow = self.allowlist({})
        self.assertEqual(self.run_main(self.junit(), allow), 2)
        self.assertEqual(self.run_main(self.dir / "missing.xml", allow), 2)
        self.assertEqual(self.run_main(self.junit(raw="<testsuites><testsuite"), allow), 2)

    def test_k5_malformed_allowlist_exits_2(self):
        junit = self.junit(case("m", "test_a", "skip"))
        self.assertEqual(self.run_main(junit, self.allowlist({"m::test_a": ""})), 2)
        self.assertEqual(self.run_main(junit, self.allowlist({"m::test_a": "   "})), 2)
        self.assertEqual(
            self.run_main(junit, self.allowlist(raw='{"m::test_a": "r1", "m::test_a": "r2"}')), 2
        )
        self.assertEqual(self.run_main(junit, self.allowlist(raw='["m::test_a"]')), 2)

    def test_k6_keys_match_exactly(self):
        spaced = "test_x[Peer consent-[a] Always allow]"
        junit = self.junit(case("m", "test_x[1]", "skip"), case("m.C", "test_x", "skip"), case("m", spaced, "skip"))
        allow = self.allowlist({"m::test_x": REASON, f"m::{spaced}": REASON})
        self.assertEqual(
            checker.skip_violations(junit, allow),
            {"unlisted": {"m::test_x[1]", "m.C::test_x"}, "stale": {"m::test_x"}},
        )

    def test_k7_xfail_counts_as_a_skip(self):
        junit = self.junit(case("m", "test_a", "xfail"))
        self.assertEqual(checker.skip_violations(junit, self.allowlist({})), {"unlisted": {"m::test_a"}, "stale": set()})
        self.assertEqual(self.run_main(junit, self.allowlist({})), 1)

    def test_failed_case_is_not_a_skip(self):
        junit = self.junit(case("m", "test_a", "fail"))
        self.assertEqual(checker.skip_violations(junit, self.allowlist({})), {"unlisted": set(), "stale": set()})

    def test_k8_committed_allowlist_is_well_formed(self):
        entries = checker.load_allowlist(ALLOWLIST)
        self.assertTrue(entries)
        for key, reason in entries.items():
            self.assertIn("::", key)
            self.assertIn("tui-", reason)


if __name__ == "__main__":
    unittest.main()
