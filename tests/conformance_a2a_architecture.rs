use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

#[test]
fn a2a_domain_types_are_wire_and_transport_free() {
    let domain = source("src/domain/models/a2a_peer_spec.rs");
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
            "pure A2A domain model contains forbidden dependency {forbidden:?}"
        );
    }
    assert!(domain.contains("RedactedUrl"));
    assert!(domain.contains("fn trust_tier(&self) -> TrustTier"));
}

#[test]
fn a2a_feature_is_off_by_default_and_declares_the_documented_fallback() {
    let manifest: toml::Value = toml::from_str(&source("Cargo.toml")).unwrap();
    let features = manifest["features"].as_table().unwrap();
    let defaults: Vec<_> = features["default"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(!defaults.contains(&"a2a"));

    let a2a: BTreeSet<_> = features["a2a"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        a2a,
        BTreeSet::from([
            "dep:axum",
            "dep:reqwest",
            "dep:rustls",
            "dep:rustls-pemfile",
            "dep:serde_jcs",
            "dep:subtle",
            "dep:tokio-rustls",
            "mcp",
        ]),
        "DF-17-4a-3 fallback must remain explicit until composite gating is separated. \
         Story 18.1b adds the server-side TLS stack (rustls/tokio-rustls/rustls-pemfile) \
         and the constant-time credential comparison (subtle) — all `a2a`-gated, none \
         reachable from a default build."
    );
    for dependency in [
        "reqwest",
        "serde_jcs",
        "axum",
        "rustls",
        "tokio-rustls",
        "rustls-pemfile",
        "subtle",
    ] {
        assert_eq!(
            manifest["dependencies"][dependency]["optional"].as_bool(),
            Some(true),
            "{dependency} must stay optional so a default build links none of it"
        );
    }
    assert!(
        !source("Cargo.toml").contains("a2a-lf"),
        "R11 fired the a2a-lf drop-trigger"
    );
}

#[test]
fn only_config_parsing_compiles_without_the_a2a_feature() {
    let module = source("src/adapters/a2a/mod.rs");

    // Two modules are deliberately ungated, and each has a production consumer
    // that would not compile otherwise.
    //
    // `config`: startup must reject configured peers loudly rather than silently
    // omit them (this test's original subject).
    //
    // `transparency`: Story 18.3 made `TransparencySink` the mandatory recorder for
    // every live verified peer frame, and the RAP path that holds it is ungated core
    // (ADR-18-3-01 D3 — when two adapters must meet, they meet at a domain port).
    // It is named ungated at `infrastructure/startup.rs:978` and
    // `adapters/daemon/mod.rs:486,646`. 18.3 widened the feature-off surface here and
    // left this list unchanged, so the list — not the module — was stale; it failed
    // on disk at `530dbd2` and is corrected by Story 18.3b rather than relabelled.
    for ungated in ["config", "transparency"] {
        assert!(
            module.contains(&format!("pub mod {ungated};")),
            "{ungated} must exist"
        );
        assert!(
            !module.contains(&format!("#[cfg(feature = \"a2a\")]\npub mod {ungated};")),
            "{ungated} must stay UNGATED — it has ungated production consumers, and gating it \
             would break the feature-off daemon build"
        );
    }

    for adapter in [
        "admission",
        "auth",
        "card",
        "card_cache",
        "client",
        "error",
        "exec",
        "jws",
        "projection",
        "provider",
        "server",
        "tls",
    ] {
        assert!(
            module.contains(&format!("#[cfg(feature = \"a2a\")]\npub mod {adapter};")),
            "{adapter} must be feature-gated"
        );
    }
}

#[test]
fn concrete_a2a_client_and_provider_are_named_only_at_the_startup_boundary() {
    let src = root().join("src");
    let mut references = BTreeSet::new();
    visit_rs(&src, &mut |path, text| {
        if text.contains("adapters::a2a::client::A2aClientAdapter")
            || text.contains("adapters::a2a::provider::A2aProvider")
        {
            references.insert(path.strip_prefix(&src).unwrap().to_path_buf());
        }
    });
    assert_eq!(
        references,
        BTreeSet::from([PathBuf::from("infrastructure/startup.rs")])
    );
}

#[test]
fn every_public_a2a_enum_is_non_exhaustive() {
    let files = [
        "src/adapters/a2a/admission.rs",
        "src/adapters/a2a/auth.rs",
        "src/adapters/a2a/config.rs",
        "src/adapters/a2a/error.rs",
        "src/adapters/a2a/exec.rs",
        "src/adapters/a2a/projection.rs",
        "src/adapters/a2a/transparency.rs",
        "src/domain/models/a2a_peer_spec.rs",
    ];
    let mut enums = BTreeSet::new();
    for file in files {
        let text = source(file);
        let lines: Vec<_> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("pub enum ") else {
                continue;
            };
            let name = rest.split_whitespace().next().unwrap();
            let attributes = lines[index.saturating_sub(4)..index].join("\n");
            assert!(
                attributes.contains("#[non_exhaustive]"),
                "public enum {name} in {file} must be #[non_exhaustive]"
            );
            enums.insert(name.to_owned());
        }
    }
    assert_eq!(
        enums,
        BTreeSet::from([
            "A2aAdmissionPolicy".to_owned(),
            "A2aConfigError".to_owned(),
            "A2aError".to_owned(),
            "A2aPeerSource".to_owned(),
            "A2aPeerSpecError".to_owned(),
            "A2aServerAuth".to_owned(),
            "AdmissionVerdict".to_owned(),
            "AuthOutcome".to_owned(),
            "BindDecision".to_owned(),
            "Disclosure".to_owned(),
            "InboundOutcome".to_owned(),
            "PendingAuth".to_owned(),
            "PinnedKeyAlgorithm".to_owned(),
            "SubmitterTrust".to_owned(),
            "TrustTier".to_owned(),
        ])
    );
}

#[test]
fn signature_verification_remains_a_pure_raw_payload_function() {
    let jws = source("src/adapters/a2a/jws.rs");
    assert!(jws.contains(
        "pub fn verify_card(raw_bytes: &str, pinned: &PinnedKey) -> Result<(), A2aError>"
    ));
    assert!(!jws.contains("async fn verify_card"));
    assert!(!jws.contains("reqwest"));
    assert!(jws.contains("serde_json::from_str(raw_bytes)"));
    assert!(jws.contains("verify_strict"));
}

fn visit_rs(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, visit);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            visit(&path, &text);
        }
    }
}

/// ADR-17-CC-05: `transparency.jsonl` is a **regenerable export**, never a
/// second source of truth.
///
/// Structural, not behavioural, because the failure mode is a *future* edit:
/// someone adds a read of the export file "just for the panel" and the product
/// quietly grows a second log to keep consistent — the one that drifts is the
/// one nobody is looking at. The only permitted mentions are the path helper
/// that mints it, the export shell that writes it, and documentation.
#[test]
fn no_code_path_reads_the_transparency_export() {
    const WRITER: &str = "src/infrastructure/transparency.rs";
    const PATH_OWNER: &str = "src/infrastructure/paths.rs";

    let mut mentions: Vec<String> = Vec::new();
    visit_rs(&root().join("src"), &mut |path, text| {
        let relative = path
            .strip_prefix(root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == WRITER || relative == PATH_OWNER {
            return;
        }
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Doc and ordinary comments are how the invariant is explained;
            // they are not a read.
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains("transparency.jsonl") {
                mentions.push(format!("{relative}:{}", index + 1));
            }
        }
    });
    assert!(
        mentions.is_empty(),
        "`transparency.jsonl` may only be named by the export shell ({WRITER}) and the path \
         helper ({PATH_OWNER}). Every transparency fact is read from the room journal.\n\
         Offending sites:\n{}",
        mentions.join("\n")
    );
}

/// The export path is owned by `infrastructure::paths`, which declares itself
/// the single source of truth for paths. An inline `join(".rustain")` is how a
/// second, subtly different location gets created.
#[test]
fn the_transparency_export_path_is_minted_only_by_the_paths_module() {
    let paths = source("src/infrastructure/paths.rs");
    assert!(
        paths.contains("pub fn transparency_export_path("),
        "the export path helper must live in infrastructure::paths"
    );
    let writer = source("src/infrastructure/transparency.rs");
    assert!(
        writer.contains("paths::transparency_export_path"),
        "the export shell must resolve its path through the paths module"
    );
    assert!(
        !writer.contains("join(\".rustain\")"),
        "the export shell must not inline the workspace runtime directory"
    );
}
