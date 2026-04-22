"""Fixture helpers for Story 5-3 custom slash command TUI tests."""

from __future__ import annotations

from pathlib import Path


def write_custom_command(
    workspace: Path,
    rel_path: str,
    body: str = "# Body\n",
    description: str | None = None,
) -> Path:
    """Write a ``.claude/commands/<rel_path>`` markdown file under the workspace.

    ``rel_path`` is relative to ``.claude/commands/``, e.g. ``"review.md"``
    or ``"deploy/staging.md"``.  Parent directories are created automatically.

    If ``description`` is provided it is emitted as YAML frontmatter; otherwise
    the file is plain markdown.
    """
    full = workspace / ".claude" / "commands" / rel_path
    full.parent.mkdir(parents=True, exist_ok=True)
    if description is None:
        full.write_text(body)
    else:
        full.write_text(f"---\ndescription: {description}\n---\n\n{body}")
    return full
