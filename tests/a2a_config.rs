use rustain::adapters::a2a::config::{
    extract_profile_a2a_peers, merge_a2a_specs, parse_workspace_a2a_config,
};
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, PinnedKey, PinnedKeyAlgorithm, RedactedUrl, TrustTier,
};

const ED25519_X: &str = "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w";

fn peer(id: &str, source: A2aPeerSource, pinned_key: Option<PinnedKey>) -> A2aPeerSpec {
    A2aPeerSpec {
        id: id.to_owned(),
        url: RedactedUrl::from(format!("https://{id}.example")),
        pinned_key,
        source,
    }
}

fn ed25519_pin() -> PinnedKey {
    PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        ED25519_X.to_owned(),
        Some("key-2026".to_owned()),
    )
}

#[test]
fn trust_tier_is_derived_only_from_the_configured_pin() {
    assert_eq!(
        peer("verified", A2aPeerSource::Workspace, Some(ed25519_pin())).trust_tier(),
        TrustTier::Verified
    );
    assert_eq!(
        peer("unverified", A2aPeerSource::Workspace, None).trust_tier(),
        TrustTier::Unverified
    );
}

#[test]
fn unsupported_pinned_algorithm_is_a_typed_actionable_error() {
    let error = PinnedKey::parse("ES256", ED25519_X.to_owned(), None)
        .expect_err("ES256 must not silently degrade to an unverified peer");

    assert_eq!(error.algorithm(), Some("ES256"));
    let message = error.to_string();
    assert!(message.contains("remove the pin"), "{message}");
    assert!(message.contains("DF-17-4a-2"), "{message}");
}

#[test]
fn peer_ids_reject_empty_and_double_underscore_names() {
    let empty = peer(" ", A2aPeerSource::Workspace, None)
        .validate_id()
        .expect_err("blank ids collide in capability names");
    assert!(empty.to_string().contains("empty"));

    let reserved = peer("east__scanner", A2aPeerSource::Workspace, None)
        .validate_id()
        .expect_err("double underscore is reserved by CapabilityId");
    assert!(reserved.to_string().contains("double-underscore"));
}

#[test]
fn workspace_config_parses_agents_from_rustain_a2a_json() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let config_dir = workspace.path().join(".rustain");
    std::fs::create_dir_all(&config_dir).expect("create .rustain");
    let path = config_dir.join("a2a.json");
    std::fs::write(
        &path,
        format!(
            r#"{{
  "agents": {{
    "ci-runner": {{
      "url": "https://ci.example",
      "pinnedKey": {{ "alg": "EdDSA", "x": "{ED25519_X}", "kid": "ci-1" }}
    }},
    "unsigned-search": {{ "url": "https://search.example" }}
  }}
}}"#,
        ),
    )
    .expect("write workspace config");

    let peers = parse_workspace_a2a_config(&path).expect("valid workspace config");
    assert_eq!(peers.len(), 2);
    assert_eq!(peers[0].id, "ci-runner");
    assert_eq!(peers[0].trust_tier(), TrustTier::Verified);
    assert_eq!(peers[0].source, A2aPeerSource::Workspace);
    assert_eq!(peers[1].id, "unsigned-search");
    assert_eq!(peers[1].trust_tier(), TrustTier::Unverified);
}

#[test]
fn profile_config_parses_a2a_peer_table() {
    let value: toml::Value = toml::from_str(&format!(
        r#"
[a2a.profile-peer]
url = "https://profile.example"

[a2a.profile-peer.pinned_key]
alg = "EdDSA"
x = "{ED25519_X}"
kid = "profile-1"
"#,
    ))
    .expect("valid TOML fixture");

    let peers =
        extract_profile_a2a_peers(Some(&value), "coding").expect("valid profile A2A config");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, "profile-peer");
    assert_eq!(peers[0].trust_tier(), TrustTier::Verified);
    assert_eq!(
        peers[0].source,
        A2aPeerSource::Profile {
            profile_name: "coding".to_owned()
        }
    );
}

#[test]
fn present_non_table_profile_a2a_config_fails_loud() {
    let value: toml::Value = toml::from_str(r#"a2a = "not-a-table""#).unwrap();
    let error = extract_profile_a2a_peers(Some(&value), "coding")
        .expect_err("configured malformed A2A must not disappear");
    assert!(error.to_string().contains("a2a must be a TOML table"));
}

#[test]
fn workspace_peer_wins_over_profile_peer_with_the_same_id() {
    let workspace = peer("shared", A2aPeerSource::Workspace, None);
    let profile = peer(
        "shared",
        A2aPeerSource::Profile {
            profile_name: "coding".to_owned(),
        },
        Some(ed25519_pin()),
    );

    let merged = merge_a2a_specs(vec![workspace.clone()], vec![profile]);
    assert_eq!(merged, vec![workspace]);
}

#[test]
#[serial_test::serial]
fn profile_resolver_loads_workspace_rustain_a2a_config() {
    use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
    use rustain::domain::ports::ProfileResolver;

    let workspace = tempfile::tempdir().expect("temporary workspace");
    let config_dir = workspace.path().join(".rustain");
    std::fs::create_dir_all(&config_dir).expect("create .rustain");
    std::fs::write(
        config_dir.join("a2a.json"),
        r#"{"agents":{"workspace-peer":{"url":"https://peer.example"}}}"#,
    )
    .expect("write A2A config");

    let profiles = tempfile::tempdir().expect("temporary profile directory");
    let original_dir = std::env::current_dir().expect("current directory");
    std::env::set_current_dir(workspace.path()).expect("enter workspace");
    let _restore_dir = scopeguard::guard(original_dir, |path| {
        std::env::set_current_dir(path).expect("restore current directory");
    });

    let resolver = TomlProfileResolver::new("coding", profiles.path().to_path_buf())
        .expect("resolve embedded coding profile");
    let resolved = resolver.resolve_active().expect("active profile");
    assert_eq!(resolved.a2a_peers.len(), 1);
    assert_eq!(resolved.a2a_peers[0].id, "workspace-peer");
    assert_eq!(resolved.a2a_peers[0].trust_tier(), TrustTier::Unverified);
}
