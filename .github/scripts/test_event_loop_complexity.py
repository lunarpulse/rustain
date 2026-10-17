#!/usr/bin/env python3
"""Behavioral controls for the complete-function architectural metric CLI."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

CLI = Path(__file__).with_name("event-loop-complexity.py")


class CompleteFunctionMetricTests(unittest.TestCase):
    def invoke(self, source, *, no_site=False):
        with tempfile.TemporaryDirectory(prefix="metric-control-") as directory:
            path = Path(directory) / "event_loop.rs"
            path.write_bytes(source.encode("utf-8"))
            return subprocess.run(
                [sys.executable, *(["-S"] if no_site else []), str(CLI), str(path)],
                cwd=directory, capture_output=True, text=True, check=False,
            )

    def measure(self, source):
        result = self.invoke(source)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_complete_function_not_truncated_by_rust_literals_or_labels(self):
        source = r'''// if match while outside the function

pub async fn run<'a>(input: &'a str) {
    let character = '?';
    let escaped = '\'';
    let quoted = "escaped \" if for && || ?";
    let raw = r###"match while \" 'a || ?"###;
    let r#if = false;
    // if for while match && || ?
    /* outer if /* nested match */ while */
    'case: for x in 0..1 {
        if x > 0 && !input.is_empty() || x == 0 { break 'case; }
    }
    while false {}
    let callback = || { if false {} };
    let with_argument = |x| { if x {} };
    let future = async { if true {} };
    fn nested() { if true {} while false {} }
    scripted! { match input { _ => { if false {} } } }
    let value = operation()?;
    match input { _ => {} }
}
fn after() { if true {} }
'''
        report = self.measure(source)
        self.assertEqual(report["ccn"], 14, "lexical decision count mismatch")
        self.assertEqual(report["decisions"], {
            "&&": 1, "?": 1, "for": 1, "if": 5,
            "match": 2, "while": 1, "||": 2,
        }, "lexical decision count mismatch")
        self.assertEqual((report["start_line"], report["end_line"]), (3, 22),
                         "lexical function boundary mismatch")

    def test_multiline_signature_where_and_complete_function_identity(self):
        before = "// Unicode prefix: λ\nfn unrelated() {}\n"
        function = """pub async fn run<
    'a,
    T,
>(
    input: &'a T,
) -> bool
where
    T: AsRef<str> + Send + Sync,
{
    if input.as_ref().is_empty() { return false; }
    true
}"""
        after = "\nfn after() { while false {} }\n"
        source = before + function + after
        report = self.measure(source)
        start = len(before.encode("utf-8"))
        end = start + len(function.encode("utf-8"))
        # Explicit failures keep the semantic diagnostic on the assertion's
        # first line; assertEqual on tuples puts a multi-line diff first.
        bounds = (report["start_byte"], report["end_byte"])
        if bounds != (start, end):
            self.fail(f"function boundary mismatch: complete multiline signature: "
                      f"{bounds} != {(start, end)}")
        lines = (report["start_line"], report["end_line"])
        if lines != (3, 14):
            self.fail(f"function boundary mismatch: complete multiline lines: {lines} != (3, 14)")
        self.assertEqual(report["function"], "run", "function boundary mismatch: wrong function")
        self.assertEqual(report["function_sha256"],
                         hashlib.sha256(function.encode("utf-8")).hexdigest(),
                         "function boundary mismatch: complete function hash")
        self.assertEqual(report["source_sha256"],
                         hashlib.sha256(source.encode("utf-8")).hexdigest(),
                         "function boundary mismatch: source hash")
        self.assertEqual(report["ccn"], 3, "multiline where decision count mismatch")
        self.assertEqual(report["decisions"], {"if": 1, "where": 1},
                         "multiline where decision count mismatch")

    def test_only_top_level_run_is_the_qualified_function(self):
        report = self.measure("fn run() { fn run() { if true {} } if false {} }\n"
                              "mod other { fn run() { while false {} } }\n"
                              "impl Worker { fn run() { for x in values {} } }\n")
        self.assertEqual(report["ccn"], 2)
        self.assertEqual(report["decisions"], {"if": 1})
        self.assertEqual((report["start_line"], report["end_line"]), (1, 1))

    def test_raw_name_and_all_written_cfg_branches_are_counted(self):
        report = self.measure("fn r#run() { #[cfg(a)] if true {} #[cfg(b)] if false {} }")
        self.assertEqual(report["ccn"], 3)
        self.assertEqual(report["configuration"], "unexpanded-source-all-cfg")

    def test_nested_or_method_only_run_is_not_a_valid_target(self):
        for source in ["fn unrelated() {}\nmod nested { fn run() {} }",
                       "fn unrelated() {}\nimpl Worker { fn run() {} }"]:
            with self.subTest(source=source):
                result = self.invoke(source)
                self.assertNotEqual(result.returncode, 0, "run function missing: accepted unrelated free function")
                self.assertIn("run function missing", result.stderr)
                self.assertEqual(result.stdout, "")

    def test_multiple_cfg_targets_are_ambiguous_not_first_match_wins(self):
        result = self.invoke("#[cfg(a)] fn run() {}\n#[cfg(b)] fn run() {}")
        self.assertNotEqual(result.returncode, 0, "run function ambiguous: accepted first candidate")
        self.assertIn("run function ambiguous", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_syntax_error_anywhere_in_source_fails_closed(self):
        for source in ["fn run() { if }", "fn run() {}\nfn broken( {"]:
            with self.subTest(source=source):
                result = self.invoke(source)
                self.assertNotEqual(result.returncode, 0, "Rust parse failed: accepted invalid source")
                self.assertIn("Rust parse failed", result.stderr)
                self.assertEqual(result.stdout, "")

    def test_missing_parser_is_an_error_not_a_skipped_gate(self):
        result = self.invoke("fn run() {}", no_site=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("metric tool unavailable", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
