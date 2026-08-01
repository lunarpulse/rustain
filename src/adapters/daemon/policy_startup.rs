//! NFR66 startup validation — the daemon shell over the policy decision core
//! (Story 18.3b, AC5).
//!
//! # The trap this module exists to avoid
//!
//! `rustain doctor` renders through `display_results`
//! (`adapters/cli/doctor/mod.rs:250`), which is built on `println!`. **A daemon is
//! not attached to a terminal** — `logging.rs` sends all tracing to
//! `{data_dir}/rustain.log` and zero bytes to stdout. Reusing the *printer* would
//! send the operator's policy explanation into the void.
//!
//! So this is a second **shell** over the same **core**: it resolves through
//! `adapters::policy::resolve_workspace_policy` — byte-for-byte what the doctor
//! explainer calls — and reports through `tracing`. One decision, two shells.
//!
//! # Severity
//!
//! NFR66 says conflicts are *"reported with resolution guidance"*, so a conflict —
//! a team floor stricter than the individual on urgency, an unenforced per-type
//! key, an unpinned per-sender target — is **normal, expected, and non-fatal**. A
//! **malformed file** is fatal (AC2 fail-closed). The two must not be conflated:
//! refusing to start over a team floor would make the feature unusable, and
//! starting anyway over a malformed file would silently upgrade autonomy.
//!
//! # No TTY check, deliberately
//!
//! There is no terminal detection anywhere below. The daemon is the *primary* case
//! for this validation, not the excluded one; gating on a TTY would disable the
//! report exactly where it matters most.

use std::path::Path;

use anyhow::Result;

use crate::domain::models::{A2aPeerSpec, EffectivePolicy};
use crate::domain::services::team_policy::{PolicyExplanation, PolicyNotice};

/// The log line every successful startup emits, whatever the policy says.
///
/// Unconditional so an operator can always confirm the check ran: "no output"
/// would be indistinguishable from "validation was skipped", which is exactly the
/// no-op mutant this AC guards against.
pub(crate) const STARTUP_BANNER: &str = "interaction policy resolved (NFR66)";
pub(crate) const MESSAGE_TYPE_DEFERRAL_NOTICE: &str = "per-sender response mode is enforced; \
    semantic message type is not carried, so per-message-type response overrides remain deferred \
    (DF-18-3b-MSGTYPE)";
pub(crate) const AUTO_AUTHORITY_WARNING: &str = "server.admission=\"allow\" authorizes inbound \
    execution and at least one notify-and-auto sender lacks journaled or TOML-implied consent; \
    grant that sender, use admission=\"ask\", or lower its effective response mode";
pub(crate) const UNKNOWN_ADMISSION_WARNING: &str = "could not determine server admission posture \
    from .rustain/a2a.json; treating the unknown posture as an authority-widening hazard until \
    the configuration is repaired";

/// Resolve both policy files and report the outcome through the daemon's log.
///
/// Returns `Err` only when a policy file is **malformed**; a conflict is logged and
/// start proceeds.
pub(crate) fn validate_startup_policies(
    workspace: &std::path::Path,
    peers: &[A2aPeerSpec],
    projection: &dyn crate::domain::ports::ConsentProjectionQuery,
) -> Result<EffectivePolicy> {
    let (policy, explanation) =
        crate::adapters::policy::resolve_workspace_policy(workspace, peers, projection)?;
    report_to_log(&explanation);
    tracing::info!("{MESSAGE_TYPE_DEFERRAL_NOTICE}");
    Ok(policy)
}
pub(crate) fn report_auto_authority_widening(
    admission: crate::adapters::a2a::config::A2aAdmissionPolicy,
    policy: &EffectivePolicy,
    peers: &[A2aPeerSpec],
    projection: &dyn crate::domain::ports::ConsentProjectionQuery,
) {
    if should_warn_auto_authority_widening(admission, policy, peers, projection) {
        tracing::warn!("{AUTO_AUTHORITY_WARNING}");
    }
}

pub(crate) fn report_unknown_admission_posture(error: &dyn std::fmt::Display) {
    tracing::warn!(%error, "{UNKNOWN_ADMISSION_WARNING}");
    tracing::warn!("{AUTO_AUTHORITY_WARNING}");
}

fn should_warn_auto_authority_widening(
    admission: crate::adapters::a2a::config::A2aAdmissionPolicy,
    policy: &EffectivePolicy,
    peers: &[A2aPeerSpec],
    projection: &dyn crate::domain::ports::ConsentProjectionQuery,
) -> bool {
    use crate::domain::models::{ResponseMode, SenderPolicy};
    use crate::domain::ports::ConsentState;

    if admission != crate::adapters::a2a::config::A2aAdmissionPolicy::Allow {
        return false;
    }
    let default_auto = policy.automation.value == ResponseMode::NotifyAndAuto;
    if peers.is_empty() {
        return default_auto;
    }
    peers.iter().any(|peer| {
        let Some(peer_id) = peer.pinned_identity() else {
            return default_auto;
        };
        let sender_override: Option<&SenderPolicy> = policy
            .sender_overrides
            .iter()
            .find(|sender| sender.identity.peer_id() == Some(&peer_id));
        let effective_auto = sender_override
            .and_then(|sender| sender.response_mode.as_ref())
            .map_or(default_auto, |mode| {
                mode.value == ResponseMode::NotifyAndAuto
            });
        let has_consent =
            sender_override.is_some() || projection.consent_for(&peer_id) == ConsentState::Trusted;
        effective_auto && !has_consent
    })
}

/// The daemon's report. `tracing`, never `println!`.
fn report_to_log(explanation: &PolicyExplanation) {
    tracing::info!(conflicts = explanation.warning_count(), "{STARTUP_BANNER}");
    for row in &explanation.rows {
        match row.notice {
            PolicyNotice::Warning => match &row.guidance {
                Some(guidance) => {
                    tracing::warn!(policy = %row.detail, resolution = %guidance, "policy conflict")
                }
                None => tracing::warn!(policy = %row.detail, "policy conflict"),
            },
            PolicyNotice::Info => tracing::info!(policy = %row.detail, "policy"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{INDIVIDUAL_POLICY_FILE, NotificationUrgency, TEAM_POLICY_FILE};

    fn write(dir: &Path, name: &str, body: &str) {
        let rustain = dir.join(".rustain");
        std::fs::create_dir_all(&rustain).unwrap();
        std::fs::write(rustain.join(name), body).unwrap();
    }

    /// The startup shell resolves and returns; a conflict does not stop it.
    #[test]
    fn startup_validation_returns_the_resolved_policy_despite_a_conflict() {
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
        let policy = validate_startup_policies(
            dir.path(),
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        )
        .expect("a conflict must not stop the daemon");
        assert_eq!(policy.urgency.value, NotificationUrgency::Immediate);
    }

    /// A malformed file stops the daemon rather than falling through to a
    /// permissive default.
    #[test]
    fn startup_validation_fails_on_a_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), INDIVIDUAL_POLICY_FILE, "not = = toml");
        let error = validate_startup_policies(
            dir.path(),
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        )
        .expect_err("a malformed policy file must stop startup");
        assert!(
            error.to_string().contains(INDIVIDUAL_POLICY_FILE),
            "{error}"
        );
    }

    /// A workspace with no policy files at all is a normal, silent success — the
    /// fail-closed defaults, no conflict.
    #[test]
    fn startup_validation_succeeds_with_no_policy_files() {
        let dir = tempfile::tempdir().unwrap();
        let policy = validate_startup_policies(
            dir.path(),
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        )
        .expect("no files is not an error");
        assert_eq!(
            policy.automation.value,
            crate::domain::models::ResponseMode::NotifyAndWait
        );
        assert!(!policy.team_file_present);
    }

    #[test]
    fn authority_widening_warning_has_both_positive_controls() {
        use crate::adapters::a2a::config::A2aAdmissionPolicy;
        use crate::domain::models::ResponseMode;

        let dir = tempfile::tempdir().unwrap();
        let mut policy = validate_startup_policies(
            dir.path(),
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        )
        .expect("default policy resolves");
        policy.automation.value = ResponseMode::NotifyAndAuto;
        assert!(should_warn_auto_authority_widening(
            A2aAdmissionPolicy::Allow,
            &policy,
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        ));
        assert!(!should_warn_auto_authority_widening(
            A2aAdmissionPolicy::Ask,
            &policy,
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        ));
        policy.automation.value = ResponseMode::NotifyAndWait;
        assert!(!should_warn_auto_authority_widening(
            A2aAdmissionPolicy::Allow,
            &policy,
            &[],
            &crate::adapters::policy::EmptyConsentProjection,
        ));
    }

    #[test]
    fn authority_warning_narrows_only_when_every_auto_sender_has_consent() {
        use crate::adapters::a2a::config::A2aAdmissionPolicy;
        use crate::domain::models::{
            A2aPeerSource, PinnedKey, PinnedKeyAlgorithm, RedactedUrl, ResponseMode, RoomEvent,
        };
        use base64::Engine as _;

        let peer = |alias: &str, key: [u8; 32]| A2aPeerSpec {
            id: alias.to_owned(),
            url: RedactedUrl::new("https://peer.example/a2a".to_owned()),
            pinned_key: Some(PinnedKey::new(
                PinnedKeyAlgorithm::EdDsa,
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
                None,
            )),
            source: A2aPeerSource::Workspace,
        };
        let trusted = peer("trusted", [7; 32]);
        let trusted_id = trusted.pinned_identity().unwrap();
        let projection = crate::adapters::policy::JournalConsentProjection::default();
        projection.apply(&RoomEvent::ConsentGranted {
            sender: Some(trusted_id),
            granted_at: 1,
        });
        let mut policy = validate_startup_policies(
            tempfile::tempdir().unwrap().path(),
            std::slice::from_ref(&trusted),
            &projection,
        )
        .unwrap();
        policy.automation.value = ResponseMode::NotifyAndAuto;

        assert!(
            !should_warn_auto_authority_widening(
                A2aAdmissionPolicy::Allow,
                &policy,
                std::slice::from_ref(&trusted),
                &projection,
            ),
            "a journal grant narrows the warning for the configured sender"
        );
        let override_dir = tempfile::tempdir().unwrap();
        write(
            override_dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.overrides.\"trusted\"]\nresponse_mode = \"notify-and-auto\"\n",
        );
        let override_policy = validate_startup_policies(
            override_dir.path(),
            std::slice::from_ref(&trusted),
            &crate::adapters::policy::EmptyConsentProjection,
        )
        .unwrap();
        assert!(
            !should_warn_auto_authority_widening(
                A2aAdmissionPolicy::Allow,
                &override_policy,
                std::slice::from_ref(&trusted),
                &crate::adapters::policy::EmptyConsentProjection,
            ),
            "an identity-bound TOML override implies consent"
        );
        let untrusted = peer("rotated", [8; 32]);
        assert!(
            should_warn_auto_authority_widening(
                A2aAdmissionPolicy::Allow,
                &policy,
                &[trusted, untrusted],
                &projection,
            ),
            "a rotated pin is a new untrusted sender and re-opens the warning"
        );
    }
}
