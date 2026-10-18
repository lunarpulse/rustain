"""Explicit, source-bound publication and selection of genuine conflict captures."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile

VIEWPORTS = ("80x24", "130x30", "60x16")
MANIFEST_ENV = "RUSTAIN_CONFLICT_RECEIPT_MANIFEST"
RUN_ENV = "RUSTAIN_CONFLICT_CAPTURE_RUN_ID"
_SOURCE_FILES = (
    "Cargo.toml", "Cargo.lock",
    "tests_tui/test_story_19_21b_conflict_flag.py",
    "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json",
    "tests_tui/conflict_receipt_manifest.py",
    # Local execution dependencies, including the harness's transitive keys and
    # pytest setup. Changing an observation oracle requires a fresh capture.
    "tests_tui/test_story_19_21_act1_distribution.py",
    "tests_tui/test_story_19_21b_standalone_modes.py",
    "tests_tui/harness.py", "tests_tui/keys.py", "tests_tui/conftest.py",
    "tests_tui/fixtures/__init__.py", "tests_tui/fixtures/scene_provider.py",
)


def _require(condition, message):
    if not condition:
        raise RuntimeError("Prerequisite: " + message)


def _sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()

def _contained(repository, path):
    path = Path(path)
    if not path.is_absolute():
        path = repository / path
    resolved = path.resolve()
    _require(resolved.is_relative_to(repository), f"path escapes product root: {path}")
    return resolved


def _selection():
    manifest = os.environ.get(MANIFEST_ENV, "")
    token = os.environ.get(RUN_ENV, "")
    _require(bool(manifest.strip()), f"{MANIFEST_ENV} must explicitly select a run manifest")
    _require(bool(token.strip()), f"{RUN_ENV} must independently identify the intended run")
    return Path(manifest).expanduser().resolve(), token


def _source_identity(repository):
    source = repository / "src"
    _require(source.is_dir() and not source.is_symlink(), "Rust source directory is absent or linked")
    paths = set(_SOURCE_FILES)
    for path in source.rglob("*"):
        # The executor's full product snapshot does not copy source symlinks.
        # Refuse them rather than silently omit a linked module subtree.
        _require(not path.is_symlink(), f"source input is linked: {path.relative_to(repository)}")
        if path.is_file():
            paths.add(path.relative_to(repository).as_posix())
    if (repository / "build.rs").exists() or (repository / "build.rs").is_symlink():
        paths.add("build.rs")
    identity = {}
    for relative in sorted(paths):
        _require(not (repository / relative).is_symlink(), f"source input is linked: {relative}")
        path = _contained(repository, relative)
        _require(path.is_file(), f"source input is absent: {relative}")
        identity[relative] = _sha(path)
    # The canonical checker is deliberately NOT a runtime-source input: M38 must
    # exercise its real mutated semantic consumer, not a fingerprint precheck.
    return identity


def _binary_identity(binary):
    binary = Path(binary).resolve()
    _require(binary.is_file(), f"explicit binary is absent: {binary}")
    return _sha(binary)


def _read_manifest(path):
    try:
        with path.open(encoding="utf-8") as stream:
            document = json.load(stream)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"Prerequisite: selected run manifest is unavailable or invalid: {path}") from error
    _require(isinstance(document, dict), "run manifest must be an object")
    return document


def _validate_identity(document, repository, binary, token):
    _require(document.get("schema") == 1, "unsupported run manifest schema")
    _require(document.get("run_id") == token, "selected manifest is not the intended run")
    _require(document.get("binary_sha256") == _binary_identity(binary),
             "selected run binary bytes have drifted")
    _require(document.get("source_identity") == _source_identity(repository),
             "selected run source inputs have drifted (path set or bytes)")


def _replace_manifest(path, document):
    """Replace the index atomically; leave its previous state on write failure."""
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix=".conflict-index-", delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(document, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


class RunRecorder:
    def __init__(self, repository, binary, manifest_path, document):
        self.repository = repository
        self.binary = binary
        self.manifest_path = manifest_path
        self.document = document

    def publish_receipt(self, viewport, staged_path, final_path, *, exported_staged_path=None):
        """Finalize retained copies after teardown, then commit their run index."""
        _require(viewport in self.document["expected_viewports"], "unexpected receipt viewport")
        _require(viewport not in self.document["receipts"], "viewport already published")
        manifest, token = _selection()
        _require(manifest == self.manifest_path, "selected run manifest changed during capture")
        _validate_identity(self.document, self.repository, self.binary, token)
        _require(_read_manifest(manifest) == self.document, "run manifest changed during capture")
        staged = _contained(self.repository, staged_path)
        final = _contained(self.repository, final_path)
        _require(staged.is_file() and not staged.name.endswith(".transcript.txt"),
                 "receipt must be complete nonreceipt staged data")
        _require(final.name.startswith("story-19-21b-conflict-flag-")
                 and final.name.endswith(".transcript.txt"), "receipt path is not finalized")
        _require(not final.exists(), "finalized receipt already exists")
        digest = _sha(staged)
        publications = [(staged, final)]
        if exported_staged_path is not None:
            exported_staged = Path(exported_staged_path).resolve()
            _require(_sha(exported_staged) == digest, "exported staged receipt bytes drifted")
            publications.append((exported_staged, exported_staged.parent / final.name))
        updated = dict(self.document)
        updated["receipts"] = dict(self.document["receipts"])
        updated["receipts"][viewport] = {
            "path": final.relative_to(self.repository).as_posix(), "sha256": digest,
        }
        updated["complete"] = set(updated["receipts"]) == set(updated["expected_viewports"])
        published = []
        try:
            # Each staged copy lives beside its final name, including exports
            # on another filesystem. Exclusive links never overwrite evidence.
            for source, destination in publications:
                os.link(source, destination)
                published.append(destination)
            for source, _ in publications:
                source.unlink()
            _replace_manifest(manifest, updated)
        except BaseException:
            for destination in published:
                destination.unlink(missing_ok=True)
            raise
        self.document = updated
        return final


def begin_run(repository, binary, viewports):
    """Exclusively create an incomplete manifest for this invocation's scope."""
    repository = Path(repository).resolve()
    binary = Path(binary).resolve()
    manifest, token = _selection()
    viewports = list(viewports)
    _require(bool(viewports) and len(viewports) == len(set(viewports))
             and set(viewports) <= set(VIEWPORTS), "invalid or duplicate native viewport scope")
    document = {
        "schema": 1, "run_id": token, "complete": False,
        "expected_viewports": viewports, "binary_sha256": _binary_identity(binary),
        "source_identity": _source_identity(repository), "receipts": {},
    }
    manifest.parent.mkdir(parents=True, exist_ok=True)
    try:
        with manifest.open("x", encoding="utf-8") as stream:
            json.dump(document, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
    except FileExistsError as error:
        raise RuntimeError("Prerequisite: run manifest must be a fresh exclusive path") from error
    return RunRecorder(repository, binary, manifest, document)


def selected_receipts(repository, binary):
    """Require the intended finalized three-width run, never historical globbing."""
    repository = Path(repository).resolve()
    manifest, token = _selection()
    document = _read_manifest(manifest)
    _validate_identity(document, repository, binary, token)
    expected = document.get("expected_viewports")
    _require(isinstance(expected, list) and len(expected) == len(VIEWPORTS)
             and set(expected) == set(VIEWPORTS), "selected run must contain all three native widths")
    receipts = document.get("receipts")
    _require(document.get("complete") is True and isinstance(receipts, dict)
             and set(receipts) == set(VIEWPORTS), "selected run has incomplete finalized receipts")
    selected = {}
    for viewport in VIEWPORTS:
        entry = receipts[viewport]
        _require(isinstance(entry, dict) and isinstance(entry.get("path"), str),
                 f"invalid finalized receipt entry for {viewport}")
        relative = Path(entry["path"])
        _require(not relative.is_absolute() and ".." not in relative.parts,
                 "receipt identity must be product-relative and contained")
        receipt = _contained(repository, relative)
        _require(receipt.is_file() and receipt.name.startswith("story-19-21b-conflict-flag-")
                 and receipt.name.endswith(".transcript.txt"),
                 f"finalized receipt is absent for {viewport}")
        _require(entry.get("sha256") == _sha(receipt), f"finalized receipt bytes drifted for {viewport}")
        selected[viewport] = receipt
    _require(len(set(selected.values())) == len(VIEWPORTS), "native widths must have distinct receipts")
    return selected
