"""Subprocess-free controls for the inspector smoke's binary prerequisites."""
import json

import pytest

import test_story_21_5_tool_inspector as inspector


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """These prerequisite controls never build or launch a binary."""


@pytest.fixture(autouse=True)
def no_processes(monkeypatch):
    def forbidden(*_args, **_kwargs):
        pytest.fail("binary prerequisite controls must not launch a process")
    monkeypatch.setattr(inspector.subprocess, "Popen", forbidden)
    monkeypatch.setattr(inspector.pexpect, "spawn", forbidden)
    monkeypatch.setattr(inspector.SceneStub, "start", forbidden)
    monkeypatch.setattr(inspector._TUI, "start", forbidden)


@pytest.fixture
def binaries(monkeypatch, tmp_path):
    default = tmp_path / "target/debug/rustain"
    override = tmp_path / "selected/debug/rustain"
    for path, content in ((default, b"ordinary debug binary"), (override, b"explicit debug binary")):
        path.parent.mkdir(parents=True)
        path.write_bytes(content)
    monkeypatch.setattr(inspector.harness, "BINARY", default)
    monkeypatch.delenv("RUSTAIN_TUI_BINARY", raising=False)
    monkeypatch.delenv("RUSTAIN_TOOL_INSPECTOR_DESKCHECK", raising=False)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("TMPDIR", str(tmp_path))
    monkeypatch.setenv("RUSTAIN_CAPTURE_EVIDENCE_DIR", str(tmp_path / "evidence"))
    return default, override


@pytest.mark.parametrize("selection", ["default", "override"])
def test_inspector_fixture_resolves_binary(monkeypatch, binaries, selection):
    default, override = binaries
    if selection == "override":
        monkeypatch.setenv("RUSTAIN_TUI_BINARY", "selected/debug/rustain")
        # An explicit override must work even without the ordinary debug build.
        default.unlink()
    inspector.build_binary.__wrapped__()


@pytest.mark.parametrize("selection", ["default", "override"])
def test_inspector_live_resolves_binary_without_launch(monkeypatch, tmp_path, binaries, selection):
    default, override = binaries
    selected = default
    if selection == "override":
        monkeypatch.setenv("RUSTAIN_TUI_BINARY", "selected/debug/rustain")
        selected = override
    expected_bytes = selected.read_bytes()
    observed = []

    class BinaryResolved(Exception):
        pass

    def stop_at_binary_hash(data):
        observed.append(data)
        raise BinaryResolved("prerequisites resolved; stop before workspace or UI launch")

    # _live hashes the chosen file immediately after validating its path. Stop
    # there, exercising the real prerequisite without starting a smoke attempt.
    monkeypatch.setattr(inspector.hashlib, "sha256", stop_at_binary_hash)
    with pytest.raises(BinaryResolved):
        inspector._live(monkeypatch)
    assert observed == [expected_bytes]
    manifests = list((tmp_path / "evidence").glob("a-*/run.json"))
    assert len(manifests) == 1
    assert "BinaryResolved" in json.loads(manifests[0].read_text())["exception"]


def test_inspector_offline_skips_binary_resolution_and_spawn(monkeypatch):
    monkeypatch.setenv("RUSTAIN_TOOL_INSPECTOR_DESKCHECK", "1")
    monkeypatch.delenv("RUSTAIN_TUI_BINARY", raising=False)
    monkeypatch.setenv("RUSTAIN_TOOL_INSPECTOR_FRAMES", "saved-paints")
    monkeypatch.setenv("RUSTAIN_TOOL_INSPECTOR_FAILED_FRAMES", "saved-failed-paints")
    observed = []

    def forbidden(*_args, **_kwargs):
        pytest.fail("offline deskcheck must not resolve a binary or enter live mode")

    def deskcheck(directory, failed_directory):
        observed.append((directory, failed_directory))
        return 1

    monkeypatch.setattr(inspector.harness, "_resolve_binary", forbidden)
    monkeypatch.setattr(inspector, "_live", forbidden)
    monkeypatch.setattr(inspector, "_deskcheck", deskcheck)
    inspector.build_binary.__wrapped__()
    inspector.test_tool_inspector_smoke_leg_a(monkeypatch)
    assert observed == [("saved-paints", "saved-failed-paints")]
