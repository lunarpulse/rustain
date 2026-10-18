"""Isolated real-file publication/identity boundaries; no capture or cargo build."""
import json
from pathlib import Path

import pytest

from conflict_receipt_manifest import VIEWPORTS, begin_run, selected_receipts


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """These file-integrity tests never launch the product."""


@pytest.fixture
def run_inputs(tmp_path, monkeypatch):
    repository = tmp_path / "product"
    inputs = {
        "src/main.rs": 'fn main() { println!("actual source input"); }\n',
        "src/nested/module.rs": "pub const VALUE: u8 = 1;\n",
        "Cargo.toml": '[package]\nname = "isolated-boundary"\nversion = "0.1.0"\n',
        "Cargo.lock": "version = 4\n",
        "build.rs": "fn main() {}\n",
        "tests_tui/test_story_19_21b_conflict_flag.py": "# capture source bytes\n",
        "tests_tui/conflict_receipt_manifest.py": "# isolated helper identity bytes\n",
        "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json": '{"scene": "isolated"}\n',
        "tests_tui/test_story_19_21_act1_distribution.py": "def _events(): return []\n",
        "tests_tui/test_story_19_21b_standalone_modes.py": "class _ObservedHttp: pass\n",
        "tests_tui/harness.py": "class RustainTUI: pass\n",
        "tests_tui/keys.py": "ENTER = '\\r'\n",
        "tests_tui/conftest.py": "# pytest capture setup\n",
        "tests_tui/fixtures/__init__.py": "# fixture package setup\n",
        "tests_tui/fixtures/scene_provider.py": "class SceneStub: pass\n",
        ".github/scripts/check-receipt.sh": "#!/bin/sh\nexit 0\n",
    }
    for relative, content in inputs.items():
        path = repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    binary = repository / "capture-runtime/rustain"
    binary.parent.mkdir()
    binary.write_bytes(b"isolated real binary identity bytes\x00\x01")
    manifest = repository / "captures/current.json"
    monkeypatch.setenv("RUSTAIN_CONFLICT_RECEIPT_MANIFEST", str(manifest))
    monkeypatch.setenv("RUSTAIN_CONFLICT_CAPTURE_RUN_ID", "intended-independent-run")
    return repository, binary, manifest


def _publish(recorder, repository, viewport, *, label="current"):
    directory = repository / "receipts" / f"{label}-{viewport}"
    directory.mkdir(parents=True)
    staged = directory / "capture.staged"
    staged.write_bytes(f"complete retained capture for {viewport}\n".encode())
    final = directory / "story-19-21b-conflict-flag-20261016T010203Z.transcript.txt"
    recorder.publish_receipt(viewport, staged, final)
    return final


def _complete(inputs):
    repository, binary, _ = inputs
    recorder = begin_run(repository, binary, VIEWPORTS)
    return {viewport: _publish(recorder, repository, viewport) for viewport in VIEWPORTS}


def test_complete_transition_publishes_only_selected_finalized_receipts(run_inputs):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, VIEWPORTS)
    assert json.loads(manifest.read_text())["complete"] is False
    published = {}
    for index, viewport in enumerate(VIEWPORTS):
        published[viewport] = _publish(recorder, repository, viewport)
        state = json.loads(manifest.read_text())
        assert state["complete"] is (index == len(VIEWPORTS) - 1)
        assert set(state["receipts"]) == set(published)
        assert all(not path.is_absolute() for path in
                   (Path(entry["path"]) for entry in state["receipts"].values()))
        if not state["complete"]:
            with pytest.raises(RuntimeError, match="incomplete finalized"):
                selected_receipts(repository, binary)
    assert selected_receipts(repository, binary) == published
    assert not list((repository / "receipts").rglob("*.staged"))


@pytest.fixture
def exported_publication(run_inputs, tmp_path):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [VIEWPORTS[0]])
    staged = repository / "retained/receipt.staged"
    staged.parent.mkdir()
    staged.write_bytes(b"completed native capture\n")
    final = staged.parent / "story-19-21b-conflict-flag-20261016T010203Z.transcript.txt"
    exported_staged = tmp_path / "artifact/receipt.staged"
    exported_staged.parent.mkdir()
    exported_staged.write_bytes(staged.read_bytes())
    return recorder, manifest, staged, final, exported_staged


def test_exported_receipt_is_finalized_before_run_is_marked_complete(exported_publication):
    recorder, manifest, staged, final, exported_staged = exported_publication
    expected = staged.read_bytes()
    recorder.publish_receipt(VIEWPORTS[0], staged, final,
                             exported_staged_path=exported_staged)
    exported_final = exported_staged.parent / final.name
    assert final.read_bytes() == exported_final.read_bytes() == expected
    assert not staged.exists() and not exported_staged.exists()
    state = json.loads(manifest.read_text())
    assert state["complete"] is True
    assert state["receipts"][VIEWPORTS[0]]["path"] == final.relative_to(
        recorder.repository).as_posix()


@pytest.mark.parametrize("failure", ["missing", "drifted", "collision"])
def test_failed_export_never_publishes_success_or_overwrites_evidence(
        exported_publication, failure):
    recorder, manifest, staged, final, exported_staged = exported_publication
    original_index = manifest.read_bytes()
    exported_final = exported_staged.parent / final.name
    if failure == "missing":
        exported_staged.unlink()
        expected_error = FileNotFoundError
    elif failure == "drifted":
        exported_staged.write_bytes(b"changed exported evidence\n")
        expected_error = RuntimeError
    else:
        exported_final.write_bytes(b"another run owns this receipt\n")
        expected_error = FileExistsError
    with pytest.raises(expected_error):
        recorder.publish_receipt(VIEWPORTS[0], staged, final,
                                 exported_staged_path=exported_staged)
    assert manifest.read_bytes() == original_index
    assert staged.read_bytes() == b"completed native capture\n"
    assert not final.exists()
    if failure == "collision":
        assert exported_final.read_bytes() == b"another run owns this receipt\n"
    else:
        assert not exported_final.exists()


def test_failed_index_commit_rolls_back_both_finalized_copies(
        exported_publication, monkeypatch):
    recorder, manifest, staged, final, exported_staged = exported_publication
    original_index = manifest.read_bytes()

    def unavailable_replace(source, destination):
        raise OSError("isolated index replacement failure")

    monkeypatch.setattr("conflict_receipt_manifest.os.replace", unavailable_replace)
    with pytest.raises(OSError, match="isolated index replacement failure"):
        recorder.publish_receipt(VIEWPORTS[0], staged, final,
                                 exported_staged_path=exported_staged)
    assert manifest.read_bytes() == original_index
    assert not final.exists()
    assert not (exported_staged.parent / final.name).exists()


def test_wrong_independent_intended_run_rejects_complete_old_run(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    monkeypatch.setenv("RUSTAIN_CONFLICT_CAPTURE_RUN_ID", "different-intended-run")
    with pytest.raises(RuntimeError, match="not the intended run"):
        selected_receipts(repository, binary)


def test_absent_explicit_manifest_cannot_fall_back_to_old_files(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    older = _complete(run_inputs)
    monkeypatch.setenv("RUSTAIN_CONFLICT_RECEIPT_MANIFEST", str(repository / "absent-new.json"))
    monkeypatch.setenv("RUSTAIN_CONFLICT_CAPTURE_RUN_ID", "new-intended-run")
    with pytest.raises(RuntimeError, match="manifest is unavailable"):
        selected_receipts(repository, binary)
    assert all(path.is_file() for path in older.values())


def test_partial_new_run_cannot_borrow_older_widths(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    monkeypatch.setenv("RUSTAIN_CONFLICT_RECEIPT_MANIFEST", str(repository / "captures/new.json"))
    monkeypatch.setenv("RUSTAIN_CONFLICT_CAPTURE_RUN_ID", "new-intended-run")
    recorder = begin_run(repository, binary, VIEWPORTS)
    _publish(recorder, repository, VIEWPORTS[0], label="new")
    with pytest.raises(RuntimeError, match="incomplete finalized"):
        selected_receipts(repository, binary)


def test_complete_subset_is_not_three_width_evidence(run_inputs):
    repository, binary, _ = run_inputs
    recorder = begin_run(repository, binary, [VIEWPORTS[0]])
    _publish(recorder, repository, VIEWPORTS[0])
    with pytest.raises(RuntimeError, match="all three native widths"):
        selected_receipts(repository, binary)


def test_missing_final_receipt_does_not_select_an_older_file(run_inputs):
    repository, binary, _ = run_inputs
    selected = _complete(run_inputs)
    lost = selected[VIEWPORTS[1]]
    older = repository / "receipts/older/story-19-21b-conflict-flag-old.transcript.txt"
    older.parent.mkdir()
    older.write_bytes(lost.read_bytes())
    lost.unlink()
    with pytest.raises(RuntimeError, match="finalized receipt is absent"):
        selected_receipts(repository, binary)
    assert older.is_file()


@pytest.mark.parametrize("drift", ["rust-bytes", "rust-added", "rust-deleted", "build-added",
                                   "build-deleted", "binary-bytes", "receipt-bytes"])
def test_actual_file_or_path_set_drift_is_rejected(run_inputs, drift):
    repository, binary, _ = run_inputs
    if drift == "build-added":
        (repository / "build.rs").unlink()
    receipts = _complete(run_inputs)
    if drift == "rust-bytes":
        (repository / "src/main.rs").write_text("fn main() { panic!(); }\n")
    elif drift == "rust-added":
        (repository / "src/new.rs").write_text("pub const NEW: bool = true;\n")
    elif drift == "rust-deleted":
        (repository / "src/nested/module.rs").unlink()
    elif drift == "build-added":
        (repository / "build.rs").write_text("fn main() {}\n")
    elif drift == "build-deleted":
        (repository / "build.rs").unlink()
    elif drift == "binary-bytes":
        binary.write_bytes(binary.read_bytes() + b"changed")
    else:
        receipts[VIEWPORTS[0]].write_bytes(b"changed finalized receipt")
    expected = "binary bytes" if drift == "binary-bytes" else (
        "receipt bytes" if drift == "receipt-bytes" else "source inputs")
    with pytest.raises(RuntimeError, match=expected):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("dependency", [
    "tests_tui/test_story_19_21_act1_distribution.py",
    "tests_tui/test_story_19_21b_standalone_modes.py",
    "tests_tui/harness.py", "tests_tui/keys.py", "tests_tui/conftest.py",
    "tests_tui/fixtures/__init__.py", "tests_tui/fixtures/scene_provider.py",
])
def test_capture_dependency_drift_rejects_previously_valid_receipts(run_inputs, dependency):
    repository, binary, _ = run_inputs
    receipts = _complete(run_inputs)
    assert selected_receipts(repository, binary) == receipts
    path = repository / dependency
    path.write_text(path.read_text() + "\ndef changed_observation(): return False\n")
    with pytest.raises(RuntimeError, match="source inputs have drifted"):
        selected_receipts(repository, binary)


def test_checker_mutation_is_not_runtime_source_drift(run_inputs):
    repository, binary, _ = run_inputs
    receipts = _complete(run_inputs)
    (repository / ".github/scripts/check-receipt.sh").write_text("#!/bin/sh\nexit 1\n")
    assert selected_receipts(repository, binary) == receipts


def test_failed_index_replacement_rolls_back_only_own_publication(run_inputs, monkeypatch):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, VIEWPORTS)
    previous = _publish(recorder, repository, VIEWPORTS[0])
    previous_bytes = previous.read_bytes()
    index_bytes = manifest.read_bytes()

    def unavailable_replace(source, destination):
        raise OSError("isolated index replacement failure")

    monkeypatch.setattr("conflict_receipt_manifest.os.replace", unavailable_replace)
    with pytest.raises(OSError, match="index replacement failure"):
        _publish(recorder, repository, VIEWPORTS[1])
    assert manifest.read_bytes() == index_bytes
    assert previous.read_bytes() == previous_bytes
    assert list((repository / "receipts").rglob("*.transcript.txt")) == [previous]
    assert not list(manifest.parent.glob(".conflict-index-*"))


def test_existing_manifest_and_final_receipt_are_never_overwritten(run_inputs):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, VIEWPORTS)
    before = manifest.read_bytes()
    with pytest.raises(RuntimeError, match="fresh exclusive"):
        begin_run(repository, binary, VIEWPORTS)
    assert manifest.read_bytes() == before
    final = _publish(recorder, repository, VIEWPORTS[0])
    original = final.read_bytes()
    with pytest.raises(RuntimeError, match="already published"):
        recorder.publish_receipt(VIEWPORTS[0], final.parent / "absent.staged", final)
    assert final.read_bytes() == original
    staged = final.parent / "second.staged"
    staged.write_bytes(b"another complete capture")
    index_before = manifest.read_bytes()
    with pytest.raises(RuntimeError, match="finalized receipt already exists"):
        recorder.publish_receipt(VIEWPORTS[1], staged, final)
    assert final.read_bytes() == original
    assert staged.read_bytes() == b"another complete capture"
    assert manifest.read_bytes() == index_before


@pytest.mark.parametrize("entry_path", ["../outside.transcript.txt", "/tmp/outside.transcript.txt"])
def test_manifest_cannot_select_uncontained_receipts(run_inputs, entry_path):
    repository, binary, manifest = run_inputs
    _complete(run_inputs)
    state = json.loads(manifest.read_text())
    state["receipts"][VIEWPORTS[0]]["path"] = entry_path
    manifest.write_text(json.dumps(state))
    with pytest.raises(RuntimeError, match="product-relative and contained"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("environment", ["RUSTAIN_CONFLICT_RECEIPT_MANIFEST",
                                        "RUSTAIN_CONFLICT_CAPTURE_RUN_ID"])
def test_selection_requires_both_independent_explicit_inputs(run_inputs, monkeypatch, environment):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    monkeypatch.delenv(environment)
    with pytest.raises(RuntimeError, match=environment):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("viewports", [[], ["80x24", "80x24"], ["100x20"]])
def test_invalid_native_scope_cannot_create_a_manifest(run_inputs, viewports):
    repository, binary, manifest = run_inputs
    with pytest.raises(RuntimeError, match="invalid or duplicate"):
        begin_run(repository, binary, viewports)
    assert not manifest.exists()


def test_source_drift_during_capture_cannot_publish_success(run_inputs):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, VIEWPORTS)
    initial_manifest = manifest.read_bytes()
    (repository / "src/nested/module.rs").write_text("pub const VALUE: u8 = 2;\n")
    with pytest.raises(RuntimeError, match="source inputs"):
        _publish(recorder, repository, VIEWPORTS[0])
    assert manifest.read_bytes() == initial_manifest
    assert not list((repository / "receipts").rglob("*.transcript.txt"))
    assert list((repository / "receipts").rglob("*.staged"))


def test_linked_source_subtree_cannot_be_silently_omitted(run_inputs, tmp_path):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    outside = tmp_path / "linked-module"
    outside.mkdir()
    (outside / "mod.rs").write_text("pub const LINKED: bool = true;\n")
    (repository / "src/linked").symlink_to(outside, target_is_directory=True)
    with pytest.raises(RuntimeError, match="source input is linked"):
        selected_receipts(repository, binary)
