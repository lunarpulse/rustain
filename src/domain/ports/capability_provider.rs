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

    // ── Integration-test wiring (Grand Plan P0-3, docs/rustain-grand-plan-2026-10-22.md §4) ──
    //
    // Integration tests are explicit area binaries (`tests/<area>/main.rs`) that load
    // the former `tests/*.rs` roots via `#[path]`; `autotests = false` keeps cargo from
    // compiling each root a second time as its own target. The two guards below
    // replace the retired per-lane name lists (a2a / p2p / mcp): a root that no
    // binary names never compiles, and a gated root whose feature no CI invocation
    // enables never runs — both the false green those lists existed to prevent.
    // They run under `cargo test --lib`, so they fail the DEFAULT lane.

    struct TestTarget {
        name: String,
        path: String,
    }

    fn declared_test_targets(manifest: &str) -> Vec<TestTarget> {
        let mut targets = Vec::new();
        let mut current: Option<(Option<String>, Option<String>)> = None;
        for line in manifest.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                if let Some((Some(name), Some(path))) = current.take() {
                    targets.push(TestTarget { name, path });
                }
                current = (line == "[[test]]").then_some((None, None));
                continue;
            }
            let Some(entry) = current.as_mut() else {
                continue;
            };
            let value = |key: &str| {
                line.strip_prefix(key)
                    .map(str::trim_start)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(|rest| rest.trim().trim_matches('"').to_owned())
            };
            if let Some(name) = value("name") {
                entry.0 = Some(name);
            } else if let Some(path) = value("path") {
                entry.1 = Some(path);
            }
        }
        if let Some((Some(name), Some(path))) = current {
            targets.push(TestTarget { name, path });
        }
        targets
    }

    /// `#[path = "../<stem>.rs"]` module references of an area binary.
    fn area_roots(main_rs: &str) -> Vec<String> {
        main_rs
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("#[path = \"../")
                    .and_then(|rest| rest.strip_suffix(".rs\"]"))
            })
            .filter(|stem| !stem.contains('/'))
            .map(str::to_owned)
            .collect()
    }

    /// root stem -> names of the `[[test]]` targets that compile it.
    fn root_owners(
        root: &std::path::Path,
        manifest: &str,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut owners: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for target in declared_test_targets(manifest) {
            if let Some(stem) = target
                .path
                .strip_prefix("tests/")
                .and_then(|rest| rest.strip_suffix(".rs"))
                && !stem.contains('/')
            {
                owners
                    .entry(stem.to_owned())
                    .or_default()
                    .push(target.name.clone());
            } else if target.path.ends_with("/main.rs") {
                let main_rs = std::fs::read_to_string(root.join(&target.path))
                    .unwrap_or_else(|error| panic!("{} must be readable: {error}", target.path));
                for stem in area_roots(&main_rs) {
                    assert!(
                        root.join("tests").join(format!("{stem}.rs")).is_file(),
                        "{} names tests/{stem}.rs, which does not exist",
                        target.path
                    );
                    owners.entry(stem).or_default().push(target.name.clone());
                }
            }
        }
        owners
    }

    #[test]
    fn every_integration_test_root_is_owned_by_one_area_binary() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
        assert!(
            manifest.contains("\nautotests = false\n"),
            "Cargo.toml must keep `autotests = false`: with autodiscovery every tests/*.rs \
             root is compiled twice — once in its area binary and once as its own target"
        );
        let owners = root_owners(root, &manifest);

        let mut unowned = Vec::new();
        let mut shared = Vec::new();
        for entry in std::fs::read_dir(root.join("tests"))
            .expect("tests/")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                match owners.get(stem) {
                    None => unowned.push(stem.to_owned()),
                    Some(names) if names.len() > 1 => shared.push(format!("{stem} <- {names:?}")),
                    Some(_) => {}
                }
            }
        }
        assert!(
            unowned.is_empty(),
            "tests/*.rs roots that no area binary compiles (add one `#[path = \"../<stem>.rs\"] \
             mod <stem>;` to exactly one tests/<area>/main.rs, or a [[test]] entry): {unowned:?}"
        );
        assert!(
            shared.is_empty(),
            "tests/*.rs roots compiled by more than one binary (their tests would run twice): \
             {shared:?}"
        );
    }

    struct CiInvocation {
        features: std::collections::BTreeSet<String>,
        no_default_features: bool,
        /// `None` = `--all-targets`; otherwise the `--test` names (empty for `--lib`).
        targets: Option<std::collections::BTreeSet<String>>,
    }

    /// Every `cargo test` invocation in ci.yml, with `\` continuations joined.
    fn ci_test_invocations(ci: &str) -> Vec<CiInvocation> {
        let mut joined = Vec::new();
        let mut pending = String::new();
        for line in ci.lines() {
            let line = line.trim();
            if line.starts_with('#') && pending.is_empty() {
                continue;
            }
            if let Some(head) = line.strip_suffix('\\') {
                pending.push_str(head);
                pending.push(' ');
                continue;
            }
            pending.push_str(line);
            joined.push(std::mem::take(&mut pending));
        }
        joined
            .iter()
            .filter(|command| command.contains("cargo test") && !command.contains("--no-run"))
            .map(|command| {
                let words: Vec<&str> = command.split_whitespace().collect();
                let mut invocation = CiInvocation {
                    features: Default::default(),
                    no_default_features: false,
                    targets: Some(Default::default()),
                };
                let mut index = 0;
                while index < words.len() {
                    let word = words[index];
                    if word == "--" {
                        break;
                    }
                    let feature_list =
                        word.strip_prefix("--features=")
                            .map(str::to_owned)
                            .or_else(|| {
                                (word == "--features").then(|| {
                                    index += 1;
                                    words.get(index).copied().unwrap_or_default().to_owned()
                                })
                            });
                    if let Some(list) = feature_list {
                        invocation.features.extend(
                            list.split([',', ' '])
                                .filter(|f| !f.is_empty())
                                .map(str::to_owned),
                        );
                    } else if word == "--no-default-features" {
                        invocation.no_default_features = true;
                    } else if word == "--all-targets" {
                        invocation.targets = None;
                    } else if word == "--test" {
                        index += 1;
                        if let (Some(targets), Some(name)) =
                            (invocation.targets.as_mut(), words.get(index))
                        {
                            targets.insert((*name).to_owned());
                        }
                    }
                    index += 1;
                }
                invocation
            })
            .collect()
    }

    /// `feature = "x"` names in a root's leading `#![cfg(...)]` attribute, and whether
    /// the predicate is an `any(...)` (one suffices) or a conjunction (all required).
    fn root_feature_requirement(source: &str) -> Option<(Vec<String>, bool)> {
        let cfg = source
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("#![cfg("))?;
        let any = cfg.starts_with("#![cfg(any(");
        let features: Vec<String> = cfg
            .split("feature = \"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .map(str::to_owned)
            .collect();
        (!features.is_empty()).then_some((features, any))
    }

    fn default_features(manifest: &str) -> std::collections::BTreeSet<String> {
        let features_section = manifest.split("\n[features]\n").nth(1).unwrap_or_default();
        let default = features_section
            .split("default = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .unwrap_or_default();
        default
            .split(',')
            .map(|item| item.trim().trim_matches('"').to_owned())
            .filter(|item| !item.is_empty())
            .collect()
    }

    #[test]
    fn every_integration_test_root_runs_in_a_ci_lane_with_its_features() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
        let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect(".github/workflows/ci.yml must be readable");
        let invocations = ci_test_invocations(&ci);
        assert!(
            !invocations.is_empty(),
            "no `cargo test` invocation found in ci.yml"
        );
        let defaults = default_features(&manifest);

        let mut unexecuted = Vec::new();
        for (stem, owners) in root_owners(root, &manifest) {
            let source = std::fs::read_to_string(root.join("tests").join(format!("{stem}.rs")))
                .unwrap_or_else(|error| panic!("tests/{stem}.rs must be readable: {error}"));
            let requirement = root_feature_requirement(&source);
            let executed = invocations.iter().any(|invocation| {
                let runs_binary = invocation
                    .targets
                    .as_ref()
                    .is_none_or(|targets| owners.iter().any(|owner| targets.contains(owner)));
                let enabled = |feature: &String| {
                    invocation.features.contains(feature)
                        || (!invocation.no_default_features && defaults.contains(feature))
                };
                let features_satisfied = match &requirement {
                    None => true,
                    Some((features, true)) => features.iter().any(enabled),
                    Some((features, false)) => features.iter().all(enabled),
                };
                runs_binary && features_satisfied
            });
            if !executed {
                unexecuted.push(format!("{stem} (binary {owners:?}, needs {requirement:?})"));
            }
        }
        assert!(
            unexecuted.is_empty(),
            "integration roots that no ci.yml `cargo test` invocation executes with their \
             features enabled — a test that never runs is a false green:\n{}",
            unexecuted.join("\n")
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
