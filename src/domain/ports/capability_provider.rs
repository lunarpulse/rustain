use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::models::ToolResult;
use crate::domain::models::capability::Capability;
use crate::domain::models::capability::CapabilityError;
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::provider_capabilities::ProviderCapabilities;

/// Contract for a pluggable capability provider.
///
/// # Lifecycle
///
/// `discover → register → invoke → render`
///
/// Implementations are discovered via `discover()`, registered into the
/// `CapabilityRegistry` by `CompositeToolsetAdapter`, and invoked via
/// `invoke()` when the LLM requests a tool call.
///
/// # Provider types
///
/// | Protocol | Implementation | Story |
/// |----------|---------------|-------|
/// | `"mcp"` | `McpProvider` (wraps `McpClientAdapter`) | 9.3a |
/// | `"builtin"` | `BuiltinProvider` (refactor of `ToolSetAdapter`) | 9.3b |
/// | `"skill"` | `SkillsProvider` (refactor of skill executor) | 9.3b |
/// | `"a2a"` | `A2aProvider` | Epic 14 |
/// | `"subagent"` | `SubagentProvider` | Epic 10 |
///
/// # Design decisions
///
/// - **4 methods, not 5:** No `activate()` step. MCP + builtin providers have no
///   activation step; SkillsProvider in 9.3b will delegate to `SkillActivator`
///   (existing) rather than expose activation on the trait. (Decision Gate 3.1)
/// - **No `PermissionChain` integration:** The trait focuses on capabilities,
///   not authorization. Permission checks live in `ToolScheduler` and use the
///   existing `permission_chain` module.
///
/// # Related
///
/// - FR48: "Capability Provider Architecture (CPA) trait and MCP registration"
/// - `src/domain/models/capability_registry.rs` — the registry that holds capabilities
/// - `src/adapters/mcp/mcp_provider.rs` — the MCP implementation
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Stable protocol identifier — used by `CapabilityId` namespace (AC-3).
    ///
    /// Returns one of: `"mcp"`, `"builtin"`, `"skill"`, `"a2a"`, `"subagent"`.
    fn protocol(&self) -> &str;

    /// Provider's static feature support (AC-9-3a-6 export).
    ///
    /// Pattern-matched by Story 9.4 Phase A capability matrix at session handshake.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Discover capabilities currently exposed by this provider.
    ///
    /// For `McpProvider`: reads `McpClientAdapter::cached_tools()` and projects
    /// per ADR-06-08. Pure read of in-memory state; NO I/O on the hot path
    /// (the actual MCP `tools/list` network call is owned by
    /// `McpClientAdapter::refresh_cached_tools` per Story 9.2 AC-6).
    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError>;

    /// Invoke a capability with input and cancellation token.
    ///
    /// Returns the same `ToolResult` shape that `ToolSetPort::execute` already
    /// produces (zero conversion overhead — 9.3a uses domain `ToolResult` directly
    /// rather than introducing a parallel result type).
    async fn invoke(
        &self,
        capability_id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError>;
}

#[cfg(test)]
mod a2a_conformance {
    #[test]
    fn a2a_domain_model_remains_transport_and_wire_free() {
        let domain = include_str!("../models/a2a_peer_spec.rs");
        for forbidden in [
            "reqwest",
            "axum",
            "JsonRpc",
            "serde_jcs",
            "ed25519_dalek",
            "base64::",
            "AgentCardView",
            "A2aClientAdapter",
        ] {
            assert!(
                !domain.contains(forbidden),
                "A2A domain model contains forbidden dependency {forbidden:?}"
            );
        }
    }

    #[test]
    fn a2a_cpa_slot_is_filled_and_composed() {
        let provider = include_str!("../../adapters/a2a/provider.rs");
        assert!(provider.contains("impl CapabilityProvider for A2aProvider"));
        assert!(provider.contains("fn protocol(&self) -> &str"));
        assert!(provider.contains("\"a2a\""));
        assert!(provider.contains("17.4b"));

        let composite = include_str!("../../adapters/composite_toolset_adapter.rs");
        assert!(composite.contains("a2a_provider: std::sync::OnceLock"));
        assert!(composite.contains("pub fn set_a2a_provider"));
        assert!(composite.contains("discover_and_register_all(a2a_provider.as_ref(), \"a2a\")"));
    }

    #[test]
    fn a2a_wire_types_absent_from_entire_src_domain() {
        // AC4 [K], CI-enforced. The full-domain grep guard in tests/conformance.rs still
        // never executes in CI — the Check job runs `cargo test --lib`, and the `a2a`
        // lane (Story 17.4b) runs only the A2A integration targets, not the general
        // conformance suite. So replicate the whole-`src/domain` scan here. Scans
        // `use`/`extern` lines only — comments and doc mentions (e.g. the `reqwest`
        // reference in errors.rs) are fine. ed25519_dalek and base64 are deliberately
        // NOT forbidden: pure crypto is allowed in domain (capability_token.rs,
        // authority_ledger.rs). Forbidden set is exactly AC4's: A2A adapter/wire
        // types plus reqwest/serde_jcs.
        const FORBIDDEN: [&str; 7] = [
            "reqwest",
            "serde_jcs",
            "axum",
            "adapters::a2a",
            "AgentCardView",
            "A2aClientAdapter",
            "JsonRpc",
        ];
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let domain = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
        let mut files = Vec::new();
        collect_rs(&domain, &mut files);
        assert!(!files.is_empty(), "no .rs files found under src/domain");

        let mut violations = Vec::new();
        for file in &files {
            let content = std::fs::read_to_string(file).unwrap_or_default();
            for (idx, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if !(trimmed.starts_with("use ") || trimmed.starts_with("extern crate")) {
                    continue;
                }
                for token in FORBIDDEN {
                    if trimmed.contains(token) {
                        violations.push(format!(
                            "{}:{} — forbidden A2A/transport import `{token}`: {trimmed}",
                            file.display(),
                            idx + 1
                        ));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "A2A wire types must not appear in src/domain:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn every_a2a_integration_test_is_wired_into_the_ci_a2a_lane() {
        // Story 17.4b standing ruling. Before the `a2a` lane existed, all nine
        // tests/a2a_*.rs files compiled but NEVER executed in CI (the Check job runs
        // `cargo test --lib` only) — nine test files that were a false green.
        //
        // The lane names its targets explicitly (`--test <name>`), which is precise but
        // would silently rot the moment someone adds a tenth file. This guard makes that
        // rot impossible: it runs under `cargo test --lib`, so it fails in the DEFAULT
        // lane even for a contributor who never builds `--features a2a`.
        //
        // Adding a new A2A integration test? Add `--test <name>` to the `a2a` job in
        // .github/workflows/ci.yml. That is the whole contract.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let mut targets = Vec::new();
        for entry in std::fs::read_dir(root.join("tests"))
            .expect("tests/ must be readable")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && stem.contains("a2a")
            {
                targets.push(stem.to_owned());
            }
        }
        targets.sort();
        assert!(
            !targets.is_empty(),
            "expected A2A integration tests under tests/"
        );

        let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect(".github/workflows/ci.yml must be readable");

        let missing: Vec<&String> = targets
            .iter()
            .filter(|stem| !ci.contains(&format!("--test {stem}")))
            .collect();

        assert!(
            missing.is_empty(),
            "A2A integration tests exist that the CI `a2a` lane never runs — a test that \
             never executes is a false green. Add `--test <name>` to the `a2a` job in \
             .github/workflows/ci.yml for: {missing:?}"
        );
    }

    /// Story 18.1b, R1 — the async-lock ratchet was never executed by CI.
    ///
    /// `MAX_KNOWN_STD_SYNC_LOCKS` lives in `tests/conformance.rs`, and until this
    /// story **no workflow ran `--test conformance`**: the Check job runs
    /// `cargo test --lib`, and every feature lane names its targets explicitly.
    /// The policy the whole codebase is held to was a manual-only gate, and an
    /// untagged `std::sync::Mutex` in `adapters/` or `infrastructure/` could
    /// merge with all lanes green.
    ///
    /// This story adds a task map and per-request server state, so it owns the
    /// subject matter and closes the gap. The guard runs under `cargo test --lib`
    /// so it fails the DEFAULT lane if someone removes the wiring.
    #[test]
    fn the_async_lock_ratchet_is_executed_by_ci() {
        let ci = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml"),
        )
        .expect(".github/workflows/ci.yml must be readable");
        assert!(
            ci.contains("--test conformance\n") || ci.contains("--test conformance "),
            "no CI job runs `--test conformance`, so MAX_KNOWN_STD_SYNC_LOCKS is a \
             manual-only gate again and an untagged std::sync lock can merge green"
        );
    }

    /// Story 18.1b — the zero-mutation ratchet (AC2b) and the card-signing
    /// counter (AC7b) are both `#[cfg(any(test, feature = "test-instrumentation"))]`.
    /// In an *integration* target the `test` cfg applies to the test crate, not to
    /// the library, so without the feature those keystones silently degrade to
    /// their behavioural halves — green, and no longer proving the structural
    /// invariant they exist for.
    #[test]
    fn the_ci_a2a_lane_enables_the_zero_mutation_ratchet() {
        let ci = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml"),
        )
        .expect(".github/workflows/ci.yml must be readable");
        assert!(
            ci.contains("--features a2a,test-instrumentation"),
            "the CI a2a lane must run with `test-instrumentation`, or AC2b's \
             zero-mutation ratchet and AC7b's signing counter are compiled out"
        );
    }

    #[test]
    fn every_mcp_integration_test_is_wired_into_the_ci_mcp_lane() {
        // Story 17.5a. The default CI job runs only `cargo test --lib`, which
        // leaves integration targets false-green unless the MCP lane names them.
        //
        // Select by observable MCP behavior, not filename: a target either
        // exercises production adapter/client code, starts the in-tree fake
        // server with an MCP spec, or verifies MCP's canonical approval
        // namespace. The narrow exclusions avoid unrelated tests that merely
        // inspect the MCP error type or mention the fake in a comment.
        //
        // Adding a behavioral MCP target? Add `--test <name>` to the `mcp` job in
        // .github/workflows/ci.yml. This guard runs under `cargo test --lib`, so
        // the omission fails in the default lane too.
        fn exercises_mcp_surface(content: &str) -> bool {
            let exercises_adapter = content.contains("rustain::adapters::mcp::")
                && !content.contains("rustain::adapters::mcp::error::McpError");
            let spawns_fake =
                content.contains("fake-mcp-server") && content.contains("McpServerSpec");
            let verifies_approval_namespace =
                content.contains("mcp__") && content.contains("ApprovalSource");

            exercises_adapter
                || content.contains("McpClientAdapter")
                || spawns_fake
                || verifies_approval_namespace
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut targets = Vec::new();
        for entry in std::fs::read_dir(root.join("tests"))
            .expect("tests/ must be readable")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && exercises_mcp_surface(
                    &std::fs::read_to_string(&path).unwrap_or_else(|error| {
                        panic!("{} must be readable: {error}", path.display())
                    }),
                )
            {
                targets.push(stem.to_owned());
            }
        }
        targets.sort();
        assert!(
            !targets.is_empty(),
            "expected behavioral MCP integration tests under tests/"
        );

        let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect(".github/workflows/ci.yml must be readable");
        let mut in_mcp_lane = false;
        let mut ci_targets = Vec::new();
        for line in ci.lines() {
            if line == "  mcp:" {
                in_mcp_lane = true;
                continue;
            }
            if in_mcp_lane && line.starts_with("  ") && !line.starts_with("    ") {
                break;
            }
            if in_mcp_lane
                && let Some(target) = line
                    .trim_start()
                    .strip_prefix("--test ")
                    .and_then(|rest| rest.split_whitespace().next())
            {
                ci_targets.push(target);
            }
        }
        assert!(
            in_mcp_lane,
            "expected an `mcp` job in .github/workflows/ci.yml"
        );

        // Parsing complete `--test` arguments makes `conformance_mcp` distinct
        // from `conformance_mcp_config`; a substring check would not.
        let missing: Vec<&str> = targets
            .iter()
            .map(String::as_str)
            .filter(|stem| !ci_targets.iter().any(|target| target == stem))
            .collect();

        assert!(
            missing.is_empty(),
            "MCP integration tests exist that the CI `mcp` lane never runs — a test that \
             never executes is a false green. Add `--test <name>` to the `mcp` job in \
             .github/workflows/ci.yml for: {missing:?}"
        );
    }

    #[test]
    fn a2a_adapter_never_calls_the_silent_set_state_shim() {
        // Story 17.4b (R-E), CI-enforced in the DEFAULT lane (runs under
        // `cargo test --lib`). The node tree's `set_state` returns `()` and silently
        // swallows illegal FSM edges (`Suspended -> Completed` stays `Suspended`
        // forever with only a `tracing::warn!`). The A2A path MUST use
        // `try_set_state -> Result` exclusively so every illegal edge fails loudly.
        // A convention is a false-green with a nice haircut; this is the guard.
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let a2a = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/a2a");
        let mut files = Vec::new();
        collect_rs(&a2a, &mut files);
        assert!(
            !files.is_empty(),
            "no .rs files found under src/adapters/a2a"
        );

        let mut violations = Vec::new();
        for file in &files {
            let content = std::fs::read_to_string(file).unwrap_or_default();
            for (idx, line) in content.lines().enumerate() {
                // Match the method call `.set_state(` but not `.try_set_state(`.
                if let Some(pos) = line.find(".set_state(") {
                    let is_try = pos >= 4 && line[..pos].ends_with(".try");
                    if !is_try {
                        violations.push(format!("{}:{}: {}", file.display(), idx + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "the A2A path must use try_set_state, never the silent set_state shim:\n{}",
            violations.join("\n")
        );
    }
}
