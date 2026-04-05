# Manual Smoke Test Checklist

Run these tests before marking an epic as done. Estimated time: under 5 minutes.

## Prerequisites

- `cargo test` passes (all unit + integration + E2E tests green)
- `cargo clippy` clean (no new warnings)
- Build: `cargo build`

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
