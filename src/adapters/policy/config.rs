//! Parse the two interaction-policy files — **deliberately not through figment**.
//!
//! # Why this is not a figment layer (read before "simplifying" this module)
//!
//! `crate::infrastructure::config` (see its module docs, `config.rs:1-19`) loads
//! `.rustain/config.toml` through **figment** with an 8-layer precedence and
//! documented **field-level merge**: "adding `[pricing."my-model"]` ADDS to the
//! default catalog instead of REPLACING it", and later layers override earlier ones
//! at the key level.
//!
//! Folding these two files into that stack would be **silently, dangerously
//! wrong**. Figment's semantic is *later layer overrides earlier*. FR96's semantic
//! is a **stricter-wins lattice over two quantities of opposite polarity** plus one
//! that is not merged at all. A field-level override of a sharing or automation
//! setting produces a **looser** effective policy than the lattice — and it would
//! look like it worked: green tests, wrong answer, no error.
//!
//! So: `toml` 0.8 directly, our own error enum, no figment. The precedent is
//! `crate::adapters::a2a::config` for `.rustain/a2a.json` — own error enum,
//! `#[non_exhaustive]`, per-file `Read`/parse variants, and parsed **without** the
//! `a2a` feature so a misconfigured build fails loudly at startup instead of
//! silently ignoring the operator's intent. Policy resolution is feature-independent
//! for exactly the same reason.
//!
//! # Fail-closed
//!
//! A **missing** file yields the documented defaults (`notify-and-wait` + `queue`
//! — the most restrictive pair, so the agent does nothing on your behalf). A
//! **malformed** file is a hard, named error that never falls through to a
//! permissive default: a config error must never silently upgrade the agent's
//! autonomy.
//!
//! Unknown fields are rejected, and an unrecognised `response_mode` is an error
//! rather than a silent default. Contrast 18-2's `RoomEvent` `#[serde(other)]
//! Unrecognized`, which exists so a durable journal survives forward-version reads:
//! a config file is authored by a human *right now*, and forgiving it hides their
//! typo.

use std::path::{Path, PathBuf};

use crate::domain::models::{
    INDIVIDUAL_POLICY_FILE, IndividualPolicy, TEAM_POLICY_FILE, TeamPolicy,
};

/// Failure to load an interaction-policy file.
///
/// Every variant names the file, because "policy failed to parse" is useless when
/// there are two of them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyConfigError {
    #[error("reading policy file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} is malformed and was NOT applied: {source}\n\
         Policy is fail-closed: rather than fall back to a permissive default, \
         startup refuses. Fix the file or remove it to accept the documented \
         defaults (response_mode = \"notify-and-wait\", notification = \"queue\")."
    )]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("serializing policy for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
}

impl PolicyConfigError {
    /// The file the failure belongs to.
    pub fn path(&self) -> &Path {
        match self {
            Self::Read { path, .. } | Self::Parse { path, .. } | Self::Serialize { path, .. } => {
                path
            }
        }
    }
}

/// Both policy files, loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyFiles {
    pub individual: IndividualPolicy,
    /// `None` when no team file exists — which is different from an empty one:
    /// an absent file contributes nothing to the lattice, while a present-but-empty
    /// one still carries `[team.transparency]` invariants the explainer reports.
    pub team: Option<TeamPolicy>,
    pub individual_path: PathBuf,
    pub team_path: PathBuf,
}

/// `.rustain/` inside a workspace.
fn policy_dir(workspace: &Path) -> PathBuf {
    workspace.join(".rustain")
}

/// Path of the individual policy file for a workspace.
pub fn individual_policy_path(workspace: &Path) -> PathBuf {
    policy_dir(workspace).join(INDIVIDUAL_POLICY_FILE)
}

/// Path of the team policy file for a workspace.
pub fn team_policy_path(workspace: &Path) -> PathBuf {
    policy_dir(workspace).join(TEAM_POLICY_FILE)
}

/// Load both policy files from a workspace.
///
/// Missing files are defaults; malformed files are errors.
pub fn load_workspace_policies(workspace: &Path) -> Result<PolicyFiles, PolicyConfigError> {
    let individual_path = individual_policy_path(workspace);
    let team_path = team_policy_path(workspace);
    Ok(PolicyFiles {
        individual: parse_individual_policy(&individual_path)?.unwrap_or_default(),
        team: parse_team_policy(&team_path)?,
        individual_path,
        team_path,
    })
}

/// The root of `a2a-interaction.toml`: everything sits under `[interaction]`
/// (`prd.md:576`).
///
/// A wrapper rather than a flattened domain struct, mirroring
/// `crate::adapters::a2a::config::WorkspaceRoot`: the file's shape is the
/// adapter's business, and the domain type stays free of the outer table name.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IndividualRoot {
    #[serde(default)]
    interaction: IndividualPolicy,
}

/// The root of `team-policy.toml`: everything sits under `[team]`
/// (`prd.md:846`).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamRoot {
    #[serde(default)]
    team: TeamPolicy,
}

/// Parse `a2a-interaction.toml`. `Ok(None)` means the file does not exist.
pub fn parse_individual_policy(path: &Path) -> Result<Option<IndividualPolicy>, PolicyConfigError> {
    Ok(parse_policy_file::<IndividualRoot>(path)?.map(|root| root.interaction))
}

/// Parse `team-policy.toml`. `Ok(None)` means the file does not exist.
pub fn parse_team_policy(path: &Path) -> Result<Option<TeamPolicy>, PolicyConfigError> {
    Ok(parse_policy_file::<TeamRoot>(path)?.map(|root| root.team))
}

fn parse_policy_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, PolicyConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PolicyConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| PolicyConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

/// Render an individual policy back to TOML, `[interaction]` root included.
///
/// One schema, both directions: 18-3c's `rustain init` writes this file, and a
/// writer that drifts from the reader fails silently. The round-trip test in this
/// module is what keeps them honest.
pub fn render_individual_policy(policy: &IndividualPolicy) -> Result<String, PolicyConfigError> {
    toml::to_string_pretty(&IndividualRoot {
        interaction: policy.clone(),
    })
    .map_err(|source| PolicyConfigError::Serialize {
        path: PathBuf::from(INDIVIDUAL_POLICY_FILE),
        source,
    })
}

/// Render a team policy back to TOML, `[team]` root included.
pub fn render_team_policy(policy: &TeamPolicy) -> Result<String, PolicyConfigError> {
    toml::to_string_pretty(&TeamRoot {
        team: policy.clone(),
    })
    .map_err(|source| PolicyConfigError::Serialize {
        path: PathBuf::from(TEAM_POLICY_FILE),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        DEFAULT_DIGEST_INTERVAL_MINUTES, MessageTypeOverride, NotificationUrgency, ResponseMode,
        SenderOverride,
    };

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let rustain = dir.join(".rustain");
        std::fs::create_dir_all(&rustain).unwrap();
        let path = rustain.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    // ── AC2: missing → defaults, malformed → hard error ──

    /// Missing files retain field absence so the decision core can apply the
    /// fail-closed values while reporting `default` provenance.
    #[test]
    fn a_missing_file_preserves_absence_for_default_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_workspace_policies(dir.path()).expect("missing files are not an error");
        assert_eq!(loaded.individual.defaults.response_mode, None);
        assert_eq!(loaded.individual.defaults.notification, None);
        assert_eq!(loaded.team, None);
    }

    /// The failure path is exercised by *causing* the failure, not by reading the
    /// code — the 18-1b/18-2 fail-open defect class.
    #[test]
    fn a_malformed_individual_file_is_a_hard_named_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), INDIVIDUAL_POLICY_FILE, "this is not = = toml");
        let error = load_workspace_policies(dir.path())
            .expect_err("a malformed file must not fall through to a permissive default");
        assert!(matches!(error, PolicyConfigError::Parse { .. }));
        assert!(error.path().ends_with(INDIVIDUAL_POLICY_FILE));
        assert!(
            error.to_string().contains("fail-closed"),
            "the error must explain why nothing was applied: {error}"
        );
    }

    #[test]
    fn a_malformed_team_file_is_a_hard_named_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), TEAM_POLICY_FILE, "[team.defaults\n");
        let error = load_workspace_policies(dir.path()).expect_err("malformed team file");
        assert!(error.path().ends_with(TEAM_POLICY_FILE));
    }

    /// Positive control: the parser CAN reach the permissive value when the
    /// operator actually asks for it. Without this, a parser hard-wired to
    /// `notify-and-wait` would pass every fail-closed test above.
    #[test]
    fn positive_control_an_operator_can_reach_notify_and_auto() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\n",
        );
        let loaded = load_workspace_policies(dir.path()).unwrap();
        assert_eq!(
            loaded.individual.defaults.response_mode,
            Some(ResponseMode::NotifyAndAuto)
        );
    }

    #[test]
    fn an_unrecognised_response_mode_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nresponse_mode = \"yolo\"\n",
        );
        let error = load_workspace_policies(dir.path()).expect_err("unknown variant must fail");
        assert!(error.to_string().contains("yolo"), "{error}");
    }

    #[test]
    fn an_unknown_top_level_field_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\nrespose_mode = \"notify-and-wait\"\n",
        );
        assert!(load_workspace_policies(dir.path()).is_err());
    }

    /// 🔗 The `digest_interval_minutes` regression guard. `deny_unknown_fields`
    /// plus a missing key would be a hard break: 18-3c's `rustain init` writes
    /// `[interaction.defaults]` into this exact file including this key, so every
    /// init-written file would fail to parse in 18-3b's own loader.
    ///
    /// The fixture is written the way the addendum's §5 wizard screen would write
    /// it.
    #[test]
    fn a_file_written_by_the_18_3c_init_wizard_parses() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            r#"
# Written by `rustain init`.
[interaction.defaults]
response_mode = "notify-and-draft"
notification = "digest"
digest_interval_minutes = 30
"#,
        );
        let loaded = load_workspace_policies(dir.path()).expect("init-written file must parse");
        assert_eq!(loaded.individual.defaults.digest_interval_minutes, 30);
        assert_eq!(
            loaded.individual.defaults.notification,
            Some(NotificationUrgency::Digest)
        );
    }

    #[test]
    fn digest_interval_defaults_to_fifteen_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.defaults]\n",
        );
        let loaded = load_workspace_policies(dir.path()).unwrap();
        assert_eq!(
            loaded.individual.defaults.digest_interval_minutes,
            DEFAULT_DIGEST_INTERVAL_MINUTES
        );
    }

    /// The PRD's own team-file sample must parse: both tiers plus the invariant
    /// block (`prd.md:842-871`).
    #[test]
    fn the_prd_team_policy_sample_parses_with_both_tiers() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            TEAM_POLICY_FILE,
            r#"
[team.defaults]
response_mode = "notify-and-wait"
notification = "queue"

[team.overrides]
story_assignment_notification = "immediate"
architecture_updates = "immediate"
bug_reports = "immediate"
status_request_response = "notify-and-auto"
status_detail_minimum = "story-and-blockers"

[team.transparency]
retract_always_available = true
transparency_log_visible_to_self = true
transparency_log_visible_to_others = false
auto_response_always_marked = true
"#,
        );
        let team = load_workspace_policies(dir.path())
            .unwrap()
            .team
            .expect("team file present");
        // The type-agnostic tier is what the merge consumes.
        assert_eq!(
            team.defaults.response_mode,
            Some(ResponseMode::NotifyAndWait)
        );
        assert_eq!(team.defaults.notification, Some(NotificationUrgency::Queue));
        // The type-keyed tier parses but is never resolved.
        assert_eq!(team.overrides.configured_keys().len(), 4);
        assert_eq!(
            team.overrides.status_detail_minimum.as_deref(),
            Some("story-and-blockers")
        );
        assert!(team.transparency.retract_always_available);
        assert!(!team.transparency.transparency_log_visible_to_others);
    }

    /// A present-but-empty team file is NOT the same as an absent one: it still
    /// contributes `[team.transparency]` defaults for the explainer to report.
    #[test]
    fn an_empty_team_file_is_distinguishable_from_an_absent_one() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), TEAM_POLICY_FILE, "");
        let loaded = load_workspace_policies(dir.path()).unwrap();
        let team = loaded.team.expect("an empty file still parses to a policy");
        assert_eq!(team.defaults.response_mode, None);
        assert_eq!(team.defaults.notification, None);
        assert!(team.transparency.retract_always_available);
    }

    /// Per-type overrides parse into the schema without pretending they resolve
    /// (Trap 2, `DF-18-3b-MSGTYPE`).
    #[test]
    fn per_sender_and_per_type_overrides_parse_from_the_prd_shape() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            r#"
[interaction.defaults]
response_mode = "notify-and-wait"
notification = "queue"

[interaction.overrides."marcus-arch"]
response_mode = "notify-and-draft"
notification = "immediate"

[interaction.overrides."lena-po".story_assignment]
response_mode = "notify-and-auto"
auto_response = "Received. I'll review the story spec and confirm."
notification = "immediate"

[interaction.overrides."*".status_request]
response_mode = "notify-and-auto"
notification = "queue"
"#,
        );
        let individual = load_workspace_policies(dir.path()).unwrap().individual;
        assert_eq!(individual.overrides.len(), 3);
        assert_eq!(
            individual.overrides["marcus-arch"].response_mode,
            Some(ResponseMode::NotifyAndDraft)
        );
        assert!(individual.overrides["marcus-arch"].per_type.is_empty());
        assert_eq!(
            individual.overrides["lena-po"].per_type["story_assignment"].auto_response,
            Some("Received. I'll review the story spec and confirm.".to_owned())
        );
        assert!(
            individual.overrides["*"]
                .per_type
                .contains_key("status_request")
        );
    }

    /// A misspelled scalar inside a sender block must be an error rather than be
    /// swallowed as a novel message type.
    #[test]
    fn a_typo_inside_a_sender_block_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.overrides.\"marcus-arch\"]\nrespone_mode = \"notify-and-auto\"\n",
        );
        let error = load_workspace_policies(dir.path()).expect_err("a typo must be rejected");
        assert!(error.to_string().contains("respone_mode"), "{error}");
    }

    // ── AC2 🔗 round-trip: the reader and the 18-3c writer cannot diverge ──
    //
    // Both round-trips re-read through `load_workspace_policies` — the same
    // function the daemon and `doctor` call — rather than through a bare
    // `toml::from_str`. Serde symmetry on the domain struct is not the property
    // that matters; agreement with the *shipped reader*, root table and all, is.
    #[test]
    fn malformed_declared_peer_id_is_a_named_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            INDIVIDUAL_POLICY_FILE,
            "[interaction.overrides.peer]\npeer_id = \"not-hex\"\n",
        );
        let error = load_workspace_policies(dir.path())
            .expect_err("an invalid identity binding must fail closed");
        assert!(error.to_string().contains("peer_id"), "{error}");
        assert!(error.path().ends_with(INDIVIDUAL_POLICY_FILE));
    }

    #[test]
    fn individual_policy_round_trips_through_toml() {
        let mut per_type = std::collections::BTreeMap::new();
        per_type.insert(
            "story_assignment".to_owned(),
            MessageTypeOverride {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Immediate),
                auto_response: Some("Received.".to_owned()),
            },
        );
        let mut original = IndividualPolicy {
            defaults: crate::domain::models::IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndDraft),
                notification: Some(NotificationUrgency::Digest),
                digest_interval_minutes: 45,
                status_detail_minimum: Some("story-and-blockers".to_owned()),
            },
            ..IndividualPolicy::default()
        };
        original.overrides.insert(
            "marcus-arch".to_owned(),
            SenderOverride {
                peer_id: Some(
                    crate::domain::models::PeerId::from_public_key(&[1u8; 32])
                        .unwrap()
                        .as_str()
                        .to_owned(),
                ),
                response_mode: Some(ResponseMode::NotifyAndDraft),
                notification: Some(NotificationUrgency::Immediate),
                auto_response: None,
                per_type,
            },
        );

        let rendered = render_individual_policy(&original).expect("policy renders");
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), INDIVIDUAL_POLICY_FILE, &rendered);
        let reparsed = load_workspace_policies(dir.path())
            .unwrap_or_else(|e| panic!("the shipped reader rejected our writer:\n{rendered}\n{e}"))
            .individual;
        assert_eq!(reparsed, original);
    }

    #[test]
    fn team_policy_round_trips_through_toml() {
        let original = TeamPolicy {
            defaults: crate::domain::models::TeamDefaults {
                response_mode: Some(ResponseMode::NotifyAndDraft),
                notification: Some(NotificationUrgency::Immediate),
            },
            overrides: crate::domain::models::TeamOverrides {
                story_assignment_notification: Some(NotificationUrgency::Immediate),
                architecture_updates: None,
                bug_reports: Some(NotificationUrgency::Immediate),
                status_request_response: Some(ResponseMode::NotifyAndAuto),
                status_detail_minimum: Some("story-and-blockers".to_owned()),
            },
            transparency: crate::domain::models::TeamTransparency::default(),
        };
        let rendered = render_team_policy(&original).expect("policy renders");
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), TEAM_POLICY_FILE, &rendered);
        let reparsed = load_workspace_policies(dir.path())
            .unwrap_or_else(|e| panic!("the shipped reader rejected our writer:\n{rendered}\n{e}"))
            .team
            .expect("a rendered team policy must read back as present");
        assert_eq!(reparsed, original);
    }

    /// A read failure that is not `NotFound` must surface, not read as "missing".
    #[test]
    #[cfg(unix)]
    fn a_directory_where_a_policy_file_belongs_is_a_read_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".rustain").join(INDIVIDUAL_POLICY_FILE)).unwrap();
        let error = load_workspace_policies(dir.path())
            .expect_err("a directory in the file's place is not `missing`");
        assert!(matches!(error, PolicyConfigError::Read { .. }));
    }
}
