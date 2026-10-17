#!/usr/bin/env python3
"""Exercise the actual signoff CLI against disposable repositories and commits."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

CLI = Path(__file__).with_name("ratchet-signoff-guard.py").resolve()
NAMES = (
    "MAX_KNOWN_BYPASSES", "MAX_KNOWN_STD_SYNC_LOCKS",
    "MAX_KNOWN_DURABLE_ITEM_CLAIM_IDENTIFIERS", "EVENT_LOOP_BASELINE_LINES",
    "EVENT_LOOP_BASELINE_SHA", "EVENT_LOOP_SOFT_BUDGET", "EVENT_LOOP_HARD_BUDGET",
    "EXPECTED_HANDLE_COUNT", "EVENT_LOOP_RUN_BASELINE_CCN", "COMPLEXITY_MULTIPLIER_PCT",
)
BASELINE = "EVENT_LOOP_RUN_BASELINE_CCN"
MULTIPLIER = "COMPLEXITY_MULTIPLIER_PCT"
SOURCE = "".join(f"const {name}: usize = 1;\n" for name in NAMES)


def signoff(name):
    return f"RATCHET-SIGNOFF: {name} — behavioral control"


def changed(*names):
    source = SOURCE
    for name in names:
        source = source.replace(f"const {name}: usize = 1;", f"const {name}: usize = 2;")
    return source


class RatchetSignoffGuardTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ratchet-signoff-control-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.source = self.repo / "tests" / "conformance.rs"
        self.source.parent.mkdir()
        self.git("init", "--quiet")
        self.git("config", "user.name", "Ratchet Behavioral Control")
        self.git("config", "user.email", "ratchet-control@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        hooks = self.repo / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.base = self.commit(SOURCE)

    def git(self, *arguments):
        result = subprocess.run(["git", *arguments], cwd=self.repo, capture_output=True,
                                text=True, check=True)
        return result.stdout.strip()

    def commit(self, source, message="Behavioral control"):
        if source is None:
            self.source.unlink()
        else:
            self.source.write_text(source, encoding="utf-8")
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def invoke(self, source=SOURCE, *, message="Behavioral control", body="", base=None,
               head=None, body_file=False):
        commit = self.commit(source, message)
        arguments = [sys.executable, str(CLI), "--base", base or self.base,
                     "--head", head or commit]
        environment = dict(os.environ, PR_BODY=body)
        if body_file:
            path = self.repo / "pr-body.txt"
            path.write_text(body, encoding="utf-8")
            arguments.extend(["--pr-body-file", str(path)])
            environment["PR_BODY"] = ""
        return subprocess.run(arguments, cwd=self.repo, env=environment,
                              capture_output=True, text=True, check=False)

    def assert_passes(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)

    def assert_unsigned(self, result, *names):
        self.assertEqual(result.returncode, 1, result.stderr)
        for name in names:
            self.assertIn(f"RATCHET-SIGNOFF: {name} —", result.stderr)

    def test_same_source_without_signoff_passes(self):
        self.assert_passes(self.invoke())

    def test_baseline_change_fails_unsigned(self):
        self.assert_unsigned(self.invoke(changed(BASELINE)), BASELINE)

    def test_multiplier_change_fails_unsigned(self):
        self.assert_unsigned(self.invoke(changed(MULTIPLIER)), MULTIPLIER)

    def test_every_preexisting_protected_constant_fails_unsigned(self):
        for name in NAMES[:-2]:
            with self.subTest(name=name):
                self.assert_unsigned(self.invoke(changed(name)), name)

    def test_unrelated_signoff_fails(self):
        self.assert_unsigned(self.invoke(changed(BASELINE), body=signoff(MULTIPLIER)), BASELINE)

    def test_two_changes_with_only_one_signoff_fail(self):
        result = self.invoke(changed(BASELINE, MULTIPLIER), body=signoff(BASELINE))
        self.assert_unsigned(result, MULTIPLIER)
        self.assertNotIn(f"RATCHET-SIGNOFF: {BASELINE} —", result.stderr)

    def test_repeated_signoff_does_not_cover_another_constant(self):
        result = self.invoke(changed(BASELINE, MULTIPLIER), body="\n".join([signoff(BASELINE)] * 2))
        self.assert_unsigned(result, MULTIPLIER)

    def test_all_matching_pr_body_signoffs_pass(self):
        self.assert_passes(self.invoke(changed(*NAMES), body="\n".join(map(signoff, NAMES))))

    def test_commit_message_signoff_passes(self):
        self.assert_passes(self.invoke(changed(BASELINE), message="Control\n\n" + signoff(BASELINE)))

    def test_signoffs_across_commits_and_body_pass(self):
        self.commit(SOURCE, "Control\n\n" + signoff(BASELINE))
        self.assert_passes(self.invoke(changed(BASELINE, MULTIPLIER), body=signoff(MULTIPLIER)))

    def test_pr_body_file_signoff_passes(self):
        self.assert_passes(self.invoke(changed(BASELINE), body=signoff(BASELINE), body_file=True))

    def test_suffixed_constant_token_is_not_exact(self):
        self.assert_unsigned(self.invoke(changed(BASELINE), body=signoff(BASELINE + "_OTHER")), BASELINE)

    def test_two_names_on_one_trailer_only_sign_the_first(self):
        result = self.invoke(changed(BASELINE, MULTIPLIER), body=f"RATCHET-SIGNOFF: {BASELINE} {MULTIPLIER}")
        self.assert_unsigned(result, MULTIPLIER)

    def test_base_commit_signoff_is_not_in_the_pr_range(self):
        self.base = self.commit(SOURCE, "Control\n\n" + signoff(BASELINE))
        self.assert_unsigned(self.invoke(changed(BASELINE)), BASELINE)

    def test_multiline_value_only_change_fails_unsigned(self):
        before = SOURCE.replace(f"const {BASELINE}: usize = 1;", f"const\n{BASELINE}: usize =\n    1;\n")
        self.base = self.commit(before)
        after = before.replace("    1;", "    2;")
        self.assert_unsigned(self.invoke(after), BASELINE)

    def test_declaration_deletion_requires_its_signoff(self):
        source = SOURCE.replace(f"const {BASELINE}: usize = 1;\n", "")
        self.assert_unsigned(self.invoke(source), BASELINE)
        self.assert_passes(self.invoke(source, body=signoff(BASELINE)))

    def test_declaration_addition_requires_its_signoff(self):
        self.base = self.commit(SOURCE.replace(f"const {MULTIPLIER}: usize = 1;\n", ""))
        self.assert_unsigned(self.invoke(SOURCE), MULTIPLIER)
        self.assert_passes(self.invoke(SOURCE, body=signoff(MULTIPLIER)))

    def test_whitespace_and_nested_comments_do_not_need_signoff(self):
        source = SOURCE.replace("const ", "const /* outer /* nested */ comment */\n").replace(" = 1;", "\n=\n 1 ; // value\n")
        self.assert_passes(self.invoke(source))

    def test_commented_and_literal_fake_declarations_are_ignored(self):
        source = SOURCE + f'''// const {BASELINE}: usize = 999;
/* const {MULTIPLIER}: usize = 999; /* nested */ */
const NOTE: &str = "const {BASELINE}: usize = 999;";
const RAW: &str = r###"const {MULTIPLIER}: usize = 999; /*"###;
const CHARACTER: char = '"';
'''
        self.assert_passes(self.invoke(source))

    def test_commenting_out_real_declaration_requires_signoff(self):
        source = SOURCE.replace(f"const {BASELINE}:", f"// const {BASELINE}:")
        self.assert_unsigned(self.invoke(source), BASELINE)

    def test_string_constant_value_change_requires_signoff(self):
        name = "EVENT_LOOP_BASELINE_SHA"
        before = SOURCE.replace(f"const {name}: usize = 1;", f'const {name}: &str = "before";')
        self.base = self.commit(before)
        self.assert_unsigned(self.invoke(before.replace('"before"', '"after"')), name)

    def test_missing_base_ref_fails_closed(self):
        result = self.invoke(base="missing-base-ref")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("failed closed", result.stderr)

    def test_missing_head_ref_fails_closed(self):
        result = self.invoke(head="missing-head-ref")
        self.assertEqual(result.returncode, 2, result.stderr)

    def test_missing_source_read_fails_closed(self):
        result = self.invoke(None)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("git show", result.stderr)

    def test_unterminated_comment_fails_closed(self):
        result = self.invoke(SOURCE + "/* unfinished")
        self.assertEqual(result.returncode, 2, result.stderr)

    def test_duplicate_declaration_change_cannot_hide_behind_unchanged_one(self):
        source = SOURCE + f"const {BASELINE}: usize = 3;\n"
        self.assert_unsigned(self.invoke(source), BASELINE)


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RatchetSignoffGuardTests)
    if suite.countTestCases() == 0:
        raise SystemExit("No ratchet signoff behavioral controls selected")
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
