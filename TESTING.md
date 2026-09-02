# Manual Smoke Test Checklist

Run these tests before marking an epic as done. Estimated time: under 5 minutes.

## Prerequisites — the gate every story must run before claiming "gates green"

- **`cargo test --all-targets --no-fail-fast`** — the WHOLE suite, not a hand-picked target list.
  ⚠ **This wording is deliberate and was paid for.** Stories 19-3, 19-7, 19-8 and 19-1 each reported "gates green" from a narrow `--test <target>` enumeration, and all four missed that `conformance_18_3a_f_resolution` was red — a hard gate in CI's `check` job with no `continue-on-error`, so CI was red for four consecutive stories while every story record said otherwise. A ratchet you do not run is not a ratchet. `--no-fail-fast` matters too: without it the first failure hides the rest.
- **Ratchet targets, which a narrow run is most likely to skip.** Both of these pin `event_loop.rs` line growth, and they must agree:
  - `cargo test --test conformance` — `EVENT_LOOP_BASELINE_LINES` + soft/hard budget, plus the async-lock ratchet
  - `cargo test --test conformance_18_3a_f_resolution -- --test-threads=1` — the resolve-verb budget (same effective ceiling by construction)
- `cargo clippy --all-targets -- -D warnings` — **strict, not advisory.** A story's own files must be warning-free.
- `cargo fmt --check`
- `cargo build --release`

---

## 1. Core User Journey

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 1.1 | Fresh build, no prior sessions | Run `cargo run` | TUI launches, welcome screen visible, status bar shows model name + "Normal" |
| 1.2 | TUI running, input focused | Type "Hello, what is 2+2?" and press Enter | Message appears in chat, typing indicator shows, streaming response appears |
| 1.3 | Response complete | Press Esc to focus chat, then j/k to scroll | Scroll offset changes, content moves |
| 1.4 | Chat focused | Press G | Jumps to bottom of conversation |
| 1.5 | Chat focused | Press i | Returns focus to input box |
| 1.6 | Input focused | Type another message, press Enter | Multi-turn conversation works, both messages visible |
| 1.7 | Chat focused | Press q | TUI exits cleanly, terminal restored |
| 1.8 | Input focused, text typed | Press Ctrl+Q | TUI exits cleanly from any focus (input, chat, sidebar panels) — Story 19.3 / Journey 0 |
| 1.9 | A turn used Write/Edit (e.g. ask the model to edit a file) | Expand the tool block (Enter/x on the block) | Inline line diff instead of the byte-count text: green `+` additions, red `-` deletions, dim context. Long unchanged runs collapse to `⋯ N unchanged lines`, so a change deep in a large file is still visible. Diffs cap at 200 lines keeping the HEAD **and** the TAIL with `… N more lines` between them — a wholesale rewrite must show removals AND additions, never only removals. When the original was not captured the block shows one muted line naming the real reason: `overwrote N bytes — previous content not captured (no active checkpoint)` for the no-checkpoint case, otherwise `wrote N bytes — previous content not captured (<reason>)` where reason is one of: snapshot unavailable / snapshot read failed / original is not valid UTF-8 / superseded by another write in the same batch / recorded before this feature, or re-projected on attach. A write that changed nothing shows `no line changes — the file content is unchanged`; a line-ending-only change shows `⚠ line endings or trailing newline changed`. — Story 19.1 / FR30 / Journeys 0+2 |
| 1.10 | A turn used two or more tools, e.g. `Read` then `Write` | In Chat focus: `g`, then `]]` to the assistant turn you want, then `Tab` once per invocation past the first, then Enter | **Every** tool block is expandable — not just the conversation's first. Focus follows the turn you navigated to and seats on its first visible block; `Tab` cycles within that turn and the selection survives the next frame; a block that scrolls out of view releases focus to the top-most visible one; with no block on screen Enter is a no-op. ⚠ Keyboard tool-block focus follows a block's FIRST line: a block expanded taller than the pane releases focus once its top border scrolls above the viewport, and Enter cannot collapse it again until its start scrolls back into view — deliberate, pinned by `focus_is_released_when_its_block_scrolls_out_of_view`. ⚠ `Esc` in Chat focus toggles BACK to Input — one press only. ⚠ `Tab` needs two or more invocations in the focused turn and advances one per press from where focus already sits, so one press too many wraps back to the first block. — Story 19.9 / FR29 / Journeys 0+2 (`gate J0` and `gate J2` assert it through the front door: the expanded pane in each receipt shows the `Write`, not the first `Read`) |
| 1.11 | A workspace skill exists, e.g. `.agents/skills/safe-deploy/SKILL.md` with the scalar `allowed-tools: Bash(kubectl:*) Bash(helm:*) Read` | Ask the model to use it so it calls `activate_skill` (not `/safe-deploy`, which is the user-driven route) | The turn stops on `New project skill detected: "safe-deploy"` / `Trust and enable this skill for this session?` / `[y] Yes  [n] No  [i] Inspect`. `i` shows the canonical file and `Esc` returns to the prompt; `n` declines and activates nothing; `y` activates. An excluded tool dispatched after activation returns a denial and does not execute, including when emitted beside activation in one provider response. The filtered catalogue and `<skill>` prompt appear on the next submitted message. Two documented gaps remain: the prompt's "session" wording is per-conversation and memory-only, and the unmatched-items Advisory is queued in default `Focus` density until density changes. Receipt: `tests_tui/journeys/j3/receipts/journey-J3-*.transcript.txt` — Story 19.10 / `gate J3`. |

## 2. Session Lifecycle

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 2.1 | Completed at least one conversation turn | Exit with q | Clean exit, no errors |
| 2.2 | Previous session exists | Run `cargo run` again | Last conversation restored, messages visible |
| 2.3 | Session restored | Verify auto-generated title | Title visible in status bar (if first turn completed) |
| 2.4 | Session restored | Send a new message | Conversation continues seamlessly |

## 3. Crash Recovery

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 3.1 | Active conversation with messages | Kill process: `kill -9 $(pgrep rustain)` | Process terminates immediately |
| 3.2 | After kill -9 | Run `cargo run` | Recovery prompt appears: "Recovered: 'Title' ... [Enter/y] continue [n] new" |
| 3.3 | Recovery prompt visible | Press Enter or y | Conversation restored, input focused |
| 3.4 | Recovery prompt visible (re-test) | Kill and relaunch, press n | New empty session starts, old session preserved |

## 4. CLI Subcommands

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 4.1 | Terminal (not TUI) | Run `cargo run -- init` | Interactive wizard starts, detects API key status |
| 4.2 | Config already exists | Run `cargo run -- init` | Warns "Configuration already exists", asks to overwrite |
| 4.3 | Terminal (not TUI) | Run `cargo run -- doctor` | Health checks run, pass/fail indicators shown |
| 4.4 | No API key set | Unset keys, run `cargo run -- doctor` | Reports API key failure with fix suggestion |
| 4.5 | Doctor with failures | Check exit code: `echo $?` | Exit code 1 (not 0), no duplicate error output |

## 5. Terminal Compatibility

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 5.1 | Inside tmux | Run `cargo run` | TUI launches, no key conflicts with basic operations |
| 5.2 | Inside tmux | Run `cargo run -- doctor --terminal` | Reports tmux detected, mentions prefix key conflict |
| 5.3 | Direct terminal (no multiplexer) | Run `cargo run` | Full color support, no warnings |
| 5.4 | SSH session (if available) | Run `cargo run` | TUI launches, session persistence works |

## 6. Edge Cases

| # | Precondition | Action | Expected Outcome |
|---|-------------|--------|-----------------|
| 6.1 | Input focused | Press Enter with empty input | No message sent, no crash |
| 6.2 | During streaming response | Press Ctrl+C | Streaming aborts, partial response preserved |
| 6.3 | During streaming response | Resize terminal window | Layout adjusts, no crash or rendering glitch |
| 6.4 | Input focused | Type a very long message (500+ chars) | Input scrolls, message sends correctly |
| 6.5 | A skill's `allowed-tools` (any of the four forms, e.g. scalar `Bash(kubectl:*) Bash(helm:*) Read`) declares an item this build cannot match — a pattern, a typo, or an MCP tool whose server is down | Send any turn that activates the skill | Exactly ONE warning on that turn naming the unmatched items (sorted) as unavailable *for this turn*; honoured items (e.g. `Read`) still offered; `Bash` itself stays filtered out (patterns never widen to the bare tool); the warning does NOT say the skill failed to load or validate; the disjoint warning does not fire unless the filters truly share no tool; **the turn keeps running and answers** — the disclosure is advisory, never turn-fatal (a plain `Warning` cancelled the turn it described; caught at code review) — Story 19.2 / FR42-a, see `docs/skills.md`. ⚠ In the DEFAULT `Focus` density the advisory is queued and never painted (`DF-19-10-ADVISORY-QUEUED-IN-FOCUS`): press `Ctrl+X` then `w` to drain it. Committed receipt of the whole beat — the scalar form loading, the trust gate, the filtered catalogue on the wire and this very sentence on screen — is `tests_tui/journeys/j3/receipts/journey-J3-*.transcript.txt` (Story 19.10, `gate J3`) |

---

## Automated Test Reference

These manual tests complement the automated suite:

| Automated Suite | Test Count | Covers |
|----------------|-----------|--------|
| `tests/e2e_harness.rs` | 13 | Core user journey, streaming, tool use, P0 regression |
| `tests/e2e_crash_recovery.rs` | 14 | Crash detection, recovery prompt, context rebuild |
| `tests/conformance.rs` | 2 | Hexagonal architecture enforcement |
| `tests/doctor_health.rs` | 35 | Health check framework, API validation |
| `tests/init_wizard.rs` | 12 | Init wizard, TTY detection, config creation |
| `tests/session_persistence.rs` | 3 | Save/load round-trip, atomic writes |
| `tests/title_generation.rs` | 22 | Auto-title trigger, post-processing |
| All integration tests | 33 files | Full feature coverage |

Run all automated tests: `cargo test`
