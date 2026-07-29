//! The FR96 policy lattice and the NFR66 explainer — two composed decision cores
//! (Story 18.3b).
//!
//! Effect-free, value-returning, deterministically testable. No `tokio`, no I/O,
//! no lock: file reading is `crate::adapters::policy::config`, and the two shells
//! that render the result are the daemon startup path and `rustain doctor`.
//! One decision, two shells.
//!
//! # The two tiers
//!
//! 1. [`resolve_effective_policy`] — *what is my effective policy?* A projection
//!    fold `(IndividualPolicy, Option<&TeamPolicy>) -> EffectivePolicy`.
//! 2. [`explain_effective_policy`] — *what happened to each quantity, and why?*
//!    Consumes tier 1's output plus the consent projection and produces the rows
//!    both shells render.
//!
//! Tier 2 composes over tier 1 rather than duplicating it (`architecture.md:1744`).
//! Startup reports tier 2's warnings; `doctor` renders every row.
//!
//! # The lattice
//!
//! ```text
//! urgency     effective = max(individual, team)   binds UPWARD
//! automation  effective = min(individual, team)   binds DOWNWARD
//! sharing     effective = individual              not merged at all
//! ```
//!
//! The load-bearing invariant is `effective_sharing ⊆ individual_consent` for
//! every `(individual, team)` pair. On sharing it now holds *trivially* because
//! nothing is merged — which is exactly why the property test's non-trivial case
//! is **automation**, and why the positive controls live on the two binding
//! quantities. A resolver that returned `individual` unchanged for everything
//! would satisfy every invariant vacuously.

use std::collections::BTreeMap;

use crate::domain::models::{
    A2aPeerSpec, DeferredKey, EffectivePolicy, INDIVIDUAL_POLICY_FILE, IndividualPolicy,
    MSGTYPE_DEFERRAL, NotificationUrgency, PeerId, PolicySource, Resolved, ResponseMode,
    SenderBinding, SenderIdentity, SenderIdentityConflict, SenderPolicy, SharingBreadth,
    TEAM_POLICY_FILE, TeamPolicy, TransparencyInvariant,
};

/// The deferral that owns sharing-breadth enforcement semantics.
pub const SHARING_DEFERRAL: &str = "DF-18-3b-SHARING-SEMANTICS";

/// The deferral that owns the consent projection this story stubs.
pub const CONSENT_DEFERRAL: &str = "DF-18-3b-CONSENT-PROJECTION";

// ──────────────────────────────────────────────────────────────────
// Tier 1 — the merge
// ──────────────────────────────────────────────────────────────────

/// Fold an individual policy and an optional team policy into one effective
/// policy under the stricter-wins lattice.
///
/// Pure. Given the same inputs it returns the same value, and it touches nothing
/// outside them.
pub fn resolve_effective_policy(
    individual: &IndividualPolicy,
    team: Option<&TeamPolicy>,
    peers: &[A2aPeerSpec],
) -> EffectivePolicy {
    let authored_urgency = individual.defaults.notification;
    let authored_automation = individual.defaults.response_mode;
    let individual_urgency = authored_urgency.unwrap_or_default();
    let individual_automation = authored_automation.unwrap_or_default();
    let team_urgency = team.and_then(|team| team.defaults.notification);
    let team_automation = team.and_then(|team| team.defaults.response_mode);

    // Notification urgency: louder is stricter, so the team binds UPWARD.
    //
    // Mutant to keep RED: flipping this `max` to `min` would let a member's
    // `digest` silence a team floor of `immediate`.
    let urgency_value = match team_urgency {
        Some(team) => individual_urgency.max(team),
        None => individual_urgency,
    };

    // Response automation: less autonomy is stricter, so the team binds DOWNWARD.
    //
    // Mutant to keep RED: flipping this `min` to `max` is the polarity error that
    // sat in `prd.md:1304` for months — it would let a team agreement *raise* a
    // member's automation above what they consented to.
    let automation_value = match team_automation {
        Some(team) => individual_automation.min(team),
        None => individual_automation,
    };

    let urgency = Resolved {
        value: urgency_value,
        source: raise_source(
            individual_urgency,
            urgency_value,
            authored_urgency.is_some(),
            team_urgency.is_some(),
        ),
        individual: individual_urgency,
        team: team_urgency,
    };
    let automation = Resolved {
        value: automation_value,
        source: cap_source(
            individual_automation,
            automation_value,
            authored_automation.is_some(),
            team_automation.is_some(),
        ),
        individual: individual_automation,
        team: team_automation,
    };

    // Sharing breadth is NOT merged. Neither `max` (which would force disclosure
    // past consent, violating FR96) nor `min` (which would cap a member who
    // chooses to share more, violating `prd.md:863`) is correct, because the
    // quantity binds in neither direction. The value passes through untouched and
    // the team's is carried alongside as a displayed norm.
    let individual_sharing = individual.defaults.status_detail_minimum.clone();
    let sharing = SharingBreadth {
        effective: individual_sharing.clone(),
        source: if individual_sharing.is_some() {
            PolicySource::Individual {
                file: INDIVIDUAL_POLICY_FILE.to_owned(),
            }
        } else {
            PolicySource::Default
        },
        individual: individual_sharing,
        team_norm: team.and_then(|team| team.overrides.status_detail_minimum.clone()),
    };

    let (sender_overrides, sender_conflicts) = resolve_sender_overrides(individual, team, peers);
    EffectivePolicy {
        urgency,
        automation,
        sharing,
        digest_interval_minutes: individual.defaults.digest_interval_minutes,
        team_file_present: team.is_some(),
        deferred_overrides: collect_deferred_overrides(individual, team),
        transparency_invariants: collect_transparency_invariants(team),
        sender_overrides,
        sender_conflicts,
    }
}

/// Provenance for an upward-binding quantity.
fn raise_source<T: PartialEq>(
    individual: T,
    effective: T,
    individual_authored: bool,
    team_present: bool,
) -> PolicySource {
    if team_present && effective != individual {
        PolicySource::TeamRaised {
            file: TEAM_POLICY_FILE.to_owned(),
        }
    } else if individual_authored {
        PolicySource::Individual {
            file: INDIVIDUAL_POLICY_FILE.to_owned(),
        }
    } else {
        PolicySource::Default
    }
}

/// Provenance for a downward-binding quantity.
fn cap_source<T: PartialEq>(
    individual: T,
    effective: T,
    individual_authored: bool,
    team_present: bool,
) -> PolicySource {
    if team_present && effective != individual {
        PolicySource::TeamCapped {
            file: TEAM_POLICY_FILE.to_owned(),
        }
    } else if individual_authored {
        PolicySource::Individual {
            file: INDIVIDUAL_POLICY_FILE.to_owned(),
        }
    } else {
        PolicySource::Default
    }
}

/// Every per-message-type key either file configured, all of them unenforceable.
///
/// `MessageKind` has three transport variants and `MessageHeader` carries no
/// semantic type field, so there is no key to match on in production today. These
/// are surfaced rather than dropped: the operator wrote them.
fn collect_deferred_overrides(
    individual: &IndividualPolicy,
    team: Option<&TeamPolicy>,
) -> Vec<DeferredKey> {
    let mut deferred = Vec::new();
    for (alias, override_) in &individual.overrides {
        for key in override_.per_type.keys() {
            deferred.push(DeferredKey {
                key: format!("interaction.overrides.\"{alias}\".{key}"),
                file: INDIVIDUAL_POLICY_FILE.to_owned(),
                deferral: MSGTYPE_DEFERRAL.to_owned(),
            });
        }
        if alias == "*" {
            for (key, configured) in [
                ("response_mode", override_.response_mode.is_some()),
                ("notification", override_.notification.is_some()),
                ("auto_response", override_.auto_response.is_some()),
            ] {
                if configured {
                    deferred.push(DeferredKey {
                        key: format!("interaction.overrides.\"*\".{key}"),
                        file: INDIVIDUAL_POLICY_FILE.to_owned(),
                        deferral: MSGTYPE_DEFERRAL.to_owned(),
                    });
                }
            }
        }
    }
    if let Some(team) = team {
        for key in team.overrides.configured_keys() {
            deferred.push(DeferredKey {
                key: format!("team.overrides.{key}"),
                file: TEAM_POLICY_FILE.to_owned(),
                deferral: MSGTYPE_DEFERRAL.to_owned(),
            });
        }
    }
    deferred
}

fn collect_transparency_invariants(team: Option<&TeamPolicy>) -> Vec<TransparencyInvariant> {
    let Some(team) = team else {
        return Vec::new();
    };
    team.transparency
        .keys()
        .into_iter()
        .map(|(key, configured, enforced)| TransparencyInvariant {
            key: key.to_owned(),
            configured,
            enforced,
            file: TEAM_POLICY_FILE.to_owned(),
        })
        .collect()
}

/// Resolve per-sender overrides, clamp every authored value through the same
/// team lattice as the global defaults, and remove ambiguous identity bindings.
fn resolve_sender_overrides(
    individual: &IndividualPolicy,
    team: Option<&TeamPolicy>,
    peers: &[A2aPeerSpec],
) -> (Vec<SenderPolicy>, Vec<SenderIdentityConflict>) {
    let team_urgency = team.and_then(|team| team.defaults.notification);
    let team_automation = team.and_then(|team| team.defaults.response_mode);
    let mut policies: Vec<SenderPolicy> = individual
        .overrides
        .iter()
        .filter(|(alias, _)| alias.as_str() != "*")
        .map(|(alias, override_)| {
            let response_mode = override_.response_mode.map(|individual| {
                let value = team_automation.map_or(individual, |team| individual.min(team));
                Resolved {
                    value,
                    source: cap_source(individual, value, true, team_automation.is_some()),
                    individual,
                    team: team_automation,
                }
            });
            let notification = override_.notification.map(|individual| {
                let value = team_urgency.map_or(individual, |team| individual.max(team));
                Resolved {
                    value,
                    source: raise_source(individual, value, true, team_urgency.is_some()),
                    individual,
                    team: team_urgency,
                }
            });
            SenderPolicy {
                alias: alias.clone(),
                identity: resolve_sender_identity(alias, override_.peer_id.as_deref(), peers),
                response_mode,
                notification,
                auto_response: override_.auto_response.clone(),
                deferred_types: override_.per_type.keys().cloned().collect(),
            }
        })
        .collect();

    let mut aliases_by_identity: BTreeMap<PeerId, Vec<String>> = BTreeMap::new();
    for policy in &policies {
        if let Some(peer_id) = policy.identity.peer_id() {
            aliases_by_identity
                .entry(peer_id.clone())
                .or_default()
                .push(policy.alias.clone());
        }
    }
    let conflicts: Vec<SenderIdentityConflict> = aliases_by_identity
        .into_iter()
        .filter_map(|(peer_id, aliases)| {
            (aliases.len() > 1).then_some(SenderIdentityConflict { peer_id, aliases })
        })
        .collect();
    policies.retain(|policy| {
        !conflicts.iter().any(|conflict| {
            policy
                .identity
                .peer_id()
                .is_some_and(|peer_id| peer_id == &conflict.peer_id)
        })
    });
    (policies, conflicts)
}

/// Bind one override to a peer identity.
///
/// 1. An explicit `peer_id` matches on the **pinned identity**, so the alias is
///    irrelevant and a rename cannot break the binding.
/// 2. Otherwise the alias is looked up in `a2a.json` as an authoring convenience,
///    and a pinned peer's identity takes over from there.
/// 3. An unpinned or unknown peer yields a *reported*, never-silently-granted
///    outcome. Reporting is the boundary: refusing unknown peers would make this
///    an admission gate, and admission is FR157 / Story 18.4.
pub fn resolve_sender_identity(
    alias: &str,
    declared_peer_id: Option<&str>,
    peers: &[A2aPeerSpec],
) -> SenderIdentity {
    if let Some(declared) = declared_peer_id {
        let Ok(declared) = PeerId::parse(declared) else {
            // Production TOML rejects this in `SenderOverride::deserialize`.
            return SenderIdentity::Unknown;
        };
        return match peers
            .iter()
            .find(|peer| peer.pinned_identity().as_ref() == Some(&declared))
        {
            Some(_) => SenderIdentity::Pinned {
                peer_id: declared,
                binding: SenderBinding::DeclaredPeerId,
            },
            None => SenderIdentity::DeclaredMismatch { peer_id: declared },
        };
    }

    match peers.iter().find(|peer| peer.id == alias) {
        Some(peer) => match peer.pinned_identity() {
            Some(peer_id) => SenderIdentity::Pinned {
                peer_id,
                binding: SenderBinding::AliasLookup,
            },
            None if peer.pinned_key.is_some() => SenderIdentity::InvalidPin,
            None => SenderIdentity::Unpinned {
                pseudonym: peer.alias_pseudonym(),
            },
        },
        None => SenderIdentity::Unknown,
    }
}

/// Look a resolved override up by the identity a delivery would carry.
///
/// The lookup 18-3c will drive. Keyed on `PeerId`, so it is immune to alias
/// churn by construction.
pub fn sender_policy_for<'a>(
    policy: &'a EffectivePolicy,
    sender: &PeerId,
) -> Option<&'a SenderPolicy> {
    policy
        .sender_overrides
        .iter()
        .find(|override_| override_.identity.peer_id() == Some(sender))
}

// ──────────────────────────────────────────────────────────────────
// Tier 2 — the explainer
// ──────────────────────────────────────────────────────────────────

/// Whether a row is merely informative or wants the operator's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyNotice {
    Info,
    Warning,
}

/// One line of the NFR66 explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRow {
    /// Stable machine key, mirrored into `doctor --json`.
    pub key: String,
    /// The human sentence.
    pub detail: String,
    /// Resolution guidance — NFR66's "reported with resolution guidance".
    pub guidance: Option<String>,
    pub notice: PolicyNotice,
}

/// What the consent projection knows about one sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentLine {
    /// How the sender is displayed — an alias when one is known, else the id.
    pub sender: String,
    /// `journaled` when the projection produced it, `toml-implied` when the only
    /// live source is the policy file.
    pub source: ConsentSource,
    pub state: crate::domain::ports::ConsentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentSource {
    Journaled,
    TomlImplied,
}

/// The typed derivation both shells render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyExplanation {
    pub rows: Vec<PolicyRow>,
    pub consent: Vec<ConsentLine>,
}

impl PolicyExplanation {
    /// Rows worth surfacing at startup. A clean policy pair yields none.
    pub fn warnings(&self) -> impl Iterator<Item = &PolicyRow> {
        self.rows
            .iter()
            .filter(|row| row.notice == PolicyNotice::Warning)
    }

    pub fn warning_count(&self) -> usize {
        self.warnings().count()
    }
}

/// Explain what happened to every quantity, and never stay silent about a source.
///
/// Each of the three quantities gets its **own sentence**. A single templated
/// "floor" sentence reused across all three is a defect: on automation it names
/// the wrong direction, and on sharing it claims an enforcement that does not
/// exist.
pub fn explain_effective_policy(
    policy: &EffectivePolicy,
    consent: &[ConsentLine],
) -> PolicyExplanation {
    let mut rows = Vec::new();

    // ── notification urgency: the only quantity where "which floor bit" is a
    //    truthful question ──
    let urgency = &policy.urgency;
    match (&urgency.source, urgency.team) {
        (PolicySource::TeamRaised { file }, Some(team)) => rows.push(PolicyRow {
            key: "notification_urgency".to_owned(),
            detail: format!(
                "notification urgency: raised from `{}` to `{}` by the team floor `{}` ({file}) \
                 — your edit cannot lower it",
                urgency.individual,
                urgency.value,
                team,
                file = file
            ),
            guidance: Some(format!(
                "The team agreed a floor of `{team}`. To be interrupted more often, raise \
                 `notification` in {INDIVIDUAL_POLICY_FILE}; to be interrupted less, the floor \
                 has to change in {TEAM_POLICY_FILE}."
            )),
            notice: PolicyNotice::Warning,
        }),
        (_, team) => rows.push(PolicyRow {
            key: "notification_urgency".to_owned(),
            detail: format!(
                "notification urgency: `{}` (yours: `{}`, team: {}) — {}",
                urgency.value,
                urgency.individual,
                describe_team_input(team.as_ref()),
                source_tail(&urgency.source),
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        }),
    }

    // ── response automation: "floor" is the WRONG WORD here. The team caps
    //    automation downward; a stricter individual setting simply wins ──
    let automation = &policy.automation;
    match (&automation.source, automation.team) {
        (PolicySource::TeamCapped { file }, Some(team)) => rows.push(PolicyRow {
            key: "response_automation".to_owned(),
            detail: format!(
                "response automation: lowered from `{}` to `{}` by the team agreement `{}` ({file}) \
                 — the team caps how much autonomy you may grant",
                automation.individual, automation.value, team, file = file
            ),
            guidance: Some(format!(
                "The team agreement caps automation at `{team}`. Your \
                 `{}` in {INDIVIDUAL_POLICY_FILE} is not applied; a looser setting requires the \
                 team agreement to change in {TEAM_POLICY_FILE}.",
                automation.individual
            )),
            notice: PolicyNotice::Warning,
        }),
        (_, Some(team)) if automation.individual < team => rows.push(PolicyRow {
            key: "response_automation".to_owned(),
            detail: format!(
                "response automation: `{}` — your setting is stricter than the team agreement \
                 (`{team}`); yours applies",
                automation.value
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        }),
        (source, team) => rows.push(PolicyRow {
            key: "response_automation".to_owned(),
            detail: format!(
                "response automation: `{}` (yours: `{}`, team: {}) — {}",
                automation.value,
                automation.individual,
                describe_team_input(team.as_ref()),
                source_tail(source),
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        }),
    }

    // ── sharing breadth: neither a floor nor a ceiling. Saying "not enforced"
    //    out loud is the point; silence would imply enforcement ──
    let sharing = &policy.sharing;
    rows.push(match (&sharing.team_norm, &sharing.effective) {
        (Some(norm), effective) => PolicyRow {
            key: "sharing_breadth".to_owned(),
            detail: format!(
                "sharing breadth: team norm `{norm}` ({TEAM_POLICY_FILE}). Not enforced by this \
                 version — yours applies unchanged ({}).",
                describe_sharing(effective.as_deref())
            ),
            guidance: Some(format!(
                "A team norm can never raise how much you disclose. Enforcement semantics are \
                 deferred ({SHARING_DEFERRAL}); today the value in {INDIVIDUAL_POLICY_FILE} is \
                 the only one that governs."
            )),
            notice: PolicyNotice::Warning,
        },
        (None, effective) => PolicyRow {
            key: "sharing_breadth".to_owned(),
            detail: format!(
                "sharing breadth: {} — yours applies; no team norm is configured.",
                describe_sharing(effective.as_deref())
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        },
    });

    if policy.digest_interval_minutes != crate::domain::models::DEFAULT_DIGEST_INTERVAL_MINUTES {
        rows.push(PolicyRow {
            key: "digest_interval".to_owned(),
            detail: format!(
                "digest cadence: every {} minutes ({INDIVIDUAL_POLICY_FILE})",
                policy.digest_interval_minutes
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        });
    }

    // ── keys that parsed but cannot act ──
    for deferred in &policy.deferred_overrides {
        rows.push(PolicyRow {
            key: format!("deferred:{}", deferred.key),
            detail: format!(
                "`{}` ({}) parsed but is NOT yet enforced: no semantic message type exists to \
                 match on, so this key currently changes nothing",
                deferred.key, deferred.file
            ),
            guidance: Some(format!(
                "Tracked as {}. The key is retained, not dropped — remove it if you expected it \
                 to take effect today.",
                deferred.deferral
            )),
            notice: PolicyNotice::Warning,
        });
    }

    // ── per-sender overrides and identity-binding hazards ──
    for conflict in &policy.sender_conflicts {
        rows.push(PolicyRow {
            key: format!("sender-conflict:{}", conflict.peer_id),
            detail: format!(
                "per-sender aliases {} resolve to the same pinned identity {} — no override \
                 for that identity is applied",
                conflict.aliases.join(", "),
                short_id(&conflict.peer_id)
            ),
            guidance: Some(
                "Keep exactly one per-sender block for this `peer_id`; alias ordering never \
                 decides authorization policy."
                    .to_owned(),
            ),
            notice: PolicyNotice::Warning,
        });
    }
    for sender in &policy.sender_overrides {
        let values = describe_sender_policy(sender);
        match &sender.identity {
            SenderIdentity::Pinned {
                peer_id,
                binding: SenderBinding::DeclaredPeerId,
            } => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` binds to declared pinned identity {} and is \
                     rename-stable; {values}",
                    sender.alias,
                    short_id(peer_id)
                ),
                guidance: None,
                notice: PolicyNotice::Info,
            }),
            SenderIdentity::Pinned {
                peer_id,
                binding: SenderBinding::AliasLookup,
            } => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` currently resolves to pinned identity {}, but the \
                     binding came from the mutable alias and will not survive a rename; {values}",
                    sender.alias,
                    short_id(peer_id)
                ),
                guidance: Some(format!(
                    "Set `peer_id = \"{}\"` on `[interaction.overrides.\"{}\"]` before renaming \
                     the peer.",
                    peer_id, sender.alias
                )),
                notice: PolicyNotice::Warning,
            }),
            SenderIdentity::Unpinned { pseudonym } => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` names an UNPINNED peer (trust tier unverified); \
                     alias pseudonym {} is diagnostic only and the override is NOT applied; \
                     {values}",
                    sender.alias,
                    short_id(pseudonym)
                ),
                guidance: Some(
                    "Pin the peer's key, then set the resulting canonical `peer_id` on the \
                     per-sender block."
                        .to_owned(),
                ),
                notice: PolicyNotice::Warning,
            }),
            SenderIdentity::InvalidPin => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` names a peer with a malformed pinned key and is \
                     NOT applied; {values}",
                    sender.alias
                ),
                guidance: Some("Fix the peer's Ed25519 JWK `x` value.".to_owned()),
                notice: PolicyNotice::Warning,
            }),
            SenderIdentity::DeclaredMismatch { peer_id } => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` declares identity {}, but no active pinned peer \
                     has that identity; the override is NOT applied; {values}",
                    sender.alias,
                    short_id(peer_id)
                ),
                guidance: Some(
                    "Update `peer_id` after verifying the peer's current pin, or restore the \
                     intended pinned peer configuration."
                        .to_owned(),
                ),
                notice: PolicyNotice::Warning,
            }),
            SenderIdentity::Unknown => rows.push(PolicyRow {
                key: format!("sender:{}", sender.alias),
                detail: format!(
                    "per-sender override `{}` names no active configured peer; it resolves to no \
                     identity and is NOT applied; {values}",
                    sender.alias
                ),
                guidance: Some(
                    "Add the peer to workspace or active-profile A2A configuration, or remove \
                     the override."
                        .to_owned(),
                ),
                notice: PolicyNotice::Warning,
            }),
        }
    }

    // ── `[team.transparency]`: welded on, including contradictory inputs ──
    for invariant in &policy.transparency_invariants {
        let contradicted = invariant.configured != invariant.enforced;
        rows.push(PolicyRow {
            key: format!("invariant:{}", invariant.key),
            detail: if contradicted {
                format!(
                    "configured `team.transparency.{} = {}` cannot change behaviour; the \
                     enforced team-wide invariant remains `{}`",
                    invariant.key, invariant.configured, invariant.enforced
                )
            } else {
                format!(
                    "`team.transparency.{} = {}` matches the team-wide invariant already \
                     enforced unconditionally",
                    invariant.key, invariant.enforced
                )
            },
            guidance: contradicted.then(|| {
                format!(
                    "Set `team.transparency.{} = {}` in {} so the file describes actual \
                     behaviour.",
                    invariant.key, invariant.enforced, invariant.file
                )
            }),
            notice: if contradicted {
                PolicyNotice::Warning
            } else {
                PolicyNotice::Info
            },
        });
    }

    // ── effective consent beside the TOML policy ──
    // Announce an empty journal independently of TOML-implied sender rows.
    if !consent
        .iter()
        .any(|line| line.source == ConsentSource::Journaled)
    {
        rows.push(PolicyRow {
            key: "consent:none".to_owned(),
            detail: "effective consent per sender: no journaled consent grants recorded — \
                     TOML policy precedence is the only live source"
                .to_owned(),
            guidance: Some(format!(
                "The consent projection is a stub in this version ({CONSENT_DEFERRAL}); grants \
                 and revocations are journaled by a later story."
            )),
            notice: PolicyNotice::Info,
        });
    }
    for line in consent {
        rows.push(PolicyRow {
            key: format!("consent:{}", line.sender),
            detail: format!(
                "effective consent for `{}`: {} (source: {})",
                line.sender,
                describe_consent(line.state),
                match line.source {
                    ConsentSource::Journaled => "journaled",
                    ConsentSource::TomlImplied => "TOML-implied",
                }
            ),
            guidance: None,
            notice: PolicyNotice::Info,
        });
    }

    PolicyExplanation {
        rows,
        consent: consent.to_vec(),
    }
}

fn describe_sender_policy(sender: &SenderPolicy) -> String {
    let response = sender.response_mode.as_ref().map_or_else(
        || "response automation inherits the global effective value".to_owned(),
        |resolved| {
            format!(
                "response automation `{}` (authored `{}`, source {})",
                resolved.value,
                resolved.individual,
                source_tail(&resolved.source)
            )
        },
    );
    let notification = sender.notification.as_ref().map_or_else(
        || "notification urgency inherits the global effective value".to_owned(),
        |resolved| {
            format!(
                "notification urgency `{}` (authored `{}`, source {})",
                resolved.value,
                resolved.individual,
                source_tail(&resolved.source)
            )
        },
    );
    let auto_response = if sender.auto_response.is_some() {
        "auto-response content retained"
    } else {
        "no auto-response content"
    };
    format!("{response}; {notification}; {auto_response}")
}

fn describe_consent(state: crate::domain::ports::ConsentState) -> &'static str {
    use crate::domain::ports::ConsentState;
    match state {
        ConsentState::Trusted => "trusted",
        ConsentState::Revoked => "revoked",
        ConsentState::None => "no grant recorded",
    }
}

fn describe_team_input<T: std::fmt::Display>(team: Option<&T>) -> String {
    match team {
        Some(value) => format!("`{value}`"),
        None => "not configured".to_owned(),
    }
}

fn describe_sharing(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("yours: `{value}`"),
        None => "you configured none".to_owned(),
    }
}

fn source_tail(source: &PolicySource) -> String {
    match source.file() {
        Some(file) => format!("source: {} ({file})", source.label()),
        None => format!("source: {}", source.label()),
    }
}

/// First 12 hex characters — enough to recognise, short enough to read.
fn short_id(peer_id: &PeerId) -> String {
    let hex = peer_id.as_str();
    match hex.char_indices().nth(12) {
        Some((cut, _)) => format!("{}…", &hex[..cut]),
        None => hex.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        A2aPeerSource, IndividualDefaults, PinnedKey, PinnedKeyAlgorithm, RedactedUrl,
        SenderOverride, TeamDefaults, TeamOverrides, TeamTransparency, alias_pseudonym,
    };
    use base64::Engine as _;

    fn individual(
        response_mode: ResponseMode,
        notification: NotificationUrgency,
    ) -> IndividualPolicy {
        IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(response_mode),
                notification: Some(notification),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        }
    }

    fn team(
        response_mode: Option<ResponseMode>,
        notification: Option<NotificationUrgency>,
    ) -> TeamPolicy {
        TeamPolicy {
            defaults: TeamDefaults {
                response_mode,
                notification,
            },
            ..TeamPolicy::default()
        }
    }

    fn peer(alias: &str, pin: Option<[u8; 32]>) -> A2aPeerSpec {
        A2aPeerSpec {
            id: alias.to_owned(),
            url: RedactedUrl::new("https://peer.example/a2a".to_owned()),
            pinned_key: pin.map(|bytes| {
                PinnedKey::new(
                    PinnedKeyAlgorithm::EdDsa,
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
                    None,
                )
            }),
            source: A2aPeerSource::Workspace,
        }
    }

    const ALL_MODES: [ResponseMode; 3] = [
        ResponseMode::NotifyAndWait,
        ResponseMode::NotifyAndDraft,
        ResponseMode::NotifyAndAuto,
    ];
    const ALL_URGENCIES: [NotificationUrgency; 3] = [
        NotificationUrgency::Digest,
        NotificationUrgency::Queue,
        NotificationUrgency::Immediate,
    ];

    #[test]
    fn omitted_policy_values_use_fail_closed_defaults_with_default_provenance() {
        let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
        assert_eq!(effective.automation.value, ResponseMode::NotifyAndWait);
        assert_eq!(effective.urgency.value, NotificationUrgency::Queue);
        assert_eq!(effective.automation.source, PolicySource::Default);
        assert_eq!(effective.urgency.source, PolicySource::Default);
        assert_eq!(effective.sharing.source, PolicySource::Default);
    }

    // ── AC1 keystone: the ⊆ invariant over generated pairs ──

    /// The load-bearing FR96 invariant: **no team policy can make you share more
    /// than you consented to**, for every `(individual, team)` pair.
    ///
    /// Asserted over the full cross product rather than three hand-picked cases.
    /// Because sharing is not merged the sharing half now holds trivially, so the
    /// same sweep also asserts the *non-trivial* half — that automation is never
    /// looser than the individual's setting, which shares sharing's polarity and
    /// is where a polarity flip would actually bite.
    #[test]
    fn effective_sharing_is_never_broader_than_individual_consent() {
        let breadths = [None, Some("story-only"), Some("story-and-blockers")];
        for individual_mode in ALL_MODES {
            for individual_urgency in ALL_URGENCIES {
                for individual_sharing in &breadths {
                    for team_mode in ALL_MODES.map(Some).into_iter().chain([None]) {
                        for team_urgency in ALL_URGENCIES.map(Some).into_iter().chain([None]) {
                            for team_sharing in &breadths {
                                let mut ind = individual(individual_mode, individual_urgency);
                                ind.defaults.status_detail_minimum =
                                    individual_sharing.map(str::to_owned);
                                let mut tm = team(team_mode, team_urgency);
                                tm.overrides.status_detail_minimum =
                                    team_sharing.map(str::to_owned);

                                let effective = resolve_effective_policy(&ind, Some(&tm), &[]);

                                assert_eq!(
                                    effective.sharing.effective,
                                    individual_sharing.map(str::to_owned),
                                    "sharing breadth must pass through untouched"
                                );
                                assert!(
                                    effective.automation.value <= individual_mode,
                                    "automation {:?} must never exceed the individual's {:?} \
                                     (individual={individual_mode:?} team={team_mode:?})",
                                    effective.automation.value,
                                    individual_mode
                                );
                                assert!(
                                    effective.urgency.value >= individual_urgency,
                                    "urgency must never fall below the individual's setting"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Positive control on the UPWARD-binding quantity: a louder team floor must
    /// actually raise the effective value. Without this a resolver that returned
    /// `individual` unchanged would satisfy every invariant above vacuously.
    #[test]
    fn positive_control_team_floor_raises_urgency() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndWait, NotificationUrgency::Queue),
            Some(&team(None, Some(NotificationUrgency::Immediate))),
            &[],
        );
        assert_eq!(effective.urgency.value, NotificationUrgency::Immediate);
        assert_eq!(effective.urgency.individual, NotificationUrgency::Queue);
        assert!(matches!(
            effective.urgency.source,
            PolicySource::TeamRaised { .. }
        ));
    }

    /// Positive control on the DOWNWARD-binding quantity — the merge that is not
    /// trivially satisfied by pass-through.
    #[test]
    fn positive_control_team_agreement_lowers_automation() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndAuto, NotificationUrgency::Queue),
            Some(&team(Some(ResponseMode::NotifyAndWait), None)),
            &[],
        );
        assert_eq!(effective.automation.value, ResponseMode::NotifyAndWait);
        assert_eq!(effective.automation.individual, ResponseMode::NotifyAndAuto);
        assert!(matches!(
            effective.automation.source,
            PolicySource::TeamCapped { .. }
        ));
    }

    /// A member stricter than the team keeps their own setting; the team never
    /// raises automation.
    #[test]
    fn a_stricter_individual_beats_a_looser_team_agreement() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndWait, NotificationUrgency::Queue),
            Some(&team(Some(ResponseMode::NotifyAndAuto), None)),
            &[],
        );
        assert_eq!(effective.automation.value, ResponseMode::NotifyAndWait);
        assert!(matches!(
            effective.automation.source,
            PolicySource::Individual { .. }
        ));
    }

    /// The two binding quantities are genuinely independent: raising urgency must
    /// not touch automation, and capping automation must not touch urgency.
    #[test]
    fn the_two_binding_quantities_are_independent() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndAuto, NotificationUrgency::Digest),
            Some(&team(
                Some(ResponseMode::NotifyAndDraft),
                Some(NotificationUrgency::Immediate),
            )),
            &[],
        );
        assert_eq!(effective.urgency.value, NotificationUrgency::Immediate);
        assert_eq!(effective.automation.value, ResponseMode::NotifyAndDraft);
        assert!(matches!(
            effective.urgency.source,
            PolicySource::TeamRaised { .. }
        ));
        assert!(matches!(
            effective.automation.source,
            PolicySource::TeamCapped { .. }
        ));
    }

    /// Provenance assertions (AC1 🔗). A team-raised value names the team file; an
    /// individual-authored value names the individual file; an unconfigured value
    /// reports `default`.
    #[test]
    fn provenance_names_the_owning_file_per_dimension() {
        let raised = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndDraft, NotificationUrgency::Queue),
            Some(&team(None, Some(NotificationUrgency::Immediate))),
            &[],
        );
        assert_eq!(raised.urgency.source.file(), Some(TEAM_POLICY_FILE));
        assert_eq!(raised.urgency.source.label(), "team-floor-raised");
        assert_eq!(
            raised.automation.source.file(),
            Some(INDIVIDUAL_POLICY_FILE)
        );
        assert_eq!(raised.automation.source.label(), "individual");
        // Sharing was never configured by anyone → `default`, not a file.
        assert_eq!(raised.sharing.source, PolicySource::Default);
        assert_eq!(raised.sharing.source.label(), "default");
    }

    /// UX-DR-PP-02 — a team urgency floor must never appear as a source of the
    /// response mode.
    #[test]
    fn a_team_urgency_floor_is_not_a_source_of_the_response_mode() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndDraft, NotificationUrgency::Digest),
            Some(&team(None, Some(NotificationUrgency::Immediate))),
            &[],
        );
        assert!(
            !effective.automation.source.is_team_moved(),
            "urgency provenance leaked into automation: {:?}",
            effective.automation.source
        );
    }

    #[test]
    fn a_missing_team_file_resolves_to_the_individual_policy_verbatim() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndDraft, NotificationUrgency::Digest),
            None,
            &[],
        );
        assert_eq!(effective.automation.value, ResponseMode::NotifyAndDraft);
        assert_eq!(effective.urgency.value, NotificationUrgency::Digest);
        assert_eq!(effective.automation.team, None);
        assert!(!effective.team_file_present);
        assert!(effective.transparency_invariants.is_empty());
    }

    // ── AC3: identity keying ──

    /// The AC3 keystone. The alias moves in `a2a.json`; the pinned key does not;
    /// the override must still bind.
    ///
    /// Mutant this must turn RED: key the override map on the raw alias string
    /// with no identity resolution.
    #[test]
    fn a_per_sender_override_survives_an_alias_rename() {
        let key = [42u8; 32];
        let identity = PeerId::from_public_key(&key).unwrap();

        let mut policy = IndividualPolicy::default();
        policy.overrides.insert(
            "marcus-arch".to_owned(),
            SenderOverride {
                peer_id: Some(identity.as_str().to_owned()),
                response_mode: Some(ResponseMode::NotifyAndDraft),
                ..SenderOverride::default()
            },
        );

        // Before the rename.
        let before = resolve_effective_policy(&policy, None, &[peer("marcus-arch", Some(key))]);
        assert_eq!(
            sender_policy_for(&before, &identity)
                .and_then(|sender| sender.response_mode.as_ref())
                .map(|resolved| resolved.value),
            Some(ResponseMode::NotifyAndDraft)
        );

        // After: the operator renamed the peer in a2a.json and left the policy
        // file's alias alone. The binding is the identity, so it holds.
        let after = resolve_effective_policy(&policy, None, &[peer("marcus", Some(key))]);
        assert_eq!(
            sender_policy_for(&after, &identity)
                .and_then(|sender| sender.response_mode.as_ref())
                .map(|resolved| resolved.value),
            Some(ResponseMode::NotifyAndDraft),
            "an explicit identity binding must survive an alias rename"
        );
        assert!(after.sender_overrides[0].identity.is_pinned());
    }

    /// Positive control: an override does apply to the peer it names. A resolver
    /// that matched nothing would pass the rename test trivially.
    #[test]
    fn positive_control_an_override_applies_to_the_peer_it_names() {
        let key = [11u8; 32];
        let mut policy = IndividualPolicy::default();
        policy.overrides.insert(
            "lena-po".to_owned(),
            SenderOverride {
                notification: Some(NotificationUrgency::Immediate),
                ..SenderOverride::default()
            },
        );
        let effective = resolve_effective_policy(&policy, None, &[peer("lena-po", Some(key))]);
        let identity = PeerId::from_public_key(&key).unwrap();
        assert_eq!(
            sender_policy_for(&effective, &identity)
                .and_then(|sender| sender.notification.as_ref())
                .map(|resolved| resolved.value),
            Some(NotificationUrgency::Immediate)
        );
    }

    #[test]
    fn an_unpinned_peer_is_reported_not_granted_an_identity_binding() {
        let mut policy = IndividualPolicy::default();
        policy.overrides.insert(
            "drive-by".to_owned(),
            SenderOverride {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                ..SenderOverride::default()
            },
        );
        let effective = resolve_effective_policy(&policy, None, &[peer("drive-by", None)]);
        assert!(matches!(
            effective.sender_overrides[0].identity,
            SenderIdentity::Unpinned { .. }
        ));
        assert_eq!(
            sender_policy_for(&effective, &alias_pseudonym("drive-by")),
            None,
            "an alias pseudonym is diagnostic and must never grant a sender policy"
        );

        // Reported, never refused — admission is FR157 / Story 18.4.
        let explanation = explain_effective_policy(&effective, &[]);
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.detail.contains("UNPINNED") && row.detail.contains("drive-by")),
            "an unpinned per-sender target must be reported: {:?}",
            explanation.rows
        );
    }

    #[test]
    fn an_alias_naming_no_registered_peer_resolves_to_no_identity() {
        let mut policy = IndividualPolicy::default();
        policy
            .overrides
            .insert("ghost".to_owned(), SenderOverride::default());
        let effective = resolve_effective_policy(&policy, None, &[]);
        assert_eq!(
            effective.sender_overrides[0].identity,
            SenderIdentity::Unknown
        );
        assert_eq!(
            sender_policy_for(&effective, &alias_pseudonym("ghost")),
            None
        );
    }

    #[test]
    fn a_malformed_declared_peer_id_resolves_to_unknown() {
        assert_eq!(
            resolve_sender_identity("x", Some("not-hex"), &[peer("x", Some([1u8; 32]))]),
            SenderIdentity::Unknown
        );
    }

    #[test]
    fn sender_values_are_clamped_through_team_floor_and_cap() {
        let key = [17u8; 32];
        let identity = PeerId::from_public_key(&key).unwrap();
        let mut policy = IndividualPolicy::default();
        policy.overrides.insert(
            "peer".to_owned(),
            SenderOverride {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Digest),
                auto_response: Some("Received.".to_owned()),
                ..SenderOverride::default()
            },
        );
        let team_policy = team(
            Some(ResponseMode::NotifyAndWait),
            Some(NotificationUrgency::Immediate),
        );

        let effective =
            resolve_effective_policy(&policy, Some(&team_policy), &[peer("peer", Some(key))]);
        let sender = sender_policy_for(&effective, &identity).expect("sender policy");
        assert_eq!(
            sender.response_mode.as_ref().map(|resolved| resolved.value),
            Some(ResponseMode::NotifyAndWait)
        );
        assert!(matches!(
            sender
                .response_mode
                .as_ref()
                .map(|resolved| &resolved.source),
            Some(PolicySource::TeamCapped { .. })
        ));
        assert_eq!(
            sender.notification.as_ref().map(|resolved| resolved.value),
            Some(NotificationUrgency::Immediate)
        );
        assert!(matches!(
            sender
                .notification
                .as_ref()
                .map(|resolved| &resolved.source),
            Some(PolicySource::TeamRaised { .. })
        ));
        assert_eq!(sender.auto_response.as_deref(), Some("Received."));
        let explanation = explain_effective_policy(&effective, &[]);
        let row = explanation
            .rows
            .iter()
            .find(|row| row.key == "sender:peer")
            .expect("sender explanation");
        for expected in [
            "notify-and-wait",
            "notify-and-auto",
            "immediate",
            "digest",
            "auto-response content retained",
        ] {
            assert!(
                row.detail.contains(expected),
                "missing `{expected}`: {}",
                row.detail
            );
        }
    }

    #[test]
    fn duplicate_aliases_for_one_identity_publish_no_sender_policy() {
        let key = [18u8; 32];
        let identity = PeerId::from_public_key(&key).unwrap();
        let mut policy = IndividualPolicy::default();
        for alias in ["first", "second"] {
            policy.overrides.insert(
                alias.to_owned(),
                SenderOverride {
                    peer_id: Some(identity.as_str().to_owned()),
                    response_mode: Some(ResponseMode::NotifyAndDraft),
                    ..SenderOverride::default()
                },
            );
        }

        let effective = resolve_effective_policy(&policy, None, &[peer("renamed", Some(key))]);
        assert!(effective.sender_overrides.is_empty());
        assert_eq!(effective.sender_conflicts.len(), 1);
        assert_eq!(sender_policy_for(&effective, &identity), None);
        let explanation = explain_effective_policy(&effective, &[]);
        assert!(explanation.rows.iter().any(|row| {
            row.key.starts_with("sender-conflict:")
                && row.notice == PolicyNotice::Warning
                && row.detail.contains("first")
                && row.detail.contains("second")
        }));
    }

    #[test]
    fn alias_lookup_binding_is_not_claimed_to_be_rename_stable() {
        let key = [19u8; 32];
        let mut policy = IndividualPolicy::default();
        policy
            .overrides
            .insert("mutable".to_owned(), SenderOverride::default());
        let effective = resolve_effective_policy(&policy, None, &[peer("mutable", Some(key))]);
        assert!(matches!(
            effective.sender_overrides[0].identity,
            SenderIdentity::Pinned {
                binding: SenderBinding::AliasLookup,
                ..
            }
        ));
        let explanation = explain_effective_policy(&effective, &[]);
        let row = explanation
            .rows
            .iter()
            .find(|row| row.key == "sender:mutable")
            .expect("sender row");
        assert!(
            row.detail.contains("will not survive a rename"),
            "{}",
            row.detail
        );
        assert!(
            row.guidance
                .as_deref()
                .is_some_and(|text| text.contains("peer_id"))
        );
    }

    #[test]
    fn wildcard_override_is_deferred_not_treated_as_a_peer_alias() {
        let mut policy = IndividualPolicy::default();
        let mut wildcard = SenderOverride {
            notification: Some(NotificationUrgency::Immediate),
            ..SenderOverride::default()
        };
        wildcard.per_type.insert(
            "status_request".to_owned(),
            crate::domain::models::MessageTypeOverride::default(),
        );
        policy.overrides.insert("*".to_owned(), wildcard);

        let effective = resolve_effective_policy(&policy, None, &[]);
        assert!(effective.sender_overrides.is_empty());
        assert!(
            effective
                .deferred_overrides
                .iter()
                .any(|key| key.key == "interaction.overrides.\"*\".notification")
        );
        assert!(
            effective
                .deferred_overrides
                .iter()
                .any(|key| key.key == "interaction.overrides.\"*\".status_request")
        );
    }

    #[test]
    fn declared_identity_mismatch_gets_specific_guidance() {
        let declared = PeerId::from_public_key(&[20u8; 32]).unwrap();
        let mut policy = IndividualPolicy::default();
        policy.overrides.insert(
            "peer".to_owned(),
            SenderOverride {
                peer_id: Some(declared.as_str().to_owned()),
                ..SenderOverride::default()
            },
        );
        let effective = resolve_effective_policy(&policy, None, &[peer("peer", Some([21u8; 32]))]);
        assert!(matches!(
            effective.sender_overrides[0].identity,
            SenderIdentity::DeclaredMismatch { .. }
        ));
        let explanation = explain_effective_policy(&effective, &[]);
        let row = explanation
            .rows
            .iter()
            .find(|row| row.key == "sender:peer")
            .expect("sender row");
        assert!(row.detail.contains("declares identity"), "{}", row.detail);
        assert!(
            row.guidance
                .as_deref()
                .is_some_and(|text| text.contains("peer_id"))
        );
    }

    // ── AC6: three quantities, three different sentences ──

    /// A single templated "floor" sentence reused across all three quantities is
    /// a defect. On automation it names the wrong direction; on sharing it claims
    /// an enforcement that does not exist.
    #[test]
    fn the_three_quantities_get_three_distinct_sentences() {
        let mut policy = individual(ResponseMode::NotifyAndAuto, NotificationUrgency::Queue);
        policy.defaults.status_detail_minimum = Some("everything".to_owned());
        let mut team_policy = team(
            Some(ResponseMode::NotifyAndWait),
            Some(NotificationUrgency::Immediate),
        );
        team_policy.overrides.status_detail_minimum = Some("story-and-blockers".to_owned());

        let effective = resolve_effective_policy(&policy, Some(&team_policy), &[]);
        let explanation = explain_effective_policy(&effective, &[]);
        let row = |key: &str| {
            explanation
                .rows
                .iter()
                .find(|row| row.key == key)
                .unwrap_or_else(|| panic!("missing row {key}: {:?}", explanation.rows))
        };

        let urgency = &row("notification_urgency").detail;
        let automation = &row("response_automation").detail;
        let sharing = &row("sharing_breadth").detail;

        // Urgency is the ONLY quantity where "the floor raised it" is truthful.
        assert!(urgency.contains("raised from"), "{urgency}");
        assert!(urgency.contains("team floor"), "{urgency}");

        // Automation is capped downward — naming it a floor would be the wrong
        // direction.
        assert!(automation.contains("lowered"), "{automation}");
        assert!(
            !automation.contains("floor"),
            "automation must not be described as a floor: {automation}"
        );

        // Sharing claims no enforcement.
        assert!(sharing.contains("Not enforced"), "{sharing}");
        assert!(sharing.contains("team norm"), "{sharing}");
        assert!(
            !sharing.contains("floor"),
            "sharing must not be described as a floor: {sharing}"
        );

        // And all three are genuinely different sentences.
        assert_ne!(urgency, automation);
        assert_ne!(automation, sharing);
        assert_ne!(urgency, sharing);
    }

    /// AC6's headline mutant: reporting the effective value without the
    /// `(individual, team)` pair that produced it. An invisible merge is the exact
    /// failure NFR66 exists to prevent.
    #[test]
    fn every_changed_quantity_reports_the_pair_that_produced_it() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndAuto, NotificationUrgency::Digest),
            Some(&team(
                Some(ResponseMode::NotifyAndDraft),
                Some(NotificationUrgency::Immediate),
            )),
            &[],
        );
        let explanation = explain_effective_policy(&effective, &[]);
        let urgency = explanation
            .rows
            .iter()
            .find(|row| row.key == "notification_urgency")
            .unwrap();
        let automation = explanation
            .rows
            .iter()
            .find(|row| row.key == "response_automation")
            .unwrap();

        // individual, team AND effective — all three, on both changed quantities.
        for needle in ["digest", "immediate"] {
            assert!(urgency.detail.contains(needle), "{}", urgency.detail);
        }
        for needle in ["notify-and-auto", "notify-and-draft"] {
            assert!(automation.detail.contains(needle), "{}", automation.detail);
        }
        assert!(
            urgency.guidance.is_some(),
            "NFR66 wants resolution guidance"
        );
        assert!(automation.guidance.is_some());
    }

    /// Positive control for the explainer: a clean pair yields NO warnings. A
    /// validator that always warns is as useless as one that never does.
    #[test]
    fn positive_control_a_clean_policy_pair_produces_no_warnings() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndWait, NotificationUrgency::Immediate),
            Some(&team(
                Some(ResponseMode::NotifyAndAuto),
                Some(NotificationUrgency::Queue),
            )),
            &[],
        );
        let explanation = explain_effective_policy(&effective, &[]);
        assert_eq!(
            explanation.warning_count(),
            0,
            "clean pair warned: {:?}",
            explanation.warnings().collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_team_policy_reports_the_individual_policy_and_flags_nothing() {
        let effective = resolve_effective_policy(
            &individual(ResponseMode::NotifyAndDraft, NotificationUrgency::Queue),
            None,
            &[],
        );
        let explanation = explain_effective_policy(&effective, &[]);
        assert_eq!(explanation.warning_count(), 0);
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.detail.contains("notify-and-draft"))
        );
    }

    /// The empty consent projection must SAY it is empty. Silence is
    /// indistinguishable from absence, and would let 18-3c's grants appear from
    /// nowhere.
    #[test]
    fn the_empty_consent_projection_announces_itself() {
        let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
        let explanation = explain_effective_policy(&effective, &[]);
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.detail.contains("no journaled consent grants recorded")),
            "an empty projection must emit an explicit line, never zero output: {:?}",
            explanation.rows
        );
    }

    #[test]
    fn toml_implied_sender_does_not_hide_empty_journal_projection() {
        let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
        let explanation = explain_effective_policy(
            &effective,
            &[ConsentLine {
                sender: "configured-peer".to_owned(),
                source: ConsentSource::TomlImplied,
                state: crate::domain::ports::ConsentState::None,
            }],
        );
        assert!(explanation.rows.iter().any(|row| row.key == "consent:none"));
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.key == "consent:configured-peer")
        );
    }

    #[test]
    fn a_populated_consent_projection_renders_one_line_per_sender() {
        use crate::domain::ports::ConsentState;
        let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
        let explanation = explain_effective_policy(
            &effective,
            &[
                ConsentLine {
                    sender: "marcus-arch".to_owned(),
                    source: ConsentSource::Journaled,
                    state: ConsentState::Trusted,
                },
                ConsentLine {
                    sender: "lena-po".to_owned(),
                    source: ConsentSource::TomlImplied,
                    state: ConsentState::Revoked,
                },
            ],
        );
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.detail.contains("marcus-arch") && row.detail.contains("journaled"))
        );
        assert!(
            explanation
                .rows
                .iter()
                .any(|row| row.detail.contains("lena-po") && row.detail.contains("revoked"))
        );
        assert!(
            !explanation.rows.iter().any(|row| row.key == "consent:none"),
            "the empty-projection line must not appear when grants exist"
        );
    }

    #[test]
    fn transparency_keys_are_reported_as_already_enforced_invariants() {
        let team_policy = TeamPolicy {
            transparency: TeamTransparency::default(),
            ..TeamPolicy::default()
        };
        let effective =
            resolve_effective_policy(&IndividualPolicy::default(), Some(&team_policy), &[]);
        let explanation = explain_effective_policy(&effective, &[]);
        for key in [
            "retract_always_available",
            "transparency_log_visible_to_self",
            "transparency_log_visible_to_others",
            "auto_response_always_marked",
        ] {
            let row = explanation
                .rows
                .iter()
                .find(|row| row.key == format!("invariant:{key}"))
                .unwrap_or_else(|| panic!("missing invariant row for {key}"));
            assert!(
                row.detail.contains("matches the team-wide invariant"),
                "{}",
                row.detail
            );
            assert_eq!(
                row.notice,
                PolicyNotice::Info,
                "an already-enforced invariant is not a conflict"
            );
        }
    }
    #[test]
    fn contradictory_transparency_value_reports_actual_enforced_invariant() {
        let mut transparency = TeamTransparency::default();
        transparency.retract_always_available = false;
        let team_policy = TeamPolicy {
            transparency,
            ..TeamPolicy::default()
        };
        let effective =
            resolve_effective_policy(&IndividualPolicy::default(), Some(&team_policy), &[]);
        let explanation = explain_effective_policy(&effective, &[]);
        let row = explanation
            .rows
            .iter()
            .find(|row| row.key == "invariant:retract_always_available")
            .expect("invariant row");
        assert_eq!(row.notice, PolicyNotice::Warning);
        assert!(row.detail.contains("configured"));
        assert!(row.detail.contains("remains `true`"), "{}", row.detail);
        assert!(
            row.guidance
                .as_deref()
                .is_some_and(|text| text.contains("= true"))
        );
    }

    #[test]
    fn unenforced_per_type_keys_from_both_files_are_reported() {
        let mut policy = IndividualPolicy::default();
        let mut per_type = std::collections::BTreeMap::new();
        per_type.insert(
            "story_assignment".to_owned(),
            crate::domain::models::MessageTypeOverride::default(),
        );
        policy.overrides.insert(
            "lena-po".to_owned(),
            SenderOverride {
                per_type,
                ..SenderOverride::default()
            },
        );
        let team_policy = TeamPolicy {
            overrides: TeamOverrides {
                bug_reports: Some(NotificationUrgency::Immediate),
                ..TeamOverrides::default()
            },
            ..TeamPolicy::default()
        };

        let effective = resolve_effective_policy(&policy, Some(&team_policy), &[]);
        assert_eq!(effective.deferred_overrides.len(), 2);
        let explanation = explain_effective_policy(&effective, &[]);
        for needle in [
            "interaction.overrides.\"lena-po\".story_assignment",
            "team.overrides.bug_reports",
        ] {
            let row = explanation
                .rows
                .iter()
                .find(|row| row.detail.contains(needle))
                .unwrap_or_else(|| panic!("missing deferred row for {needle}"));
            assert!(row.detail.contains("NOT yet enforced"), "{}", row.detail);
            assert!(
                row.guidance
                    .as_deref()
                    .is_some_and(|g| g.contains(MSGTYPE_DEFERRAL))
            );
            assert_eq!(row.notice, PolicyNotice::Warning);
        }
    }
}
