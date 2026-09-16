use rustain::adapters::a2a::config::{
    extract_profile_a2a_peers, merge_a2a_specs, parse_workspace_a2a_config,
};
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, PinnedKey, PinnedKeyAlgorithm, RedactedUrl, TrustTier,
};

const ED25519_X: &str = "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w";

fn peer(id: &str, source: A2aPeerSource, pinned_key: Option<PinnedKey>) -> A2aPeerSpec {
    A2aPeerSpec::new(
        id,
        RedactedUrl::from(format!("https://{id}.example")),
        source,
    )
    .with_pinned_key(pinned_key)
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

// ── Story 19.14 `AC1` — the roster carries a credential NAME and a trust anchor,
//    additively ───────────────────────────────────────────────────────────────
//
// Front door: the real parsers reached as production reads them —
// `TomlProfileResolver::new` → `parse_workspace_a2a_config` /
// `extract_profile_a2a_peers` → `merge_a2a_specs` → `resolve_ca_cert_paths`.
// ⛔ Forbidden bypass: constructing an `A2aPeerSpec` literal and asserting its
// fields. ⛔ A parser test that never reaches the merge does not satisfy Rule 1.
//
// ⚠ Every test below changes the process cwd (that is where the roster root comes
// from), so each carries `#[serial]` and restores cwd with a `scopeguard`.

/// Resolve the active profile with `root` as the process cwd — production's own
/// path to the roster.
fn resolve_peers_from(root: &std::path::Path) -> Vec<A2aPeerSpec> {
    use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
    use rustain::domain::ports::ProfileResolver;

    let profiles = tempfile::tempdir().expect("temporary profile directory");
    let original_dir = std::env::current_dir().expect("current directory");
    std::env::set_current_dir(root).expect("enter workspace");
    let _restore_dir = scopeguard::guard(original_dir, |path| {
        std::env::set_current_dir(path).expect("restore current directory");
    });

    let resolver = TomlProfileResolver::new("coding", profiles.path().to_path_buf())
        .expect("a roster with auth/caCert must load, exported or not, present or not");
    resolver
        .resolve_active()
        .expect("active profile")
        .a2a_peers
        .clone()
}

fn write_roster(root: &std::path::Path, json: &str) {
    let config_dir = root.join(".rustain");
    std::fs::create_dir_all(&config_dir).expect("create .rustain");
    std::fs::write(config_dir.join("a2a.json"), json).expect("write A2A config");
}

/// `AC1(a)(b)(c)`: both fields parse, ⛔ neither is dereferenced, and a relative
/// `caCert` is joined to the root that located `a2a.json` — ⛔ not to
/// `<root>/.rustain/`, which is where the file itself lives.
#[test]
#[serial_test::serial]
fn a_workspace_roster_accepts_an_auth_variable_name_and_a_relative_anchor_path() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    write_roster(
        workspace.path(),
        r#"{"agents":{"anchored":{
             "url":"https://peer.example",
             "auth":"RUSTAIN_19_14_UNSET_AT_PARSE_TIME",
             "caCert":"certs/ca.pem"
           }}}"#,
    );
    // ⛔ The anchor deliberately DOES NOT EXIST: a roster must load whether or
    // not the file is there, and an unloadable anchor refuses at send time.

    let peers = resolve_peers_from(workspace.path());

    assert_eq!(peers.len(), 1);
    assert_eq!(
        peers[0].auth.as_deref(),
        Some("RUSTAIN_19_14_UNSET_AT_PARSE_TIME"),
        "`auth` holds the variable's NAME and is never dereferenced at parse time"
    );
    assert_eq!(
        peers[0].ca_cert.as_deref(),
        Some(workspace.path().join("certs").join("ca.pem").as_path()),
        "a relative anchor joins the root that located a2a.json — ⛔ not \
         <root>/.rustain/, and ⛔ never `stat`ed"
    );
}

/// `AC1(a)`: the snake_case spelling is accepted too, and an absolute path
/// passes through untouched.
#[test]
#[serial_test::serial]
fn a_workspace_roster_accepts_snake_case_ca_cert_and_leaves_an_absolute_path_alone() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let absolute = workspace.path().join("elsewhere").join("anchor.pem");
    write_roster(
        workspace.path(),
        &format!(
            r#"{{"agents":{{"snake":{{"url":"https://peer.example","ca_cert":{}}}}}}}"#,
            serde_json::to_string(&absolute.to_string_lossy()).expect("json path")
        ),
    );

    let peers = resolve_peers_from(workspace.path());

    assert_eq!(peers[0].ca_cert.as_deref(), Some(absolute.as_path()));
}

/// `AC1(a)`: the profile `[tools.config]` path carries both fields too, and a
/// profile peer's relative anchor resolves against the **roster root** — ⛔ not
/// the profile TOML's directory. Entered through `TomlProfileResolver::new`,
/// the production caller (`A27`): the resolver's own wiring — extraction, merge,
/// and path resolution — is the thing under test, so removing any of those calls
/// turns this keystone red.
#[test]
#[serial_test::serial]
fn a_profile_peer_carries_both_fields_and_resolves_against_the_roster_root() {
    use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
    use rustain::domain::ports::ProfileResolver;

    let profiles = tempfile::tempdir().expect("temporary profile directory");
    std::fs::write(
        profiles.path().join("coding.toml"),
        r#"
name = "coding"
description = "review-fixture override of the embedded coding profile"

[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"

[tools.config.a2a.profile-peer]
url = "https://profile.example"
auth = "RUSTAIN_19_14_PROFILE_VAR"
ca_cert = "certs/profile-ca.pem"
"#,
    )
    .expect("write profile");

    let workspace = tempfile::tempdir().expect("temporary workspace");
    write_roster(
        workspace.path(),
        r#"{"agents":{"workspace-peer":{"url":"https://peer.example"}}}"#,
    );
    let original_dir = std::env::current_dir().expect("current directory");
    std::env::set_current_dir(workspace.path()).expect("enter workspace");
    let _restore_dir = scopeguard::guard(original_dir, |path| {
        std::env::set_current_dir(path).expect("restore current directory");
    });

    let resolver = TomlProfileResolver::new("coding", profiles.path().to_path_buf())
        .expect("a profile roster with auth/ca_cert must load through the resolver");
    let resolved = resolver.resolve_active().expect("active profile");

    let profile_peer = resolved
        .a2a_peers
        .iter()
        .find(|spec| spec.id == "profile-peer")
        .expect("profile peer survives the resolver's merge");
    assert_eq!(
        profile_peer.auth.as_deref(),
        Some("RUSTAIN_19_14_PROFILE_VAR")
    );
    assert_eq!(
        profile_peer.ca_cert.as_deref(),
        Some(
            workspace
                .path()
                .join("certs")
                .join("profile-ca.pem")
                .as_path()
        ),
        "a profile peer's anchor resolves against the roster root, ⛔ not the \
         profile TOML's directory"
    );
    assert!(
        resolved
            .a2a_peers
            .iter()
            .any(|spec| spec.id == "workspace-peer"),
        "the workspace peer survives the merge alongside it"
    );
}

/// `AC1(d)` + the structural ratchet (Rule 4): for an **unchanged** config the
/// WHOLE parsed spec set equals the baseline's — ⛔ not field by field, which
/// would pass while a new field silently defaulted to something else.
#[test]
#[serial_test::serial]
fn a_roster_with_neither_new_field_parses_byte_for_byte_as_before() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    write_roster(
        workspace.path(),
        &format!(
            r#"{{"agents":{{
                 "plain":{{"url":"https://plain.example"}},
                 "pinned":{{"url":"https://pinned.example","pinnedKey":{{
                   "alg":"EdDSA","x":"{ED25519_X}","kid":"key-2026"}}}}
               }}}}"#
        ),
    );

    let peers = resolve_peers_from(workspace.path());

    let expected = vec![
        A2aPeerSpec::new(
            "pinned",
            RedactedUrl::from("https://pinned.example"),
            A2aPeerSource::Workspace,
        )
        .with_pinned_key(Some(ed25519_pin())),
        A2aPeerSpec::new(
            "plain",
            RedactedUrl::from("https://plain.example"),
            A2aPeerSource::Workspace,
        ),
    ];
    assert_eq!(
        peers, expected,
        "the whole spec set must be unchanged for a config that predates both fields"
    );
    // Positive control: the baseline config still yields the same trust tiers.
    assert_eq!(peers[0].trust_tier(), TrustTier::Verified);
    assert_eq!(peers[1].trust_tier(), TrustTier::Unverified);
}
