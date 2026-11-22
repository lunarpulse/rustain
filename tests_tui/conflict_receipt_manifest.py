"""Explicit, source-bound publication and selection of genuine conflict captures."""
from __future__ import annotations

import hashlib
from datetime import datetime
import json
import os
from pathlib import Path
import re
import tempfile

VIEWPORTS = ("80x24", "130x30", "60x16")
CURRENT_VIEWPORT = "80x24"
CAPTURE_ONLY_ENV = "RUSTAIN_CONFLICT_CAPTURE_ONLY"
MANIFEST_ENV = "RUSTAIN_CONFLICT_RECEIPT_MANIFEST"
RUN_ENV = "RUSTAIN_CONFLICT_CAPTURE_RUN_ID"
RECEIPT_ROOT = Path("tests_tui/journeys/j10/receipts")
RECEIPT_PREFIX = "story-19-21b-conflict-flag-"
TRANSCRIPT_SUFFIX = ".transcript.txt"
SOURCE = "tests_tui/test_story_19_21b_conflict_flag.py"
SCENE = "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json"
_TOKEN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
_STAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", re.ASCII)
_PTY_HOSTS = {"lena-po", "jun-dev", "marcus-arch"}
_SOURCE_FILES = (
    "Cargo.toml", "Cargo.lock",
    # Product files embedded by the shipped executable from outside src/.
    "models_variants.json",
    "dist/rustain-relay.service.template", "dist/rustain.service.template",
    "dist/com.rustain.daemon.plist.template",
    "profiles/base.toml", "profiles/coding.toml", "profiles/personal-assistant.toml",
    "profiles/personas/personal-assistant.md",
    SOURCE, SCENE, "tests_tui/conflict_receipt_manifest.py",
    "tests_tui/test_story_19_21_act1_distribution.py",
    "tests_tui/test_story_19_21b_standalone_modes.py",
    "tests_tui/harness.py", "tests_tui/keys.py",
    "tests_tui/fixtures/__init__.py", "tests_tui/fixtures/scene_provider.py",
    # Collection and fixture behavior for the selected producer.
    "tests_tui/pyproject.toml", "tests_tui/conftest.py",
    "tests_tui/lane_guard.py", "tests_tui/lanes.json",
)


def _require(condition, message):
    if not condition:
        raise RuntimeError("Prerequisite: " + message)


def _digest(stream):
    digest = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(block)
    return digest.hexdigest()


def _sha(path):
    with path.open("rb") as stream:
        return _digest(stream)


def unlinked_path(path, label):
    """Check the lexical path, including parents, rather than resolving aliases."""
    path = Path(path).expanduser().absolute()
    _require(".." not in path.parts, f"{label} contains parent traversal: {path}")
    for component in (*reversed(path.parents), path):
        _require(not component.is_symlink(), f"{label} is linked: {component}")
    return path


def _file_key(stat):
    return {"device": stat.st_dev, "inode": stat.st_ino}


def _owner(repository, token, viewport):
    return unlinked_path(repository / RECEIPT_ROOT / f"19-21b-{viewport}-run-{token}",
                         "receipt producer directory")


def _single_file(path, label):
    _require(path.is_file(), f"{label} is absent: {path}")
    stat = path.stat()
    _require(stat.st_nlink == 1, f"{label} is linked: {path}")
    return stat


def _selection():
    manifest = os.environ.get(MANIFEST_ENV, "")
    token = os.environ.get(RUN_ENV, "")
    _require(bool(manifest.strip()), f"{MANIFEST_ENV} must explicitly select a run manifest")
    _require(_TOKEN.fullmatch(token), f"{RUN_ENV} must independently identify the intended run "
             "with a safe token (1–128 ASCII letters, digits, dots, underscores or hyphens)")
    return unlinked_path(manifest, "selected run manifest"), token


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
        path = unlinked_path(repository / relative, "source input")
        _require(path.is_file(), f"source input is absent: {relative}")
        identity[relative] = _sha(path)
    # The canonical checker is deliberately NOT a runtime-source input: M38 must
    # exercise its real mutated semantic consumer, not a fingerprint precheck.
    return identity


def binary_identity(binary):
    binary = unlinked_path(binary, "explicit binary")
    _require(binary.is_file(), f"explicit binary is absent: {binary}")
    with binary.open("rb") as stream:
        stat = os.fstat(stream.fileno())
        return {"path": str(binary), "bytes": stat.st_size, "sha256": _digest(stream),
                **_file_key(stat)}


def _unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate key: {key}")
        result[key] = value
    return result


def _read_object(path, label):
    try:
        with path.open(encoding="utf-8") as stream:
            document = json.load(stream, object_pairs_hook=_unique_keys)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"Prerequisite: {label} is unavailable or invalid: {path}") from error
    _require(isinstance(document, dict), f"{label} must be an object")
    return document


def _read_manifest(path):
    _single_file(path, "selected run manifest")
    return _read_object(path, "selected run manifest")


def _validate_identity(document, repository, binary, token):
    _require(document.get("schema") == 2, "unsupported run manifest schema")
    _require(document.get("run_id") == token, "selected manifest is not the intended run")
    _require(document.get("binary") == binary_identity(binary),
             "selected run binary bytes or file identity have drifted")
    _require(document.get("source_identity") == _source_identity(repository),
             "selected run source inputs have drifted (path set or bytes)")


def _check_capture(identity, document, owner, viewport):
    _require(isinstance(identity, dict), "capture identity must be an object")
    _require(identity.get("capture_run_id") == document["run_id"]
             and identity.get("run") == owner.name, "receipt producer token or capture identity differs")
    columns, rows = map(int, viewport.split("x"))
    geometry = (identity.get("columns"), identity.get("rows"))
    _require(all(type(value) is int for value in geometry)
             and geometry == (columns, rows), "receipt declared geometry differs")
    windows = identity.get("pty_winsizes")
    _require(isinstance(windows, dict) and set(windows) == _PTY_HOSTS
             and all(isinstance(size, list) and all(type(value) is int for value in size)
                     and size == [rows, columns] for size in windows.values()),
             "receipt measured PTY geometry differs")
    binary = document["binary"]
    for role in ("daemon", "client"):
        for suffix, key in (("", "path"), ("_sha256", "sha256"), ("_bytes", "bytes"),
                            ("_device", "device"), ("_inode", "inode")):
            actual = identity.get(role + "_binary" + suffix)
            expected = binary[key]
            _require(type(actual) is type(expected) and actual == expected,
                     f"receipt {role} binary {key} differs")
    for label, source in (("source", SOURCE), ("scene", SCENE)):
        _require(identity.get(label + "_sha256") == document["source_identity"][source],
                 f"receipt {label} identity differs")
    recorded = identity.get("recorded_at")
    _require(isinstance(recorded, str) and _STAMP.fullmatch(recorded),
             "receipt recorded-at stamp is invalid")
    try:
        return datetime.strptime(recorded, "%Y-%m-%dT%H:%M:%SZ").strftime("%Y-%m-%dT%H-%M-%SZ")
    except ValueError as error:
        raise RuntimeError("Prerequisite: receipt recorded-at stamp is invalid") from error


def _capture_record(document, owner, viewport):
    path = unlinked_path(owner / "identity.json", "receipt identity record")
    _single_file(path, "receipt identity record")
    identity = _read_object(path, "receipt identity record")
    stamp = _check_capture(identity, document, owner, viewport)
    return identity, owner / f"{RECEIPT_PREFIX}{stamp}{TRANSCRIPT_SUFFIX}"


def _check_transcript(path, identity, document, owner, viewport):
    try:
        with path.open(encoding="utf-8") as stream:
            recorded = stream.readline()
            source_headers = (stream.readline(), stream.readline())
            _require(stream.readline() == "\n" and stream.readline() == "{\n",
                     "receipt has no embedded capture identity")
            lines = ["{\n"]
            for line in stream:
                lines.append(line)
                if line == "}\n":
                    break
        embedded = json.loads("".join(lines), object_pairs_hook=_unique_keys)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"Prerequisite: receipt capture identity is unreadable: {path}") from error
    _check_capture(embedded, document, owner, viewport)
    _require(recorded == f"recorded-at: {identity['recorded_at']}\n", "receipt recorded-at stamp differs")
    for header, label, source in zip(source_headers, ("scene", "source"), (SCENE, SOURCE)):
        _require(header == f"{label}: {source} sha256: {document['source_identity'][source]}\n",
                 f"receipt {label} header differs")
    _require(embedded == identity, "receipt embedded capture identity differs from producer record")


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
    def __init__(self, repository, binary, manifest_path, document, owners):
        self.repository = repository
        self.binary = binary
        self.manifest_path = manifest_path
        self.document = document
        self._owners = owners
        self.token = document["run_id"]

    def receipt_directory(self, viewport):
        _require(viewport in self._owners, "unexpected receipt viewport")
        owner = _owner(self.repository, self.token, viewport)
        _require(owner.is_dir() and _file_key(owner.stat()) == self._owners[viewport],
                 "receipt producer directory has changed during capture")
        return owner

    def publish_receipt(self, viewport, staged_path, final_path, *, exported_staged_path=None):
        """Finalize retained copies after teardown, then commit their run index."""
        _require(viewport in self.document["expected_viewports"], "unexpected receipt viewport")
        _require(viewport not in self.document["receipts"], "viewport already published")
        manifest, token = _selection()
        _require(manifest == self.manifest_path, "selected run manifest changed during capture")
        _validate_identity(self.document, self.repository, self.binary, token)
        _require(_read_manifest(manifest) == self.document, "run manifest changed during capture")
        owner = self.receipt_directory(viewport)
        identity, expected = _capture_record(self.document, owner, viewport)
        staged = unlinked_path(self.repository / staged_path, "staged receipt")
        final = unlinked_path(self.repository / final_path, "finalized receipt")
        _require(final == expected and staged == owner / (
            expected.name.removesuffix(TRANSCRIPT_SUFFIX) + ".staged"
        ), "staged and finalized receipt must use the producer-owned path")
        staged_stat = _single_file(staged, "staged receipt")
        _require(not final.exists(), "finalized receipt already exists")
        _check_transcript(staged, identity, self.document, owner, viewport)
        digest = _sha(staged)
        publications = [(staged, final)]
        if exported_staged_path is not None:
            exported_staged = unlinked_path(exported_staged_path, "exported staged receipt")
            _single_file(exported_staged, "exported staged receipt")
            _require(exported_staged.parent != owner, "export must not alias the producer directory")
            _require(_sha(exported_staged) == digest, "exported staged receipt bytes drifted")
            exported_final = unlinked_path(exported_staged.parent / final.name, "exported final receipt")
            publications.append((exported_staged, exported_final))
        updated = dict(self.document)
        updated["receipts"] = dict(self.document["receipts"])
        updated["receipts"][viewport] = {
            "path": final.relative_to(self.repository).as_posix(), "sha256": digest,
            **_file_key(staged_stat),
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
    """Exclusively allocate the selected run paths before any product is spawned."""
    repository = unlinked_path(repository, "product root")
    binary = unlinked_path(binary, "explicit binary")
    manifest, token = _selection()
    viewports = list(viewports)
    _require(bool(viewports) and len(viewports) == len(set(viewports))
             and set(viewports) <= set(VIEWPORTS), "invalid or duplicate native viewport scope")
    capture_only = os.environ.get(CAPTURE_ONLY_ENV) == "1"
    _require(capture_only or viewports == [CURRENT_VIEWPORT],
             f"automated scope is {CURRENT_VIEWPORT}; use {CAPTURE_ONLY_ENV}=1 for manual captures")
    owners = {viewport: _owner(repository, token, viewport) for viewport in viewports}
    _require(all(not manifest.is_relative_to(owner) for owner in owners.values()),
             "run manifest must not be inside a receipt producer directory")
    document = {
        "schema": 2, "run_id": token, "complete": False, "capture_only": capture_only,
        "expected_viewports": viewports, "binary": binary_identity(binary),
        "source_identity": _source_identity(repository), "receipts": {},
    }
    occupied = []
    created_manifest = False
    manifest.parent.mkdir(parents=True, exist_ok=True)
    try:
        with manifest.open("x", encoding="utf-8") as stream:
            created_manifest = True
            (repository / RECEIPT_ROOT).mkdir(parents=True, exist_ok=True)
            for owner in owners.values():
                owner.mkdir(mode=0o700)
                occupied.append(owner)
            keys = {viewport: _file_key(owner.stat()) for viewport, owner in owners.items()}
            json.dump(document, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException as error:
        for owner in reversed(occupied):
            owner.rmdir()
        if created_manifest:
            manifest.unlink()
        if isinstance(error, FileExistsError):
            raise RuntimeError("Prerequisite: manifest and receipt producer paths must be fresh exclusive paths") from error
        raise
    return RunRecorder(repository, binary, manifest, document, keys)


def selected_receipts(repository, binary):
    """Select only the current finalized automated 80x24 receipt."""
    repository = unlinked_path(repository, "product root")
    manifest, token = _selection()
    document = _read_manifest(manifest)
    _validate_identity(document, repository, binary, token)
    _require(document.get("capture_only") is False
             and document.get("expected_viewports") == [CURRENT_VIEWPORT],
             f"selected run must have the automated {CURRENT_VIEWPORT} scope")
    receipts = document.get("receipts")
    _require(document.get("complete") is True and isinstance(receipts, dict)
             and set(receipts) == {CURRENT_VIEWPORT}, "selected run has incomplete or extra finalized receipts")
    entry = receipts[CURRENT_VIEWPORT]
    _require(isinstance(entry, dict) and isinstance(entry.get("path"), str),
             "invalid finalized receipt entry")
    owner = _owner(repository, token, CURRENT_VIEWPORT)
    identity, expected = _capture_record(document, owner, CURRENT_VIEWPORT)
    _require(entry["path"] == expected.relative_to(repository).as_posix(),
             "finalized receipt is not the expected producer-owned product path")
    receipt = unlinked_path(expected, "finalized receipt")
    key = _file_key(_single_file(receipt, "finalized receipt"))
    _require(all(type(entry.get(field)) is int and entry[field] == value for field, value in key.items()),
             "finalized receipt device or inode differs")
    _require(entry.get("sha256") == _sha(receipt), "finalized receipt bytes drifted")
    _check_transcript(receipt, identity, document, owner, CURRENT_VIEWPORT)
    return {CURRENT_VIEWPORT: receipt}
