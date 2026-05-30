"""Pytest configuration and shared fixtures for rustain TUI E2E tests."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

# Ensure tests_tui/ is on the import path
sys.path.insert(0, str(Path(__file__).parent))

from harness import RustainTUI, PROJECT_ROOT, BINARY


# ── Guards ───────────────────────────────────────────────────────────────────

def pytest_configure(config):
    """Register custom markers.

    All story markers are declared in pyproject.toml; this is a fallback for
    environments where pyproject.toml discovery is unreliable.
    """
    markers = [
        "requires_api: test requires a live API key and network access",
        "slow: test takes >30s (tool execution + AI response)",
        "story_3_1: Story 3.1 — Multi-line Input & History",
        "story_3_3: Story 3.3 — Command Palette & Which-Key",
        "story_3_5: Story 3.5 — Help Overlay & Discoverability",
        "story_4_3a: Story 4-3a — Fork Conversations",
        "story_4_3b: Story 4-3b — Rewind with File Snapshot",
        "story_4_4: Story 4-4 — Search, Bookmarks & Export",
        "story_5_0: Story 5-0 — Python TUI Contract Test Infrastructure",
        "story_5_0b: Story 5-0b — Permission System Redesign",
        "story_5_1: Story 5-1 — Agent Skills Discovery & Catalog",
        "story_5_2: Story 5-2 — Agent Skills Progressive Disclosure & Execution",
        "story_5_3: Story 5-3 — Custom Slash Commands",
        "story_5_4: Story 5-4 — Custom Agents",
        "story_6_0a: Story 6-0a — Cancellation Token Tree & Dual-Channel EventBus",
        "story_6_0b: Story 6-0b — ToolScheduler with ToolCall 7-Variant FSM",
        "story_6_0c: Story 6-0c — ApprovalRuntime Pub/Sub",
        "story_6_0d: Story 6-0d — Plan Mode Workflow",
        "story_6_1a: Story 6-1a — Inline Plan Card",
        "story_6_2a: Story 6-2a — Sequential Task Execution & Dependencies",
        "story_6_3: Story 6-3 — Task Panel & Progress Monitoring",
        "story_6_4: Story 6-4 — Task Control & Plan Deviation",
        "story_9_1: Story 9-1 — MCP Server Configuration & Connection",
        "story_9_2: Story 9-2 — MCP Tool Discovery & Invocation",
        "story_9_3: Story 9-3 — Capability Provider Architecture / Built-in Refactor",
        "story_9_4: Story 9-4 — Tool Exposure Strategy",
        "story_9_5: Story 9-5 — Sandbox Policy (Landlock)",
        "story_9_6: Story 9-6 — Skill Exposure Port",
        "story_10_4: Story 10-4 — Subagent Panel & Agent Inspector",
        "story_10_5: Story 10-5 — Task Delegation to Subagents",
        "story_10_7: Story 10-7 — `task` Tool & Subagent Dispatch",
        "story_10_x: Story 10-x — Live-LLM subagent / task-tool smoke",
        "mcp: Tests that exercise MCP (stdio) integration via the fixture server",
        "subagent: Tests that exercise in-process subagent spawning / the task tool",
    ]
    for marker in markers:
        config.addinivalue_line("markers", marker)


def pytest_collection_modifyitems(config, items):
    """Auto-skip requires_api tests when ANTHROPIC_API_KEY is not set."""
    has_key = bool(os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN"))

    # Also check .env file
    env_file = PROJECT_ROOT / ".env"
    if not has_key and env_file.exists():
        for line in env_file.read_text().splitlines():
            line = line.strip().removeprefix("export ").strip()
            if line.startswith("ANTHROPIC_API_KEY=") or line.startswith("ANTHROPIC_AUTH_TOKEN="):
                val = line.split("=", 1)[1].strip()
                if val and val != '""' and val != "''":
                    has_key = True
                    break

    for item in items:
        if "requires_api" in item.keywords:
            # Serialize all API tests to the same xdist worker to avoid
            # hammering the provider with parallel requests (rate-limit
            # flakiness under api.z.ai / glm-4.5-Air).
            item.add_marker(pytest.mark.xdist_group("api"))
            if not has_key:
                item.add_marker(
                    pytest.mark.skip(reason="No API key — set ANTHROPIC_API_KEY or add to .env")
                )


# ── Fixtures ─────────────────────────────────────────────────────────────────

@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Build the rustain binary once per test session."""
    result = subprocess.run(
        ["cargo", "build"],
        cwd=PROJECT_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.fail(f"cargo build failed:\n{result.stderr}")
    if not BINARY.exists():
        pytest.fail(f"Binary not at {BINARY}")


@pytest.fixture
def tui(build_binary):
    """Provide a fresh RustainTUI instance (auto-cleanup).

    The TUI starts with --new (fresh conversation) and uses a
    temporary workspace so tests are isolated from each other.

    Requires @pytest.mark.requires_api for tests that send messages.
    """
    harness = RustainTUI(fresh=True, build=False)
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture
def tui_monitor(build_binary):
    """Provide a fresh RustainTUI with ``density_mode = "monitor"``.

    The default ``tui`` fixture leaves density at the layout default, which
    hides sidebars regardless of ``state.sidebar_visible`` (same reason
    ``tui_with_mcp`` sets monitor density). Sidebar-panel tests — History,
    Tasks, Agents — need the sidebar to actually render, so use this fixture.
    """
    tmp = tempfile.TemporaryDirectory(prefix="rustain_monitor_test_")
    ws = Path(tmp.name)
    allow_list = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]
    config_dir = ws / ".rustain"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text(
        "[permissions]\n"
        f"always_tools = {json.dumps(allow_list)}\n"
        "\n"
        "[layout]\n"
        'density_mode = "monitor"\n'
    )
    harness = RustainTUI(fresh=True, build=False, workspace=ws, allowed_tools=allow_list)
    harness.start()
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()


@pytest.fixture
def tui_in_project(build_binary):
    """Provide a RustainTUI running inside the actual project workspace.

    Use this when tests need the real .env, CLAUDE.md, or existing
    session data.  Mutations (file writes, session creation) happen
    in the REAL workspace — use sparingly and clean up after.
    """
    harness = RustainTUI(fresh=True, build=False, workspace=PROJECT_ROOT)
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture
def tui_strict(build_binary):
    """Provide a RustainTUI with NO AlwaysAllow rules.

    Like ``tui`` but the temp workspace's settings.json only contains
    safe (read-only) tools.  Use this to test permission prompts for
    Standard/Elevated tools like Bash, Write, Edit.
    """
    harness = RustainTUI(fresh=True, build=False, allowed_tools=["Read", "Glob", "Grep"])
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture
def tui_write_only(build_binary):
    """Provide a RustainTUI that only auto-allows Read/Write (no Bash/Edit).

    Use this for rewind/snapshot tests where the model must use the Write
    tool so that file snapshots are created.  Bash bypasses snapshotting.
    """
    harness = RustainTUI(fresh=True, build=False, allowed_tools=["Read", "Write", "Glob", "Grep"])
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture(autouse=True)
def _reset_tui_state(request):
    """After each test, send Esc to close any open overlays and return to
    a known state.  Only applies to tests that used a ``tui`` or
    ``tui_in_project`` fixture.

    The fixture value is captured *before* yield (during setup) so it is
    still available during teardown — after the ``tui`` fixture's own
    finalizer has run, ``request.getfixturevalue()`` raises AssertionError.
    """
    tui_fixtures = {"tui", "tui_monitor", "tui_in_project", "tui_strict"}
    active = tui_fixtures.intersection(request.fixturenames)
    tui_instance = None
    if active:
        try:
            tui_instance = request.getfixturevalue(next(iter(active)))
        except Exception:
            pass
    yield
    if tui_instance is None:
        return
    try:
        from keys import ESC
        for _ in range(3):
            tui_instance.send(ESC)
        tui_instance.wait_for_screen("Ready", timeout=2.0)
    except Exception:
        pass


@pytest.fixture
def workspace(tmp_path):
    """Provide a temporary workspace directory (no TUI, for unit-style helpers)."""
    return tmp_path


# ── MCP fixtures (Epic 9) ────────────────────────────────────────────────────

MCP_FIXTURE = Path(__file__).parent / "fixtures" / "mcp_fixture.py"


def _write_test_mcp_profile(config_dir: Path, profile_name: str = "tests-mcp") -> None:
    """Write a profile that selects the composite tools adapter so workspace
    .claude/mcp.json is honored. Default 'coding' profile uses 'builtin-full'
    which short-circuits MCP loading entirely.
    """
    profiles_dir = config_dir / "profiles"
    profiles_dir.mkdir(parents=True, exist_ok=True)
    (profiles_dir / f"{profile_name}.toml").write_text(
        f'name = "{profile_name}"\n'
        'description = "Test profile — composite tools adapter for MCP E2E tests."\n'
        'extends = "coding"\n'
        "\n"
        "[tools]\n"
        'adapter = "composite"\n'
    )


def _write_mcp_json(
    workspace_path: Path,
    servers: dict[str, dict] | None = None,
    *,
    server_name: str = "echo",
    server_env: dict[str, str] | None = None,
) -> Path:
    """Write ``.claude/mcp.json`` under the workspace.

    When ``servers`` is None, registers a single stdio server (``server_name``)
    pointing at the local Python fixture with optional env overrides.
    """
    claude_dir = workspace_path / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    if servers is None:
        spec: dict = {
            "command": sys.executable,
            "args": [str(MCP_FIXTURE)],
        }
        if server_env:
            spec["env"] = server_env
        servers = {server_name: spec}
    path = claude_dir / "mcp.json"
    path.write_text(json.dumps({"mcpServers": servers}, indent=2))
    return path


@pytest.fixture
def tui_with_mcp(build_binary, request):
    """Provide a RustainTUI wired to the local stdio MCP fixture server.

    Behavior:
      * Workspace is an isolated temp dir (auto-cleaned).
      * ``.claude/mcp.json`` registers the ``echo`` server (overridable via
        ``request.param``) pointing at ``tests_tui/fixtures/mcp_fixture.py``.
      * A ``tests-mcp`` profile is written into ``RUSTAIN_CONFIG_DIR/profiles/``
        with ``[tools] adapter = "composite"`` so the workspace MCP config is
        actually loaded (the default ``coding`` profile uses ``builtin-full``).
      * ``RUSTAIN_PROFILE=tests-mcp`` is injected via ``env_overrides``.
      * Permissions: only Read/Glob/Grep are pre-allowed — MCP tools always
        prompt by default so tests can drive the approval flow.

    Parametrize with ``@pytest.mark.parametrize("tui_with_mcp", [{...}], indirect=True)``
    where the dict can contain:
      * ``servers``: full ``mcpServers`` dict (overrides default echo server)
      * ``server_env``: env vars for the spawned fixture (e.g. ``FAKE_MCP_DROP_AFTER_MS``)
      * ``server_name``: name registered in mcp.json (default ``echo``)
      * ``allowed_tools``: override the auto-allow list
    """
    param = getattr(request, "param", {}) or {}
    servers = param.get("servers")
    server_env = param.get("server_env")
    server_name = param.get("server_name", "echo")
    allowed_tools = param.get("allowed_tools", ["Read", "Glob", "Grep"])

    tmp = tempfile.TemporaryDirectory(prefix="rustain_mcp_test_")
    ws = Path(tmp.name)

    # Workspace MCP config
    _write_mcp_json(ws, servers=servers, server_name=server_name, server_env=server_env)

    # Profile that selects composite tools adapter
    config_dir = ws / ".rustain"
    config_dir.mkdir(parents=True, exist_ok=True)
    _write_test_mcp_profile(config_dir, profile_name="tests-mcp")
    # Pre-create config.toml so sidebar/panel TUI tests can render. Default
    # Focus density hides sidebars at the layout level regardless of
    # `state.sidebar_visible`. We pre-write the config because RustainTUI.start
    # only auto-creates it when missing; pre-creating lets us layer [layout]
    # on top of the permissions block.
    #
    # Also registers an `anthropic` provider from env-var auth so live-LLM
    # tests can actually talk to a model. The user's ~/.config/rustain
    # config.toml is merged BEFORE our workspace config; without an explicit
    # `[provider.anthropic]` block here, only the user's `[provider.openrouter]`
    # would be present, which fails to construct in the default test build
    # (`openai` feature not enabled). Setting enabled=false on openrouter
    # overrides the user's inherited config so it doesn't poison the registry.
    #
    # Effect for tests without API keys: rustain will warn that the provider
    # can't reach the endpoint, but the deterministic tier doesn't depend on
    # the provider working — only `@requires_api` tests do, and those skip
    # cleanly via the existing collection-modify hook + the runtime
    # `_skip_if_provider_unreachable` helper.
    config_toml = config_dir / "config.toml"
    config_toml.write_text(
        "[permissions]\n"
        f"always_tools = {json.dumps(allowed_tools)}\n"
        "\n"
        "[layout]\n"
        'density_mode = "monitor"\n'
        "\n"
        "# Disable any user-inherited openrouter provider — the default test\n"
        "# build is missing the openai feature so it can't construct.\n"
        "[provider.openrouter]\n"
        "enabled = false\n"
        "provider_id = \"openrouter\"\n"
        "model_id = \"unused\"\n"
        "api_key_env = \"OPENROUTER_API_KEY\"\n"
        "\n"
        "# Register the anthropic provider from env-var auth. The harness\n"
        "# already loads ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY from .env\n"
        "# into the spawned process env.\n"
        "[provider.anthropic]\n"
        "provider_id = \"anthropic\"\n"
        "model_id = \"glm-4.7\"\n"
        "api_key_env = \"ANTHROPIC_AUTH_TOKEN\"\n"
        "enabled = true\n"
        "kind = \"anthropic\"\n"
    )

    harness = RustainTUI(
        fresh=True,
        build=False,
        workspace=ws,
        allowed_tools=allowed_tools,
        env_overrides={"RUSTAIN_PROFILE": "tests-mcp"},
    )
    harness.start()
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()


# ── Subagent fixtures (Epic 10) ───────────────────────────────────────────────

def _write_subagent_provider_config(config_dir: Path, allowed_tools: list[str]) -> None:
    """Write config.toml registering an anthropic provider (env-var auth) plus
    the composite tools adapter so the SubagentProvider + `task` tool are wired.

    Mirrors the provider block in ``tui_with_mcp``: the default test build lacks
    the ``openai`` feature, so any user-inherited ``[provider.openrouter]`` is
    disabled to avoid poisoning the registry, and the anthropic provider is
    registered from ``ANTHROPIC_AUTH_TOKEN`` (loaded by the harness from .env).
    """
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text(
        "[permissions]\n"
        f"always_tools = {json.dumps(allowed_tools)}\n"
        "\n"
        "[layout]\n"
        'density_mode = "monitor"\n'
        "\n"
        "[provider.openrouter]\n"
        "enabled = false\n"
        'provider_id = "openrouter"\n'
        'model_id = "unused"\n'
        'api_key_env = "OPENROUTER_API_KEY"\n'
        "\n"
        "[provider.anthropic]\n"
        'provider_id = "anthropic"\n'
        'model_id = "glm-4.7"\n'
        'api_key_env = "ANTHROPIC_AUTH_TOKEN"\n'
        "enabled = true\n"
        'kind = "anthropic"\n'
    )


def _write_subagent_profile(config_dir: Path, profile_name: str = "tests-subagent") -> None:
    """Write a profile selecting the composite tools adapter so the
    SubagentProvider (and therefore the `task` tool) is registered. The default
    'coding' profile uses 'builtin-full' which short-circuits subagent dispatch.
    """
    profiles_dir = config_dir / "profiles"
    profiles_dir.mkdir(parents=True, exist_ok=True)
    (profiles_dir / f"{profile_name}.toml").write_text(
        f'name = "{profile_name}"\n'
        'description = "Test profile — composite tools adapter for subagent E2E tests."\n'
        'extends = "coding"\n'
        "\n"
        "[tools]\n"
        'adapter = "composite"\n'
    )


@pytest.fixture
def tui_with_subagent(build_binary):
    """Provide a RustainTUI wired so a live LLM can delegate to a subagent.

    Behavior:
      * Isolated temp workspace (auto-cleaned).
      * A custom agent ``.claude/agents/echo-agent.md`` is pre-populated so the
        SubagentProvider discovers a delegation target named ``echo-agent``.
      * A ``tests-subagent`` profile with ``[tools] adapter = "composite"`` is
        written into ``RUSTAIN_CONFIG_DIR/profiles/`` and selected via
        ``RUSTAIN_PROFILE`` so the `task` tool is registered.
      * An ``anthropic`` provider is registered from env-var auth so the
        live-LLM ``@requires_api`` tests can reach a model (skip cleanly if not).
    """
    import tempfile as _tempfile

    from fixtures.agents import write_custom_agent

    allowed_tools = ["Read", "Glob", "Grep", "task"]

    tmp = _tempfile.TemporaryDirectory(prefix="rustain_subagent_test_")
    ws = Path(tmp.name)

    # Delegation target: a minimal, deterministic echo-style agent.
    write_custom_agent(
        ws,
        "echo-agent",
        "Echoes back a single requested word for E2E smoke testing.",
        body="You are a deterministic echo agent. Reply with exactly the word "
        "the caller asks for and nothing else.\n",
        allowed_tools=["Read"],
    )

    config_dir = ws / ".rustain"
    _write_subagent_provider_config(config_dir, allowed_tools)
    _write_subagent_profile(config_dir, profile_name="tests-subagent")

    harness = RustainTUI(
        fresh=True,
        build=False,
        workspace=ws,
        allowed_tools=allowed_tools,
        env_overrides={"RUSTAIN_PROFILE": "tests-subagent"},
    )
    harness.start()
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()
