"""Assign every collected tests_tui test to one lane (lanes.json); fail collection otherwise.

Lanes: ``core`` and ``governance`` run per PR, ``native_nightly`` runs in the
integrator's parity leg until a nightly job exists, ``manual`` is never selected,
and ``requires_api`` tests form the API lane (never in ``lanes.json``).

The plugin is loaded by ``conftest.py`` (``pytest_plugins``). Its hook runs
``tryfirst``, so it sees every collected item before ``-m``, ``-k`` and
``--deselect`` remove any. A violation is reported as one failed collect report
named ``lanes.json``: raising or ``pytest.exit`` inside an xdist worker would
crash the controller and lose the message.

Scope: only items collected under this directory are judged (by collection path); items of
another suite in the same session are ignored; an item collected under this directory
through a symlink whose target leaves it is a violation (``symlink_escape``).

Ledger paths must exist. Existing keys are judged stale only for modules pytest
collected in full, including modules with no tests. Explicit node selections and
files ignored or pruned by last-failed are not full collections. An empty
last-failed cache normally collects everything, unless its active mode selects none.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

LANES = ("core", "governance", "native_nightly")
LANE_MARKERS = frozenset(LANES) | {"manual"}
CAPABILITY = frozenset({"a2a_binary", "p2p_binary"})
API = "requires_api"
KINDS = (
    "unassigned",
    "api_listed",
    "capability_in_core",
    "lane_marker_in_source",
    "stale",
)
HERE = Path(__file__).resolve().parent
TABLE = HERE / "lanes.json"
REPORT_ID = "lanes.json"


def _no_duplicates(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError(f"duplicate key {key!r}")
        out[key] = value
    return out


def load_table(path):
    """Return ``(modules, tests, manual)``; raise ``ValueError`` on a structural error."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=_no_duplicates)
    except json.JSONDecodeError as exc:
        raise ValueError(f"not valid JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise ValueError("top level is not an object")
    unknown = sorted(set(data) - {"modules", "tests", "manual"})
    if unknown:
        raise ValueError(f"unknown top-level key {unknown[0]!r}")
    maps = {}
    for name in ("modules", "tests", "manual"):
        section = data.get(name, {})
        if not isinstance(section, dict):
            raise ValueError(f"{name!r} is not an object")
        maps[name] = section
    modules, tests, manual = maps["modules"], maps["tests"], maps["manual"]
    for name, section in (("modules", modules), ("tests", tests)):
        for key, lane in section.items():
            if lane not in LANES:
                raise ValueError(f"{name}[{key!r}]: lane {lane!r} is not one of {LANES}")
    for key, reason in manual.items():
        if not isinstance(reason, str) or "tui-" not in reason:
            raise ValueError(f"manual[{key!r}]: the reason must name the owner row (tui-...)")
    for key in modules:
        if "::" in key or not key.endswith(".py"):
            raise ValueError(f"modules key {key!r} is not a module path ending in .py")
    for name, section in (("tests", tests), ("manual", manual)):
        for key in section:
            module, sep, _ = key.partition("::")
            if name == "tests" and not sep:
                raise ValueError(f"tests key {key!r} has no '::'")
            if not module.endswith(".py"):
                raise ValueError(f"{name} key {key!r} does not start with a module path ending in .py")
            if "[" in key:
                raise ValueError(f"{name} key {key!r} carries a parametrize suffix")
    for key in sorted(set(modules) & set(manual)) + sorted(set(tests) & set(manual)):
        raise ValueError(f"key {key!r} appears in two maps")
    return modules, tests, manual


def test_key(rel, nodeid):
    """``<module>::<Class>::<function>`` of an item: no parametrize suffix, no rootdir prefix."""
    _, _, rest = nodeid.partition("::")
    return f"{rel}::{rest.partition('[')[0]}"


class _ModuleScope:
    """Observe full module collection independently of the selected items."""

    def __init__(self):
        self.full_modules = set()
        self._reports = {}

    @pytest.hookimpl(wrapper=True, trylast=True)
    def pytest_make_collect_report(self, collector):
        report = yield
        # Pytest's runner attaches CallInfo until the make-report hook returns.
        # The empty placeholder from last-failed's skip-files hook has no call.
        if isinstance(collector, pytest.Module) and report.passed and hasattr(report, "call"):
            path = collector.path
            if path.is_relative_to(HERE) and path.resolve().is_relative_to(HERE):
                # This inner wrapper sees the result before last-failed trims it.
                self._reports[report] = (path.relative_to(HERE).as_posix(), len(report.result))
        return report

    def pytest_collectreport(self, report):
        # Pytest emits a successful module report when it traverses that module;
        # probing it to resolve an explicit :: target does not emit the report.
        candidate = self._reports.pop(report, None)
        if candidate is not None:
            rel, child_count = candidate
            if len(report.result) == child_count:
                self.full_modules.add(rel)

    def pytest_collection_finish(self):
        self._reports.clear()


_MODULE_SCOPE = pytest.StashKey[_ModuleScope]()


def lane_violations(rows, modules, tests, manual, *, root, full):
    """``rows`` = ``[(rel, nodeid, marker_names)]`` -> ``({nodeid: lane|"api"}, {kind: {entry}})``."""
    lanes = {}
    bad = {kind: set() for kind in KINDS}
    used = set()
    for rel, nodeid, marks in rows:
        key = test_key(rel, nodeid)
        if LANE_MARKERS & marks:
            bad["lane_marker_in_source"].add(key)
        if API in marks:
            lanes[nodeid] = "api"
            for entry in (tests, manual):
                if key in entry:
                    bad["api_listed"].add(key)
                    used.add(key)
            continue
        if key in tests:
            lane, hit = tests[key], key
        elif key in manual:
            lane, hit = "manual", key
        elif rel in modules:
            lane, hit = modules[rel], rel
        elif rel in manual:
            lane, hit = "manual", rel
        else:
            bad["unassigned"].add(key)
            continue
        used.add(hit)
        lanes[nodeid] = lane
        if lane == "core" and CAPABILITY & marks:
            bad["capability_in_core"].add(key)
    for entry in set(modules) | set(tests) | set(manual):
        rel = entry.partition("::")[0]
        if not (Path(root) / rel).is_file():
            bad["stale"].add(f"{entry} (file does not exist)")
        elif rel in full and entry not in used:
            bad["stale"].add(entry)
    return lanes, bad


def violation_lines(bad):
    return [f"{kind}: {entry}" for kind in KINDS for entry in sorted(bad.get(kind, ()))]


def owned_items(items, here=HERE):
    """``(owned, escaped)``: ``owned`` = ``[(item, path relative to tests_tui/)]``, ``escaped`` = items.

    Ownership follows the collection path (as pytest collected it, symlinks not followed).
    An owned item whose resolved target leaves ``here`` is returned in ``escaped`` (the
    caller fails the collection: it would otherwise be judged by nothing). Items whose
    collection path is outside ``here`` belong to another suite in the same session and
    are ignored.
    """
    lexical = Path(os.path.abspath(Path(here)))
    resolved = Path(here).resolve()
    owned, escaped = [], []
    for item in items:
        collected = Path(os.path.abspath(item.path))
        try:
            rel = collected.relative_to(lexical)
        except ValueError:
            try:
                rel = collected.relative_to(resolved)
            except ValueError:
                continue
        try:
            Path(item.path).resolve().relative_to(resolved)
        except ValueError:
            escaped.append(item)
            continue
        owned.append((item, rel.as_posix()))
    return owned, escaped


def _report(config, lines):
    text = "tests_tui/lane_guard.py: lanes.json does not assign every test to exactly one lane\n" + "\n".join(lines)
    config.hook.pytest_collectreport(report=pytest.CollectReport(REPORT_ID, "failed", text, []))


def pytest_configure(config):
    scope = config.stash[_MODULE_SCOPE] = _ModuleScope()
    config.pluginmanager.register(scope)


@pytest.hookimpl(tryfirst=True)
def pytest_collection_modifyitems(config, items):
    try:
        modules, tests, manual = load_table(TABLE)
    except (OSError, ValueError) as exc:
        _report(config, [f"malformed: {exc}"])
        return
    owned, escaped = owned_items(items)
    rows = [(rel, item.nodeid, {mark.name for mark in item.iter_markers()}) for item, rel in owned]
    full = config.stash[_MODULE_SCOPE].full_modules
    last_failed = config.pluginmanager.get_plugin("lfplugin")
    # Use the live plugin state: a nonempty cache with no matching selected
    # failures is not the same as the cache plugin's no-failures fallback.
    if (
        config.getoption("last_failed_no_failures", "all") == "none"
        and last_failed is not None
        and last_failed.active
        and not last_failed.lastfailed
    ):
        full = ()
    lanes, bad = lane_violations(rows, modules, tests, manual, root=HERE, full=full)
    for item, _ in owned:
        lane = lanes.get(item.nodeid)
        if lane and lane != "api":
            item.add_marker(getattr(pytest.mark, lane))
    lines = violation_lines(bad) + [
        f"symlink_escape: {item.nodeid} (collected under tests_tui/, target outside it)" for item in escaped
    ]
    if lines:
        _report(config, lines)
