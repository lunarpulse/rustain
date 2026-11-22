"""Lane guard keystones, including subprocess coverage of pytest's actual collection scope."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from shlex import join
from shutil import copyfile
from time import monotonic
from types import SimpleNamespace

import pytest

import lane_guard as lg


def _root(tmp_path, *modules):
    for module in modules:
        path = tmp_path / module
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("", encoding="utf-8")
    return tmp_path


def _row(rel, name, *marks, param=None, cls=None):
    nodeid = f"{rel}::" + (f"{cls}::" if cls else "") + name + (f"[{param}]" if param else "")
    return rel, nodeid, set(marks)


def _judge(tmp_path, rows, modules=None, tests=None, manual=None, full=()):
    root = _root(tmp_path, *{rel for rel, _, _ in rows}, *(modules or {}), *(manual or {}))
    return lg.lane_violations(
        rows, modules or {}, tests or {}, manual or {}, root=root, full=set(full)
    )


def _empty(bad):
    return {kind: entries for kind, entries in bad.items() if entries} == {}


def test_k1_an_item_of_a_module_without_an_entry_is_unassigned(tmp_path):
    rows = [_row("a.py", "test_x"), _row("b.py", "test_y")]
    lanes, bad = _judge(tmp_path, rows, modules={"a.py": "core"})
    assert bad["unassigned"] == {"b.py::test_y"}
    assert lanes == {"a.py::test_x": "core"}

    # items of another suite in the same session (outside tests_tui/) are not the guard's business
    def item(path, nodeid):
        return SimpleNamespace(path=path, nodeid=nodeid, iter_markers=lambda: [])

    mine = item(lg.HERE / "test_x.py", "test_x.py::test_a")
    foreign = item(tmp_path / "other" / "test_y.py", "other/test_y.py::test_b")
    assert lg.owned_items([foreign, mine, foreign]) == ([(mine, "test_x.py")], [])

    # a collected path under tests_tui/ whose target escapes it is rejected, never silently ignored
    here = tmp_path / "tt"
    here.mkdir()
    (tmp_path / "elsewhere.py").write_text("", encoding="utf-8")
    (here / "linked.py").symlink_to(tmp_path / "elsewhere.py")
    (here / "real.py").write_text("", encoding="utf-8")
    linked = item(here / "linked.py", "linked.py::test_l")
    real = item(here / "real.py", "real.py::test_r")
    outside = item(tmp_path / "elsewhere.py", "elsewhere.py::test_e")
    assert lg.owned_items([linked, real, outside], here=here) == ([(real, "real.py")], [linked])


def test_k2_a_fully_assigned_suite_is_clean_and_lanes_follow_the_table(tmp_path):
    rows = [
        _row("a.py", "test_x"),
        _row("a.py", "test_over"),
        _row("g.py", "test_g"),
        _row("m.py", "test_m"),
        _row("n.py", "test_n"),
    ]
    lanes, bad = _judge(
        tmp_path,
        rows,
        modules={"a.py": "core", "g.py": "governance"},
        tests={"a.py::test_over": "native_nightly"},
        manual={"m.py": "needs a build; owner tui-5", "n.py::test_n": "owner tui-5"},
        full={"a.py", "g.py", "m.py", "n.py"},
    )
    assert _empty(bad)
    assert lanes == {
        "a.py::test_x": "core",
        "a.py::test_over": "native_nightly",
        "g.py::test_g": "governance",
        "m.py::test_m": "manual",
        "n.py::test_n": "manual",
    }


def test_k3_parametrized_and_class_ids_follow_their_function_key(tmp_path):
    assert lg.test_key("m.py", "tests_tui/m.py::t[a-b]") == "m.py::t"
    assert lg.test_key("m.py", "m.py::C::t[x]") == "m.py::C::t"
    rows = [
        _row("m.py", "t", param="a"),
        _row("m.py", "t", param="b"),
        _row("m.py", "t", cls="C"),
        _row("m.py", "t", cls="D"),
    ]
    lanes, bad = _judge(
        tmp_path,
        rows,
        modules={"m.py": "core"},
        tests={"m.py::t": "native_nightly", "m.py::C::t": "governance"},
        full={"m.py"},
    )
    assert _empty(bad)
    assert lanes == {
        "m.py::t[a]": "native_nightly",
        "m.py::t[b]": "native_nightly",
        "m.py::C::t": "governance",
        "m.py::D::t": "core",
    }


def test_k4_an_api_item_takes_the_api_lane_and_no_entry_may_name_it(tmp_path):
    rows = [_row("m.py", "test_api", "requires_api"), _row("m.py", "test_plain")]
    lanes, bad = _judge(tmp_path, rows, modules={"m.py": "core"}, full={"m.py"})
    assert _empty(bad)
    assert lanes == {"m.py::test_api": "api", "m.py::test_plain": "core"}
    for tests, manual in (
        ({"m.py::test_api": "core"}, {}),
        ({}, {"m.py::test_api": "owner tui-5"}),
    ):
        _, bad = _judge(tmp_path, rows, modules={"m.py": "core"}, tests=tests, manual=manual, full={"m.py"})
        assert bad["api_listed"] == {"m.py::test_api"}
        assert not bad["stale"]


def test_k5_capability_markers_stay_out_of_core_and_lane_markers_stay_out_of_source(tmp_path):
    rows = [_row("m.py", "test_a2a", "a2a_binary"), _row("m.py", "test_p2p", "p2p_binary")]
    _, bad = _judge(tmp_path, rows, modules={"m.py": "core"})
    assert bad["capability_in_core"] == {"m.py::test_a2a", "m.py::test_p2p"}
    _, bad = _judge(tmp_path, rows, modules={"m.py": "native_nightly"})
    assert _empty(bad)
    for lane_marker in ("core", "governance", "native_nightly", "manual"):
        rows = [
            _row("m.py", "test_src", lane_marker),
            _row("m.py", "test_api", lane_marker, "requires_api"),
            _row("m.py", "test_clean"),
        ]
        _, bad = _judge(tmp_path, rows, modules={"m.py": "native_nightly"}, full={"m.py"})
        assert bad["lane_marker_in_source"] == {"m.py::test_src", "m.py::test_api"}, lane_marker
        assert not bad["capability_in_core"]


def test_k6_stale_entries_are_judged_only_where_the_module_was_collected_in_full(tmp_path):
    rows = [_row("a.py", "test_x")]
    modules = {"a.py": "core", "gone.py": "core", "unused.py": "core"}
    tests = {"a.py::test_gone": "native_nightly"}
    root = _root(tmp_path, "a.py", "unused.py")
    full = {"a.py", "unused.py"}
    _, bad = lg.lane_violations(rows, modules, tests, {}, root=root, full=full)
    assert bad["stale"] == {"gone.py (file does not exist)", "unused.py", "a.py::test_gone"}
    # collected only through a node id (a.py), or not collected at all: not judged beyond file existence
    _, bad = lg.lane_violations(rows, modules, tests, {}, root=root, full={"unused.py"})
    assert bad["stale"] == {"gone.py (file does not exist)", "unused.py"}
    _, bad = lg.lane_violations(rows, modules, tests, {}, root=root, full=set())
    assert bad["stale"] == {"gone.py (file does not exist)"}
    _check_collection_scope(tmp_path / "pytest-scope")


def _check_collection_scope(root):
    tui = root / "tests_tui"
    empty = ("nested/test_folder_empty.py", "test_empty.py", "test_glob_empty.py")
    _root(tui, *empty)
    foreign_suite = root / "other_suite"
    foreign_suite.mkdir()
    copyfile(Path(lg.__file__), tui / "lane_guard.py")
    (root / "conftest.py").write_text('pytest_plugins = ("lane_guard",)\n', encoding="utf-8")
    (root / "pytest.ini").write_text(
        "[pytest]\ntestpaths = tests_tui\npythonpath = tests_tui\n"
        "markers =\n    core: fixture core lane\n    native_nightly: fixture nightly lane\n",
        encoding="utf-8",
    )
    (tui / "test_scope.py").write_text(
        "import pytest\n"
        "@pytest.mark.parametrize('viewport', ['80', '80x24'])\n"
        "def test_viewport(viewport):\n"
        "    assert viewport == '80x24'\n"
        "def test_nightly():\n"
        "    pass\n",
        encoding="utf-8",
    )
    (tui / "test_stable.py").write_text("def test_stable(): pass\n", encoding="utf-8")
    (foreign_suite / "test_foreign.py").write_text("def test_foreign(): pass\n", encoding="utf-8")
    removed = "test_scope.py::test_removed"
    (tui / "lanes.json").write_text(
        json.dumps({
            "modules": dict.fromkeys((*empty, "test_scope.py", "test_stable.py"), "core"),
            "tests": {"test_scope.py::test_nightly": "native_nightly", removed: "core"},
        }),
        encoding="utf-8",
    )
    case80 = "tests_tui/test_scope.py::test_viewport[80]"
    case80x24 = "tests_tui/test_scope.py::test_viewport[80x24]"
    nightly = "tests_tui/test_scope.py::test_nightly"
    stable = "tests_tui/test_stable.py::test_stable"
    foreign = "other_suite/test_foreign.py::test_foreign"
    scope_ids = (case80, case80x24, nightly)
    all_ids = (*scope_ids, stable)
    stale_keys = (*empty, removed)

    timings = root / "subcalls.log"
    suite_timeout = 30
    subcall_timeout = 10
    deadline = monotonic() + suite_timeout

    def run(*args):
        cmd = [sys.executable, "-m", "pytest", "-q", "--color=no", "--strict-markers", *args]
        command = join(cmd)
        started = monotonic()
        timeout = min(subcall_timeout, deadline - started)
        with timings.open("a", encoding="utf-8") as log:
            if timeout <= 0:
                print(f"not started: {command}", file=log)
                pytest.fail(f"K6 exceeded its {suite_timeout}s budget before {command}; see {timings}", pytrace=False)
            print(f"start (deadline in {timeout:.1f}s): {command}", file=log, flush=True)
            try:
                result = subprocess.run(
                    cmd,
                    cwd=root,
                    env={"PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1"},
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    encoding="utf-8",
                    timeout=timeout,
                )
            except subprocess.TimeoutExpired as exc:
                print(f"timed out after {monotonic() - started:.2f}s", file=log)
                partial = (exc.stdout or b"").decode("utf-8", errors="replace")
                pytest.fail(f"{command} exceeded its {timeout:.1f}s deadline; see {timings}\n{partial}", pytrace=False)
            print(f"exit {result.returncode} after {monotonic() - started:.2f}s", file=log)
            return result

    def diagnostic(result):
        return f"{join(result.args)} exited {result.returncode}; see {timings}\n{result.stdout}"

    def collect(*args, selected, stale=()):
        result = run("--collect-only", *args)
        expected_exit = pytest.ExitCode.OK if selected else pytest.ExitCode.NO_TESTS_COLLECTED
        if stale:
            expected_exit = pytest.ExitCode.INTERRUPTED
        assert result.returncode == expected_exit, diagnostic(result)
        assert ("ERROR collecting lanes.json" in result.stdout) == bool(stale), diagnostic(result)
        lines = result.stdout.splitlines()
        actual_stale = [line.removeprefix("stale: ") for line in lines if line.startswith("stale: ")]
        assert actual_stale == sorted(stale), diagnostic(result)
        assert sorted(
            line for line in lines if line.startswith(("tests_tui/", "other_suite/")) and "::" in line
        ) == sorted(selected), diagnostic(result)

    # Full collection still checks ledger rows removed by pytest's later item filters.
    collect(selected=all_ids, stale=stale_keys)
    collect("-m", "core", "--deselect", case80, selected=(case80x24, stable), stale=stale_keys)
    collect("--lf", "--cache-clear", selected=all_ids, stale=stale_keys)
    collect("--last-failed-no-failures=none", "--cache-clear", selected=all_ids, stale=stale_keys)
    collect("--lf", "--last-failed-no-failures=none", "--cache-clear", selected=())

    collect(case80x24, selected=(case80x24,))
    collect("tests_tui/test_scope.py", selected=scope_ids, stale=(removed,))
    collect("tests_tui/test_empty.py", selected=(), stale=("test_empty.py",))
    collect("other_suite", selected=(foreign,))
    collect(
        "--ignore=tests_tui/test_empty.py", "--ignore=tests_tui/nested",
        "--ignore-glob=tests_tui/test_glob_*.py",
        selected=all_ids,
        stale=(removed,),
    )

    # Let pytest, not the fixture, create a nonempty lastfailed cache.
    seeded = run("--cache-clear", case80)
    assert seeded.returncode == pytest.ExitCode.TESTS_FAILED, diagnostic(seeded)
    cached = root / ".pytest_cache/v/cache/lastfailed"
    assert cached.is_file(), diagnostic(seeded)
    assert json.loads(cached.read_text(encoding="utf-8")) == {case80: True}, diagnostic(seeded)
    collect("--lf", selected=(case80,))
    # Excluding the known failure means pytest collects everything else, even with lfnf=none.
    collect(
        "--lf", "--last-failed-no-failures=none", "--ignore=tests_tui/test_scope.py",
        selected=(stable,),
        stale=empty,
    )
    timings.unlink()


def test_k7_structural_errors_in_the_table_raise_value_error(tmp_path):
    path = tmp_path / "lanes.json"
    good = {"modules": {"a.py": "core"}, "tests": {"a.py::t": "governance"}, "manual": {"b.py": "owner tui-5"}}
    path.write_text(json.dumps(good), encoding="utf-8")
    assert lg.load_table(path) == (good["modules"], good["tests"], good["manual"])
    bad_texts = {
        "non-object": "[]",
        "unknown top-level key": '{"lanes": {}}',
        "duplicate key": '{"modules": {"a.py": "core", "a.py": "governance"}}',
        "key in two maps": '{"modules": {"a.py": "core"}, "manual": {"a.py": "owner tui-5"}}',
        "test key in two maps": '{"tests": {"a.py::t": "core"}, "manual": {"a.py::t": "owner tui-5"}}',
        "unknown lane": '{"modules": {"a.py": "manual"}}',
        "module key without .py": '{"modules": {"a": "core"}}',
        "test key without ::": '{"tests": {"a.py": "core"}}',
        "test key with a suffix": '{"tests": {"a.py::t[x]": "core"}}',
        "empty manual reason": '{"manual": {"a.py": ""}}',
        "row-free manual reason": '{"manual": {"a.py": "needs a build"}}',
        "invalid JSON": "{",
    }
    for label, text in bad_texts.items():
        path.write_text(text, encoding="utf-8")
        try:
            lg.load_table(path)
        except ValueError:
            continue
        pytest.fail(f"{label}: load_table accepted the table")


def test_k8_the_committed_table_loads_and_every_manual_reason_names_a_row():
    modules, tests, manual = lg.load_table(lg.TABLE)
    assert modules and manual
    assert all("tui-" in reason for reason in manual.values())
    assert all((lg.HERE / rel.partition("::")[0]).is_file() for rel in {*modules, *tests, *manual})
