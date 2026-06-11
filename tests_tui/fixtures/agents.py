"""Fixture helpers for Story 5-4 custom agent TUI tests."""

from __future__ import annotations

from pathlib import Path


def write_custom_agent(
    workspace: Path,
    name: str,
    description: str,
    body: str = "You are a helpful assistant.\n",
    *,
    allowed_tools: list[str] | None = None,
    exclude_tools: list[str] | None = None,
    model: str | None = None,
) -> Path:
    """Write a ``.claude/agents/<name>.md`` file under the workspace.

    Produces a valid agent markdown file with YAML frontmatter.
    Returns the path to the created file.
    """
    agents_dir = workspace / ".claude" / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)
    path = agents_dir / f"{name}.md"
    lines = ["---", f"name: {name}", f"description: {description}"]
    if allowed_tools:
        lines.append("allowed-tools:")
        lines.extend(f"  - {t}" for t in allowed_tools)
    if exclude_tools:
        lines.append("exclude-tools:")
        lines.extend(f"  - {t}" for t in exclude_tools)
    if model:
        lines.append(f"model: {model}")
    lines.append("---")
    lines.append("")
    lines.append(body)
    path.write_text("\n".join(lines))
    return path
