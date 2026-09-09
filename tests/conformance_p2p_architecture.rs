//! Three ratchets in this module cross-check the local `../_bmad-output` planning tree.
//! They are dev-workspace-only and cannot execute in CI because that tree is outside this
//! repository. They skip rather than read a vendored copy because a vendored copy would make
//! the assertions tautologies with no mechanism to keep it in sync.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rustain::domain::ports::PeerTransport;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn planning_tree_source(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read planning tree {path:?}: {error}"),
    }
}

fn markdown_section<'a>(document: &'a str, heading: &str) -> &'a str {
    let section = &document[document
        .find(heading)
        .unwrap_or_else(|| panic!("positive control: missing {heading}"))..];
    let end = section[heading.len()..]
        .find("\n### ")
        .map_or(section.len(), |offset| heading.len() + offset);
    &section[..end]
}

#[test]
fn peer_transport_port_has_the_complete_sibling_shape() {
    let port = source("src/domain/ports/peer_transport.rs");
    assert!(
        port.contains("pub trait PeerTransport"),
        "positive control: PeerTransport declaration must be readable"
    );

    for required in [
        "fn local_address(",
        "async fn dial(",
        "async fn send_to(",
        "fn inbound(",
        "async fn shutdown(",
        "pub struct InboundFrame",
        "pub struct PeerAddress",
    ] {
        assert!(
            port.contains(required),
            "missing port capability {required}"
        );
    }

    for reason in [
        "AllowlistAbsent",
        "AllowlistEmpty",
        "AllowlistMalformed",
        "PeerUnpinned",
        "PeerUnlisted",
        "SignatureInvalid",
        "Dial",
        "Unreachable",
    ] {
        assert!(
            port.contains(reason),
            "PeerTransportError cannot distinguish {reason}"
        );
    }
    for forbidden in ["use iroh", "iroh::", "use noq", "noq::"] {
        assert!(
            !port.contains(forbidden),
            "domain port leaked transport dependency {forbidden}"
        );
    }
}

#[test]
fn peer_transport_is_object_safe() {
    fn accepts_object(_: Option<&dyn PeerTransport>) {}
    accepts_object(None);
}

#[test]
fn agent_transport_remains_the_exact_two_method_in_process_port() {
    let port = source("src/domain/ports/agent_transport.rs");
    assert!(
        port.contains("pub trait AgentTransport"),
        "positive control: AgentTransport declaration must be readable"
    );
    assert_eq!(port.matches("async fn send(").count(), 1);
    assert_eq!(port.matches("fn subscribe(").count(), 1);
    for forbidden in [
        "fn dial(",
        "fn accept(",
        "fn send_to(",
        "PeerId",
        "fn shutdown(",
    ] {
        assert!(
            !port.contains(forbidden),
            "AgentTransport gained peer capability {forbidden}"
        );
    }
}

#[test]
fn all_three_transport_names_explain_their_distinct_roles() {
    let agent = source("src/domain/ports/agent_transport.rs");
    let rap = source("src/adapters/rap/transport.rs");
    let peer = source("src/domain/ports/peer_transport.rs");

    for (name, text, peers) in [
        ("AgentTransport", agent, ["RapTransport", "PeerTransport"]),
        ("RapTransport", rap, ["AgentTransport", "PeerTransport"]),
        ("PeerTransport", peer, ["AgentTransport", "RapTransport"]),
    ] {
        for peer_name in peers {
            assert!(
                text.contains(peer_name),
                "{name} docs do not disambiguate {peer_name}"
            );
        }
    }
}

#[test]
fn adr_records_the_sibling_decision_and_measured_dependency_cost() {
    let path = root().join(
        "../_bmad-output/planning-artifacts/architecture/adr/ADR-18-4-01-peer-transport-sibling-port.md",
    );
    let Some(adr) = planning_tree_source(&path) else {
        println!(
            "SKIP: adr_records_the_sibling_decision_and_measured_dependency_cost — planning tree {path:?} \
             absent (standalone checkout); the ADR decision and dependency-cost ratchet was not checked"
        );
        return;
    };
    for required in [
        "PeerTransport",
        "AgentTransport",
        "RapTransport",
        "presets::Minimal",
        "EndpointId",
        "noq",
        "net +131",
        "## Architecture compliance",
    ] {
        assert!(
            adr.contains(required),
            "ADR missing decision evidence {required}"
        );
    }
}

#[test]
fn p2p_feature_is_isolated_and_pins_the_i_roh_stack() {
    let manifest: toml::Value = toml::from_str(&source("Cargo.toml")).unwrap();
    let features = manifest["features"].as_table().unwrap();
    let defaults: Vec<_> = features["default"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(!defaults.contains(&"p2p"), "p2p must remain off by default");

    let p2p: BTreeSet<_> = features["p2p"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(p2p, BTreeSet::from(["dep:iroh"]));
    assert_eq!(
        manifest["dependencies"]["iroh"]["version"].as_str(),
        Some("=1.0.3"),
        "iroh must stay exactly pinned until rename and MSRV changes are reviewed"
    );
    assert_eq!(
        manifest["dependencies"]["iroh"]["optional"].as_bool(),
        Some(true)
    );

    // Story 18.4c-b AC1 — the relay SERVER key. ⛔ Deliberately NOT part of the
    // `p2p` set-equality assertion above: `relay-server` is a separate key, so
    // `:180` stays byte-identical and this block is what covers the new one.
    // Until this landed there was NO version-pin test for `iroh-relay` at all.
    let relay_server: BTreeSet<_> = features["relay-server"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        relay_server,
        BTreeSet::from(["dep:iroh-relay", "dep:rustls", "dep:rustls-pemfile"]),
        "the relay server needs iroh-relay plus the rustls pair that names ServerConfig"
    );
    assert!(
        !defaults.contains(&"relay-server"),
        "relay-server must remain off by default: the server tree is ~15 crates"
    );
    assert_eq!(
        manifest["dependencies"]["iroh-relay"]["version"].as_str(),
        Some("=1.0.3"),
        "iroh-relay must move in lockstep with iroh, or client and server link \
         different relay protocol versions"
    );
    assert_eq!(
        manifest["dependencies"]["iroh-relay"]["optional"].as_bool(),
        Some(true),
        "a non-optional iroh-relay links the whole server tree into every default build"
    );
    assert_eq!(
        manifest["dependencies"]["iroh-relay"]["default-features"].as_bool(),
        Some(false)
    );
    let relay_dep_features: BTreeSet<_> = manifest["dependencies"]["iroh-relay"]["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        relay_dep_features,
        BTreeSet::from(["server"]),
        "`server` is the only feature wanted, and it re-implies `metrics`, which \
         AC4's ask-the-relay leg needs to be non-vacuous"
    );

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
        "p2p must not widen the independent A2A HTTP feature"
    );

    let deny = source("deny.toml");
    assert!(
        deny.contains("hickory-proto:<0.26.1"),
        "RUSTSEC-2026-0119 needs a cargo-deny version gate"
    );
}

#[test]
fn p2p_ci_lane_names_the_current_targets() {
    let ci = source(".github/workflows/ci.yml");
    let lane: String = ci
        .lines()
        .skip_while(|line| *line != "  p2p:")
        .skip(1)
        .take_while(|line| line.is_empty() || !line.starts_with("  ") || line.starts_with("    "))
        .collect();
    assert!(lane.contains("--features p2p"));
    assert!(lane.contains("--test conformance_p2p_architecture"));
}

// ── RETIRED 2026-08-14 by Story 18.4b ───────────────────────────────────────
//
// `p2p_has_no_event_loop_palette_or_slash_command_surface` asserted that
// `event_loop.rs`, `command_registry.rs` and `palette_registry.rs` contained
// none of `p2p` / `peer transport` / `peer listener`.
//
// It was a **scope fence for cut 1, not a bug**: Story 18.4 shipped the
// transport substrate with zero operator surface on purpose, and this assertion
// held that line. Story 18.4b's whole deliverable is that surface — the `/peer`
// slash command, its palette entry and its event-loop dispatch arm — so the
// assertion goes red by construction the moment the story lands. It is retired
// here explicitly, by name, in the same commit that adds the surface, rather
// than quietly weakened or deleted without a record.
//
// ⛔ It is NOT replaced by a laxer version. What the fence protected is now
// covered by narrower, still-live assertions:
//
// * Story 19.13 retired the event-loop line-count/source-text test: it did
//   not exercise behavior and failed on unrelated autocomplete dispatch;
// * the operator copy stays honest — `conformance_p2p_ingress.rs`'s wording
//   ceiling, whose `owned_modules` Story 18.4b extended to every module it
//   added;
// * the transport types stay out of the domain's authority and provenance
//   models — `conformance_p2p_transport.rs::nfr74_…`.

#[test]
fn deferred_work_records_every_story_18_4_disposition() {
    let path = root().join("../_bmad-output/implementation-artifacts/deferred-work.md");
    let Some(deferred) = planning_tree_source(&path) else {
        println!(
            "SKIP: deferred_work_records_every_story_18_4_disposition — planning tree {path:?} \
             absent (standalone checkout); the Story 18.4 deferred-work dispositions were not checked"
        );
        return;
    };
    for (id, status, reason) in [
        (
            "DF-18-3a-ROLE-PROJECTION",
            "NOT APPLICABLE to Story 18.4 (2026-08-13); trigger remains armed",
            "src/adapters/a2a/projection.rs",
        ),
        (
            "DF-18-2-AUTHENTICATED-JOURNAL",
            "AMENDED 2026-08-13 by Story 18.4 — network peer, no journal replication",
            "no room-journal frame",
        ),
        (
            "DF-18-1-MTLS",
            "RE-ARMED 2026-08-13 by Story 18.4; retargeted to Story 18.4b",
            "FR157",
        ),
        (
            "DF-18-1-JKU",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "no JWKS",
        ),
        (
            "DF-18-2-JOURNAL-GROWTH",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "no room-journal reader",
        ),
        (
            "DF-18-3a-HOSTRETURN",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "no host-reattach",
        ),
        (
            "DF-18-3a-f-OPERATOR-SINGULARITY",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "no second local operator",
        ),
        (
            "DF-18-3a-b-INBOX-SURFACE",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "ClientFrame::InputResponse",
        ),
        (
            "DF-18-CRYPTO-CLUSTER",
            "NOT APPLICABLE to Story 18.4 (2026-08-13)",
            "no X25519",
        ),
        (
            "DF-18-4-PEER-UX-CONTRACT",
            "CONFIRMED STILL ACCURATE 2026-08-13 by Story 18.4",
            "zero TUI",
        ),
        (
            "DF-18-4-CROSSHOST-RETRACT",
            "CONFIRMED STILL ACCURATE 2026-08-13 by Story 18.4",
            "no recipient-side durable record",
        ),
        (
            "DF-18-4-NOQ-ADVISORY-BLINDSPOT",
            "CONFIRMED STILL ACCURATE 2026-08-13 by Story 18.4",
            "iroh 1.0.3",
        ),
    ] {
        let section = markdown_section(&deferred, &format!("### {id}:"));
        let normalized = section.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(status), "{id} missing status {status}");
        assert!(normalized.contains(reason), "{id} missing reason {reason}");
    }
}

#[test]
fn prd_amendments_still_name_the_built_cut_and_its_deferred_owners() {
    let path = root().join("../_bmad-output/planning-artifacts/prd.md");
    let Some(prd) = planning_tree_source(&path) else {
        println!(
            "SKIP: prd_amendments_still_name_the_built_cut_and_its_deferred_owners — planning tree {path:?} \
             absent (standalone checkout); the PRD built/deferred split was not checked"
        );
        return;
    };
    for required in [
        "NEW sibling port",
        "The **\"cannot read\" clause defers** with `DF-18-CRYPTO-CLUSTER`",
        "allowlist removal → the peer's next frame is rejected at verify",
        "Story 18.4 (cut 1) claims NEITHER",
        "assert on the **domain types that may not appear**",
        "Zero-phone-home requires starting from **`presets::Minimal`**",
    ] {
        assert!(
            prd.contains(required),
            "PRD amendment no longer describes the built/deferred split: {required}"
        );
    }
}
