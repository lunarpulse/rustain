//! The relay-mode config: `.rustain/relay.json` (Story 18.4c, AC2, FR159).
//!
//! # Why a third file and not a key in `p2p.json`
//!
//! `p2p.json`'s root is `deny_unknown_fields`, so a `relay` key there makes an
//! older binary read the whole **admission** list as `Malformed` — and a
//! malformed allowlist means *"this host admits no peer."* A reachability
//! addition would become a silent security-posture change on downgrade. This is
//! the tree's own argument, already written down for exactly this shape at
//! [`crate::infrastructure::paths::workspace_p2p_reach_path`]: two files is the
//! structural form of `reach ≠ trust`, and relay mode is reachability.
//!
//! ⛔ This file is never read by, merged into, or able to invalidate
//! `p2p.json`. Nothing here can refuse a peer; nothing there can add a relay.
//!
//! # Absent is a posture, ⛔ not an error
//!
//! No file means `disabled`, which is **byte-for-byte the composition every
//! shipped build already has**. Story 18.4c may not switch an existing install
//! onto a vendor's relay on upgrade, so the absent case is the default and it
//! nags about nothing.
//!
//! # Malformed degrades, ⛔ it does not refuse
//!
//! The loud-failure precedent in `startup.rs` fires when operator intent is
//! **known and unhonorable** — an enabled listener a build cannot honor. ⚑ A
//! malformed file's intent is **unreadable**, and you cannot ignore an intent
//! you never parsed. Refusing would turn one corrupt byte into a denial of
//! service on the entire peer transport; degrading costs **reach only**, and
//! `disabled` is the mode that contacts nobody — so an attacker who corrupts
//! this file to force it gains nothing. It is the safer direction on both axes,
//! and it lands in a state whose shipped copy is already true.
//!
//! ⛔ But `disabled`-because-broken must stay distinguishable from
//! `disabled`-because-chosen, or the operator degrades into a mode nobody can
//! tell apart from the intended one and never learns the file is broken. That
//! is what [`crate::domain::models::RelayConfigState::Malformed`] carries.
//!
//! # The file
//!
//! ```json
//! { "mode": "configured", "relays": ["https://relay.example.com/"] }
//! ```
//!
//! `mode` is one of `disabled`, `default` or `configured`. `relays` is required
//! by — and only meaningful for — `configured`.

use std::path::Path;

use serde::Deserialize;

use crate::domain::models::{MAX_CONFIGURED_RELAYS, RelayConfigState, RelayMode, RelaySet};
use crate::domain::services::peer_reach_filter::canonical_relay_url;

/// The file's shape. `deny_unknown_fields` is fail-closed on purpose: a typo in
/// a reachability file must surface as a named degradation, ⛔ never as a mode
/// the operator did not ask for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayDocument {
    mode: String,
    #[serde(default)]
    relays: Vec<String>,
}

/// Read `.rustain/relay.json`.
///
/// Never fails: an unreadable relay config is a **state**, and its consequence
/// is `disabled` plus a sentence saying so, rather than an error every caller
/// has to invent a policy for.
#[must_use]
pub fn load_workspace_relay_config(path: &Path) -> RelayConfigState {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RelayConfigState::Absent;
        }
        Err(error) => {
            return RelayConfigState::Malformed {
                reason: format!("failed to read {}: {error}", path.display()),
            };
        }
    };
    let document: RelayDocument = match serde_json::from_str(&content) {
        Ok(document) => document,
        Err(error) => {
            return RelayConfigState::Malformed {
                reason: format!("invalid JSON in {}: {error}", path.display()),
            };
        }
    };
    match mode_from_document(document) {
        Ok(mode) => RelayConfigState::Present(mode),
        Err(reason) => RelayConfigState::Malformed {
            reason: format!(
                "{} names no mode this build composes: {reason}",
                path.display()
            ),
        },
    }
}

fn mode_from_document(document: RelayDocument) -> Result<RelayMode, String> {
    match document.mode.as_str() {
        "disabled" => match document.relays.is_empty() {
            // ⛔ Not "ignore the list": an operator who wrote relays and
            // `disabled` wrote two different intentions, and this host cannot
            // tell which one they meant.
            false => Err("mode \"disabled\" names relays; remove one or the other".to_owned()),
            true => Ok(RelayMode::Disabled),
        },
        "default" => match document.relays.is_empty() {
            false => Err("mode \"default\" names relays, but it uses the n0 list".to_owned()),
            true => Ok(RelayMode::N0Default),
        },
        "configured" => {
            if document.relays.is_empty() {
                return Err("mode \"configured\" names no relay".to_owned());
            }
            if document.relays.len() > MAX_CONFIGURED_RELAYS {
                return Err(format!(
                    "mode \"configured\" names {} relays, past the {MAX_CONFIGURED_RELAYS} this \
                     host composes",
                    document.relays.len()
                ));
            }
            let mut urls = Vec::with_capacity(document.relays.len());
            for relay in &document.relays {
                // One canonical form, produced by the same WHATWG parser iroh's
                // own `RelayUrl` uses — so what is composed, rendered and
                // compared for membership is one string, not three.
                let canonical = canonical_relay_url(relay)
                    .ok_or_else(|| format!("{relay:?} is not an https relay URL"))?;
                if !urls.contains(&canonical) {
                    urls.push(canonical);
                }
            }
            Ok(RelayMode::Configured { urls })
        }
        other => Err(format!(
            "{other:?} is not one of \"disabled\", \"default\" or \"configured\""
        )),
    }
}

/// The relay hosts this process may contact under `mode` (Story 18.4c, D13).
///
/// ⚑ This is what turns the membership rule from a test-only assertion into an
/// **enforced invariant**: the set a ticket's relay URL is checked against is
/// the set the endpoint is composed with. `Disabled` expands to nothing, which
/// matches nothing — ⛔ an empty set is not a wildcard.
///
/// `Configured` expands to the operator's own canonical URLs verbatim, so the
/// composed relay map and the membership test are provably the same set rather
/// than two derivations that could drift.
#[must_use]
pub fn relay_url_set(mode: &RelayMode) -> RelaySet {
    match mode {
        RelayMode::Disabled => RelaySet::empty(),
        RelayMode::Configured { urls } => urls.iter().cloned().collect(),
        RelayMode::N0Default => n0_default_relay_set(),
    }
}

#[cfg(feature = "p2p")]
fn n0_default_relay_set() -> RelaySet {
    crate::adapters::iroh::n0_default_relay_set()
}

/// ⛔ A build with no transport adapter contacts no relay at all — it never
/// binds an endpoint that could — so the set of relay hosts it may contact is
/// empty, and saying so is the accurate answer rather than a stub.
#[cfg(not(feature = "p2p"))]
fn n0_default_relay_set() -> RelaySet {
    RelaySet::empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = crate::infrastructure::paths::workspace_relay_config_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
        path
    }

    /// The shipped default: no file, no relay, no nag.
    #[test]
    fn an_absent_file_is_the_disabled_mode_and_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crate::infrastructure::paths::workspace_relay_config_path(dir.path());
        let state = load_workspace_relay_config(&path);
        assert_eq!(state, RelayConfigState::Absent);
        assert_eq!(state.mode(), RelayMode::Disabled);
        assert_eq!(state.degraded_reason(), None);
    }

    #[test]
    fn the_three_modes_read_back_as_the_three_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            load_workspace_relay_config(&write(&dir, r#"{"mode":"disabled"}"#)).mode(),
            RelayMode::Disabled
        );
        assert_eq!(
            load_workspace_relay_config(&write(&dir, r#"{"mode":"default"}"#)).mode(),
            RelayMode::N0Default
        );
        assert_eq!(
            load_workspace_relay_config(&write(
                &dir,
                r#"{"mode":"configured","relays":["https://relay.example.com"]}"#
            ))
            .mode(),
            // ⚑ Canonicalised on the way in: the operator wrote no trailing
            // slash and iroh's own parser adds one.
            RelayMode::Configured {
                urls: vec!["https://relay.example.com/".to_owned()]
            }
        );
    }

    /// AC2 mutant (a): a malformed file falls back to a *more* connected mode.
    /// AC2 mutant (e): it degrades but reads exactly like a chosen `disabled`.
    #[test]
    fn a_malformed_file_degrades_to_disabled_and_stays_distinguishable() {
        let dir = tempfile::tempdir().expect("tempdir");
        for body in [
            "{ not json",
            r#"{"mode":"n0"}"#,
            r#"{"mode":"configured"}"#,
            r#"{"mode":"configured","relays":["http://relay.example.com"]}"#,
            r#"{"mode":"configured","relays":["ftp://relay.example.com"]}"#,
            r#"{"mode":"default","relays":["https://relay.example.com"]}"#,
            r#"{"mode":"disabled","relays":["https://relay.example.com"]}"#,
            r#"{"mode":"configured","relays":["https://a"],"listen":true}"#,
            // Story 18.4c review: credentials would ride the canonical string
            // into reach records, tickets and terminal output.
            r#"{"mode":"configured","relays":["https://user:secret@relay.example.com"]}"#,
            // Story 18.4c review: port 0 names no listener, as it does for a
            // socket.
            r#"{"mode":"configured","relays":["https://relay.example.com:0"]}"#,
        ] {
            let state = load_workspace_relay_config(&write(&dir, body));
            assert_eq!(state.mode(), RelayMode::Disabled, "{body}");
            assert!(
                state.degraded_reason().is_some(),
                "a broken config must not be indistinguishable from a chosen one: {body}"
            );
        }

        // Positive control: a *chosen* disabled is not degraded.
        let chosen = load_workspace_relay_config(&write(&dir, r#"{"mode":"disabled"}"#));
        assert_eq!(chosen.mode(), RelayMode::Disabled);
        assert_eq!(chosen.degraded_reason(), None);
    }

    #[test]
    fn a_configured_list_is_bounded_and_deduplicated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let many: Vec<String> = (0..=MAX_CONFIGURED_RELAYS)
            .map(|n| format!("https://relay{n}.example.com"))
            .collect();
        let body = serde_json::json!({ "mode": "configured", "relays": many }).to_string();
        assert!(
            load_workspace_relay_config(&write(&dir, &body))
                .degraded_reason()
                .is_some()
        );

        let duplicated = load_workspace_relay_config(&write(
            &dir,
            r#"{"mode":"configured","relays":["https://relay.example.com","https://relay.example.com/"]}"#,
        ));
        assert_eq!(
            duplicated.mode(),
            RelayMode::Configured {
                urls: vec!["https://relay.example.com/".to_owned()]
            }
        );
    }

    /// AC2 mutant (b) / ratchet: the two files stay independent **by
    /// construction**, not by convention. `p2p_config.rs` must not learn a word
    /// of relay vocabulary, or a reachability edit lands in the file whose
    /// `deny_unknown_fields` root decides admission.
    #[test]
    fn the_admission_loader_knows_nothing_about_relays() {
        let source = include_str!("p2p_config.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        let lowered = production.to_ascii_lowercase();
        for forbidden in ["relay", "relay.json"] {
            assert!(
                !lowered.contains(forbidden),
                "the admission loader must not carry relay vocabulary: {forbidden}"
            );
        }
        // Positive control: the scan really is reading that module.
        assert!(lowered.contains("deny_unknown_fields"));
    }
}
