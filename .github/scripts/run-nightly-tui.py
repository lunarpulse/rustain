#!/usr/bin/env python3
"""Run one credential-free debug TUI lane and verify its exact pytest evidence."""

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET


PRODUCT = Path(__file__).resolve().parents[2]
SCRIPTS = PRODUCT / ".github" / "scripts"
TUI = PRODUCT / "tests_tui"
DEFAULT_FEATURES = {"anthropic", "openai", "ollama", "clipboard", "mcp", "meta-search", "models-dev"}
LANES = {
    "plain": ("native_nightly and not a2a_binary and not p2p_binary", "auto", 300, "j21", "RUSTAIN_J21_RECEIPT_DIR"),
    "federation": ("native_nightly and (a2a_binary or p2p_binary)", "0", 420, "19-20c", "RUSTAIN_19_20C_EVIDENCE"),
}
CAPTURE_MODULES = (
    "test_story_19_19_act2_consultation.py",
    "test_story_19_21_act1_distribution.py",
    "test_story_19_21b_conflict_flag.py",
    "test_story_19_21b_standalone_modes.py",
)


def physical_path(path):
    if not path.is_absolute() or path.resolve() != path:
        raise ValueError(f"expected an absolute physical path: {path}")
    return path


def report_key(nodeid):
    """Translate a product-relative node ID to pytest xunit2's (classname, name)."""
    filename, sep, test = nodeid.partition("::")
    module = Path(filename).relative_to("tests_tui")
    if not sep or module.suffix != ".py" or ".." in module.parts or not test:
        raise ValueError(f"invalid nightly node ID: {nodeid!r}")
    qualified, bracket, parameter = test.partition("[")
    classes, _, function = qualified.rpartition("::")
    classname = ".".join(module.with_suffix("").parts + tuple(classes.split("::") if classes else ()))
    return classname, function + bracket + parameter


def expected_cases(lane):
    ledger = json.loads((TUI / "nightly-cases.json").read_text(encoding="utf-8"))
    if not isinstance(ledger, dict) or set(ledger) != set(LANES):
        raise ValueError("nightly case ledger must define exactly plain and federation")
    nodes = ledger[lane]
    if not isinstance(nodes, list) or not nodes or any(not isinstance(node, str) for node in nodes):
        raise ValueError(f"invalid {lane} nightly case list")
    by_key = {report_key(node): node for node in nodes}
    if len(by_key) != len(nodes):
        raise ValueError(f"duplicate or ambiguous identities in {lane} nightly case ledger")
    return by_key


def require_membership(expected, actual, source):
    counts = Counter(actual)
    missing = sorted(set(expected) - counts.keys())
    unexpected = sorted(counts.keys() - set(expected))
    duplicate = sorted(case for case, count in counts.items() if count > 1)
    if missing or unexpected or duplicate:
        raise ValueError(f"{source} case identity mismatch: missing={missing}; unexpected={unexpected}; duplicate={duplicate}")


def pytest_collection_finish(session):
    """Loaded only in the child; verify the selected items before any test runs."""
    import pytest

    lane = os.environ["RUSTAIN_NIGHTLY_LANE"]
    try:
        actual = []
        for item in session.items:
            filename, _, _ = item.nodeid.partition("::")
            if item.path.resolve() != TUI / filename:
                raise ValueError(f"collection path does not match its ID: {item.nodeid}: {item.path}")
            if item.get_closest_marker("requires_api") or item.get_closest_marker("xfail"):
                raise ValueError(f"nightly collection contains API/xfail-marked case: {item.nodeid}")
            actual.append(f"tests_tui/{item.nodeid}")
        require_membership(expected_cases(lane).values(), actual, "pytest collection")
    except (OSError, ValueError) as error:
        raise pytest.UsageError(str(error)) from error


def validate_junit(report, suite_name, expected):
    root = ET.parse(report).getroot()
    suites = list(root) if root.tag == "testsuites" else [root]
    if len(suites) != 1 or suites[0].tag != "testsuite" or suites[0].get("name") != suite_name:
        raise ValueError(f"JUnit run identity mismatch: expected one suite named {suite_name!r}")
    suite = suites[0]
    if any(child.tag not in {"testcase", "properties", "system-out", "system-err"} for child in suite):
        raise ValueError("JUnit contains an unexpected suite element")
    cases = suite.findall("testcase")
    keys = [(case.get("classname", ""), case.get("name", "")) for case in cases]
    require_membership(expected, keys, "JUnit")
    bad = [(expected[key], child.tag, child.get("type")) for key, case in zip(keys, cases) for child in case
           if child.tag not in {"properties", "system-out", "system-err"}]
    if bad:
        raise ValueError(f"JUnit cases did not pass cleanly: {bad}")
    totals = {"tests": str(len(cases)), "failures": "0", "errors": "0", "skipped": "0"}
    if any(suite.get(key) != value for key, value in totals.items()) or any(
        suite.get(key, "0") != "0" for key in ("reruns", "xfail", "xfailed", "xpassed")
    ):
        raise ValueError(f"JUnit suite totals do not describe {len(cases)} passes: {suite.attrib}")


def binary_identity(binary, lane, env, emit):
    def fingerprint():
        st = physical_path(binary).stat()
        return st.st_dev, st.st_ino, st.st_mode, st.st_size, st.st_mtime_ns

    if sys.platform != "linux":
        raise ValueError("nightly TUI requires Linux")
    before = fingerprint()
    if binary.name != "rustain" or binary.parent.name != "debug" or not stat.S_ISREG(before[2]) or not os.access(binary, os.X_OK):
        raise ValueError(f"expected an executable Cargo debug/rustain: {binary}")

    elf = subprocess.run(["readelf", "-W", "-h", "-S", "--", str(binary)], cwd=PRODUCT,
                         env={**env, "LC_ALL": "C"}, stdin=subprocess.DEVNULL,
                         capture_output=True, text=True, timeout=10)
    expected_elf = f"expected an unstripped ELF debug executable: {binary}"
    if elf.returncode or elf.stderr:
        problem = elf.stderr.strip() or f"readelf exited with status {elf.returncode}"
        raise ValueError(f"{expected_elf}: {problem}")
    elf_type = re.search(r"^[ \t]*Type:[ \t]*(.+)$", elf.stdout, re.MULTILINE)
    kind = elf_type[1].strip() if elf_type else "missing ELF file type"
    if kind not in {"EXEC (Executable file)", "DYN (Position-Independent Executable file)"}:
        raise ValueError(f"{expected_elf}: {kind}")
    # Dynamically linked stripped binaries still have DYNSYM.
    if not re.search(r"^[ \t]*\[[ \t]*\d+\][ \t]+\.symtab[ \t]+SYMTAB[ \t]", elf.stdout, re.MULTILINE):
        raise ValueError(f"{expected_elf}: missing ordinary .symtab (SYMTAB) section")
    file_type = f"ELF {kind}, not stripped"
    version = subprocess.run([str(binary), "--version"], cwd=PRODUCT, env=env,
                             stdin=subprocess.DEVNULL, capture_output=True, text=True,
                             check=True, timeout=10).stdout.strip()
    features = [line.removeprefix("features: ").split(", ") for line in version.splitlines() if line.startswith("features: ")]
    wanted = DEFAULT_FEATURES | ({"a2a", "p2p"} if lane == "federation" else set())
    if not version.startswith("rustain ") or len(features) != 1 or sorted(features[0]) != sorted(wanted):
        raise ValueError(f"{lane} debug feature profile mismatch: expected {sorted(wanted)}, got {features}")
    digest = hashlib.sha256()
    with binary.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    if fingerprint() != before:
        raise ValueError(f"binary changed while reading its identity: {binary}")
    sha = digest.hexdigest()
    emit(f"{lane} executable: {binary}\nsha256={sha} bytes={before[3]} file={file_type}\n{version}")
    return before, sha, file_type, version


def child_environment(lane, binary, root, scratch):
    _, _, _, capture_dir, capture_var = LANES[lane]
    home = scratch / "h"
    home.mkdir()
    env = {
        "PATH": os.environ.get("PATH", os.defpath), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
        "CI": "true", "GITHUB_ACTIONS": "true", "HOME": str(home), "TMPDIR": str(scratch),
        "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
        "XDG_CACHE_HOME": str(home / "cache"), "XDG_STATE_HOME": str(home / "state"),
        "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1", "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
        "PYTHONPATH": str(SCRIPTS), "RUSTAIN_NIGHTLY_LANE": lane, "RUSTAIN_TUI_BINARY": str(binary),
        "RUSTAIN_MODELS_DEV_URL": "http://127.0.0.1:1",  # scripted fixtures override the deny-by-default URL
        "RUSTAIN_CAPTURE_EVIDENCE_DIR": str(root / "captures"), capture_var: str(root / capture_dir),
    }
    for path in (root / "captures", root / capture_dir):
        path.mkdir()
    return env


def run_lane(lane, binary, root, interrupted):
    physical_path(root).mkdir(parents=True, exist_ok=True, mode=0o700)
    if any(root.iterdir()):
        raise ValueError(f"nightly lane root is not empty: {root}")
    with (root / "pytest.log").open("x", encoding="utf-8") as log:
        def emit(text):
            print(text, flush=True)
            print(text, file=log, flush=True)

        try:
            # Several real fixtures create Unix sockets below pytest's tmp_path.
            with tempfile.TemporaryDirectory(prefix="nt-", dir="/tmp") as tmp:
                scratch = Path(tmp)
                env = child_environment(lane, binary, root, scratch)
                expected = expected_cases(lane)
                # The harness merges the checkout's .env after the process env.
                dotenv = PRODUCT / ".env"
                if dotenv.is_symlink() or (dotenv.exists() and (not dotenv.is_file() or dotenv.stat().st_size)):
                    raise ValueError("nightly TUI requires a checkout without a nonempty or linked .env")
                if interrupted:
                    return 128 + interrupted[0]
                before = binary_identity(binary, lane, env, emit)
                if interrupted:
                    return 128 + interrupted[0]
                marker, workers, timeout, _, _ = LANES[lane]
                report = root / "result.xml"
                suite = f"nightly-{lane}-{scratch.name}"
                command = [sys.executable, "-m", "pytest", "tests_tui", "--rootdir=tests_tui", "-m", marker,
                           "-p", "xdist.plugin", "-p", "pytest_timeout", "-p", "no:rerunfailures",
                           "-p", "no:cacheprovider", "-p", "run-nightly-tui", "-n", workers,
                           f"--timeout={timeout}", "--durations=0", "-v", f"--basetemp={scratch / 'p'}",
                           f"--junitxml={report}", "-o", "junit_family=xunit2", "-o", f"junit_suite_name={suite}",
                           "-o", "xfail_strict=true"]
                if lane == "federation":
                    command.extend(f"--ignore=tests_tui/{module}" for module in CAPTURE_MODULES)
                emit(f"{lane} pytest argv: {shlex.join(command)}\nlog: {root / 'pytest.log'}")
                # Intentionally share the supervising parity process group. Signal handlers only
                # record the first signal; do not unwind or clean scratch while pytest can write.
                outcome = subprocess.run(command, cwd=PRODUCT, env=env, stdin=subprocess.DEVNULL,
                                         stdout=log, stderr=subprocess.STDOUT).returncode
                if interrupted:
                    emit(f"{lane} interrupted by signal {interrupted[0]} after joining pytest (exit {outcome})")
                    return 128 + interrupted[0]
                failures = []
                if outcome:
                    failures.append(f"pytest exited {outcome}")
                try:
                    if binary_identity(binary, lane, env, emit) != before:
                        raise ValueError(f"binary identity changed during {lane} pytest: {binary}")
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    failures.append(str(error))
                if physical_path(report).is_file():
                    skips = subprocess.run([sys.executable, str(SCRIPTS / "check-tui-skips.py"), str(report),
                                            str(TUI / "skip-allowlist.json")], cwd=PRODUCT, env=env,
                                           stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT).returncode
                    if skips:
                        failures.append(f"TUI skip checker exited {skips}")
                    try:
                        validate_junit(report, suite, expected)
                    except (ValueError, ET.ParseError) as error:
                        failures.append(str(error))
                else:
                    failures.append(f"missing regular JUnit report: {report}")
                if failures:
                    raise ValueError("; ".join(failures))
                emit(f"{lane}: {len(expected)} exact nightly cases passed once without skips")
                return 0
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            emit(f"nightly TUI: {error}")
            return 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", required=True, choices=LANES)
    parser.add_argument("--binary", type=Path, required=True, help="absolute path to Cargo debug/rustain")
    parser.add_argument("--root", type=Path, required=True, help="fresh absolute lane evidence directory")
    args = parser.parse_args()
    if Path.cwd() != PRODUCT:
        parser.error(f"run from the product root: {PRODUCT}")
    interrupted = []

    def remember_signal(signum, _frame):
        if not interrupted:
            interrupted.append(signum)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, remember_signal)
    try:
        status = run_lane(args.lane, args.binary, args.root, interrupted)
    except (OSError, ValueError) as error:
        print(f"nightly TUI: {error}", file=sys.stderr)
        status = 1
    return 128 + interrupted[0] if interrupted else status


if __name__ == "__main__":
    sys.exit(main())
