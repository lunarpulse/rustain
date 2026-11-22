"""Isolated real-file publication/identity boundaries; no capture or cargo build."""
import hashlib
import json
import os
from pathlib import Path

import pytest

from conflict_receipt_manifest import begin_run, selected_receipts

CURRENT = "80x24"
MANUAL = "130x30"
RUN_ENV = "RUSTAIN_CONFLICT_CAPTURE_RUN_ID"
MANIFEST_ENV = "RUSTAIN_CONFLICT_RECEIPT_MANIFEST"
CAPTURE_ONLY_ENV = "RUSTAIN_CONFLICT_CAPTURE_ONLY"
RECEIPT_ROOT = Path("tests_tui/journeys/j10/receipts")
RECEIPT_STEM = "story-19-21b-conflict-flag-2026-10-16T01-02-03Z"
RECEIPT_NAME = RECEIPT_STEM + ".transcript.txt"
CAPTURE_SOURCE = "tests_tui/test_story_19_21b_conflict_flag.py"
SCENE = "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json"

# This audit list is independent of the implementation's source-membership tuple.
# Each path has its own real writer -> selector -> mutate one file -> selector case.
DEPENDENCIES = (
    "Cargo.toml", "Cargo.lock", "models_variants.json",
    "dist/rustain-relay.service.template", "dist/rustain.service.template",
    "dist/com.rustain.daemon.plist.template",
    "profiles/base.toml", "profiles/coding.toml", "profiles/personal-assistant.toml",
    "profiles/personas/personal-assistant.md",
    CAPTURE_SOURCE, SCENE, "tests_tui/conflict_receipt_manifest.py",
    "tests_tui/test_story_19_21_act1_distribution.py",
    "tests_tui/test_story_19_21b_standalone_modes.py",
    "tests_tui/harness.py", "tests_tui/keys.py",
    "tests_tui/fixtures/__init__.py", "tests_tui/fixtures/scene_provider.py",
    "tests_tui/pyproject.toml", "tests_tui/conftest.py",
    "tests_tui/lane_guard.py", "tests_tui/lanes.json",
)


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """These file-integrity tests never launch the product."""


@pytest.fixture
def run_inputs(tmp_path, monkeypatch):
    repository = tmp_path / "product"
    inputs = {
        **{path: f"fixture input for {path}\n" for path in DEPENDENCIES},
        "build.rs": "fn main() {}\n",
        "src/main.rs": 'fn main() { println!("actual source input"); }\n',
        "src/nested/module.rs": "pub const VALUE: u8 = 1;\n",
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
    monkeypatch.setenv(MANIFEST_ENV, str(manifest))
    monkeypatch.setenv(RUN_ENV, "intended-independent-run")
    monkeypatch.delenv(CAPTURE_ONLY_ENV, raising=False)
    return repository, binary, manifest


def _sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _entry(repository, path):
    stat = path.stat()
    return {"path": path.relative_to(repository).as_posix(), "sha256": _sha(path),
            "device": stat.st_dev, "inode": stat.st_ino}


def _encode_capture(identity):
    return (
        f"recorded-at: {identity['recorded_at']}\n"
        f"scene: {SCENE} sha256: {identity['scene_sha256']}\n"
        f"source: {CAPTURE_SOURCE} sha256: {identity['source_sha256']}\n\n"
        + json.dumps(identity, indent=2)
        + "\n--- isolated publication fixture, not native evidence ---\n"
    )


def _stage(recorder, viewport):
    repository, binary = recorder.repository, recorder.binary
    directory = recorder.receipt_directory(viewport)
    token = os.environ[RUN_ENV]
    assert directory == repository / RECEIPT_ROOT / f"19-21b-{viewport}-run-{token}"
    columns, rows = map(int, viewport.split("x"))
    identity = {
        "capture_run_id": token, "run": directory.name, "columns": columns, "rows": rows,
        "pty_winsizes": {alias: [rows, columns] for alias in ("lena-po", "jun-dev", "marcus-arch")},
        "recorded_at": "2026-10-16T01:02:03Z",
        "source_sha256": _sha(repository / CAPTURE_SOURCE), "scene_sha256": _sha(repository / SCENE),
    }
    stat = binary.stat()
    for role in ("daemon", "client"):
        identity.update({
            f"{role}_binary": str(binary), f"{role}_binary_sha256": _sha(binary),
            f"{role}_binary_bytes": stat.st_size, f"{role}_binary_device": stat.st_dev,
            f"{role}_binary_inode": stat.st_ino,
        })
    (directory / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    staged = directory / (RECEIPT_STEM + ".staged")
    staged.write_text(_encode_capture(identity))
    return staged, directory / RECEIPT_NAME


def _publish(recorder, viewport=CURRENT, exported_staged=None):
    staged, final = _stage(recorder, viewport)
    if exported_staged is not None:
        exported_staged.parent.mkdir(exist_ok=True)
        exported_staged.write_bytes(staged.read_bytes())
    assert recorder.publish_receipt(viewport, staged, final, exported_staged_path=exported_staged) == final
    return final


def _complete(inputs, viewport=CURRENT):
    repository, binary, _ = inputs
    return _publish(begin_run(repository, binary, [viewport]), viewport)


def _older_receipt(inputs, monkeypatch, viewport=CURRENT):
    repository, binary, manifest = inputs
    with monkeypatch.context() as env:
        env.setenv(RUN_ENV, "older-independent-run")
        env.setenv(MANIFEST_ENV, str(manifest.with_name("older.json")))
        if viewport != CURRENT:
            env.setenv(CAPTURE_ONLY_ENV, "1")
        older = _complete(inputs, viewport)
        if viewport == CURRENT:
            assert selected_receipts(repository, binary) == {CURRENT: older}
    return older


def test_complete_transition_publishes_only_selected_finalized_receipt(run_inputs, tmp_path):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [CURRENT])
    assert recorder.receipt_directory(CURRENT).is_dir()
    state = json.loads(manifest.read_text())
    assert state["complete"] is False and state["receipts"] == {}
    assert state["expected_viewports"] == [CURRENT]
    with pytest.raises(RuntimeError, match="incomplete or extra finalized"):
        selected_receipts(repository, binary)

    exported = tmp_path / "artifact/receipt.staged"
    final = _publish(recorder, exported_staged=exported)
    assert selected_receipts(repository, binary) == {CURRENT: final}
    assert final.read_bytes() == exported.with_name(RECEIPT_NAME).read_bytes()
    assert not exported.exists() and not list(final.parent.glob("*.staged"))
    state = json.loads(manifest.read_text())
    assert state["complete"] is True
    assert state["receipts"] == {CURRENT: _entry(repository, final)}


@pytest.fixture
def exported_publication(run_inputs, tmp_path):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [CURRENT])
    staged, final = _stage(recorder, CURRENT)
    exported_staged = tmp_path / "artifact/receipt.staged"
    exported_staged.parent.mkdir()
    exported_staged.write_bytes(staged.read_bytes())
    return recorder, manifest, staged, final, exported_staged


@pytest.mark.parametrize("failure", ["missing", "drifted", "collision"])
def test_failed_export_never_publishes_success_or_overwrites_evidence(
        exported_publication, failure):
    recorder, manifest, staged, final, exported_staged = exported_publication
    original_index = manifest.read_bytes()
    original_staged = staged.read_bytes()
    exported_final = exported_staged.parent / final.name
    if failure == "missing":
        exported_staged.unlink()
        error, message = RuntimeError, "exported staged receipt is absent"
    elif failure == "drifted":
        exported_staged.write_bytes(b"changed exported evidence\n")
        error, message = RuntimeError, "exported staged receipt bytes drifted"
    else:
        exported_final.write_bytes(b"another run owns this receipt\n")
        error, message = FileExistsError, "File exists"
    with pytest.raises(error, match=message):
        recorder.publish_receipt(CURRENT, staged, final,
                                 exported_staged_path=exported_staged)
    assert manifest.read_bytes() == original_index
    assert staged.read_bytes() == original_staged
    assert not final.exists()
    if failure == "collision":
        assert exported_final.read_bytes() == b"another run owns this receipt\n"
    else:
        assert not exported_final.exists()


def test_wrong_independent_intended_run_rejects_complete_old_run(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    monkeypatch.setenv(RUN_ENV, "different-intended-run")
    with pytest.raises(RuntimeError, match="not the intended run"):
        selected_receipts(repository, binary)


def test_absent_explicit_manifest_cannot_fall_back_to_old_files(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    older = _older_receipt(run_inputs, monkeypatch)
    monkeypatch.setenv(MANIFEST_ENV, str(repository / "absent-new.json"))
    with pytest.raises(RuntimeError, match="manifest is absent"):
        selected_receipts(repository, binary)
    assert older.is_file()


def test_complete_manual_capture_is_not_current_evidence(run_inputs, monkeypatch):
    repository, binary, manifest = run_inputs
    monkeypatch.setenv(CAPTURE_ONLY_ENV, "1")
    receipt = _complete(run_inputs, MANUAL)
    state = json.loads(manifest.read_text())
    assert state["complete"] is True and state["receipts"] == {MANUAL: _entry(repository, receipt)}
    with pytest.raises(RuntimeError, match="automated 80x24 scope"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("defect", ["incomplete", "missing-entry", "extra-entry", "wrong-scope"])
def test_index_requires_exact_complete_current_scope(run_inputs, defect):
    repository, binary, manifest = run_inputs
    _complete(run_inputs)
    state = json.loads(manifest.read_text())
    expected = "incomplete or extra finalized"
    if defect == "incomplete":
        state["complete"] = False
    elif defect == "missing-entry":
        state["receipts"].pop(CURRENT)
    elif defect == "extra-entry":
        state["receipts"][MANUAL] = state["receipts"][CURRENT]
    else:
        state["expected_viewports"] = [CURRENT, MANUAL]
        expected = "automated 80x24 scope"
    manifest.write_text(json.dumps(state))
    with pytest.raises(RuntimeError, match=expected):
        selected_receipts(repository, binary)


def test_missing_final_receipt_does_not_select_an_older_file(run_inputs, monkeypatch):
    repository, binary, _ = run_inputs
    older = _older_receipt(run_inputs, monkeypatch)
    lost = _complete(run_inputs)
    lost.unlink()
    with pytest.raises(RuntimeError, match="finalized receipt is absent"):
        selected_receipts(repository, binary)
    assert older.is_file()


@pytest.mark.parametrize("other", ["older-80x24", "wrong-viewport", "wrong-stamp"])
def test_current_index_cannot_remap_to_a_different_receipt_with_valid_digest(run_inputs, monkeypatch, other):
    repository, binary, manifest = run_inputs
    if other == "older-80x24":
        foreign = _older_receipt(run_inputs, monkeypatch)
    elif other == "wrong-viewport":
        foreign = _older_receipt(run_inputs, monkeypatch, MANUAL)
    final = _complete(run_inputs)
    assert selected_receipts(repository, binary) == {CURRENT: final}
    if other == "wrong-stamp":
        foreign = final.with_name(RECEIPT_NAME.replace("01-02-03Z", "01-02-04Z"))
        identity = json.loads((final.parent / "identity.json").read_text())
        identity["recorded_at"] = "2026-10-16T01:02:04Z"
        foreign.write_text(_encode_capture(identity))
    state = json.loads(manifest.read_text())
    state["receipts"][CURRENT] = _entry(repository, foreign)
    manifest.write_text(json.dumps(state))
    with pytest.raises(RuntimeError, match="expected producer-owned product path"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("field,value,diagnostic", [
    ("capture_run_id", "older-independent-run", "producer token"),
    ("run", "some-other-capture", "capture identity differs"),
    ("columns", 130, "declared geometry"),
    ("rows", 30, "declared geometry"),
    ("pty_winsizes", {"lena-po": [24, 80], "jun-dev": [30, 130], "marcus-arch": [24, 80]},
     "measured PTY geometry"),
    ("daemon_binary_sha256", "0" * 64, "daemon binary sha256"),
    ("client_binary_bytes", 1, "client binary bytes"),
    ("client_binary", "/bin/other", "client binary path"),
    ("daemon_binary_device", -1, "daemon binary device"),
    ("recorded_at", "2026-10-16T01:02:04Z", "recorded-at stamp differs"),
    ("source_sha256", "0" * 64, "source identity differs"),
])
def test_embedded_capture_lie_with_recomputed_digest_is_rejected(run_inputs, field, value, diagnostic):
    repository, binary, manifest = run_inputs
    final = _complete(run_inputs)
    assert selected_receipts(repository, binary) == {CURRENT: final}
    identity = json.loads((final.parent / "identity.json").read_text())
    identity[field] = value
    final.write_text(_encode_capture(identity))
    state = json.loads(manifest.read_text())
    state["receipts"][CURRENT]["sha256"] = _sha(final)
    manifest.write_text(json.dumps(state))
    with pytest.raises(RuntimeError, match=diagnostic):
        selected_receipts(repository, binary)


def test_same_path_and_bytes_cannot_substitute_a_different_receipt_inode(run_inputs):
    repository, binary, _ = run_inputs
    current = _complete(run_inputs)
    new_file = current.with_suffix(".replacement")
    new_file.write_bytes(current.read_bytes())
    new_file.replace(current)
    with pytest.raises(RuntimeError, match="device or inode differs"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("drift", ["rust-bytes", "rust-added", "rust-deleted", "build-added",
                                   "build-deleted", "binary-bytes", "binary-path", "receipt-bytes"])
def test_actual_file_or_path_set_drift_is_rejected(run_inputs, drift):
    repository, binary, _ = run_inputs
    if drift == "build-added":
        (repository / "build.rs").unlink()
    receipt = _complete(run_inputs)
    assert selected_receipts(repository, binary) == {CURRENT: receipt}
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
    elif drift == "binary-path":
        equivalent = binary.with_name("other-rustain")
        equivalent.write_bytes(binary.read_bytes())
        binary = equivalent
    else:
        receipt.write_bytes(b"changed finalized receipt")
    expected = "binary bytes" if drift.startswith("binary-") else (
        "receipt bytes" if drift == "receipt-bytes" else "source inputs")
    with pytest.raises(RuntimeError, match=expected):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("dependency", DEPENDENCIES)
def test_capture_dependency_drift_rejects_previously_valid_receipts(run_inputs, dependency):
    repository, binary, _ = run_inputs
    receipt = _complete(run_inputs)
    assert selected_receipts(repository, binary) == {CURRENT: receipt}
    path = repository / dependency
    path.write_text(path.read_text() + "changed input bytes\n")
    with pytest.raises(RuntimeError, match="source inputs have drifted"):
        selected_receipts(repository, binary)


def test_checker_mutation_is_not_runtime_source_drift(run_inputs):
    repository, binary, _ = run_inputs
    receipt = _complete(run_inputs)
    (repository / ".github/scripts/check-receipt.sh").write_text("#!/bin/sh\nexit 1\n")
    assert selected_receipts(repository, binary) == {CURRENT: receipt}


def test_failed_index_replacement_rolls_back_only_own_publication(run_inputs, monkeypatch, tmp_path):
    repository, binary, manifest = run_inputs
    monkeypatch.setenv(CAPTURE_ONLY_ENV, "1")
    recorder = begin_run(repository, binary, [CURRENT, MANUAL])
    previous = _publish(recorder)
    previous_bytes = previous.read_bytes()
    index_bytes = manifest.read_bytes()
    exported = tmp_path / "artifact/receipt.staged"

    def unavailable_replace(source, destination):
        raise OSError("isolated index replacement failure")

    monkeypatch.setattr("conflict_receipt_manifest.os.replace", unavailable_replace)
    with pytest.raises(OSError, match="isolated index replacement failure"):
        _publish(recorder, MANUAL, exported_staged=exported)
    assert manifest.read_bytes() == index_bytes
    assert previous.read_bytes() == previous_bytes
    # Neither finalized copy of the failed publication survives: the repository holds only the
    # previous receipt and the exported directory holds no final.
    assert list((repository / RECEIPT_ROOT).rglob("*.transcript.txt")) == [previous]
    assert not (exported.parent / RECEIPT_NAME).exists()
    assert not list(manifest.parent.glob(".conflict-index-*"))


def test_existing_manifest_and_final_receipt_are_never_overwritten(run_inputs):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [CURRENT])
    before = manifest.read_bytes()
    with pytest.raises(RuntimeError, match="fresh exclusive"):
        begin_run(repository, binary, [CURRENT])
    staged, final = _stage(recorder, CURRENT)
    final.write_bytes(b"another run owns the existing receipt\n")
    original = final.read_bytes()
    with pytest.raises(RuntimeError, match="finalized receipt already exists"):
        recorder.publish_receipt(CURRENT, staged, final)
    assert final.read_bytes() == original and staged.is_file()
    assert manifest.read_bytes() == before
    final.unlink()
    recorder.publish_receipt(CURRENT, staged, final)
    with pytest.raises(RuntimeError, match="already published"):
        recorder.publish_receipt(CURRENT, staged, final)


def test_existing_owner_cannot_be_reopened_with_fresh_manifest(run_inputs, monkeypatch):
    repository, binary, manifest = run_inputs
    final = _complete(run_inputs)
    previous = final.read_bytes()
    other_manifest = manifest.with_name("other.json")
    monkeypatch.setenv(MANIFEST_ENV, str(other_manifest))
    with pytest.raises(RuntimeError, match="fresh exclusive"):
        begin_run(repository, binary, [CURRENT])
    assert not other_manifest.exists()
    assert final.read_bytes() == previous


@pytest.mark.parametrize("move", ["staged", "final"])
def test_publish_requires_its_exact_producer_owned_paths(run_inputs, move):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [CURRENT])
    staged, final = _stage(recorder, CURRENT)
    original = manifest.read_bytes()
    foreign = final.parent.with_name("unowned-capture")
    foreign.mkdir()
    if move == "staged":
        staged = staged.rename(foreign / staged.name)
    else:
        final = foreign / final.name
    with pytest.raises(RuntimeError, match="must use the producer-owned path"):
        recorder.publish_receipt(CURRENT, staged, final)
    assert manifest.read_bytes() == original and staged.is_file() and not final.exists()


@pytest.mark.parametrize("entry_path", ["../outside.transcript.txt", "/tmp/outside.transcript.txt"])
def test_manifest_cannot_select_uncontained_receipts(run_inputs, entry_path):
    repository, binary, manifest = run_inputs
    _complete(run_inputs)
    state = json.loads(manifest.read_text())
    state["receipts"][CURRENT]["path"] = entry_path
    manifest.write_text(json.dumps(state))
    with pytest.raises(RuntimeError, match="expected producer-owned product path"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("component", ["receipt", "owner", "receipt-root", "manifest-parent", "identity"])
def test_exact_real_paths_cannot_be_replaced_by_symlinks(run_inputs, component):
    repository, binary, manifest = run_inputs
    final = _complete(run_inputs)
    path = {
        "receipt": final, "owner": final.parent, "receipt-root": final.parent.parent,
        "manifest-parent": manifest.parent, "identity": final.parent / "identity.json",
    }[component]
    is_directory = path.is_dir()
    renamed = path.rename(path.with_name(path.name + "-real"))
    path.symlink_to(renamed, target_is_directory=is_directory)
    with pytest.raises(RuntimeError, match="is linked"):
        selected_receipts(repository, binary)


def test_finalized_receipt_with_second_hardlink_is_rejected(run_inputs):
    repository, binary, _ = run_inputs
    final = _complete(run_inputs)
    os.link(final, final.with_name("other-name"))
    with pytest.raises(RuntimeError, match="finalized receipt is linked"):
        selected_receipts(repository, binary)


@pytest.mark.parametrize("environment", [MANIFEST_ENV, RUN_ENV])
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


@pytest.mark.parametrize("viewport", ["130x30", "60x16"])
def test_nonstandard_capture_needs_explicit_opt_in(run_inputs, monkeypatch, viewport):
    repository, binary, manifest = run_inputs
    monkeypatch.setenv(CAPTURE_ONLY_ENV, "true")
    with pytest.raises(RuntimeError, match=f"{CAPTURE_ONLY_ENV}=1"):
        begin_run(repository, binary, [viewport])
    assert not manifest.exists()


@pytest.mark.parametrize("token", ["../escaped", "/absolute", "has whitespace"])
def test_producer_token_cannot_be_path_or_unsafe_component(run_inputs, monkeypatch, token):
    repository, binary, manifest = run_inputs
    monkeypatch.setenv(RUN_ENV, token)
    with pytest.raises(RuntimeError, match="safe token"):
        begin_run(repository, binary, [CURRENT])
    assert not manifest.exists()


def test_source_drift_during_capture_cannot_publish_success(run_inputs):
    repository, binary, manifest = run_inputs
    recorder = begin_run(repository, binary, [CURRENT])
    initial_manifest = manifest.read_bytes()
    (repository / "src/nested/module.rs").write_text("pub const VALUE: u8 = 2;\n")
    with pytest.raises(RuntimeError, match="source inputs"):
        _publish(recorder)
    assert manifest.read_bytes() == initial_manifest
    owner = recorder.receipt_directory(CURRENT)
    assert not list(owner.glob("*.transcript.txt"))
    assert list(owner.glob("*.staged"))


def test_linked_source_subtree_cannot_be_silently_omitted(run_inputs, tmp_path):
    repository, binary, _ = run_inputs
    _complete(run_inputs)
    outside = tmp_path / "linked-module"
    outside.mkdir()
    (outside / "mod.rs").write_text("pub const LINKED: bool = true;\n")
    (repository / "src/linked").symlink_to(outside, target_is_directory=True)
    with pytest.raises(RuntimeError, match="source input is linked"):
        selected_receipts(repository, binary)
