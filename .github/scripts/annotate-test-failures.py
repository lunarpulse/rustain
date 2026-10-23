#!/usr/bin/env python3
"""Turn test failures into GitHub workflow annotations.

Job logs need an authenticated download; check-run annotations are public and
show on the PR. A failing job runs this with `if: failure()` over whatever its
test steps left behind:

  * cargo test output saved with `tee` (`*.log`): each `test X ... FAILED`
    becomes `::error title=X::<first panic message>`;
  * pytest JUnit XML (`*.xml`): each failed or errored testcase becomes one
    annotation carrying its failure message.

Missing files are skipped, so one `if: failure()` step can name every file the
job might have produced. At most 25 annotations per job (GitHub keeps 10 error
annotations per step on the summary; the rest stay on the check run).
"""

import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

LIMIT = 25


def escape(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_property(text: str) -> str:
    # Property values additionally may not carry `:` or `,` (they delimit the command).
    return escape(text).replace(":", "%3A").replace(",", "%2C")


def cargo_failures(text: str):
    names = re.findall(r"^test (\S+) \.\.\. FAILED$", text, re.M)
    for name in dict.fromkeys(names):
        block = re.search(
            rf"^---- {re.escape(name)} stdout ----\n(.*?)(?=^---- |\Z|^failures:$)",
            text,
            re.M | re.S,
        )
        message = ""
        if block:
            lines = [line for line in block.group(1).splitlines() if line.strip()]
            panic = next((i for i, line in enumerate(lines) if "panicked at" in line), None)
            if panic is not None:
                message = "\n".join(lines[panic : panic + 4])
            else:
                message = "\n".join(lines[:4])
        yield name, message or "FAILED (no panic message captured)"
    if not names:
        error = re.search(r"^error(\[E\d+\])?: .*$", text, re.M)
        if error:
            yield "build", error.group(0)


def junit_failures(path: Path):
    for case in ET.parse(path).iter("testcase"):
        for tag in ("failure", "error"):
            node = case.find(tag)
            if node is not None:
                name = f"{case.get('classname', '')}::{case.get('name', '')}".strip(":")
                body = (node.get("message") or "") + "\n" + (node.text or "")
                yield name, "\n".join(body.strip().splitlines()[:6])


def main(paths):
    emitted = 0
    for raw in paths:
        path = Path(raw)
        if not path.is_file():
            continue
        if path.suffix == ".xml":
            failures = junit_failures(path)
        else:
            failures = cargo_failures(path.read_text(errors="replace"))
        for name, message in failures:
            if emitted == LIMIT:
                print(f"::warning::more failures than {LIMIT}; see the job log")
                return
            print(f"::error title={escape_property(name)}::{escape(message[:1500])}")
            emitted += 1
    if emitted == 0:
        print("::warning::no test failure found in the saved outputs; see the job log")


if __name__ == "__main__":
    main(sys.argv[1:])
