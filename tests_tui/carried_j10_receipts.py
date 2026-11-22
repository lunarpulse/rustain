"""Select and verify the two carried J10 receipts; never discover fresh captures."""

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys


_RECEIPT_ROOT = Path("tests_tui/journeys/j10/receipts")
CARRIED_PATHS = {
    "act1": _RECEIPT_ROOT / "80x24-20261111T203122824458Z-066843ba1cbf4c4faf592acc849c4944"
    / "story-19-21-act1-2026-11-11T20-31-22Z.transcript.txt",
    "act2": _RECEIPT_ROOT / "80-20261111T202908190976Z-0be8762110094b4bb7c7056f0b275e2f"
    / "story-19-19-act2-2026-11-11T20-29-08Z.transcript.txt",
}
REVISION_ENV = "RUSTAIN_CARRIED_J10_REVISION"
CHECKER = Path(".github/scripts/check-receipt.sh")
_BOTTOM_BORDER = "└" + "─" * 78 + "┘"


class CarriedReceiptError(ValueError):
    """Carried receipt provenance or consistency failed."""


@dataclass(frozen=True)
class CarriedReceipt:
    path: Path
    sha256: str


def _git(repository, *args):
    result = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(repository), *args],
        capture_output=True, timeout=30,
    )
    if result.returncode != 0:
        raise CarriedReceiptError(f"git {' '.join(args)}: {os.fsdecode(result.stderr).strip()}")
    return result.stdout


def _read_regular(repository, relative, label):
    path = repository
    for part in relative.parts[:-1]:
        path /= part
        try:
            mode = path.lstat().st_mode
        except FileNotFoundError:
            raise CarriedReceiptError(f"{label}: missing carried receipt") from None
        if not stat.S_ISDIR(mode):
            raise CarriedReceiptError(f"{label}: non-directory or symlink component {path}")
    path /= relative.name
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        raise CarriedReceiptError(f"{label}: missing carried receipt") from None
    if not stat.S_ISREG(mode):
        raise CarriedReceiptError(f"{label}: carried receipt is not a regular nonsymlink file")
    return path, path.read_bytes()


def _verify_geometry(act, label, contents):
    try:
        body = contents.decode("utf-8").partition("\n\n")[2]
        identity, end = json.JSONDecoder().raw_decode(body)
    except ValueError as error:
        raise CarriedReceiptError(f"{label}: invalid carried receipt identity: {error}") from error

    if isinstance(identity, dict) and identity.get("columns") == 80:
        if act == "act1":
            # Act 1 includes 24-row pyte displays, including the full-width footer.
            screens = re.split(r"\n\n--- actual painted [^\n]+ ---\n", body[end:])[1:]
            rows = [screen.split("\n") for screen in screens]
            if (identity.get("rows") == 24 and rows and all(len(screen) == 24 for screen in rows)
                    and any(screen[-1] == _BOTTOM_BORDER for screen in rows)):
                return
        else:
            # Act 2 predates a rows field. Its raw PTY paint places the full-width
            # footer on row 24. Some incremental traces contain only a partial paint.
            traces = re.split(r"\n--- raw [^\n]+\.terminal\.txt ---\n", body[end:])[1:]
            if any(re.search(r"\x1b\[24;1H(?:\x1b\[[0-9;]*m)*" + _BOTTOM_BORDER, trace) for trace in traces):
                if all(1 <= int(row) <= 24 and 1 <= int(column) <= 80
                       for trace in traces for row, column in re.findall(r"\x1b\[(\d+);(\d+)H", trace)):
                    return
    raise CarriedReceiptError(f"{label}: carried receipt does not attest 80x24 geometry")


def load_carried_receipts(repository, revision=None):
    """Validate both exact worktree paths against one immutable commit and the real checker."""
    repository = Path(repository).absolute()
    if repository.resolve(strict=True) != repository or not repository.is_dir():
        raise CarriedReceiptError(f"expected a physical product repository root: {repository}")
    if Path(os.fsdecode(_git(repository, "rev-parse", "--show-toplevel").rstrip(b"\n"))) != repository:
        raise CarriedReceiptError(f"expected the product git worktree root: {repository}")
    if revision is not None and not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", revision):
        raise CarriedReceiptError("carried receipt revision must be a full immutable commit SHA")
    commit = _git(repository, "rev-parse", "--verify", f"{revision or 'HEAD'}^{{commit}}").decode().strip()
    if revision is not None and revision != commit:
        raise CarriedReceiptError(f"carried receipt revision is not a commit SHA: {revision}")

    selected = {}
    for act, relative in CARRIED_PATHS.items():
        label = f"{act} {relative}"
        path, contents = _read_regular(repository, relative, label)
        entry = _git(repository, "ls-tree", "-z", "--full-tree", commit, "--", relative.as_posix())
        header, _, name = entry.partition(b"\t")
        regular = re.fullmatch(rb"100(?:644|755) blob ([0-9a-f]+)", header)
        if regular is None or name != os.fsencode(relative) + b"\0":
            raise CarriedReceiptError(f"{label}: not tracked as a regular blob in {commit}")
        blob = _git(repository, "cat-file", "blob", regular[1].decode())
        if contents != blob:
            raise CarriedReceiptError(f"{label}: stale carried receipt bytes differ from {commit}")
        _verify_geometry(act, label, contents)
        result = subprocess.run(
            [str(repository / CHECKER), "--product-root", str(repository), str(path)],
            cwd=repository, capture_output=True, text=True, timeout=30,
        )
        if result.returncode != 0 or "PASS" not in result.stdout:
            raise CarriedReceiptError(f"{label}: checker rejected carried receipt\n{result.stdout}{result.stderr}")
        selected[act] = CarriedReceipt(path, hashlib.sha256(contents).hexdigest())
    return selected


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True, type=Path)
    parser.add_argument("--revision", required=True, help="full immutable checkout commit SHA")
    args = parser.parse_args(argv)
    try:
        receipts = load_carried_receipts(args.repository, args.revision)
    except (CarriedReceiptError, OSError, subprocess.TimeoutExpired) as error:
        print(error, file=sys.stderr)
        return 1
    for act, receipt in receipts.items():
        print(f"carried {act} 80x24 {args.revision} {receipt.sha256} {receipt.path.relative_to(args.repository.absolute())}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
