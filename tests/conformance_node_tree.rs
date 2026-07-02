//! Story 14.1 conformance guards for the `NodeTree` promotion.
//!
//! These tests pin the architectural invariants established when the old
//! `SubagentRegistry` was promoted into the unified `NodeTree`:
//!
//! - **Single ownership** — `AppState` / `TuiState` hold no `NodeTree` field.
//!   The tree is composed into `AgentCore` and accessed via ports/snapshots.
//! - **No parallel legacy surface** — there is exactly one concrete tree type
//!   (`NodeTree`), and the legacy `SubagentRegistry` alias is gone after the
//!   AC1 cutover.
//! - **Open lifecycle** — `NodeState` is `#[non_exhaustive]` so Epic 14 peer
//!   agents can extend the lifecycle without breaking match sites.
//! - **Table-driven transitions** — the only `NodeState -> bool` predicates
//!   are `can_transition_to` / `is_terminal`. Ad-hoc predicates bypass the
//!   `TRANSITIONS` table and are forbidden.
//! - **R2 forward-compat, R1 unused** — `NodeHandle::Remote` is defined for
//!   the A2A transport work but constructed nowhere in production code.
//! - **Hexagonal boundary** — TUI widgets consume `AgentRowView`, never the
//!   tree directly.
//!
//! Two NFR69 latency tests round out the file: spawn p95 < 300 ms and
//! cascade-cancel p95 < 2000 ms (both 10× the NFR target as CI headroom).

use std::fs;

// ── helper ───────────────────────────────────────────────────────────────

fn collect_rs_files(dir: &str) -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(dir);
    let mut files = Vec::new();
    if dir.is_dir() {
        collect_recursive(dir, &mut files);
    }
    files
}

fn collect_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_recursive(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}

/// Strip every `#[cfg(test)] mod tests { … }` (or `mod tests { … }` nested
/// under cfg(test)) from `content`. Used by guards that must inspect
/// production code only — test scaffolding is allowed to exercise shapes the
/// conformance bar forbids in production (e.g. constructing `Remote`).
fn strip_test_mods(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut depth: i32 = 0;
    let mut in_test_mod: Option<i32> = None; // depth at which test mod opened
    let mut pending_test_attr = false;
    // Cross-line lexer state: raw strings (and, defensively, normal strings)
    // can span line boundaries, so the brace counter must carry context
    // between lines instead of resetting per line.
    let mut lex = LexState::default();

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(open_depth) = in_test_mod {
            // Inside test mod — track depth, pop when balanced.
            depth += count_braces(line, &mut lex);
            if depth <= open_depth {
                in_test_mod = None;
            }
            continue;
        }

        let is_attr = trimmed.starts_with("#[");
        let has_cfg_test = trimmed.contains("cfg(test)");
        // Looser `contains` (vs `starts_with`) on purpose: a false positive
        // fails the conformance test loudly, while a false negative would
        // silently leak test code into the production stream the guards scan.
        let opens_tests_mod = trimmed.contains("mod tests") && trimmed.contains('{');

        // Arm on a cfg(test) attribute. Pending stays armed across consecutive
        // attribute lines (handles `#[cfg(test)]\n#[allow]\nmod tests`) and
        // resolves on the next non-attribute line.
        if is_attr && has_cfg_test {
            pending_test_attr = true;
        }

        // Enter the test mod: same-line `#[cfg(test)] mod tests {`, or a later
        // `mod tests {` while pending from a prior cfg(test) attribute.
        if opens_tests_mod && (has_cfg_test || pending_test_attr) {
            in_test_mod = Some(depth);
            depth += count_braces(line, &mut lex);
            pending_test_attr = false;
            continue;
        }

        // A bare cfg(test)-bearing attribute line (no mod opener on it) is
        // dropped — it belongs to the test item that follows.
        if is_attr && pending_test_attr {
            continue;
        }

        // Any other non-attribute line resolves a stale pending flag.
        if !is_attr {
            pending_test_attr = false;
        }

        depth += count_braces(line, &mut lex);
        out.push_str(line);
        out.push('\n');
    }

    out
}

/// Lexer state carried across lines — raw strings can contain literal
/// newlines, so [`count_braces`] is stateful rather than purely per-line.
#[derive(Clone, Copy, Default)]
struct LexState {
    in_str: bool,
    /// `Some(n)` ⇒ currently inside a raw string fenced with `n` `#`s.
    raw_hashes: Option<usize>,
}

/// Count net braces on `line` outside char/string/raw-string literals and line
/// comments, carrying cross-line string state in `state`. Raw strings
/// (`r"…"`, `r#"…"#`, `r##"…"##`, …) — including ones that span lines — no
/// longer leak their interior `{`/`}` into the depth.
fn count_braces(line: &str, state: &mut LexState) -> i32 {
    let chars: Vec<char> = line.chars().collect();
    let mut delta: i32 = 0;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];

        // (a) Inside a raw string — scan to its close: `"` + matching fence.
        if let Some(h) = state.raw_hashes {
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '"' && closes_raw(&chars, i, h) {
                    i += 1 + h;
                    state.raw_hashes = None;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return delta; // raw string continues onto the next line
            }
            continue;
        }

        // (b) Inside a normal string — honor escapes, close on unescaped `"`.
        if state.in_str {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == '"' {
                state.in_str = false;
            }
            i += 1;
            continue;
        }

        // (c) Normal scan. Raw-string open: `r` at a word boundary + `#`* + `"`.
        if ch == 'r' && raw_string_opens(&chars, i) {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < chars.len() && chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            // j sits on the opening `"`.
            let mut k = j + 1;
            let mut closed = false;
            while k < chars.len() {
                if chars[k] == '"' && closes_raw(&chars, k, hashes) {
                    k += 1 + hashes;
                    closed = true;
                    break;
                }
                k += 1;
            }
            if closed {
                i = k;
            } else {
                state.raw_hashes = Some(hashes);
                return delta; // raw string spans subsequent lines
            }
            continue;
        }

        match ch {
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => break, // `//` line comment
            '"' => state.in_str = true,
            '\'' => {
                // Char literal vs lifetime label — peek ahead.
                let is_char_literal = if i + 1 < chars.len() {
                    let next = chars[i + 1];
                    next == '\\' || (i + 2 < chars.len() && chars[i + 2] == '\'')
                } else {
                    false
                };
                if is_char_literal {
                    // Consume through the closing quote, honoring escapes.
                    let mut j = i + 1;
                    while j < chars.len() && chars[j] != '\'' {
                        if chars[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    i = if j < chars.len() { j + 1 } else { j };
                    continue;
                }
                // Otherwise: lifetime label — the quote is brace-neutral.
            }
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    delta
}

/// True if a raw-string literal opens at `chars[i]`: `r` (not part of a longer
/// identifier) immediately followed by zero-or-more `#` and a `"`. Covers
/// `r"…"`, `r#"…"#`, `r##"…"##`, …. Byte/c-str raw variants (`br"…"`,
/// `cr"…"`) are not matched — the `b`/`c` prefix makes `r` non-word-boundary;
/// they are vanishingly rare in the scanned tree and their braces stay
/// excluded via the normal-string fallback.
fn raw_string_opens(chars: &[char], i: usize) -> bool {
    let prev_is_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
    if prev_is_ident {
        return false;
    }
    let mut j = i + 1;
    while j < chars.len() && chars[j] == '#' {
        j += 1;
    }
    j < chars.len() && chars[j] == '"'
}

/// True if `chars[pos] == '"'` closes a raw string fenced with `h` `#`s — i.e.
/// at least `h` `#`s follow the quote (for `h == 0`, any `"` closes).
fn closes_raw(chars: &[char], pos: usize, h: usize) -> bool {
    (1..=h).all(|k| chars.get(pos + k) == Some(&'#'))
}

#[test]
fn count_braces_ignores_raw_strings_and_chars() {
    // r"..." (0 fences): interior braces excluded; real trailing braces balance.
    let mut s = LexState::default();
    assert_eq!(count_braces("let a = r\"{ }\"; { }", &mut s), 0);
    // r#"..."# (1 fence): interior brace excluded.
    let mut s = LexState::default();
    assert_eq!(count_braces("let b = r#\"{ }\"#;", &mut s), 0);
    // r##"..."## (2 fences): interior excluded; a single "# inside is literal.
    let mut s = LexState::default();
    assert_eq!(count_braces("let c = r##\" { \"##;", &mut s), 0);
    // Char-literal brace + lifetime excluded; a real unbalanced brace counts.
    let mut s = LexState::default();
    assert_eq!(count_braces("fn f<'a>() { let ch = '{'; x", &mut s), 1);
}

#[test]
fn count_braces_handles_multiline_raw_string() {
    // Raw string opened on line 1, interior braces on line 2, close on line 3.
    let mut s = LexState::default();
    assert_eq!(count_braces("let s = r#\"  ", &mut s), 0); // opens, no close → stays raw
    assert_eq!(s.raw_hashes, Some(1));
    assert_eq!(count_braces("} } { {", &mut s), 0); // interior — excluded
    assert_eq!(s.raw_hashes, Some(1));
    assert_eq!(count_braces("\"#;", &mut s), 0); // closes the raw string
    assert_eq!(s.raw_hashes, None);
}

// ── 1. AppState / TuiState hold no NodeTree field ───────────────────────

/// Flag 14.1-A: the unified `NodeTree` is composed into `AgentCore`, not held
/// directly on `AppState` or `TuiState`. A direct field would let callers
/// reach around the port boundary and couple to infrastructure internals.
#[test]
fn test_single_node_tree_on_app_state() {
    let files = [
        "src/adapters/tui/state.rs",
        "src/infrastructure/runtime/app_state.rs",
    ];

    for path in &files {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{} must exist for conformance check", path));
        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            // Skip comments and `use` imports — only field declarations count.
            if trimmed.starts_with("// ")
                || trimmed.starts_with("/// ")
                || trimmed.starts_with("//! ")
                || trimmed.starts_with("use ")
            {
                continue;
            }
            if trimmed.contains("node_tree:") || trimmed.contains("NodeTree") {
                panic!(
                    "NodeTree field found on app/tui state at {}:{}\n  {}",
                    path,
                    line_no + 1,
                    line
                );
            }
            // Symmetric guard: the legacy name must not sneak back in either.
            if trimmed.contains("subagent_registry:") || trimmed.contains("SubagentRegistry") {
                panic!(
                    "SubagentRegistry field found on app/tui state at {}:{}\n  {}",
                    path,
                    line_no + 1,
                    line
                );
            }
        }
    }

    // POSITIVE CONTROL — fail loudly if the NodeTree struct was renamed/moved
    // without updating this guard. Catches the "the test passed because the
    // file vanished" silent regression.
    let node_tree_src = fs::read_to_string("src/infrastructure/subagent/node_tree.rs")
        .expect("node_tree.rs exists");
    assert!(
        node_tree_src.contains("pub struct NodeTree"),
        "POSITIVE CONTROL FAILED: `pub struct NodeTree` missing from \
         src/infrastructure/subagent/node_tree.rs — conformance target moved."
    );
}

// ── 2. No parallel legacy surface remains ────────────────────────────────

/// Flag 14.1-B: there must be exactly ONE concrete tree type. A new
/// `pub struct SubagentRegistry` would fork the abstraction, and a lingering
/// `pub type SubagentRegistry = NodeTree` alias would keep the old surface
/// alive after the AC1 migration.
#[test]
fn test_no_parallel_subagent_registry_struct() {
    let subagent_files = collect_rs_files("src/infrastructure/subagent");
    assert!(
        !subagent_files.is_empty(),
        "POSITIVE CONTROL FAILED: src/infrastructure/subagent/ yielded no .rs files"
    );

    for file in &subagent_files {
        let content = fs::read_to_string(file).unwrap_or_default();
        let prod = strip_test_mods(&content);
        for (line_no, line) in prod.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("// ") || trimmed.starts_with("/// ") {
                continue;
            }
            assert!(
                !trimmed.starts_with("pub struct SubagentRegistry")
                    && !trimmed.starts_with("struct SubagentRegistry")
                    && !trimmed.starts_with("pub type SubagentRegistry =")
                    && !trimmed.starts_with("type SubagentRegistry ="),
                "Legacy `SubagentRegistry` surface found at {}:{} — AC1 requires a \
                 single concrete `NodeTree` type with no alias shim.\n  {}",
                file.display(),
                line_no + 1,
                line
            );
        }
    }

    // POSITIVE CONTROLS — the concrete type must exist, and the alias must not.
    let node_tree_src = fs::read_to_string("src/infrastructure/subagent/node_tree.rs")
        .expect("node_tree.rs exists");
    assert!(
        node_tree_src.contains("pub struct NodeTree"),
        "POSITIVE CONTROL FAILED: `pub struct NodeTree` missing from node_tree.rs."
    );
    assert!(
        !node_tree_src.contains("pub type SubagentRegistry ="),
        "Legacy `SubagentRegistry` alias still present in node_tree.rs — AC1 cutover incomplete."
    );
}

// ── 3. NodeState stays non_exhaustive ───────────────────────────────────

/// Flag 14.1-C: `NodeState` MUST be `#[non_exhaustive]`. Epic 14 (A2A peer
/// agents) and later stories will add states like `AwaitingApproval` or
/// `RemotePeer`; the attribute forces downstream crates to include `_` arms
/// instead of assuming the variant set is closed.
#[test]
fn test_node_state_is_non_exhaustive() {
    let content = fs::read_to_string("src/domain/models/node_state.rs")
        .expect("node_state.rs must exist for conformance check");

    let non_exhaustive_hits = content
        .lines()
        .filter(|l| l.trim() == "#[non_exhaustive]")
        .count();
    assert!(
        non_exhaustive_hits >= 1,
        "src/domain/models/node_state.rs must carry `#[non_exhaustive]` so Epic 14 \
         can extend the lifecycle without breaking downstream match sites."
    );

    // POSITIVE CONTROL — if the enum was renamed/moved, the count check above
    // could pass on an empty file. Anchor on the actual enum declaration.
    assert!(
        content.contains("pub enum NodeState"),
        "POSITIVE CONTROL FAILED: `pub enum NodeState` missing from \
         src/domain/models/node_state.rs — conformance target moved."
    );
}

// ── 4. NodeState transitions stay table-driven ──────────────────────────

/// Flag 14.1-D: the legal-transition graph MUST live in the `TRANSITIONS`
/// const slice. The only `fn(NodeState) -> bool` predicates allowed are
/// `can_transition_to` and `is_terminal` — both of which consult that table
/// (directly or via the terminal set). Ad-hoc predicates like
/// `fn is_spawnable(NodeState) -> bool` would let callers encode edges that
/// aren't in the table and silently bypass the FSM.
#[test]
fn test_node_state_transitions_table_driven() {
    let src_files = collect_rs_files("src");
    assert!(
        !src_files.is_empty(),
        "POSITIVE CONTROL FAILED: src/ yielded no .rs files"
    );

    let allowed_fns = ["can_transition_to", "is_terminal"];
    let mut violations: Vec<String> = Vec::new();

    for file in &src_files {
        let content = fs::read_to_string(file).unwrap_or_default();
        let prod = strip_test_mods(&content);

        for (line_no, line) in prod.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("// ") || trimmed.starts_with("/// ") {
                continue;
            }
            // A "fn signature with NodeState and bool" looks like
            // `… fn NAME(… NodeState …) … -> bool` — match loosely and then
            // allowlist the two sanctioned predicates by name.
            let looks_like_fn = trimmed.contains("fn ") || trimmed.contains("fn(");
            let has_nodestate = trimmed.contains("NodeState");
            let returns_bool = trimmed.contains("-> bool")
                || trimmed.contains("->  bool")
                || trimmed.ends_with("-> bool");
            if !(looks_like_fn && has_nodestate && returns_bool) {
                continue;
            }
            if allowed_fns.iter().any(|allowed| trimmed.contains(allowed)) {
                continue;
            }
            violations.push(format!("{}:{}: {}", file.display(), line_no + 1, line));
        }
    }

    assert!(
        violations.is_empty(),
        "Ad-hoc `fn(NodeState) -> bool` predicates found — transitions MUST be \
         table-driven via `can_transition_to` / `is_terminal` only:\n{}",
        violations.join("\n")
    );
}

// ── 5. NodeHandle::Remote is defined but never constructed in R1 ────────

/// Flag 14.1-E: the `Remote` variant exists for forward-compatibility with R2
/// (transport-routed A2A peer agents) but MUST NOT be constructed in
/// production code under R1. Match arms that destructure `Remote` (with `..`
/// or `=>`) are allowed — only field-initialising constructions are banned.
/// Test-module constructions are also allowed (they pin the variant shape).
#[test]
fn test_remote_handle_not_used_in_r1() {
    let src_files = collect_rs_files("src");
    assert!(!src_files.is_empty());

    let mut violations: Vec<String> = Vec::new();
    for file in &src_files {
        let content = fs::read_to_string(file).unwrap_or_default();
        let prod = strip_test_mods(&content);
        for (line_no, line) in prod.lines().enumerate() {
            if !line.contains("NodeHandle::Remote") {
                continue;
            }
            if !line.contains('{') {
                continue;
            }
            // Legitimate match arms use `..` (wildcard) or terminate with `=>`.
            // A field-init construction (`Remote { transport_ref: … }`) has
            // neither on the opening line.
            let is_match_arm = line.contains("..") || line.contains("=>");
            if !is_match_arm {
                violations.push(format!("{}:{}: {}", file.display(), line_no + 1, line));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "NodeHandle::Remote constructed in production code — Remote MUST stay \
         unused in R1 (defined only for R2 forward-compat):\n{}",
        violations.join("\n")
    );
}

// ── 6. Import-site count is pinned ──────────────────────────────────────

/// Flag 14.1-F: pin the number of `src/` files that mention `NodeTree`. A
/// sudden drop suggests a caller was deleted without verifying nothing else
/// depended on the tree; a sudden spike suggests someone reached past the port
/// boundary. Update `EXPECTED` only with a deliberate change to the import
/// graph.
#[test]
fn test_import_site_count_pinned() {
    /// Update this constant ONLY when intentionally adding/removing a
    /// legitimate `NodeTree` import site in `src/`.
    const EXPECTED: usize = 9;

    let src_files = collect_rs_files("src");
    assert!(!src_files.is_empty());

    let count = src_files
        .iter()
        .filter(|file| {
            let content = fs::read_to_string(file).unwrap_or_default();
            content.contains("NodeTree")
        })
        .count();

    assert_eq!(
        count, EXPECTED,
        "NodeTree import-site count drifted: expected {}, found {}. If this is \
         intentional, update EXPECTED in this test and document the new consumer \
         in the PR description.",
        EXPECTED, count
    );
}

// ── 7. TUI widgets consume AgentRowView, never the tree ─────────────────

/// Flag 14.1-G: the TUI is a render layer over `AgentRowView` snapshots. No
/// `.rs` file under `src/adapters/tui/` may name `NodeTree` or
/// `SubagentRegistry` — reaching into the tree couples a view widget to
/// infrastructure internals and breaks the hexagonal boundary.
#[test]
fn test_tui_does_not_hold_node_tree() {
    let tui_files = collect_rs_files("src/adapters/tui");
    assert!(
        !tui_files.is_empty(),
        "POSITIVE CONTROL FAILED: src/adapters/tui/ yielded no .rs files"
    );

    let forbidden = ["NodeTree", "SubagentRegistry"];
    let mut violations: Vec<String> = Vec::new();

    for file in &tui_files {
        let content = fs::read_to_string(file).unwrap_or_default();
        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            // Skip comments — doc references to `NodeTree` explaining why a
            // widget must NOT use it are permitted and useful.
            if trimmed.starts_with("// ")
                || trimmed.starts_with("/// ")
                || trimmed.starts_with("//! ")
            {
                continue;
            }
            for token in &forbidden {
                if trimmed.contains(token) {
                    violations.push(format!(
                        "{}:{}: token `{}` — {}",
                        file.display(),
                        line_no + 1,
                        token,
                        line
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "TUI files must consume `AgentRowView`, never the tree directly:\n{}",
        violations.join("\n")
    );
}

// ── 8. R1 keeps NodeCheckpoint persistence a no-op (V3) ─────────────────

/// Flag 14.1-H: `NodeCheckpoint` is a serialization *seam* only in R1 — it
/// round-trips in memory but NO production code may write it to disk. The
/// durability layer lands in a later story; until then a silent regression to
/// on-disk persistence would violate the R1 contract (V3 vacuous-test
/// mitigation). This guard pins the no-op.
#[test]
fn test_no_checkpoint_persistence_in_r1() {
    let src_files = collect_rs_files("src");
    assert!(!src_files.is_empty());

    const PERSIST_INDICATORS: &[&str] = &["fs::write", "File::create", "to_writer", "OpenOptions"];
    let mut violations: Vec<String> = Vec::new();

    for file in &src_files {
        let content = fs::read_to_string(file).unwrap_or_default();
        let prod = strip_test_mods(&content);
        for (line_no, line) in prod.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("// ") || trimmed.starts_with("/// ") {
                continue;
            }
            // Precise to the R1 type: only `NodeCheckpoint` (not unrelated
            // `CheckpointLog` / `persist_crash`) paired with a disk-write call.
            let touches_node_checkpoint = trimmed.contains("NodeCheckpoint");
            let persists = PERSIST_INDICATORS.iter().any(|ind| trimmed.contains(ind));
            if touches_node_checkpoint && persists {
                violations.push(format!("{}:{}: {}", file.display(), line_no + 1, line));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "R1 must keep NodeCheckpoint persistence a no-op (V3) — found:\n{}",
        violations.join("\n")
    );

    // POSITIVE CONTROL — the serializable seam itself must still exist.
    let agent_node_src =
        fs::read_to_string("src/domain/models/agent_node.rs").expect("agent_node.rs exists");
    assert!(
        agent_node_src.contains("pub struct NodeCheckpoint")
            && agent_node_src.contains("pub fn checkpoint"),
        "POSITIVE CONTROL FAILED: NodeCheckpoint / checkpoint() seam missing — \
         conformance target moved."
    );
}

// ── NFR69 — spawn / cancel latency ──────────────────────────────────────
//
// CI thresholds are 10× the NFR target as headroom for noisy CI runners:
//   spawn < 30 ms NFR / 300 ms CI ; cancel < 200 ms NFR / 2000 ms CI.
// If these flip red on a saturated runner, re-run locally first; a persistent
// regression in the median indicates a real allocator/lock regression in
// NodeTree and should not be silenced.

#[tokio::test]
async fn nfr69_spawn_latency() {
    // `Arc` is part of the canonical NFR69 harness signature even though
    // this test doesn't allocate one — kept for parity with the cancel
    // latency harness and any future spawn-through-Arc variant.
    #[allow(unused_imports)]
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, watch};

    async fn spawn_test_node(
        tree: &rustain::infrastructure::subagent::NodeTree,
    ) -> rustain::domain::models::AgentId {
        let agent_id = rustain::domain::models::AgentId::new();
        let (tx, _rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(rustain::domain::models::NodeState::Created);
        let (_metrics_tx, metrics_rx) =
            watch::channel(rustain::domain::models::AgentMetrics::default());
        let handle = rustain::infrastructure::subagent::AgentHandle {
            isolated: false,
            agent_id: agent_id.clone(),
            token: rustain::domain::models::CapabilityTokenId::nil(),
            command_tx: tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            subagent_type: "perf-test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: rustain::infrastructure::subagent::MailboxBudget::new(),
        };
        tree.register(
            agent_id.clone(),
            rustain::domain::models::AgentId::root(),
            handle,
        )
        .await
        .unwrap();
        agent_id
    }

    let tree = rustain::infrastructure::subagent::NodeTree::new();
    // Warmup — prime allocators and JIT-ish branch prediction.
    for _ in 0..5 {
        let id = spawn_test_node(&tree).await;
        tree.deregister(&id).await;
    }
    // Measure
    let mut times = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        let id = spawn_test_node(&tree).await;
        times.push(start.elapsed());
        tree.deregister(&id).await;
    }
    times.sort();
    assert!(
        times.len() > 94,
        "NFR69 sample underflow: expected >=95 measurements, got {}",
        times.len()
    );
    let p95 = times[94];
    assert!(
        p95 < Duration::from_millis(300),
        "p95 spawn latency {:?} exceeds CI threshold (300ms). NFR69 target is 30ms.",
        p95
    );
}

#[tokio::test]
async fn nfr69_cancel_latency() {
    // `Arc` would be needed if the reactor task captured shared tree state
    // by Arc; the current NodeTree is already cheaply cloneable so the
    // import is unused here. Kept for parity with nfr69_spawn_latency.
    #[allow(unused_imports)]
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, watch};

    let tree = rustain::infrastructure::subagent::NodeTree::new();
    // Warmup + measure cancel
    let mut times = Vec::with_capacity(100);
    for i in 0..105 {
        let agent_id = rustain::domain::models::AgentId::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(rustain::domain::models::NodeState::Created);
        let (_metrics_tx, metrics_rx) =
            watch::channel(rustain::domain::models::AgentMetrics::default());
        let handle = rustain::infrastructure::subagent::AgentHandle {
            isolated: false,
            agent_id: agent_id.clone(),
            token: rustain::domain::models::CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            subagent_type: "perf-test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: rustain::infrastructure::subagent::MailboxBudget::new(),
        };
        tree.register(
            agent_id.clone(),
            rustain::domain::models::AgentId::root(),
            handle,
        )
        .await
        .unwrap();

        // Spawn reactor that transitions to Killed on Op::Kill
        let tree_clone = tree.clone();
        let aid = agent_id.clone();
        tokio::spawn(async move {
            while let Some(op) = cmd_rx.recv().await {
                if matches!(op, rustain::domain::models::Op::Kill) {
                    if let Some(tx) = tree_clone.status_sender(&aid).await {
                        let _ = tx.send(rustain::domain::models::NodeState::Cancelled);
                    }
                    break;
                }
            }
        });

        let start = Instant::now();
        let _ = tree.cascade_kill(&agent_id, Duration::from_secs(5)).await;
        let elapsed = start.elapsed();

        if i >= 5 {
            times.push(elapsed);
        }
    }
    times.sort();
    assert!(
        times.len() > 94,
        "NFR69 sample underflow: expected >=95 measurements, got {}",
        times.len()
    );
    let p95 = times[94];
    assert!(
        p95 < Duration::from_millis(2000),
        "p95 cancel latency {:?} exceeds CI threshold (2000ms). NFR69 target is 200ms.",
        p95
    );
}
