//! The NFR66 interaction-policy explainer — the CLI shell over the policy
//! decision core (Story 18.3b, AC6).
//!
//! The sibling of `adapters::daemon::policy_startup`: **same core, different
//! shell**. This one renders through `CheckResult` for `display_results`
//! (`println!`, terminal) and mirrors the structured form into `doctor --json`;
//! the daemon's renders through `tracing`. Neither reuses the other's printer,
//! because a daemon has no terminal and a CLI has no log to read back.
//!
//! Appended to `build_check_list` — no existing check is modified, per the
//! framework's own extensibility promise.

use async_trait::async_trait;
use std::sync::OnceLock;

use crate::domain::models::{A2aPeerSpec, INDIVIDUAL_POLICY_FILE, PolicySource, TEAM_POLICY_FILE};
use crate::domain::services::team_policy::{PolicyExplanation, PolicyNotice};

use super::{CheckResult, CheckStatus, CheckTier, HealthCheck};

/// Report the resolved effective interaction policy and everything that shaped it.
///
/// `CheckTier::Info`: a policy conflict is a report, not a reason for `doctor` to
/// exit non-zero. Only a malformed file is a `Fail`.
pub struct PolicyExplainerCheck {
    /// Override for testing; `None` resolves the real workspace.
    pub workspace: Option<std::path::PathBuf>,
    /// The exact workspace-plus-profile peer set used by runtime composition.
    pub peers: Vec<A2aPeerSpec>,
    /// Structured detail, filled during `run` and read back by `doctor --json`.
    pub(crate) machine: OnceLock<serde_json::Value>,
}

impl Default for PolicyExplainerCheck {
    fn default() -> Self {
        Self::new(None, Vec::new())
    }
}

impl PolicyExplainerCheck {
    pub(super) fn new(workspace: Option<std::path::PathBuf>, peers: Vec<A2aPeerSpec>) -> Self {
        Self {
            workspace,
            peers,
            machine: OnceLock::new(),
        }
    }

    fn result(&self, status: CheckStatus, message: String, fix: Option<String>) -> CheckResult {
        CheckResult {
            name: self.name().to_string(),
            category: "policy".to_string(),
            status,
            message,
            fix,
            latency: None,
            tier: CheckTier::Info,
        }
    }
}

#[async_trait]
impl HealthCheck for PolicyExplainerCheck {
    fn name(&self) -> &str {
        "Interaction policy"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(workspace) => workspace.clone(),
            None => match crate::infrastructure::paths::workspace_dir() {
                Ok(workspace) => workspace,
                Err(_) => {
                    return self.result(
                        CheckStatus::Warning,
                        "cannot determine workspace directory".to_string(),
                        None,
                    );
                }
            },
        };

        let projection =
            match crate::adapters::policy::JournalConsentProjection::load_workspace(&workspace)
                .await
            {
                Ok(projection) => projection,
                Err(error) => {
                    return self.result(
                        CheckStatus::Fail,
                        format!("consent journal did not load: {error}"),
                        Some(
                            "Repair or restore the workspace room journal before trusting the \
                             reported effective consent."
                                .to_string(),
                        ),
                    );
                }
            };
        let (policy, explanation) = match crate::adapters::policy::resolve_workspace_policy(
            &workspace,
            &self.peers,
            &projection,
        ) {
            Ok(resolved) => resolved,
            // A malformed policy file is the ONLY `Fail` here, and `fix` is
            // required for `Fail` (`doctor/mod.rs:44-55`).
            Err(error) => {
                return self.result(
                    CheckStatus::Fail,
                    format!("policy did not load: {error}"),
                    Some(
                        "Fix the reported file, or remove it to accept the documented \
                             defaults (response_mode = \"notify-and-wait\", notification = \
                             \"queue\"). Policy is fail-closed: nothing was applied."
                            .to_string(),
                    ),
                );
            }
        };

        let conflicts = explanation.warning_count();
        let _ = self.machine.set(machine_detail(&policy, &explanation));

        // The human view is the derivation, not just the answer. A check that
        // printed the effective value alone would not satisfy NFR66 — an invisible
        // merge is the failure mode this whole surface exists to prevent.
        let individual_contributed =
            matches!(policy.urgency.source, PolicySource::Individual { .. })
                || matches!(policy.automation.source, PolicySource::Individual { .. })
                || matches!(policy.sharing.source, PolicySource::Individual { .. })
                || policy.digest_interval_minutes
                    != crate::domain::models::DEFAULT_DIGEST_INTERVAL_MINUTES
                || !policy.sender_overrides.is_empty()
                || policy
                    .deferred_overrides
                    .iter()
                    .any(|deferred| deferred.file == INDIVIDUAL_POLICY_FILE);
        let individual_source = if individual_contributed {
            INDIVIDUAL_POLICY_FILE
        } else {
            "built-in defaults"
        };
        let mut message = if policy.team_file_present {
            format!("resolved from {individual_source} + {TEAM_POLICY_FILE}")
        } else {
            format!("resolved from {individual_source} (no {TEAM_POLICY_FILE})")
        };
        for row in &explanation.rows {
            message.push_str("\n    ");
            message.push_str(match row.notice {
                PolicyNotice::Warning => "! ",
                PolicyNotice::Info => "- ",
            });
            message.push_str(&row.detail);
        }

        // Resolution guidance collects into `fix`, which is where NFR66's guidance
        // belongs and is optional for `Warning`.
        let guidance: Vec<&str> = explanation
            .rows
            .iter()
            .filter_map(|row| row.guidance.as_deref())
            .collect();
        let fix = (!guidance.is_empty()).then(|| guidance.join("\n    "));

        let status = if conflicts == 0 {
            CheckStatus::Info
        } else {
            CheckStatus::Warning
        };
        self.result(status, message, fix)
    }

    fn machine_detail(&self) -> Option<serde_json::Value> {
        self.machine.get().cloned()
    }
}

/// The machine-readable mirror of the human view.
///
/// Carries the same fields, per-dimension: value, provenance, **and the
/// `(individual, team)` pair**. A surface that only exists in the human view is
/// half a surface.
fn machine_detail(
    policy: &crate::domain::models::EffectivePolicy,
    explanation: &PolicyExplanation,
) -> serde_json::Value {
    use serde_json::json;

    json!({
        "notification_urgency": {
            "effective": policy.urgency.value.as_str(),
            "individual": policy.urgency.individual.as_str(),
            "team": policy.urgency.team.map(|value| value.as_str()),
            "source": policy.urgency.source.label(),
            "source_file": policy.urgency.source.file(),
            "merge": "max(individual, team)",
        },
        "response_automation": {
            "effective": policy.automation.value.as_str(),
            "individual": policy.automation.individual.as_str(),
            "team": policy.automation.team.map(|value| value.as_str()),
            "source": policy.automation.source.label(),
            "source_file": policy.automation.source.file(),
            "merge": "min(individual, team)",
        },
        "sharing_breadth": {
            "effective": policy.sharing.effective,
            "individual": policy.sharing.individual,
            "team_norm": policy.sharing.team_norm,
            "source": policy.sharing.source.label(),
            "source_file": policy.sharing.source.file(),
            "merge": "not-merged",
            "enforced": false,
        },
        "digest_interval_minutes": policy.digest_interval_minutes,
        "team_file_present": policy.team_file_present,
        "deferred_overrides": policy.deferred_overrides,
        "transparency_invariants": policy.transparency_invariants,
        "sender_overrides": policy.sender_overrides,
        "sender_conflicts": policy.sender_conflicts,
        "consent": explanation.consent.iter().map(|line| json!({
            "sender": line.sender,
            "source": match line.source {
                crate::domain::services::team_policy::ConsentSource::Journaled => "journaled",
                crate::domain::services::team_policy::ConsentSource::TomlImplied => "toml-implied",
            },
            "state": match line.state {
                crate::domain::ports::ConsentState::Trusted => "trusted",
                crate::domain::ports::ConsentState::Revoked => "revoked",
                crate::domain::ports::ConsentState::None => "none",
            },
        })).collect::<Vec<_>>(),
        "journal_projection_empty": !explanation.consent.iter().any(|line| {
            line.source == crate::domain::services::team_policy::ConsentSource::Journaled
        }),
        "rows": explanation.rows.iter().map(|row| json!({
            "key": row.key,
            "detail": row.detail,
            "guidance": row.guidance,
            "notice": match row.notice {
                PolicyNotice::Warning => "warning",
                PolicyNotice::Info => "info",
            },
        })).collect::<Vec<_>>(),
        "conflicts": explanation.warning_count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::IndividualPolicy;
    use crate::domain::ports::ConsentState;
    use crate::domain::services::team_policy::{
        ConsentLine, ConsentSource, explain_effective_policy, resolve_effective_policy,
    };

    #[test]
    fn machine_detail_preserves_typed_consent_fields() {
        let policy = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
        let explanation = explain_effective_policy(
            &policy,
            &[ConsentLine {
                sender: "known-alias".to_owned(),
                source: ConsentSource::Journaled,
                state: ConsentState::Trusted,
            }],
        );

        let detail = machine_detail(&policy, &explanation);
        assert_eq!(
            detail["consent"][0],
            serde_json::json!({
                "sender": "known-alias",
                "source": "journaled",
                "state": "trusted",
            })
        );
        assert_eq!(detail["journal_projection_empty"], false);
        assert!(detail["sharing_breadth"]["source_file"].is_null());
    }

    #[tokio::test]
    async fn doctor_reads_trusted_then_revoked_state_from_the_real_journal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let sender = crate::domain::models::PeerId::from_public_key(&[7u8; 32]).unwrap();
        journal
            .append_room(crate::domain::models::RoomEvent::ConsentGranted {
                sender: Some(sender.clone()),
                granted_at: 10,
            })
            .await
            .unwrap();

        let trusted = PolicyExplainerCheck::new(Some(workspace.path().to_path_buf()), Vec::new());
        let result = trusted.run().await;
        assert_eq!(result.status, CheckStatus::Info);
        assert_eq!(
            trusted.machine_detail().unwrap()["consent"][0]["state"],
            "trusted"
        );

        journal
            .append_room(crate::domain::models::RoomEvent::ConsentRevoked {
                sender: Some(sender),
                revoked_at: 20,
            })
            .await
            .unwrap();
        let revoked = PolicyExplainerCheck::new(Some(workspace.path().to_path_buf()), Vec::new());
        let result = revoked.run().await;
        assert_eq!(result.status, CheckStatus::Info);
        assert_eq!(
            revoked.machine_detail().unwrap()["consent"][0]["state"],
            "revoked"
        );
    }

    #[tokio::test]
    async fn doctor_missing_journal_stays_explicitly_empty_without_creating_files() {
        let workspace = tempfile::TempDir::new().unwrap();
        let check = PolicyExplainerCheck::new(Some(workspace.path().to_path_buf()), Vec::new());

        let result = check.run().await;

        assert_eq!(result.status, CheckStatus::Info);
        assert!(
            result
                .message
                .contains("no journaled consent grants recorded")
        );
        assert!(!workspace.path().join(".rustain").exists());
    }
}
