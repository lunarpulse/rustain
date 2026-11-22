#!/usr/bin/env python3
"""Exercise the real local nightly entrypoint and workflow without a native build."""

import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
import venv


PRODUCT = Path(__file__).resolve().parents[2]
SCRIPT = Path(".github/scripts/run-nightly-tui.py")
WORKFLOW = PRODUCT / ".github/workflows/nightly-tui.yml"
PLAIN = "anthropic, clipboard, mcp, meta-search, models-dev, ollama, openai"
FEATURE = "a2a, " + PLAIN + ", p2p"
LEDGER = {
    "plain": ["tests_tui/test_plain.py::test_history", "tests_tui/sub/test_plain.py::TestRoute::test_size[80x24]"],
    "federation": ["tests_tui/test_peer.py::test_loopback", "tests_tui/test_approval.py::test_review[rich-attached-80x24]"],
}
CASES = {
    "plain": [["test_plain", "test_history"], ["sub.test_plain.TestRoute", "test_size[80x24]"]],
    "federation": [["test_peer", "test_loopback"], ["test_approval", "test_review[rich-attached-80x24]"]],
}
PYTEST_STUB = '''
import json
import os
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

control = json.loads(Path("control.json").read_text())
args = sys.argv[1:]
def value(prefix):
    return next(arg.removeprefix(prefix) for arg in args if arg.startswith(prefix))

report = Path(value("--junitxml="))
Path("received.json").write_text(json.dumps({"args": args, "env": dict(os.environ),
    "cwd": str(Path.cwd()), "python": sys.executable, "stdin": sys.stdin.read()}))
Path(os.environ["RUSTAIN_CAPTURE_EVIDENCE_DIR"], "observer.txt").write_text("capture from subprocess")
for key in ("RUSTAIN_J21_RECEIPT_DIR", "RUSTAIN_19_20C_EVIDENCE"):
    if key in os.environ:
        Path(os.environ[key], "observer.txt").write_text(key)
if control.get("malformed"):
    report.write_text("<testsuite")
else:
    results = ET.Element("testsuites", name="pytest tests")
    tests = control["cases"]
    suite = ET.SubElement(results, "testsuite", name=control.get("suite", value("junit_suite_name=")),
                         tests=str(len(tests)), failures="0", errors="0", skipped="0", time="0.25")
    for classname, name in tests:
        case = ET.SubElement(suite, "testcase", classname=classname, name=name, time="0.1")
        if name == control.get("bad_case"):
            ET.SubElement(case, control["status"], type=control.get("type", "fixture"))
    ET.ElementTree(results).write(report, encoding="utf-8", xml_declaration=True)
if control.get("mutate_binary"):
    with Path(os.environ["RUSTAIN_TUI_BINARY"]).open("a") as handle:
        handle.write("# changed during pytest\\n")
print("stub pytest writer ended")
sys.exit(control.get("exit", 0))
'''


class NightlyRunner(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(tmp.cleanup)
        venv.EnvBuilder(symlinks=True, with_pip=False).create(tmp.name)
        cls.python = Path(tmp.name) / "bin/python"

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name).resolve()
        self.product = self.base / "product"
        (self.product / SCRIPT.parent).mkdir(parents=True)
        for script in (SCRIPT.name, "check-tui-skips.py"):
            shutil.copyfile(PRODUCT / SCRIPT.parent / script, self.product / SCRIPT.parent / script)
        (self.product / "tests_tui").mkdir()
        (self.product / "tests_tui/nightly-cases.json").write_text(json.dumps(LEDGER))
        (self.product / "tests_tui/skip-allowlist.json").write_text("{}")
        (self.product / "pytest.py").write_text(PYTEST_STUB)
        self.shims = self.base / "bin"
        self.shims.mkdir()
        self.binary = self.base / "target/debug/rustain"
        self.binary.parent.mkdir(parents=True)
        self.root = self.base / "evidence"
        self.fake_readelf()

    @staticmethod
    def executable(path, body):
        path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        path.chmod(0o700)

    def fake_readelf(self, kind="DYN (Position-Independent Executable file)", *, symbols=True):
        sections = "  [ 6] .dynsym DYNSYM 00000000000003c0 0003c0 001758 18 A 7 1 8"
        if symbols:
            sections += "\n  [44] .symtab SYMTAB 0000000000000000 ed6fc60 f62910 18 43 671934 8"
        output = f"ELF Header:\n  Type: {kind}\nSection Headers:\n{sections}"
        self.executable(self.shims / "readelf", f'''
            [ "$LC_ALL" = C ]
            [ "$#" = 5 ]
            [ "$1 $2 $3 $4" = '-W -h -S --' ]
            [ "$5" = {shlex.quote(str(self.binary))} ]
            printf '%s\\n' {shlex.quote(output)}
        ''')

    def fake_binary(self, features):
        self.executable(self.binary, f'''
            [ "$#" = 1 ] && [ "$1" = --version ]
            printf 'rustain 0.0.0 (fixture)\\nfeatures: %s\\n' {shlex.quote(features)}
        ''')

    def run_lane(self, lane="plain", features=None, binary=None, root=None, **control):
        self.fake_binary(features if features is not None else (FEATURE if lane == "federation" else PLAIN))
        control.setdefault("cases", CASES[lane])
        (self.product / "control.json").write_text(json.dumps(control))
        return subprocess.run(
            [str(self.python), str(SCRIPT), "--lane", lane, "--binary", str(binary or self.binary),
             "--root", str(root or self.root)], cwd=self.product, text=True, capture_output=True, timeout=15,
            env={**os.environ, "PATH": f"{self.shims}:{os.defpath}", "HOME": str(self.base / "ambient-home"),
                 "ANTHROPIC_API_KEY": "ambient-secret", "OPENAI_API_KEY": "ambient-secret",
                 "GITHUB_TOKEN": "ambient-secret", "TERM": "ambient-term", "TMPDIR": str(self.base),
                 "RUSTAIN_J21_VIEWPORT": "130x30", "RUSTAIN_TUI_BINARY": "/ambient/binary",
                 "RUSTAIN_CAPTURE_EVIDENCE_DIR": "/ambient/captures", "RUSTAIN_J21_RECEIPT_DIR": "/ambient/j21",
                 "RUSTAIN_19_20C_EVIDENCE": "/ambient/approval", "PYTEST_ADDOPTS": "--reruns=3"},
        )

    def assert_red(self, result, message):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stdout + result.stderr)

    def test_both_lanes_use_the_real_cli_and_child_only_roots(self):
        for lane, marker, workers, timeout, directory, key in (
            ("plain", "native_nightly and not a2a_binary and not p2p_binary", "auto", "300", "j21", "RUSTAIN_J21_RECEIPT_DIR"),
            ("federation", "native_nightly and (a2a_binary or p2p_binary)", "0", "420", "19-20c", "RUSTAIN_19_20C_EVIDENCE"),
        ):
            with self.subTest(lane=lane):
                kind = "EXEC (Executable file)" if lane == "plain" else "DYN (Position-Independent Executable file)"
                self.fake_readelf(kind)
                root = self.root / lane
                root.mkdir(parents=True)  # the outer caller may pre-create an empty lane directory
                result = self.run_lane(lane, root=root)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                received = json.loads((self.product / "received.json").read_text())
                argv, env = received["args"], received["env"]
                self.assertEqual(received["cwd"], str(self.product))
                self.assertEqual(received["python"], str(self.python))
                self.assertEqual(received["stdin"], "")
                self.assertEqual(argv[0], "tests_tui")
                self.assertEqual(argv[argv.index("-m") + 1], marker)
                self.assertEqual(argv[argv.index("-n") + 1], workers)
                self.assertIn(f"--timeout={timeout}", argv)
                self.assertIn("--durations=0", argv)
                self.assertIn("no:rerunfailures", argv)
                self.assertIn("run-nightly-tui", argv)
                self.assertFalse(any("--reruns" in item for item in argv))
                self.assertEqual({item.removeprefix("--ignore=") for item in argv if item.startswith("--ignore=")}, {
                    "tests_tui/test_story_19_19_act2_consultation.py",
                    "tests_tui/test_story_19_21_act1_distribution.py",
                    "tests_tui/test_story_19_21b_conflict_flag.py",
                    "tests_tui/test_story_19_21b_standalone_modes.py",
                } if lane == "federation" else set())
                self.assertIn(f"--junitxml={root / 'result.xml'}", argv)
                self.assertEqual(env["RUSTAIN_TUI_BINARY"], str(self.binary))
                self.assertEqual(env["RUSTAIN_CAPTURE_EVIDENCE_DIR"], str(root / "captures"))
                self.assertEqual(env[key], str(root / directory))
                self.assertTrue((root / "captures/observer.txt").is_file())
                self.assertEqual((root / directory / "observer.txt").read_text(), key)
                forbidden = {"ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GITHUB_TOKEN", "TERM", "PYTEST_ADDOPTS",
                             "RUSTAIN_J21_VIEWPORT", "RUSTAIN_J21_RECEIPT_DIR", "RUSTAIN_19_20C_EVIDENCE"} - {key}
                self.assertFalse(forbidden & env.keys())
                scratch = Path(env["TMPDIR"])
                self.assertEqual(scratch.parent, Path("/tmp"))
                self.assertLess(len(str(scratch)), 24)
                self.assertEqual(Path(env["HOME"]).parent, scratch)
                self.assertIn(f"--basetemp={scratch / 'p'}", argv)
                self.assertFalse(scratch.exists())
                log = (root / "pytest.log").read_text()
                self.assertEqual(log.count("sha256="), 2)
                self.assertEqual(log.count(f"file=ELF {kind}, not stripped"), 2)
                self.assertIn("stub pytest writer ended", log)
                self.assertIn("0 unlisted skip(s), 0 stale", log)

    def test_same_count_wrong_lane_module_class_parameter_and_duplicate_fail(self):
        small, tagged = CASES["plain"]
        for label, cases in {
            "swapped-lane": CASES["federation"],
            "same-function-excluded-module": [["test_story_19_21_act1_distribution", small[1]], tagged],
            "wrong-class": [small, ["sub.test_plain.TestOther", tagged[1]]],
            "wrong-parameter": [small, [tagged[0], "test_size[130x30]"]],
            "duplicate": [small, small],
            "missing": [small],
        }.items():
            with self.subTest(label=label):
                self.assert_red(self.run_lane(root=self.root / label, cases=cases), "JUnit case identity mismatch")

    def test_wrong_run_and_failed_statuses_are_independent_of_process_exit(self):
        self.assert_red(self.run_lane(root=self.root / "stale", suite="nightly-plain-prior"), "JUnit run identity mismatch")
        for tag, kind in (("skipped", "pytest.skip"), ("skipped", "pytest.xfail"),
                          ("failure", "failure"), ("error", "error"), ("rerun", "rerun")):
            with self.subTest(kind=kind):
                root = self.root / kind
                self.assert_red(self.run_lane(root=root, bad_case="test_history", status=tag, type=kind),
                                "JUnit cases did not pass cleanly")
                if tag == "skipped":
                    self.assertIn("Unlisted Python TUI skip", (root / "pytest.log").read_text())
        self.assert_red(self.run_lane(root=self.root / "malformed", malformed=True), "skip checker exited 2")
        self.assert_red(self.run_lane(root=self.root / "red-exit", exit=3), "pytest exited 3")

    def test_wrong_debug_features_release_profile_and_changed_bytes_fail(self):
        self.assert_red(self.run_lane("federation", features="a2a, " + PLAIN, root=self.root / "feature"),
                        "federation debug feature profile mismatch")
        self.assert_red(self.run_lane(features=FEATURE, root=self.root / "plain"), "plain debug feature profile mismatch")
        release = self.binary.parent.with_name("release") / self.binary.name
        release.parent.mkdir()
        shutil.copy2(self.binary, release)
        self.assert_red(self.run_lane(root=self.root / "release", binary=release),
                        f"expected an executable Cargo debug/rustain: {release}")
        self.fake_readelf("DYN (Shared object file)")
        self.assert_red(self.run_lane(root=self.root / "shared-object"), "DYN (Shared object file)")
        self.fake_readelf(symbols=False)
        self.assert_red(self.run_lane(root=self.root / "stripped"),
                        f"expected an unstripped ELF debug executable: {self.binary}: missing ordinary .symtab (SYMTAB)")
        self.assertFalse((self.product / "received.json").exists())
        self.fake_readelf()
        self.assert_red(self.run_lane(root=self.root / "mutate", mutate_binary=True), "binary identity changed during plain pytest")

    def test_preexisting_evidence_and_path_aliases_fail_without_writing(self):
        self.root.mkdir()
        sentinel = self.root / "result.xml"
        sentinel.write_text("prior bytes")
        self.assert_red(self.run_lane(), "lane root is not empty")
        self.assertEqual(sentinel.read_text(), "prior bytes")
        alias = self.base / "stealth"
        alias.symlink_to(self.root, target_is_directory=True)
        self.assert_red(self.run_lane(root=alias), "expected an absolute physical path")
        binary_alias = self.binary.with_name("other")
        binary_alias.symlink_to(self.binary)
        self.assert_red(self.run_lane(root=self.base / "new", binary=binary_alias), "expected an absolute physical path")
        self.assertFalse((self.product / "received.json").exists())

    def test_dotenv_is_rejected_before_native_probe(self):
        (self.product / ".env").write_text("ANTHROPIC_API_KEY=fixture-canary\n")
        result = self.run_lane()
        self.assert_red(result, "without a nonempty or linked .env")
        self.assertNotIn("fixture-canary", result.stdout + result.stderr)
        self.assertFalse((self.product / "received.json").exists())


def parse_workflow(text):
    """Read this workflow's block mappings, sequences, scalars and literal blocks.

    No YAML dependency is installed by CI Check. Fail closed on unexpected syntax
    rather than ignoring steps or treating GitHub's `on` key as a YAML 1.1 boolean.
    """
    rows = []
    for line in text.splitlines():
        indent = len(line) - len(line.lstrip(" "))
        body = line[indent:]
        if not body or body.startswith("#"):
            continue
        if body.startswith("- "):
            rows.append((indent, "-"))
            indent, body = indent + 2, body[2:]
        rows.append((indent, body))

    def block(index):
        indent, first = rows[index]
        result = [] if first == "-" else {}
        while index < len(rows) and rows[index][0] == indent:
            _, body = rows[index]
            index += 1
            if isinstance(result, list):
                if body != "-" or index == len(rows) or rows[index][0] != indent + 2:
                    raise ValueError(f"invalid workflow sequence at {body!r}")
                entry, index = block(index)
                result.append(entry)
                continue
            key, sep, raw = body.partition(":")
            if not sep or not re.fullmatch(r"[A-Za-z0-9_-]+", key) or key in result:
                raise ValueError(f"invalid or duplicate workflow key at {body!r}")
            raw = raw.strip()
            if raw == "|":
                lines = []
                while index < len(rows) and rows[index][0] > indent:
                    lines.append(rows[index][1])
                    index += 1
                value = "\n".join(lines)
            elif raw:
                value = json.loads(raw) if raw.startswith('"') or raw.isdigit() or raw in {"true", "false"} else raw
            elif index < len(rows) and rows[index][0] > indent:
                if rows[index][0] != indent + 2:
                    raise ValueError(f"invalid workflow indentation at {key!r}")
                value, index = block(index)
            else:
                value = None
            result[key] = value
        return result, index

    parsed, end = block(0)
    if end != len(rows):
        raise ValueError("unparsed workflow rows")
    return parsed


def reached(workflow, event, *, ref="branch", failed=()):
    if event not in workflow["on"]:
        return []
    (job,) = workflow["jobs"].values()
    outcomes = {}
    executed = []
    good = True

    def atom(text):
        if text == "!cancelled()":
            return True
        match = re.fullmatch(r"(github\.(?:event_name|ref_type)|steps\.[\w-]+\.outcome) == '([\w-]+)'", text)
        if not match:
            raise ValueError(f"unsupported workflow condition {text!r}")
        operand, expected = match.groups()
        observed = {"github.event_name": event, "github.ref_type": ref}.get(operand)
        return (outcomes.get(operand.split(".")[1]) if operand.startswith("steps.") else observed) == expected

    def enabled(condition):
        expression = condition.removeprefix("${{").removesuffix("}}").strip()
        terms = expression.split(" && ") if expression else []
        return (good or "!cancelled()" in terms) and all(atom(term) for term in terms)

    if enabled(job.get("if", "")):
        for step in job["steps"]:
            if "continue-on-error" in step:
                raise ValueError("nightly failures must not be masked")
            name = step.get("id")
            if enabled(step.get("if", "")):
                if name:
                    executed.append(name)
                    outcomes[name] = "failure" if name in failed else "success"
                    good &= name not in failed
            elif name:
                outcomes[name] = "skipped"
    return executed


class NightlyWorkflow(unittest.TestCase):
    def assert_contract(self, text):
        workflow = parse_workflow(text)
        self.assertEqual(workflow["on"], {"schedule": [{"cron": "17 3 * * *"}], "workflow_dispatch": None})
        (job,) = workflow["jobs"].values()
        self.assertEqual(job["timeout-minutes"], 45)
        actions = {step["uses"]: step.get("with", {}) for step in job["steps"] if "uses" in step}
        pinned = re.search(r'^channel = "([^"]+)"', (PRODUCT / "rust-toolchain.toml").read_text(), re.M)[1]
        self.assertIn(f"dtolnay/rust-toolchain@{pinned}", actions)
        self.assertEqual(actions["actions/setup-python@v5"]["python-version"], "3.10")
        self.assertIs(actions["actions/checkout@v4"]["persist-credentials"], False)
        self.assertIn('python3 -m pip install -e "./tests_tui[dev]"', [step.get("run") for step in job["steps"]])
        steps = {step["id"]: step for step in job["steps"] if "id" in step}
        expected = ["roots", "build-plain", "plain", "build-federation", "federation"]
        for event in ("schedule", "workflow_dispatch"):
            self.assertEqual(reached(workflow, event), expected, f"{event} must reach both exact lanes in order")
            self.assertEqual(reached(workflow, event, failed=("plain",)), expected)
            self.assertEqual(reached(workflow, event, failed=("build-plain",)), [step for step in expected if step != "plain"])
            self.assertEqual(reached(workflow, event, failed=("build-federation",)), expected[:-1])
            self.assertEqual(reached(workflow, event, failed=("roots",)), ["roots"])
        for event in ("pull_request", "push"):
            self.assertEqual(reached(workflow, event), [])
        self.assertEqual(reached(workflow, "workflow_dispatch", ref="tag"), [])
        self.assertEqual(shlex.split(steps["build-plain"]["run"]), ["cargo", "build", "--locked", "--bin", "rustain"])
        self.assertEqual(shlex.split(steps["build-federation"]["run"]),
                         ["cargo", "build", "--locked", "--features", "a2a,p2p", "--bin", "rustain"])
        for lane in ("plain", "federation"):
            self.assertEqual(steps[lane]["env"], {"EVIDENCE": "${{ steps.roots.outputs.root }}"})
            self.assertEqual(shlex.split(steps[lane]["run"]), ["python3", str(SCRIPT), "--lane", lane,
                "--binary", "$CARGO_TARGET_DIR/debug/rustain", "--root", f"$EVIDENCE/{lane}"])
        uploads = {step["with"]["name"]: step["with"]["path"].splitlines()
                   for step in job["steps"] if step.get("uses") == "actions/upload-artifact@v4"}
        self.assertEqual(set(uploads), {"nightly-tui-plain", "nightly-tui-federation"})
        for lane in ("plain", "federation"):
            prefix = "${{ steps.roots.outputs.root }}/" + lane + "/"
            paths = uploads[f"nightly-tui-{lane}"]
            self.assertIn(prefix + "result.xml", paths)
            self.assertIn(prefix + "pytest.log", paths)
            self.assertTrue(all(path.startswith(prefix) and Path(path).suffix and "**" not in path for path in paths))

    def test_daily_and_dispatch_workflow_reach_both_lanes(self):
        self.assert_contract(WORKFLOW.read_text())

    def test_wrong_cadence_and_dispatch_only_lane_are_rejected(self):
        original = WORKFLOW.read_text()
        for bad, diagnostic in (
            (original.replace("17 3 * * *", "17 4 * * *"), "17 3"),
            (original.replace("steps.build-federation.outcome == 'success'", "steps.build-federation.outcome == 'success' && github.event_name == 'workflow_dispatch'"),
             "schedule must reach both exact lanes"),
        ):
            with self.subTest(diagnostic=diagnostic), self.assertRaisesRegex(AssertionError, diagnostic):
                self.assert_contract(bad)


if __name__ == "__main__":
    unittest.main()
