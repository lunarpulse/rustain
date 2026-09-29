"""Fixture helpers for workspace Agent Skills (Story 19.10 A3).

Mirrors ``fixtures/agents.py``, with one deliberate difference: this helper
**copies bytes**, it does not render frontmatter.

⛔ **Why copying is the whole point.** ``test_story_5_2_skill_activation.py``'s
local ``write_workspace_skill`` builds ``allowed-tools: ["Bash(kubectl:*)", …]``
— the *bracket* (YAML sequence) form. A capture that claims *"the PRD's own
`SKILL.md` loads"* while re-rendering its frontmatter proves
``frontmatter.rs:99-110`` (the sequence branch) and quietly does **not** prove
the **scalar** branch (``:111-157``) that Story 19.2's whole subject is. The
PRD writes ``allowed-tools: Bash(kubectl:*) Bash(helm:*) Read`` — one scalar,
whitespace-separated — so the bytes must arrive unchanged.

⛔ From ``DF-19-2-BASH-PATTERN-ALLOWLIST``: *"never downgrade a pattern item to
its bare prefix (`Bash(kubectl:*)` → `Bash`) to make the demo work."* Editing a
fixture's ``allowed-tools`` so that a beat runs is a falsified capture, not a
passing one.

The caller compares the hashes (``_lib.sha256_of`` over the source and over the
written copy) and fails its run on a mismatch — a printed-but-uncompared witness
is the trap Story 19.9's review found in J0/J2 and this story fixes.
"""

from __future__ import annotations

import shutil
from pathlib import Path


def write_workspace_skill(workspace: Path, src_path: Path) -> Path:
    """Copy ``src_path`` to ``<workspace>/.agents/skills/<dir>/SKILL.md``.

    The skill directory name is taken from ``src_path``'s parent, so the
    committed fixture's own layout (``skills/safe-deploy/SKILL.md``) is what
    lands in the workspace. ``.agents/skills`` is ``SkillSource::WorkspaceAgents``
    (``skill_registry.rs:84``) — a workspace tier, so model-driven activation is
    trust-gated (``skill_activation.rs:185``), which is the beat this exists for.

    Returns the written path.
    """
    src_path = Path(src_path)
    skill_dir = workspace / ".agents" / "skills" / src_path.parent.name
    skill_dir.mkdir(parents=True, exist_ok=True)
    dest = skill_dir / src_path.name
    shutil.copyfile(src_path, dest)
    return dest
