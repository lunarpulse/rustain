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
    assert!(module.contains("pub mod config;"));
    assert!(!module.contains("#[cfg(feature = \"a2a\")]\npub mod config;"));
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
        "transparency",
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
