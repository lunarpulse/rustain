"""Canonical executable controls; receipts prove consistency, not authenticity.

Genuine captures are prerequisites, never synthesized or silently skipped. Policy
fixtures below isolate timestamp, retention and historical exception boundaries;
they are explicitly not executing-runtime evidence.
"""
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

import pytest

REPOSITORY = Path(__file__).resolve().parents[1]
GATE = REPOSITORY / ".github/scripts/check-receipt.sh"
RECEIPTS = REPOSITORY / "tests_tui/journeys/j10/receipts"
VIEWPORTS = ("80x24", "130x30", "60x16")


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Receipt CLI controls launch no Rust binary and need no cargo bootstrap."""


def _verify(receipt, *, root=REPOSITORY, cwd=None, gate=GATE, legacy=None):
    command = [str(gate), "--product-root", str(root)]
    if legacy is not None:
        command += ["--legacy-demo-root", str(legacy)]
    command.append(str(receipt))
    environment = dict(os.environ)
    environment.pop("RUSTAIN_ROOT", None)
    environment.pop("RUSTAIN_RECEIPT_GATE", None)
    return subprocess.run(command, cwd=cwd or root, env=environment,
                          capture_output=True, text=True, timeout=30)


def _assert_valid(result):
    assert result.returncode == 0, "Prerequisite: genuine receipt is not valid\n" + result.stdout + result.stderr
    assert "PASS" in result.stdout, result.stdout


def _evidence(tmp_path, name, receipt, result):
    record = {"receipt": receipt.name, "checker_sha256": hashlib.sha256(GATE.read_bytes()).hexdigest(),
              "control_source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
    (tmp_path / (name + ".json")).write_text(json.dumps(record, indent=2) + "\n")
    # CI exports these exact diagnostic files only, never the private fixture tree.
    if destination := os.environ.get("RUSTAIN_RECEIPT_EVIDENCE_DIR"):
        target = Path(destination) / tmp_path.name
        target.mkdir(parents=True, exist_ok=True)
        (target / (name + ".json")).write_text(json.dumps(record, indent=2) + "\n")


def _changed_identity(tmp_path, receipt, leg, *, prefix=""):
    valid = _verify(receipt, cwd=tmp_path)
    _evidence(tmp_path, "positive", receipt, valid)
    _assert_valid(valid)
    body = receipt.read_text()
    if leg in {"scene", "source"}:
        changed, count = re.subn(r"^(" + leg + r": .* sha256: )[0-9a-f]{64}$",
                                lambda match: match[1] + "0" * 64, body, flags=re.M)
        assert count == 1, "Prerequisite: genuine identity header absent/ambiguous"
        expected = leg + " hash mismatch"
    else:
        match = re.search(r"^recorded-at: (.+)$", body, re.M)
        assert match, "Prerequisite: genuine recorded-at header absent"
        stamp = datetime.fromisoformat(match[1].replace("Z", "+00:00"))
        changed_stamp = (stamp + timedelta(seconds=1)).astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        changed = body.replace(match[0], "recorded-at: " + changed_stamp, 1)
        expected = "disagree"
    negative = tmp_path / receipt.name
    negative.write_text(changed)
    invalid = _verify(negative)
    _evidence(tmp_path, "negative", negative, invalid)
    assert invalid.returncode != 0, f"{prefix}{leg}: changed genuine receipt was accepted\n" + invalid.stdout
    assert expected in invalid.stdout + invalid.stderr, invalid.stdout + invalid.stderr
    restored = _verify(receipt)
    _evidence(tmp_path, "restored", receipt, restored)
    _assert_valid(restored)


@pytest.mark.a2a_binary
@pytest.mark.parametrize("leg", ["scene", "source", "stamp"])
def test_genuine_receipt_rejects_changed_identity(tmp_path, leg):
    candidates = list(RECEIPTS.glob("story-19-19-act2-*.transcript.txt"))
    candidates += list(RECEIPTS.glob("*/story-19-19-act2-*.transcript.txt"))
    assert candidates, "Prerequisite: no genuine executing Act 2 receipt"
    receipt = max(candidates, key=lambda path: (path.name, path.parent.name))
    _changed_identity(tmp_path, receipt, leg, prefix="M39 ")


@pytest.mark.a2a_binary
@pytest.mark.parametrize("leg", ["scene", "source", "stamp"])
@pytest.mark.parametrize("viewport", VIEWPORTS)
def test_genuine_act1_receipt_rejects_changed_identity(tmp_path, leg, viewport):
    candidates = [path for path in RECEIPTS.glob("*/story-19-21-act1-*.transcript.txt")
                  if path.parent.name.split("-", 1)[0] == viewport]
    assert candidates, f"Prerequisite: native receipt viewport missing: {viewport}"
    receipt = max(candidates, key=lambda path: (path.name, path.parent.name))
    _changed_identity(tmp_path, receipt, leg)


def _sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


@pytest.fixture
def policy(tmp_path):
    root = tmp_path / "product-only"
    root.mkdir()
    gate = root / ".github/scripts/check-receipt.sh"
    gate.parent.mkdir(parents=True)
    shutil.copy2(GATE, gate)
    scene = root / "tests_tui/journeys/j10/scenes/19-21-act1-v1.json"
    source = root / "tests_tui/test_story_19_21_act1_distribution.py"
    scene.parent.mkdir(parents=True)
    scene.write_text('{"policy_fixture": true}\n')
    source.write_text('# policy fixture, not a native capture\n')
    receipt = root / "story-19-21-act1-2026-10-13T13-57-50Z.transcript.txt"
    body = ("recorded-at: 2026-10-13T13:57:50Z\n"
            f"scene: {scene.relative_to(root)} sha256: {_sha(scene)}\n"
            f"source: {source.relative_to(root)} sha256: {_sha(source)}\n"
            '{"recorded_at_ms": 1791899870000}\n')
    # Derive the fixture epoch from its fixed timestamp rather than a second clock.
    millis = int(datetime(2026, 10, 13, 13, 57, 50, tzinfo=timezone.utc).timestamp() * 1000)
    body = re.sub(r'"recorded_at_ms": [0-9]{13}', f'"recorded_at_ms": {millis}', body)
    receipt.write_text(body)
    _assert_valid(_verify(receipt, root=root, gate=gate))
    return root, gate, receipt, body, scene


@pytest.mark.parametrize("fault,diagnostic", [
    ("missing-header", "no recorded-at header"),
    ("malformed-header", "recorded-at is not a timestamp"),
    ("duplicate-header", "exactly one is a record"),
    ("unsupported-stem", "no receipt shape is defined"),
    ("scene-path", "the scene: header must name"),
    ("source-path", "the source: header must name"),
    ("missing-scene", "the scene: header must name"),
    ("missing-source", "the source: header must name"),
    ("no-product-time", "records no run"),
    ("compact-journal", "records no run"),
    ("outside-before", "minted outside the run"),
    ("outside-after", "minted outside the run"),
])
def test_canonical_rejects_policy_fault(tmp_path, policy, fault, diagnostic):
    root, gate, receipt, body, scene = policy
    changed = body
    if fault == "missing-header":
        changed = "\n".join(body.splitlines()[1:]) + "\n"
    elif fault == "malformed-header":
        changed = body.replace("recorded-at: 2026-10-13T13:57:50Z", "recorded-at: not-a-timestamp")
    elif fault == "duplicate-header":
        changed = body.splitlines()[0] + "\n" + body
    elif fault == "unsupported-stem":
        receipt = root / "unsupported-2026-10-13T13-57-50Z.transcript.txt"
    elif fault in {"scene-path", "source-path"}:
        field = fault.split("-")[0]
        changed = re.sub(r"^" + field + r": [^ ]+", field + ": wrong/path.json", body, flags=re.M)
    elif fault in {"missing-scene", "missing-source"}:
        field = fault.split("-")[1]
        changed = re.sub(r"^" + field + r": .*\n", "", body, flags=re.M)
    elif fault == "no-product-time":
        changed = re.sub(r'"recorded_at_ms": [0-9]{13}', '"caller_timestamp": 1', body)
    elif fault == "compact-journal":
        changed = body.replace('"recorded_at_ms": ', '"recorded_at_ms":')
    else:
        offset = -3601 if fault == "outside-before" else 3601
        changed = re.sub(r'("recorded_at_ms": )([0-9]{13})',
                         lambda match: match[1] + str(int(match[2]) + offset * 1000), body)
    receipt.write_text(changed)
    result = _verify(receipt, root=root, gate=gate)
    _evidence(tmp_path, "negative", receipt, result)
    assert result.returncode == 1, result.stdout + result.stderr
    assert diagnostic in result.stdout, result.stdout + result.stderr
    original = root / "story-19-21-act1-2026-10-13T13-57-50Z.transcript.txt"
    original.write_text(body)
    _assert_valid(_verify(original, root=root, gate=gate))


@pytest.mark.parametrize("offset", [-3600, 3600])
def test_canonical_inclusive_hour_window(policy, offset):
    root, gate, receipt, body, scene = policy
    receipt.write_text(re.sub(r'("recorded_at_ms": )([0-9]{13})',
                             lambda match: match[1] + str(int(match[2]) + offset * 1000), body))
    _assert_valid(_verify(receipt, root=root, gate=gate))


def test_canonical_product_only_arbitrary_cwd(tmp_path, policy):
    root, gate, receipt, body, scene = policy
    elsewhere = tmp_path / "unrelated"
    elsewhere.mkdir()
    _assert_valid(_verify(receipt, root=root, gate=gate, cwd=elsewhere))
    # Also exercise the executable's own product-root resolution, not only --product-root.
    result = subprocess.run([str(gate), str(receipt)], cwd=elsewhere,
                            capture_output=True, text=True, timeout=30)
    _assert_valid(result)


def _journey(root, scene, *, journey="J1", stamp="2026-10-13T13-57-50Z", recorded="2026-10-13T13:57:50Z"):
    millis = int(datetime.fromisoformat(recorded.replace("Z", "+00:00")).timestamp() * 1000)
    receipt = root / f"journey-{journey}-{stamp}.transcript.txt"
    receipt.write_text(f"recorded-at: {recorded}\nscene: {scene.relative_to(root)} sha256:{_sha(scene)}\n"
                       f'{{"timestampMs":{millis}}}\n')
    return receipt


@pytest.mark.parametrize("retained,latest,accepted", [(True, False, True), (False, False, False), (True, True, False)])
def test_canonical_historical_scene_retention(policy, retained, latest, accepted):
    root, gate, original, body, scene = policy
    receipt = _journey(root, scene)
    if not latest:
        (root / "journey-J1-2026-10-14T13-57-50Z.transcript.txt").write_text("superseding filename fixture\n")
    if retained:
        shutil.copy2(scene, scene.with_name("retained-v1.json"))
    scene.write_text('{"policy_fixture": "evolved"}\n')
    result = _verify(receipt, root=root, gate=gate)
    if accepted:
        _assert_valid(result)
        assert "pinned scene bytes are retained" in result.stdout
    else:
        assert result.returncode == 1, result.stdout
        assert "scene hash mismatch" in result.stdout


@pytest.mark.parametrize("journey,stamp", [("J0", "2026-08-28T10-47-33Z"), ("J3", "2026-09-01T04-31-07Z")])
def test_canonical_exact_historical_stamp_exception(policy, journey, stamp):
    root, gate, original, body, scene = policy
    receipt = _journey(root, scene, journey=journey, stamp=stamp)
    _assert_valid(_verify(receipt, root=root, gate=gate))
    nearby = receipt.with_name(receipt.name.replace("33Z", "34Z") if journey == "J0" else receipt.name.replace("07Z", "08Z"))
    shutil.copy2(receipt, nearby)
    result = _verify(nearby, root=root, gate=gate)
    assert result.returncode == 1, result.stdout
    assert "disagree" in result.stdout


def test_canonical_legacy_root_is_explicit(tmp_path, policy):
    root, gate, original, body, scene = policy
    receipt = _journey(root, scene)
    legacy = tmp_path / "real-outer-demo-root"
    legacy.mkdir()
    shutil.copy2(scene, legacy / "legacy-scene.json")
    receipt.write_text(receipt.read_text().replace(str(scene.relative_to(root)), "demos/legacy-scene.json"))
    absent = _verify(receipt, root=root, gate=gate)
    assert absent.returncode == 1 and "names no scene file" in absent.stdout
    _assert_valid(_verify(receipt, root=root, gate=gate, legacy=legacy))


@pytest.mark.parametrize("fault,diagnostic", [("profile-path", "names no profile file"), ("profile-hash", "profile hash mismatch")])
def test_canonical_j8_profile(policy, fault, diagnostic):
    root, gate, original, body, scene = policy
    profile = root / "profiles/ravi.toml"
    profile.parent.mkdir()
    profile.write_text('[profile]\nname="ravi"\n')
    receipt = _journey(root, scene, journey="J8")
    receipt.write_text(receipt.read_text() + f"profile: profiles/ravi.toml sha256:{_sha(profile)}\n")
    _assert_valid(_verify(receipt, root=root, gate=gate))
    receipt.write_text(receipt.read_text().replace("profiles/ravi.toml", "profiles/missing.toml") if fault == "profile-path"
                       else receipt.read_text().replace(_sha(profile), "0" * 64))
    result = _verify(receipt, root=root, gate=gate)
    assert result.returncode == 1 and diagnostic in result.stdout, result.stdout


@pytest.mark.parametrize("stem,token", [
    ("federation-two-host", "peer-ping-42-{millis}-1-1"),
    ("j10-spine-same-host", "peer-ping-42-{millis}-1-1"),
    ("two-host-a2a", '{{"recorded_at_ms": {millis}}}'),
    ("team-send-loopback", '{{"recorded_at_ms": {millis}}}'),
])
def test_canonical_legacy_stem_policies(policy, stem, token):
    root, gate, original, body, scene = policy
    millis = int(datetime(2026, 10, 13, 13, 57, 50, tzinfo=timezone.utc).timestamp() * 1000)
    receipt = root / f"{stem}-2026-10-13T13-57-50Z.transcript.txt"
    receipt.write_text("recorded-at: 2026-10-13T13:57:50Z\n" + token.format(millis=millis) + "\n")
    _assert_valid(_verify(receipt, root=root, gate=gate))
    receipt.write_text(receipt.read_text().replace(str(millis), str(millis + 3601000)))
    result = _verify(receipt, root=root, gate=gate)
    assert result.returncode == 1 and "minted outside the run" in result.stdout, result.stdout


@pytest.mark.a2a_binary
@pytest.mark.parametrize("capture", ["act2", *VIEWPORTS])
def test_genuine_product_only_arbitrary_cwd(tmp_path, capture):
    if capture == "act2":
        candidates = list(RECEIPTS.glob("story-19-19-act2-*.transcript.txt"))
        candidates += list(RECEIPTS.glob("*/story-19-19-act2-*.transcript.txt"))
    else:
        candidates = [path for path in RECEIPTS.glob("*/story-19-21-act1-*.transcript.txt")
                      if path.parent.name.split("-", 1)[0] == capture]
    assert candidates, f"Prerequisite: no genuine {capture} receipt"
    receipt = max(candidates, key=lambda path: (path.name, path.parent.name))
    _assert_valid(_verify(receipt, cwd=tmp_path))
    root = tmp_path / "isolated-product"
    gate = root / ".github/scripts/check-receipt.sh"
    gate.parent.mkdir(parents=True)
    shutil.copy2(GATE, gate)
    # Copy only the actual files named by the genuine receipt, byte-for-byte.
    # No outer source or replacement header is introduced to make it pass.
    for field in ("scene", "source"):
        match = re.search(r"^" + field + r": ([^ ]+) sha256: [0-9a-f]{64}$", receipt.read_text(), re.M)
        assert match, f"Prerequisite: missing genuine {field} identity"
        relative = Path(match[1])
        assert not relative.is_absolute() and ".." not in relative.parts
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(REPOSITORY / relative, target)
    copied = root / receipt.name
    shutil.copy2(receipt, copied)
    result = _verify(copied, root=root, gate=gate, cwd=tmp_path)
    _evidence(tmp_path, "positive", receipt, result)
    _assert_valid(result)
    inferred = subprocess.run([str(gate), str(copied)], cwd=tmp_path,
                              capture_output=True, text=True, timeout=30)
    _assert_valid(inferred)
    assert _sha(copied) == _sha(receipt), "genuine receipt bytes changed"


def test_canonical_checks_every_product_timestamp(policy):
    root, gate, receipt, body, scene = policy
    millis = re.search(r'"recorded_at_ms": ([0-9]{13})', body)[1]
    receipt.write_text(body + '{"recorded_at_ms": ' + str(int(millis) + 3601000) + '}\n')
    result = _verify(receipt, root=root, gate=gate)
    assert result.returncode == 1 and "1 of 2 journalled milliseconds were minted outside" in result.stdout, result.stdout


def test_canonical_journey_requires_compact_product_time(policy):
    root, gate, original, body, scene = policy
    receipt = _journey(root, scene)
    _assert_valid(_verify(receipt, root=root, gate=gate))
    receipt.write_text(receipt.read_text().replace('"timestampMs":', '"timestampMs": '))
    result = _verify(receipt, root=root, gate=gate)
    assert result.returncode == 1 and "records no run" in result.stdout, result.stdout
