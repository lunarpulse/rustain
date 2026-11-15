"""Keystones K1-K8 of the lane guard (tui-4): pure functions of ``lane_guard.py``.

No process is spawned. Items are synthetic ``(module, nodeid, marker names)``
rows and the module files live in ``tmp_path``. The real-suite behaviour (the
plugin inside a real collection, through xdist) is observed by the story's RED
runs, not here.
"""

from __future__ import annotations

import json
from pathlib import Path
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
    rels = {"a.py", "unused.py"}
    assert lg.full_modules(rels, ["tests_tui/"], tmp_path, False) == rels
    node_arg = str(Path(lg.HERE) / "a.py") + "::test_x"
    assert lg.full_modules(rels, [node_arg], tmp_path, False) == rels - {"a.py"}
    assert lg.full_modules(rels, ["tests_tui/"], tmp_path, True) == set()


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
