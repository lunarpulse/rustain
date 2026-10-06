//! `K07` (AC7, `A9`/`A10`/`A15`) — written `a2a.json` → both production
//! parsers → the production resolver. Exact refusal text in the fixed order
//! `R3 → R1 → R2 → R5 → unset env → R4`, the `R4` matrix, string/object and
//! camel/snake forms, and the slot binding / adoption scope the resolver builds.
//!
//! Mutants: `M05`–`M08`, `M18`, `M36`–`M41`.

use std::collections::BTreeMap;
use std::sync::Once;

use base64::Engine as _;
use rustain::adapters::a2a::auth::{A2aServerAuth, AuthOutcome, resolve_server_auth};
use rustain::adapters::a2a::config::{
    A2aConfigError, parse_workspace_a2a_config, parse_workspace_a2a_server_config,
};
use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::domain::models::PeerId;

const KEY_A: [u8; 32] = [71; 32];
const KEY_B: [u8; 32] = [72; 32];

/// The only process env this module writes: one whitespace-only value, read
/// through the production `env_var_trimmed` chokepoint.
const TRIMMED_EMPTY_ENV: &str = "RUSTAIN_19_18D_K07_TRIMMED_EMPTY";
static SETUP: Once = Once::new();

fn setup() {
    SETUP.call_once(|| {
        // SAFETY: the only write of this uniquely named variable, before any
        // reader in this binary.
        unsafe {
            std::env::set_var(TRIMMED_EMPTY_ENV, "   ");
        }
    });
}

fn pinned_key(key: [u8; 32]) -> serde_json::Value {
    serde_json::json!({
        "alg": "EdDSA",
        "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
    })
}

fn pin(key: [u8; 32]) -> PeerId {
    PeerId::from_jwk_ed25519_x(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key))
        .unwrap()
}

/// The roster every case shares: `p1` and `p1-alt` pin the same key, `p2` a
/// distinct one, and `u` is unpinned.
fn roster() -> serde_json::Value {
    serde_json::json!({
        "p1": { "url": "https://p1.example/", "pinnedKey": pinned_key(KEY_A) },
        "p1-alt": { "url": "https://p1-alt.example/", "pinnedKey": pinned_key(KEY_A) },
        "p2": { "url": "https://p2.example/", "pinnedKey": pinned_key(KEY_B) },
        "u": { "url": "https://u.example/" },
    })
}

/// Write one `a2a.json`, parse it with both production parsers, and resolve
/// it with `env`. `Err` carries the resolver's refusal text.
fn resolve_with(
    agents: serde_json::Value,
    server: serde_json::Value,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<A2aServerAuth>, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a2a.json");
    std::fs::write(
        &path,
        serde_json::json!({ "agents": agents, "server": server }).to_string(),
    )
    .unwrap();
    let roster = parse_workspace_a2a_config(&path).expect("roster parses");
    let config = parse_workspace_a2a_server_config(&path)
        .expect("server block parses")
        .expect("server block present");
    resolve_server_auth(&config, &roster, env).map_err(|error| error.to_string())
}

/// Resolve with a fixed env map: each configured name holds `value-of-<name>`
/// unless `values` overrides it; a name mapped to `None` is unset.
fn resolve(
    server: serde_json::Value,
    values: &[(&str, Option<&str>)],
) -> Result<Option<A2aServerAuth>, String> {
    let values: BTreeMap<String, Option<String>> = values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.map(str::to_owned)))
        .collect();
    resolve_with(roster(), server, move |name| match values.get(name) {
        Some(value) => value.clone(),
        None => Some(format!("value-of-{name}")),
    })
}

fn refusal(server: serde_json::Value, values: &[(&str, Option<&str>)]) -> String {
    let message = resolve(server, values).expect_err("the configuration must be refused");
    for (_, value) in values {
        if let Some(value) = value {
            assert!(
                !message.contains(value),
                "a refusal never prints a value: {message}"
            );
        }
    }
    assert!(
        !message.contains("value-of-"),
        "a refusal never prints a value: {message}"
    );
    message
}

fn r1(env: &str, alias: &str) -> String {
    format!(
        "A2A server.peerKeys binds {env} to {alias}, but no A2A peer named {alias} is configured"
    )
}

fn r2(env: &str, alias: &str) -> String {
    format!(
        "A2A server.peerKeys binds {env} to {alias}, but {alias} has no valid pinnedKey; a key \
         can be bound only to a pinned peer"
    )
}

fn r3(env: &str) -> String {
    format!(
        "A2A API-key configuration names {env} in both server.peerKeys and \
         server.apiKeyEnv/apiKeys; a key is either bound to one peer or unbound"
    )
}

fn r4(env_a: &str, env_b: &str) -> String {
    format!(
        "A2A API-key configuration: {env_a} and {env_b} hold the same key but do not name the \
         same binding; one key value cannot identify two peers or aliases"
    )
}

fn r5(env: &str) -> String {
    format!(
        "A2A server.requirePeerBinding is set, but {env} is not bound to a peer in server.peerKeys"
    )
}

fn unset(env: &str) -> String {
    format!(
        "A2A API-key configuration names {env}, but that environment variable is unset or empty"
    )
}

#[test]
fn k07_each_refusal_is_exact() {
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "K_GHOST": "ghost" } }),
            &[]
        ),
        r1("K_GHOST", "ghost")
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "K_UNPINNED": "u" } }),
            &[]
        ),
        r2("K_UNPINNED", "u")
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "apiKeys": ["K_BOTH"], "peerKeys": { "K_BOTH": "p1" } }),
            &[]
        ),
        r3("K_BOTH")
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "apiKeyEnv": "K_BOTH", "peerKeys": { "K_BOTH": "p1" } }),
            &[]
        ),
        r3("K_BOTH")
    );
    assert_eq!(
        refusal(
            serde_json::json!({
                "requirePeerBinding": true,
                "apiKeys": ["K_UNBOUND"],
                "peerKeys": { "K_BOUND": "p1" },
            }),
            &[]
        ),
        r5("K_UNBOUND")
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "K_MISSING": "p1" } }),
            &[("K_MISSING", None)]
        ),
        unset("K_MISSING")
    );
    // Positive control: every refusal above names a fault the same config
    // without it accepts.
    assert!(
        resolve(serde_json::json!({ "peerKeys": { "K_BOUND": "p1" } }), &[])
            .unwrap()
            .is_some()
    );
    assert!(
        resolve(
            serde_json::json!({ "requirePeerBinding": true, "peerKeys": { "K_BOUND": "p1" } }),
            &[]
        )
        .unwrap()
        .is_some(),
        "requirePeerBinding admits a bound-only key set"
    );
}

#[test]
fn k07_a_trimmed_empty_value_is_unset_through_the_production_env_reader() {
    setup();
    let message = resolve_with(
        roster(),
        serde_json::json!({ "peerKeys": { TRIMMED_EMPTY_ENV: "p1" } }),
        rustain::infrastructure::utils::env_var_trimmed,
    )
    .expect_err("a whitespace-only key is no key");
    assert_eq!(message, unset(TRIMMED_EMPTY_ENV));
}

#[test]
fn k07_a_malformed_authored_pin_keeps_the_parser_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a2a.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "agents": { "bad": { "url": "https://bad.example/", "pinnedKey": { "alg": "EdDSA", "x": "!!!" } } },
            "server": { "peerKeys": { "K_BAD": "bad" } },
        })
        .to_string(),
    )
    .unwrap();
    assert!(
        matches!(
            parse_workspace_a2a_config(&path),
            Err(A2aConfigError::InvalidPeer { .. })
        ),
        "a malformed pinnedKey.x fails at parse, never routed through R2"
    );
}

/// Adjacent multi-fault fixtures: the later check's env name always sorts
/// FIRST, so a reordered resolver (or one that ran in name order) reports the
/// wrong fault. `M36`–`M39`.
#[test]
fn k07_the_first_check_wins_whatever_the_name_order() {
    assert_eq!(
        refusal(
            serde_json::json!({
                "apiKeys": ["Z_BOTH"],
                "peerKeys": { "A_GHOST": "ghost", "Z_BOTH": "p1" },
            }),
            &[]
        ),
        r3("Z_BOTH"),
        "R3 before R1"
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "A_UNPINNED": "u", "Z_GHOST": "ghost" } }),
            &[]
        ),
        r1("Z_GHOST", "ghost"),
        "R1 before R2"
    );
    assert_eq!(
        refusal(
            serde_json::json!({
                "requirePeerBinding": true,
                "apiKeys": ["A_UNBOUND"],
                "peerKeys": { "Z_UNPINNED": "u" },
            }),
            &[]
        ),
        r2("Z_UNPINNED", "u"),
        "R2 before R5"
    );
    assert_eq!(
        refusal(
            serde_json::json!({
                "requirePeerBinding": true,
                "apiKeys": ["Z_UNBOUND"],
                "peerKeys": { "A_MISSING": "p1" },
            }),
            &[("A_MISSING", None)]
        ),
        r5("Z_UNBOUND"),
        "R5 before the env read"
    );
    assert_eq!(
        refusal(
            serde_json::json!({
                "peerKeys": { "A_DUP_ONE": "p1", "A_DUP_TWO": "p2", "Z_MISSING": "p1" },
            }),
            &[
                ("A_DUP_ONE", Some("one shared value")),
                ("A_DUP_TWO", Some("one shared value")),
                ("Z_MISSING", None),
            ]
        ),
        unset("Z_MISSING"),
        "the env read before R4"
    );
    // Within one check, names in sorted order.
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "B_GHOST": "ghost-b", "A_GHOST": "ghost-a" } }),
            &[]
        ),
        r1("A_GHOST", "ghost-a")
    );
}

/// `P2` — one key value identifies one binding: only identical
/// `(alias, pin, adoptPreBinding)` metadata is accepted, as two slots.
#[test]
fn k07_one_key_value_identifies_one_binding() {
    let shared = [
        ("A_KEY", Some("one shared value")),
        ("B_KEY", Some("one shared value")),
    ];
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "A_KEY": "p1", "B_KEY": "p2" } }),
            &shared
        ),
        r4("A_KEY", "B_KEY"),
        "bound to different pins"
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "apiKeys": ["B_KEY"], "peerKeys": { "A_KEY": "p1" } }),
            &shared
        ),
        r4("A_KEY", "B_KEY"),
        "bound and unbound (`M41`)"
    );
    assert_eq!(
        refusal(
            serde_json::json!({ "peerKeys": { "A_KEY": "p1", "B_KEY": "p1-alt" } }),
            &shared
        ),
        r4("A_KEY", "B_KEY"),
        "one pin through two aliases (`M40`)"
    );
    assert_eq!(
        refusal(
            serde_json::json!({
                "peerKeys": {
                    "A_KEY": "p1",
                    "B_KEY": { "peer": "p1", "adoptPreBinding": true },
                },
            }),
            &shared
        ),
        r4("A_KEY", "B_KEY"),
        "one alias with two adoption settings"
    );
    let accepted = resolve(
        serde_json::json!({ "peerKeys": { "A_KEY": "p1", "B_KEY": { "peer": "p1" } } }),
        &shared,
    )
    .expect("identical metadata is accepted")
    .expect("keys configured");
    let AuthOutcome::Authenticated { slot } = accepted.verify(Some("one shared value"), false)
    else {
        panic!("the shared value authenticates");
    };
    let binding = accepted.binding(slot).expect("a bound slot");
    assert_eq!(binding.alias(), "p1");
    assert_eq!(binding.peer(), &pin(KEY_A));
    // Distinct values under different pins are fine: R4 compares values.
    assert!(
        resolve(
            serde_json::json!({ "peerKeys": { "A_KEY": "p1", "B_KEY": "p2" } }),
            &[]
        )
        .unwrap()
        .is_some()
    );
}

/// String and object forms, camelCase and snake_case spellings, are one
/// configuration; the resolver binds the matched slot and builds the adoption
/// scope from it.
#[test]
fn k07_forms_are_equivalent_and_the_slot_carries_its_binding() {
    let values = [
        ("K_P1A", Some("p1 a value")),
        ("K_P1B", Some("p1 b value")),
        ("K_P2", Some("p2 value")),
        ("K_U", Some("unbound value")),
    ];
    let camel = resolve(
        serde_json::json!({
            "apiKeyEnv": "K_U",
            "peerKeys": {
                "K_P1A": "p1",
                "K_P1B": { "peer": "p1", "adoptPreBinding": true },
                "K_P2": { "peer": "p2" },
            },
        }),
        &values,
    )
    .unwrap()
    .unwrap();
    let snake = resolve(
        serde_json::json!({
            "api_key_env": "K_U",
            "peer_keys": {
                "K_P1A": { "peer": "p1", "adopt_pre_binding": false },
                "K_P1B": { "peer": "p1", "adopt_pre_binding": true },
                "K_P2": "p2",
            },
        }),
        &values,
    )
    .unwrap()
    .unwrap();
    for auth in [&camel, &snake] {
        let binding_of = |value: &str| {
            let AuthOutcome::Authenticated { slot } = auth.verify(Some(value), false) else {
                panic!("{value} authenticates");
            };
            auth.binding(slot).map(|binding| {
                (
                    binding.alias().to_owned(),
                    binding.peer().clone(),
                    binding.adopt_pre_binding(),
                )
            })
        };
        assert_eq!(
            binding_of("p1 a value"),
            Some(("p1".to_owned(), pin(KEY_A), false))
        );
        assert_eq!(
            binding_of("p1 b value"),
            Some(("p1".to_owned(), pin(KEY_A), true))
        );
        assert_eq!(
            binding_of("p2 value"),
            Some(("p2".to_owned(), pin(KEY_B), false))
        );
        assert_eq!(
            binding_of("unbound value"),
            None,
            "an unbound slot has no binding"
        );
        assert_eq!(
            auth.verify(Some("no such value"), false),
            AuthOutcome::Rejected
        );
        assert_eq!(
            auth.keys().adopted_for(&pin(KEY_A)),
            vec![SubmitterKey::from_api_key("p1 b value")],
            "only the adopting slot's pseudonym joins the scope"
        );
        assert!(auth.keys().adopted_for(&pin(KEY_B)).is_empty());
    }
    let require_snake = resolve(
        serde_json::json!({ "require_peer_binding": true, "api_keys": ["K_U"] }),
        &values,
    )
    .expect_err("the snake spelling of requirePeerBinding is honoured");
    assert_eq!(require_snake, r5("K_U"));
}
