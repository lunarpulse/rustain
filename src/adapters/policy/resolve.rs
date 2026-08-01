//! Load both policy files, bind per-sender overrides to identities, and explain
//! the result (Story 18.3b).
//!
//! **The single path from "two files on disk" to "one explained answer."** Both
//! shells call this and then render: the daemon through `tracing`
//! (`adapters::daemon::policy_startup`) and `rustain doctor` through `CheckResult`
//! (`adapters::cli::doctor::policy_check`). Neither reuses the other's printer —
//! a daemon has no terminal and a CLI has no log to read back — but there is
//! exactly one resolve, so the two can never disagree about what the policy is.
//!
//! Platform-independent on purpose. The daemon adapter is `#[cfg(unix)]`; `doctor`
//! is not, so this cannot live under `adapters::daemon`.

use std::path::Path;

use crate::domain::models::{A2aPeerSpec, EffectivePolicy};
use crate::domain::ports::{ConsentProjectionQuery, ConsentState};
use crate::domain::services::team_policy::{
    ConsentLine, ConsentSource, PolicyExplanation, explain_effective_policy,
    resolve_effective_policy,
};

use super::config::{PolicyConfigError, load_workspace_policies};
#[cfg(feature = "test-instrumentation")]
static WORKSPACE_POLICY_LOAD_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "test-instrumentation")]
pub fn reset_workspace_policy_load_count() {
    WORKSPACE_POLICY_LOAD_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(feature = "test-instrumentation")]
#[must_use]
pub fn workspace_policy_load_count() -> usize {
    WORKSPACE_POLICY_LOAD_COUNT.load(std::sync::atomic::Ordering::SeqCst)
}

/// Resolve and explain a workspace's interaction policy against the exact peer
/// set the active profile composed.
///
/// The caller supplies peers because production merges workspace `a2a.json` and
/// active-profile A2A entries before composition. Re-reading only the workspace
/// file here would make a live profile peer appear unregistered to policy.
pub fn resolve_workspace_policy(
    workspace: &Path,
    peers: &[A2aPeerSpec],
    consent: &dyn ConsentProjectionQuery,
) -> Result<(EffectivePolicy, PolicyExplanation), PolicyConfigError> {
    #[cfg(feature = "test-instrumentation")]
    WORKSPACE_POLICY_LOAD_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let files = load_workspace_policies(workspace)?;
    let policy = resolve_effective_policy(&files.individual, files.team.as_ref(), peers);
    let consent_lines = collect_consent_lines(&policy, consent);
    let explanation = explain_effective_policy(&policy, &consent_lines);
    Ok((policy, explanation))
}

/// Build the consent rows from the projection plus the senders policy names.
///
/// A sender the projection has never heard of but policy names is reported as
/// `TomlImplied`: today the TOML file is the only live consent source, and saying
/// so out loud is what keeps 18-3c's journaled grants from later appearing from
/// nowhere.
pub fn collect_consent_lines(
    policy: &EffectivePolicy,
    projection: &dyn ConsentProjectionQuery,
) -> Vec<ConsentLine> {
    let journaled = projection.known_senders();
    let mut lines: Vec<ConsentLine> = journaled
        .iter()
        .map(|sender| {
            let display = policy
                .sender_overrides
                .iter()
                .find(|override_| override_.identity.peer_id() == Some(sender))
                .map_or_else(
                    || sender.as_str().to_owned(),
                    |override_| override_.alias.clone(),
                );
            ConsentLine {
                sender: display,
                source: ConsentSource::Journaled,
                state: projection.consent_for(sender),
            }
        })
        .collect();
    for override_ in &policy.sender_overrides {
        let already_journaled = override_
            .identity
            .peer_id()
            .is_some_and(|id| journaled.contains(id));
        if already_journaled {
            continue;
        }
        lines.push(ConsentLine {
            sender: override_.alias.clone(),
            source: ConsentSource::TomlImplied,
            state: ConsentState::None,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, INDIVIDUAL_POLICY_FILE, IndividualPolicy, NotificationUrgency,
        PeerId, PinnedKey, PinnedKeyAlgorithm, RedactedUrl, ResponseMode, SenderOverride,
        TEAM_POLICY_FILE,
    };
    use base64::Engine as _;

    fn write(dir: &Path, name: &str, body: &str) {
        let rustain = dir.join(".rustain");
        std::fs::create_dir_all(&rustain).unwrap();
        std::fs::write(rustain.join(name), body).unwrap();
    }

    fn resolve(dir: &Path) -> Result<(EffectivePolicy, PolicyExplanation), PolicyConfigError> {
        let peers = crate::adapters::a2a::config::parse_workspace_a2a_config(
            &dir.join(".rustain").join("a2a.json"),
        )
        .unwrap_or_default();
        resolve_workspace_policy(dir, &peers, &super::super::EmptyConsentProjection)
    }

    /// A conflict is reported, not fatal — NFR66 says "reported with resolution
    /// guidance", and refusing to start over a team floor would make the feature
    /// unusable.
    #[test]
    fn a_team_floor_conflict_is_reported_and_resolution_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nnotification = \"queue\"\n",
        );
        write(
            dir.path(),
            TEAM_POLICY_FILE,
            "[team.defaults]\nnotification = \"immediate\"\n",
        );

        let (policy, explanation) = resolve(dir.path()).expect("a conflict must not be fatal");
        assert_eq!(policy.urgency.value, NotificationUrgency::Immediate);
        assert_eq!(explanation.warning_count(), 1);
        assert!(
            explanation.warnings().next().unwrap().guidance.is_some(),
            "NFR66 demands resolution guidance"
        );
    }

    /// A malformed file IS fatal. Verified by causing the failure, not by reading
    /// the code — the 18-1b/18-2 fail-open defect class.
    #[test]
    fn a_malformed_file_stops_resolution() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), TEAM_POLICY_FILE, "[team.defaults\n");
        let error = resolve(dir.path()).expect_err("a malformed file must not be tolerated");
        assert!(error.path().ends_with(TEAM_POLICY_FILE));
    }

    #[test]
    fn profile_only_peer_participates_in_policy_identity_resolution() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.overrides.profile-peer]\nnotification = \"immediate\"\n",
        );
        let pin = [7u8; 32];
        let peer = A2aPeerSpec {
            id: "profile-peer".to_owned(),
            url: RedactedUrl::new("https://peer.example/a2a".to_owned()),
            pinned_key: Some(PinnedKey::new(
                PinnedKeyAlgorithm::EdDsa,
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pin),
                None,
            )),
            source: A2aPeerSource::Profile {
                profile_name: "team".to_owned(),
            },
        };
        let identity = PeerId::from_public_key(&pin).unwrap();

        let (policy, _) =
            resolve_workspace_policy(dir.path(), &[peer], &super::super::EmptyConsentProjection)
                .expect("profile peer resolves");

        assert!(
            crate::domain::services::team_policy::sender_policy_for(&policy, &identity).is_some()
        );
    }

    /// Positive control: a clean pair produces NO conflict. A validator that always
    /// warns is as useless as one that never does.
    #[test]
    fn positive_control_a_clean_pair_reports_no_conflict() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"immediate\"\n",
        );
        let (_, explanation) = resolve(dir.path()).unwrap();
        assert_eq!(
            explanation.warning_count(),
            0,
            "{:?}",
            explanation.warnings().collect::<Vec<_>>()
        );
    }

    /// One decision, two shells: whatever the daemon and `doctor` render, they must
    /// be rendering the SAME resolved value. A divergence here would mean two
    /// policies exist.
    #[test]
    fn resolution_matches_a_direct_fold_over_the_same_files() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\nnotification = \"digest\"\n",
        );
        write(
            dir.path(),
            TEAM_POLICY_FILE,
            "[team.defaults]\nresponse_mode = \"notify-and-draft\"\nnotification = \"immediate\"\n",
        );

        let (resolved, explanation) = resolve(dir.path()).unwrap();
        let files = load_workspace_policies(dir.path()).unwrap();
        let direct = resolve_effective_policy(&files.individual, files.team.as_ref(), &[]);
        assert_eq!(resolved, direct);
        assert_eq!(resolved.automation.value, ResponseMode::NotifyAndDraft);
        assert_eq!(resolved.urgency.value, NotificationUrgency::Immediate);
        assert_eq!(explanation.warning_count(), 2);
    }

    /// AC3 through the real loader: the alias moves in `a2a.json`, the pinned key
    /// does not, and the override still binds.
    #[test]
    fn an_identity_keyed_override_survives_a_rename_in_the_real_peer_file() {
        let key = [42u8; 32];
        let identity = PeerId::from_public_key(&key).unwrap();
        let pin = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);

        let policy_toml = format!(
            "[interaction.overrides.\"marcus-arch\"]\npeer_id = \"{}\"\nresponse_mode = \"notify-and-draft\"\n",
            identity.as_str()
        );
        let peer_json = |alias: &str| {
            format!(
                r#"{{"agents":{{"{alias}":{{"url":"https://peer.example/a2a","pinnedKey":{{"alg":"EdDSA","x":"{pin}"}}}}}}}}"#
            )
        };

        for alias in ["marcus-arch", "marcus"] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), INDIVIDUAL_POLICY_FILE, &policy_toml);
            write(dir.path(), "a2a.json", &peer_json(alias));

            let (resolved, _) = resolve(dir.path()).expect("policy resolves");
            let bound =
                crate::domain::services::team_policy::sender_policy_for(&resolved, &identity)
                    .unwrap_or_else(|| {
                        panic!("override lost after the peer was renamed to `{alias}`")
                    });
            assert_eq!(
                bound.response_mode.as_ref().map(|resolved| resolved.value),
                Some(ResponseMode::NotifyAndDraft)
            );
            assert!(bound.identity.is_pinned());
        }
    }

    /// A journaled sender is reported as `journaled` and not duplicated by the
    /// TOML-implied pass.
    #[test]
    fn a_journaled_sender_is_not_duplicated_by_the_toml_pass() {
        struct OneGrant(PeerId);
        impl ConsentProjectionQuery for OneGrant {
            fn known_senders(&self) -> Vec<PeerId> {
                vec![self.0.clone()]
            }
            fn consent_for(&self, _sender: &PeerId) -> ConsentState {
                ConsentState::Trusted
            }
        }

        let identity = PeerId::from_public_key(&[5u8; 32]).unwrap();
        let mut individual = IndividualPolicy::default();
        individual.overrides.insert(
            "marcus-arch".to_owned(),
            SenderOverride {
                peer_id: Some(identity.as_str().to_owned()),
                ..Default::default()
            },
        );
        let peers = vec![A2aPeerSpec {
            id: "marcus-arch".to_owned(),
            url: RedactedUrl::new("https://p.example/a2a".to_owned()),
            pinned_key: Some(PinnedKey::new(
                PinnedKeyAlgorithm::EdDsa,
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([5u8; 32]),
                None,
            )),
            source: A2aPeerSource::Workspace,
        }];

        let policy = resolve_effective_policy(&individual, None, &peers);
        let lines = collect_consent_lines(&policy, &OneGrant(identity));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].source, ConsentSource::Journaled);
        assert_eq!(lines[0].state, ConsentState::Trusted);
        assert_eq!(lines[0].sender, "marcus-arch");
    }

    /// With the shipped empty projection every named sender resolves to no grant
    /// and the line reads from TOML precedence alone — truthfully, because today
    /// that is the only live source.
    #[test]
    fn the_empty_projection_reports_toml_implied_lines_for_named_senders() {
        let mut individual = IndividualPolicy::default();
        individual
            .overrides
            .insert("lena-po".to_owned(), SenderOverride::default());
        let policy = resolve_effective_policy(&individual, None, &[]);
        let lines = collect_consent_lines(&policy, &super::super::EmptyConsentProjection);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sender, "lena-po");
        assert_eq!(lines[0].source, ConsentSource::TomlImplied);
        assert_eq!(lines[0].state, ConsentState::None);
    }
}
