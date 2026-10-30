# Rustain TUI E2E Tests

Pexpect-based end-to-end tests that drive the real rustain TUI through a pseudo-terminal.
These tests exercise the **full production stack**: binary startup, terminal rendering,
keyboard input, API calls, tool execution, file I/O, and session persistence.

The shared harness uses `TerminalScreen`, a pyte screen with terminal-correct
wide-glyph replacement: overwriting either column of a CJK glyph erases its
other half while preserving neighboring cells and styles. Ratatui's optimized
incremental frames rely on this terminal behavior; pyte 0.8 otherwise leaves
orphan continuation cells and raises `IndexError` while observing the screen.
`test_terminal_screen.py` covers cursor-addressed repaint, overlapping wide
glyphs, styles, wrapping, scrolling, and disabled autowrap without a live provider.

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

## Canonical receipt controls

`.github/scripts/check-receipt.sh [--product-root ROOT] [--legacy-demo-root ROOT] RECEIPT`
is the only receipt-consistency verifier. The executable defaults to its own product
checkout root, not the current directory. It uses Bash, GNU `date`, `sha256sum` and
the existing GNU text utilities; no sibling checkout, download, credentials or Rust
build is needed. The complete outer executing epic gate passes its genuine demo
root explicitly and delegates its committed-receipt sweep to this command. The
old outer `--check-receipt` entrypoint no longer exists.

```bash
# From the product checkout; also works from an unrelated CWD with absolute paths.
.github/scripts/check-receipt.sh --product-root "$PWD" /absolute/path/to/receipt.transcript.txt

# All 45 exact authored controls: 16 genuine-capture and 29 policy-boundary cases.
# Run after fresh native Act 1 (all three viewports) and Act 2 captures.
RUSTAIN_RECEIPT_EVIDENCE_DIR="$PWD/receipt-checker-evidence" \
  python3 -m pytest tests_tui/test_receipt_controls.py \
  -p no:rerunfailures -n 0 --timeout=120 --junitxml=receipt-checker-results.xml -v

# Product-only policy lane before native capture (29 cases, no binary bootstrap).
python3 -m pytest tests_tui/test_receipt_controls.py -m "not a2a_binary" \
  -p no:rerunfailures -n 0 --timeout=120 --junitxml=receipt-policy-results.xml -v
```

The two migrated mutation selectors are
`tests_tui/test_receipt_controls.py::test_genuine_receipt_rejects_changed_identity`
(3 cases) and
`tests_tui/test_receipt_controls.py::test_genuine_act1_receipt_rejects_changed_identity`
(9 cases). Native cases are marked `a2a_binary` so the generic pre-capture lane
does not certify stale source identities; dedicated receipt enforcement selects
them explicitly without marker exclusion. Missing captures are assertion failures,
not skips. Each selector picks the latest filename stamp (then run-directory
identity) for its required viewport; old flat and nested Act 2 receipts are both
recognized. Changed capture-source bytes require new genuine capture receipts:
never rewrite old headers or accept an archived source as the current source.

CI must check the JUnit exact selected counts (45 complete, 29 policy-only,
3 Act 2 identity, 9 Act 1 identity, 4 genuine product-only/CWD), with no skipped,
failed or errored nodes and reruns disabled. Run the dedicated complete step only
after the current-source genuine inputs are present. On all outcomes upload only
`receipt-checker-results.xml`, `receipt-policy-results.xml` when produced, named
command logs, and `receipt-checker-evidence/**/{positive,negative,restored}.json`
(spell the three JSON paths separately in the workflow; do not rely on brace
expansion). Never upload the pytest temporary tree, environment, homes, credentials
or full repository. These are integration requirements, not an assertion that CI
or the controls have already run.

The controls execute the real command, test independent scene/source/stamp
corruption, exact paths, required and duplicate applicable headers, all supported
timestamp shapes and the inclusive ±one-hour window. Policy fixtures isolate
retained historical scene bytes versus latest drift and the two exact historical
stamp exceptions plus adjacent nonexceptions; fixtures are not runtime evidence.
The checker asserts internal consistency, not authenticated-journal provenance.

### Executing conflict-flag capture (Story 19.21h)

The original `19-21b` scene names are retained. Run from the product checkout with
an explicit four-feature release; the driver uses that same binary for every host
and client, with `build=False` and short per-run `/tmp` workspaces. The existing
`openssl` executable is required for ephemeral local TLS and synthetic pinned
Ed25519 identities. Certificate verification stays enabled; each observing proxy
has its own trusted-CA context. No provider account or sibling checkout is needed.

```sh
openssl version
cargo build --locked --release --features self-update,relay-server,a2a,p2p
# Supply a new manifest path and an independent intended-run token for each
# native invocation; preserve both unchanged for the subsequent controls.
export RUSTAIN_TUI_BINARY="$PWD/target/release/rustain"
export RUSTAIN_CONFLICT_CAPTURE_RUN_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
export RUSTAIN_CONFLICT_RECEIPT_MANIFEST="$PWD/conflict-run-manifests/$RUSTAIN_CONFLICT_CAPTURE_RUN_ID.json"
env -u RUSTAIN_TUI_CLIENT_BINARY \
  RUSTAIN_CAPTURE_EVIDENCE_DIR="$PWD/conflict-capture-evidence" \
  python3 -m pytest \
  'tests_tui/test_story_19_21b_conflict_flag.py::test_conflict_flag_capture[80x24]' \
  'tests_tui/test_story_19_21b_conflict_flag.py::test_conflict_flag_capture[130x30]' \
  'tests_tui/test_story_19_21b_conflict_flag.py::test_conflict_flag_capture[60x16]' \
  -p no:rerunfailures -n 0 --timeout=420 --junitxml=conflict-native-results.xml -v

RUSTAIN_RECEIPT_EVIDENCE_DIR="$PWD/conflict-receipt-evidence" \
  python3 -m pytest tests_tui/test_story_19_21b_receipt_controls.py \
  -p no:rerunfailures -n 0 --timeout=120 --junitxml=conflict-integrity-results.xml -v
```

Require exactly three native viewports and nine independent scene/source/stamp
controls, with no skip/failure/error. Each integrity leg checks its current genuine
receipt, a corrupted copy and the byte-restored original through the canonical
command above. Successful native runs alone mint
`journeys/j10/receipts/<run>/story-19-21b-conflict-flag-<stamp>.transcript.txt`;
failed runs retain diagnostics without a success transcript.

The manifest path and intended-run token above are mandatory, not hints. The
native session creates the path exclusively and records the exact selected width
scope, actual binary SHA256, and every runtime source path/hash (all `src/**`
files, Cargo manifest/lock, optional `build.rs`, native driver, scene, shared
manifest helper, imported Act1 journal/navigation helpers, standalone HTTP
observer, terminal harness and its keys, pytest setup, and the scene-provider
package/implementation). Changes to an imported observation oracle invalidate
earlier captures just like Rust source changes; additions, deletions and byte
drift invalidate selection.
The canonical checker is deliberately outside this runtime-source fingerprint so
semantic checker mutations reach the real consumer.
Source symlinks are refused because the full mutation snapshot omits them.

Only after each capture's required teardown and retention succeeds do its
original and optional exported staged copies become finalized receipts. The
exported bytes must match the original, and both final names must be published
before the run index is committed with the contained product-relative path and
byte hash. Export or index publication failure rolls back this run's final
names, never another run's receipt. CI's transcript allowlist therefore retains
the finalized export rather than an unuploaded `.staged` file.
A partial native selection may finish its own manifest,
but cannot supply the controls: they require all three widths from one complete
explicitly selected run, the independently intended token, unchanged source and
binary bytes, and every indexed finalized receipt. Missing, stale, partial,
drifted or uncontained evidence is a prerequisite failure; historical files are
never searched or substituted.

For a mutation freeze, the selected manifest, finalized receipts and an identical
copy of the captured binary must be inside the product tree in snapshot-included
paths (not `target`, `.git`, `.venv` or another executor-excluded directory).
Point `RUSTAIN_TUI_BINARY` at that identical contained copy before loading the
mutation specs. M38 validates these real inputs while authoring its frozen
command and translates the manifest and binary to `{repo}`-relative paths; it
never inherits the owner's absolute paths. M37 creates a fresh phase-local
manifest and independent token from its unique `{junit}` path.

The real sequence covers arrival, explicit View/Acknowledge/context, an
operator-started model ToolUse, full Review, human Send, both attributed recipient
cards, a fresh board and sender-local `[agent-composed]` log, then a second
spoof-body proposal dismissed with zero peer/journal effects. Painted Unicode is
compared by equivalent graphemes; HTTP and journal text remain byte-exact. Raw
UTF-8 SHA256 and the product's canonical-JSON-string content hash are distinct.
This capture does not close J10 or add recipient authorship or policy authority.

The dedicated CI job selects those exact case IDs, rejects zero/skip/failure/error
and always uploads only named JUnit files, identity/checkpoint/HTTP/scene ledgers,
painted screens, public team logs, genuine transcripts, the exact selected run
manifest and the three integrity JSON diagnostics. It never uploads fixture keys,
homes, environment or a whole temporary/repository tree.

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