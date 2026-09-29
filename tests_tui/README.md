# Rustain TUI E2E Tests

Pexpect-based end-to-end tests that drive the real rustain TUI through a pseudo-terminal.
These tests exercise the **full production stack**: binary startup, terminal rendering,
keyboard input, API calls, tool execution, file I/O, and session persistence.

## Quick Start

```bash
# Install dependencies (once)
pip install pytest pexpect

# Build the binary (done automatically by fixtures, or manually)
cargo build

# Run all tests
cd tests_tui
pytest

# Run only tests that don't need an API key
pytest -m "not requires_api"

# Run a single story
pytest -m story_4_3b -v

# Run with output visible
pytest -v -s
```

## Architecture

```
tests_tui/
  pyproject.toml          # pytest config, markers, dependencies
  conftest.py             # fixtures: tui, tui_in_project, build_binary
  harness.py              # RustainTUI class — pexpect wrapper
  keys.py                 # Keyboard constants (Chat, Confirm, Permission, etc.)
  manual_test_ledger.py   # Plain script: one scripted turn + the ledger rows it minted
  fixtures/scene_provider.py    # Anthropic-API stand-in: scripted turns, no account
  fixtures/scenes/smoke.json    # The scene it serves (one text turn, one Write turn)
  test_smoke.py           # Binary existence, --help, startup/shutdown
  test_story_3_x_input.py # Stories 3.1, 3.3, 3.5 — input, palette, help
  test_story_4_3a_fork.py # Story 4-3a — fork conversations
  test_story_4_3b_rewind.py # Story 4-3b — rewind with file snapshot
  test_story_4_4_search.py  # Story 4-4 — search, bookmarks, export
  test_story_4_4_tabs.py    # Story 4-4 — multi-tab management
```

## How It Works

### The Harness

`RustainTUI` spawns the rustain binary in a **pseudo-terminal** (PTY) via pexpect.
This gives it a real terminal environment — crossterm sees it as an interactive session.

```python
from harness import RustainTUI

with RustainTUI(fresh=True) as tui:
    tui.send_message("Write 'hello' to test.txt")
    tui.wait_for_idle()
    assert tui.file_exists("test.txt")
```

### Test Isolation

Each test gets a **temporary workspace directory**. The harness:
1. Creates a temp dir
2. Copies `.env` (API key) into it
3. Creates `.claude/settings.json` with AlwaysAllow for common tools
4. Spawns rustain with `--new` and `cwd=temp_dir`
5. Cleans up on teardown

This means tests are fully isolated — no shared state, no leftover sessions.

### Model turns without a provider account (the scene provider)

`fixtures/scene_provider.py` is an Anthropic Messages API stand-in. It serves scripted
turns over HTTP, so a test can drive a **real model turn** through the real binary with
no key and no network — that is how `test_story_19_7_scene_provider.py` runs in the
non-API CI lane. A scene file (`fixtures/scenes/smoke.json`) holds one turn list per
**persona**, and the persona is the api-key value the binary presents, so several
personas share one stub process. Each turn declares `expect` (a substring that must
appear in the last user text), the `content` blocks to serve (`text` or `tool_use`),
its `stop_reason` and its `usage`. An `expect` mismatch is a **desync**: the stub
answers an SSE `error` event, logs `"desync": true`, does not advance the turn cursor,
and exits **1** — a desync is the product sending something the scene did not script.
The full scene and request-log formats are in the module docstring.

```python
from fixtures.scene_provider import SceneStub

stub = SceneStub(SCENE, tmp_path / "requests.jsonl").start()   # port 0
with RustainTUI(env_overrides=stub.env("scene-ci", scratch_home)) as tui:
    tui.send_message("scene ping")
assert stub.stop() == 0            # 0 clean, 1 desync or unknown path
```

`SceneStub.env()` is the authoritative env block and it is **not optional reading**:
`harness.py` merges `rustain/.env` into the child env, that file exports a real
`ANTHROPIC_AUTH_TOKEN`, and the binary prefers `AUTH_TOKEN` over `API_KEY` — so a test
that sets only the api key sends a developer's real credential to whatever it is
talking to. The `HOME` argument is load-bearing for the same reason: the user-global
config layer is `dirs::home_dir()/.config/rustain/config.toml`, `RUSTAIN_CONFIG_DIR`
does **not** move it, and a developer's `[provider.openrouter] enabled = true` there
makes the binary take the config provider path and ignore `ANTHROPIC_BASE_URL`
entirely. Assert product-side: the stub's request log, the usage ledger under
`tui.wp/.rustain_data/usage`, the session files — never on prose the stub was told to
serve.

`RUSTAIN_TUI_BINARY=<path>` makes the harness spawn that binary instead of building
`target/debug/rustain` (a release-profile capture uses this). A path that does not
exist raises `FileNotFoundError` before any build or spawn — never a silent fallback.
`python3 tests_tui/manual_test_ledger.py` is the plain-script version of all of this:
one scripted turn, then the ledger rows it minted.

### Key Mappings

All keyboard constants live in `keys.py`, organized by focus state:

```python
from keys import Chat, Confirm, ESC, CTRL_F

tui.send(ESC)           # Switch to Chat focus
tui.send(Chat.REWIND)   # Press R (uppercase)
tui.send(Confirm.YES)   # Press y to confirm
```

The mappings are derived from `src/adapters/tui/app.rs` and must be kept in sync.

### Critical: Character-by-Character Input

Raw-mode TUIs (crossterm) **drop burst input** from pexpect's `sendline()`.
The harness sends characters individually with a 10ms delay:

```python
def send_message(self, text, char_delay=0.01):
    for c in text:
        self.child.send(c)
        time.sleep(char_delay)
    self.child.send(ENTER)
```

This is the single most important implementation detail. Without it, input is silently lost.

## Writing New Tests

### 1. Choose the Right Fixture

| Fixture | Workspace | Use When |
|---------|-----------|----------|
| `tui` | Temp dir (isolated) | Most tests — tool execution, rewind, fork |
| `tui_in_project` | Real project dir | Need CLAUDE.md, existing sessions |
| `build_binary` | N/A | Just need the binary built |

### 2. Follow the Pattern

```python
import pytest
from harness import RustainTUI

@pytest.mark.requires_api    # Needs API key
@pytest.mark.slow            # Takes >30s
@pytest.mark.story_4_3b      # Story marker for filtering
class TestMyFeature:
    def test_something(self, tui: RustainTUI):
        # 1. Setup: pre-create files if needed
        tui.write_file("input.txt", "original content\n")

        # 2. Act: send a message, wait for AI + tool execution
        tui.send_message("Modify input.txt to say 'changed'")
        tui.wait_for_idle()

        # 3. Assert: check file state
        assert tui.file_content("input.txt") != "original content\n"

        # 4. (Optional) Trigger TUI action
        tui.chat_mode()
        tui.scroll_up(5)
        tui.rewind()

        # 5. Assert: check reverted state
        assert tui.file_content("input.txt") == "original content\n"

        # 6. (Optional) Verify via logs
        tui.assert_log_contains(r"revert_file_snapshots.*found \d+ candidates")
```

### 3. Available Actions

| Method | Focus | Description |
|--------|-------|-------------|
| `send_message(text)` | Input | Type and submit a message |
| `chat_mode()` | Any | Press Escape to enter Chat focus |
| `input_mode()` | Chat | Press i to enter Input focus |
| `scroll_up(n)` | Chat | Press k n times |
| `scroll_down(n)` | Chat | Press j n times |
| `jump_top()` | Chat | Press g |
| `jump_bottom()` | Chat | Press G |
| `rewind()` | Chat | Press R then y |
| `rewind_cancel()` | Chat | Press R then n |
| `rewind_fork_instead()` | Chat | Press R then f |
| `fork()` | Chat | Press f then y |
| `fork_cancel()` | Chat | Press f then n |
| `open_help()` | Chat | Press ? |
| `open_search()` | Any | Press Ctrl+F |
| `toggle_sidebar()` | Any | Press Ctrl+H |
| `new_tab()` | Any | Press Ctrl+T |
| `switch_tab(n)` | Chat | Press 1-9 |
| `toggle_bookmark()` | Chat | Press m |
| `open_bookmark_list()` | Chat | Press ' |
| `close_overlay()` | Overlay | Press Escape |
| `approve_permission()` | Permission | Press y |
| `wait_for_idle(sec)` | Any | Sleep (wait for turn to complete) |

### 4. Available Assertions

| Method | Description |
|--------|-------------|
| `file_exists(path)` | Check if file exists in workspace |
| `file_content(path)` | Read file content (raises if missing) |
| `assert_log_contains(regex)` | Assert log has matching line |
| `assert_log_not_contains(regex)` | Assert log has no matching line |
| `log_grep(regex)` | Return matching log lines |
| `checkpoints_exist()` | Any session has checkpoints |
| `snapshot_count()` | Count snapshot files across sessions |
| `session_ids()` | List all session IDs |

## Markers

```bash
pytest -m "not requires_api"   # CI without API key
pytest -m story_4_3b           # Single story
pytest -m "slow"               # Only slow tests
pytest -m "not slow"           # Quick tests only
```

## Adding a New Story

1. Create `test_story_X_Y_name.py`
2. Add marker to `pyproject.toml` if new
3. Use AC numbers in class/test names for traceability
4. Mark with `@pytest.mark.requires_api` if test sends messages
5. Mark with `@pytest.mark.slow` if test takes >30s

## Troubleshooting

**Test hangs forever**: The AI didn't use a tool, or used a tool not in
the AlwaysAllow list. Check `~/.rustain/rustain.log.*` for the session.

**"File not found" after tool execution**: The AI wrote the file with a
different name or path. Use `tui.log_grep("Snapshotted")` to see what
was actually written.

**Input not received**: Make sure `send_message()` is used (char-by-char),
not `sendline()` (burst). Raw-mode TUIs drop burst input.

**Permission prompt blocks**: The temp workspace's `.claude/settings.json`
must list the tool. Check `harness.py` `start()` method.

##   Key design decision learn arned:
  1. Char-by-char input — raw-mode TUIs drop burst sendline() input; must send individually with 10ms delay
  2. AlwaysAllow settings — temp workspaces need .claude/settings.json with tool permissions to avoid hanging on prompts
  3. Log-based verification — assert against ~/.rustain/rustain.log.* for internal state that isn't visible on screen