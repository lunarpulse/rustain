//! Interaction-policy value types (Story 18.3b — FR96, the FR93 *schema*).
//!
//! Two operator-authored files, one resolved answer:
//!
//! - `.rustain/a2a-interaction.toml` → [`IndividualPolicy`] — what *I* consented to.
//! - `.rustain/team-policy.toml` → [`TeamPolicy`] — what the team agreed to.
//!
//! [`crate::domain::services::team_policy::resolve_effective_policy`] folds them
//! into one [`EffectivePolicy`] under a **stricter-wins lattice over three
//! quantities, two of which bind in opposite directions**:
//!
//! | Quantity | "Stricter" means | Merge | Binds? |
//! |---|---|---|---|
//! | notification urgency | louder | `max(individual, team)` | yes — upward |
//! | response automation | less autonomy | `min(individual, team)` | yes — downward |
//! | sharing breadth | narrower | **not merged** — `effective = individual` | no |
//!
//! The load-bearing invariant is `effective_sharing ⊆ individual_consent` for
//! every `(individual, team)` pair: a team policy can raise how loudly you are
//! interrupted and cap how much autonomy you grant your own agent, but it can
//! **never** raise how much you disclose.
//!
//! These are types only. Nothing here reads a file (that is
//! `crate::adapters::policy::config`) and nothing here governs delivery — see
//! `DF-18-3b-DELIVERY-TRIGGER`, trigger-story 18.3c.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::models::PeerId;

/// The individual policy file, relative to `.rustain/`.
pub const INDIVIDUAL_POLICY_FILE: &str = "a2a-interaction.toml";

/// The team policy file, relative to `.rustain/`.
pub const TEAM_POLICY_FILE: &str = "team-policy.toml";

/// Digest cadence when the operator does not name one.
///
/// A UX default, not a derived constant (`ux-design-specification-addendum-peer-policy.md`
/// O1). Modelled here because 18-3c's `rustain init` writes the key and this
/// story's loader rejects unknown fields; the accumulator and flush machinery
/// that *act* on it are 18-3c's.
pub const DEFAULT_DIGEST_INTERVAL_MINUTES: u32 = 15;

/// The deferred-work id that owns every parsed-but-unenforceable per-type key.
pub const MSGTYPE_DEFERRAL: &str = "DF-18-3b-MSGTYPE";

// ──────────────────────────────────────────────────────────────────
// The three quantities
// ──────────────────────────────────────────────────────────────────

/// How much the agent may do on your behalf.
///
/// Ordered by **autonomy**, so less autonomy sorts lower: `notify-and-wait <
/// notify-and-draft < notify-and-auto`. That order is what makes `min()`
/// well-defined for the downward-binding merge — a team agreement may cap a
/// member's automation, and a member may always be stricter than the team.
///
/// There is deliberately no `silent` variant: notification is mandatory in every
/// mode (`prd.md:581`). Distinct from `DeliveryMode` (a bus routing decision) and
/// `DeliveryDisposition` (a consent relationship); neither is a response mode and
/// neither may be overloaded into one.
// The shared `NotifyAnd` prefix is load-bearing, not accidental: it encodes that
// notification happens in EVERY mode. `prd.md:581` — "there is no `silent` mode.
// You always see what happens." Dropping the prefix (`Wait`/`Draft`/`Auto`) would
// erase the one guarantee the whole type exists to make, and the wire spellings
// (`notify-and-wait`, …) are the operator-facing contract regardless.
#[allow(clippy::enum_variant_names)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResponseMode {
    /// Show me the message and wait. The agent does nothing on my behalf.
    NotifyAndWait,
    /// Show me the message, draft a reply, wait for my approval.
    NotifyAndDraft,
    /// Auto-respond and show me what was sent.
    NotifyAndAuto,
}

impl Default for ResponseMode {
    /// `notify-and-wait` — the most restrictive mode, and therefore the
    /// fail-closed target for a missing file (`prd.md:582`).
    fn default() -> Self {
        Self::NotifyAndWait
    }
}

impl ResponseMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotifyAndWait => "notify-and-wait",
            Self::NotifyAndDraft => "notify-and-draft",
            Self::NotifyAndAuto => "notify-and-auto",
        }
    }
}

impl fmt::Display for ResponseMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How loudly an incoming interaction interrupts you.
///
/// Ordered by **loudness**: `digest < queue < immediate`. `digest` is *quieter*
/// than `queue` — a batched periodic summary versus the next idle moment — so it
/// sits at the bottom (ADR-18-3b-01 D3). The consequence is intended: because
/// urgency merges with `max()`, a team floor of `immediate` overrides a member's
/// deliberate `digest`. That is the one direction a team is entitled to bind, and
/// it costs the member attention, never disclosure.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationUrgency {
    /// Batch into a periodic summary every `digest_interval_minutes`.
    Digest,
    /// Add to the notification queue; show on the next idle moment.
    Queue,
    /// Interrupt current work.
    Immediate,
}

impl Default for NotificationUrgency {
    /// `queue` (`prd.md:588`).
    fn default() -> Self {
        Self::Queue
    }
}

impl NotificationUrgency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Digest => "digest",
            Self::Queue => "queue",
            Self::Immediate => "immediate",
        }
    }
}

impl fmt::Display for NotificationUrgency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ──────────────────────────────────────────────────────────────────
// The individual file
// ──────────────────────────────────────────────────────────────────

/// `[interaction.defaults]` — the type-agnostic tier of the individual file.
///
/// The two binding values remain optional until resolution so the core can
/// distinguish an authored value from the built-in default. Collapsing them
/// during deserialization fabricates `a2a-interaction.toml` provenance when the
/// file or field is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndividualDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<ResponseMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<NotificationUrgency>,
    /// Digest cadence. Modelled only — 18-3c owns the accumulator.
    #[serde(default = "default_digest_interval_minutes")]
    pub digest_interval_minutes: u32,
    /// Sharing breadth. An **opaque string on purpose**: it has zero consumers in
    /// the tree and its consumer would be a status-response *content* producer,
    /// so a rich enum here would be a mechanism without a trigger
    /// (`DF-18-3b-SHARING-SEMANTICS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail_minimum: Option<String>,
}

fn default_digest_interval_minutes() -> u32 {
    DEFAULT_DIGEST_INTERVAL_MINUTES
}

impl Default for IndividualDefaults {
    fn default() -> Self {
        Self {
            response_mode: None,
            notification: None,
            digest_interval_minutes: DEFAULT_DIGEST_INTERVAL_MINUTES,
            status_detail_minimum: None,
        }
    }
}

/// A per-message-type sub-block. Parsed; not enforced (`DF-18-3b-MSGTYPE`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageTypeOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<ResponseMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<NotificationUrgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_response: Option<String>,
}

/// A per-sender block under `[interaction.overrides]`.
///
/// The map key is the operator's **alias** — the editing affordance. `peer_id` is
/// what actually **binds** (ADR-18-3b-01 D2): with it, renaming the peer in
/// `.rustain/a2a.json` cannot reroute the override, because the match is on the
/// cryptographic identity rather than on a mutable name.
///
/// # Why serde is hand-written here
///
/// This block mixes **known scalar keys** with **arbitrarily-named nested
/// tables** (the per-message-type tier). `#[serde(deny_unknown_fields)]` and
/// `#[serde(flatten)]` are mutually exclusive, so a derive can enforce at most
/// one of the two properties this schema needs. The hand-written impl gets both,
/// and splits them along the line that matters:
///
/// - an unknown **scalar** key is an **error** — that is the operator's typo, and
///   forgiving it would hide the misconfiguration a human authored moments ago;
/// - an unknown **table** is a per-message-type override — parsed, retained, and
///   reported by the explainer as configured-but-not-yet-enforced.
///
/// Serialization emits scalars before tables, which TOML requires and a derived
/// `flatten` would get wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderOverride {
    /// Canonical `PeerId` hex. When present, the binding key; the alias becomes
    /// decoration. When absent, the alias is resolved through `a2a.json` and an
    /// unpinned or unknown peer is **reported** (AC6), never silently granted.
    pub peer_id: Option<String>,
    pub response_mode: Option<ResponseMode>,
    pub notification: Option<NotificationUrgency>,
    pub auto_response: Option<String>,
    /// Per-message-type sub-blocks (`story_assignment`, `bug_report`,
    /// `status_request`, …).
    ///
    /// **Parsed, never resolved.** `MessageKind` carries three transport variants
    /// and `MessageHeader` has no semantic type field, so there is no key to
    /// match on in production today (`DF-18-3b-MSGTYPE`, trigger-story 18.3c).
    pub per_type: BTreeMap<String, MessageTypeOverride>,
}

/// The scalar keys a sender block may carry. Anything else that is not a table
/// is a typo, and the error names it.
const SENDER_SCALAR_KEYS: [&str; 4] = ["peer_id", "response_mode", "notification", "auto_response"];

impl Serialize for SenderOverride {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let scalars = usize::from(self.peer_id.is_some())
            + usize::from(self.response_mode.is_some())
            + usize::from(self.notification.is_some())
            + usize::from(self.auto_response.is_some());
        let mut map = serializer.serialize_map(Some(scalars + self.per_type.len()))?;
        // Scalars first — TOML rejects a value emitted after a table.
        if let Some(peer_id) = &self.peer_id {
            map.serialize_entry("peer_id", peer_id)?;
        }
        if let Some(response_mode) = &self.response_mode {
            map.serialize_entry("response_mode", response_mode)?;
        }
        if let Some(notification) = &self.notification {
            map.serialize_entry("notification", notification)?;
        }
        if let Some(auto_response) = &self.auto_response {
            map.serialize_entry("auto_response", auto_response)?;
        }
        for (name, override_) in &self.per_type {
            map.serialize_entry(name, override_)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SenderOverride {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SenderOverrideVisitor;

        impl<'de> Visitor<'de> for SenderOverrideVisitor {
            type Value = SenderOverride;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a per-sender interaction override table")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                use serde::de::Error as _;

                let mut out = SenderOverride::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "peer_id" => {
                            if out.peer_id.is_some() {
                                return Err(A::Error::duplicate_field("peer_id"));
                            }
                            let peer_id: String = map.next_value()?;
                            PeerId::parse(peer_id.clone()).map_err(|error| {
                                A::Error::custom(format!(
                                    "invalid `peer_id` in per-sender override: {error}"
                                ))
                            })?;
                            out.peer_id = Some(peer_id);
                        }
                        "response_mode" => {
                            if out.response_mode.is_some() {
                                return Err(A::Error::duplicate_field("response_mode"));
                            }
                            out.response_mode = Some(map.next_value()?);
                        }
                        "notification" => {
                            if out.notification.is_some() {
                                return Err(A::Error::duplicate_field("notification"));
                            }
                            out.notification = Some(map.next_value()?);
                        }
                        "auto_response" => {
                            if out.auto_response.is_some() {
                                return Err(A::Error::duplicate_field("auto_response"));
                            }
                            out.auto_response = Some(map.next_value()?);
                        }
                        // Not a known scalar. A nested table is a per-message-type
                        // override (deferred, retained); anything else is the
                        // operator's typo and must not be swallowed.
                        other => {
                            let value = map.next_value::<MessageTypeOverride>().map_err(|_| {
                                A::Error::custom(format!(
                                    "unknown key `{other}` in a per-sender override; \
                                     expected one of {} or a per-message-type table",
                                    SENDER_SCALAR_KEYS.join(", ")
                                ))
                            })?;
                            if out.per_type.insert(other.to_owned(), value).is_some() {
                                return Err(A::Error::custom(format!(
                                    "duplicate per-message-type key `{other}`"
                                )));
                            }
                        }
                    }
                }
                Ok(out)
            }
        }

        deserializer.deserialize_map(SenderOverrideVisitor)
    }
}

/// `.rustain/a2a-interaction.toml`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndividualPolicy {
    #[serde(default)]
    pub defaults: IndividualDefaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, SenderOverride>,
}

// ──────────────────────────────────────────────────────────────────
// The team file
// ──────────────────────────────────────────────────────────────────

/// `[team.defaults]` — the **type-agnostic** tier, and the only team input the
/// merge consumes.
///
/// Every field is `Option`: a team file that names one quantity must not be read
/// as having silently agreed a default for the other. An absent key contributes
/// nothing to the lattice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamDefaults {
    /// Team cap on automation — binds **downward**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<ResponseMode>,
    /// Team floor on urgency — binds **upward**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<NotificationUrgency>,
}

/// `[team.overrides]` — the per-message-type tier.
///
/// **Parsed, never resolved**, deferred under the *same* `DF-18-3b-MSGTYPE` as
/// the individual file's per-type overrides. Modelled with named fields rather
/// than a map because `prd.md:853-863` names exactly these keys, and a typo in
/// one of them should be rejected rather than accepted as a novel message type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub story_assignment_notification: Option<NotificationUrgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture_updates: Option<NotificationUrgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bug_reports: Option<NotificationUrgency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_request_response: Option<ResponseMode>,
    /// A team **norm** displayed beside the effective value, never a merge input
    /// (FR96 amendment, `prd.md:1319`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail_minimum: Option<String>,
}

impl TeamOverrides {
    /// The per-type keys this block actually set, in declaration order, for the
    /// explainer to report as *parsed but not yet enforced*.
    ///
    /// `status_detail_minimum` is deliberately absent: it is a displayed norm
    /// with its own deferral (`DF-18-3b-SHARING-SEMANTICS`), not an unenforced
    /// per-message-type key, and conflating the two would tell the operator the
    /// wrong story about why it does nothing.
    pub fn configured_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.story_assignment_notification.is_some() {
            keys.push("story_assignment_notification");
        }
        if self.architecture_updates.is_some() {
            keys.push("architecture_updates");
        }
        if self.bug_reports.is_some() {
            keys.push("bug_reports");
        }
        if self.status_request_response.is_some() {
            keys.push("status_request_response");
        }
        keys
    }
}

/// `[team.transparency]` — four booleans that are **invariants, not settings**.
///
/// Three are already unconditional product behaviour
/// (`ux-design-specification.md:2149`, FR94) and the fourth is 18-3c's marking
/// obligation. Rejecting unknown fields makes ignoring the block impossible, so
/// it is **decided rather than defaulted**: parsed into the schema, never fed to
/// the merge, and reported by the explainer as already-enforced. A boolean that
/// reads as a switch but is welded on is a lie in a config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamTransparency {
    #[serde(default = "yes")]
    pub retract_always_available: bool,
    #[serde(default = "yes")]
    pub transparency_log_visible_to_self: bool,
    #[serde(default = "no")]
    pub transparency_log_visible_to_others: bool,
    #[serde(default = "yes")]
    pub auto_response_always_marked: bool,
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

impl Default for TeamTransparency {
    fn default() -> Self {
        Self {
            retract_always_available: true,
            transparency_log_visible_to_self: true,
            transparency_log_visible_to_others: false,
            auto_response_always_marked: true,
        }
    }
}

impl TeamTransparency {
    /// `(key, configured value, enforced value)` triples. The configured value is
    /// retained so the explainer can report contradictions rather than claiming
    /// an ignored switch changed welded product behaviour.
    pub fn keys(&self) -> [(&'static str, bool, bool); 4] {
        [
            (
                "retract_always_available",
                self.retract_always_available,
                true,
            ),
            (
                "transparency_log_visible_to_self",
                self.transparency_log_visible_to_self,
                true,
            ),
            (
                "transparency_log_visible_to_others",
                self.transparency_log_visible_to_others,
                false,
            ),
            (
                "auto_response_always_marked",
                self.auto_response_always_marked,
                true,
            ),
        ]
    }
}

/// `.rustain/team-policy.toml`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamPolicy {
    #[serde(default)]
    pub defaults: TeamDefaults,
    #[serde(default)]
    pub overrides: TeamOverrides,
    #[serde(default)]
    pub transparency: TeamTransparency,
}

// ──────────────────────────────────────────────────────────────────
// The resolved answer
// ──────────────────────────────────────────────────────────────────

/// Where a resolved value came from.
///
/// `TeamRaised` and `TeamCapped` are **separate variants on purpose**. UX-DR-PP-02
/// forbids presenting a team urgency floor as a source of the response *mode*,
/// and a single shared `TeamFloor` variant reused across both quantities is
/// exactly how that lie gets told: it would name the wrong direction on
/// automation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicySource {
    /// No file named this quantity; the built-in default applies.
    Default,
    /// The individual authored it, and no team value displaced it.
    Individual { file: String },
    /// A team floor raised it **upward** past the individual's setting.
    TeamRaised { file: String },
    /// A team agreement capped it **downward** below the individual's setting.
    TeamCapped { file: String },
}

impl PolicySource {
    /// The file that owns this value, when a file does.
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Individual { file } | Self::TeamRaised { file } | Self::TeamCapped { file } => {
                Some(file)
            }
        }
    }

    /// A short provenance label for the explainer and the machine-readable view.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Individual { .. } => "individual",
            Self::TeamRaised { .. } => "team-floor-raised",
            Self::TeamCapped { .. } => "team-capped",
        }
    }

    /// `true` when the team file moved the value away from the individual's.
    pub fn is_team_moved(&self) -> bool {
        matches!(self, Self::TeamRaised { .. } | Self::TeamCapped { .. })
    }
}

/// One resolved quantity: the value, its provenance, **and the pair that
/// produced it**.
///
/// The inputs are part of the return type deliberately. AC6's named mutant is
/// "report only the effective value without the `(individual, team)` pair"; a
/// shell cannot drop a field the core hands it without the omission being
/// visible, and 18-3c must be able to snapshot provenance onto an interaction
/// event at decision time rather than recompute it later
/// (`ux-design-specification-addendum-peer-policy.md` §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved<T> {
    pub value: T,
    pub source: PolicySource,
    pub individual: T,
    pub team: Option<T>,
}

/// Sharing breadth — the quantity that **binds in neither direction**.
///
/// `effective` is always `individual`. The team value travels alongside as a
/// displayed norm so the explainer can say so out loud, because "not enforced" is
/// information the operator needs and silence would imply enforcement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharingBreadth {
    /// Always equal to `individual`. Never merged: `max()` would force disclosure
    /// past consent (FR96) and `min()` would cap a member who chooses to share
    /// more (`prd.md:863`).
    pub effective: Option<String>,
    pub individual: Option<String>,
    /// The team norm, displayed and not enforced.
    pub team_norm: Option<String>,
    pub source: PolicySource,
}

impl Default for PolicySource {
    fn default() -> Self {
        Self::Default
    }
}

/// Whether a pinned sender identity came from an explicit stable binding or from
/// resolving the current alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SenderBinding {
    /// `peer_id` was authored in the policy block. Rename-stable.
    DeclaredPeerId,
    /// The current alias resolved to a pinned peer. Valid for this resolution,
    /// but a later alias rename requires the policy file to be updated.
    AliasLookup,
}

/// How a per-sender override is bound to a peer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SenderIdentity {
    Pinned {
        peer_id: PeerId,
        binding: SenderBinding,
    },
    /// The peer exists but carries no pin. Its pseudonym is diagnostic only and
    /// can never participate in policy lookup.
    Unpinned { pseudonym: PeerId },
    /// A pin was present but could not produce a cryptographic identity.
    InvalidPin,
    /// A valid declared identity names no currently configured pinned peer.
    DeclaredMismatch { peer_id: PeerId },
    /// The alias names no registered peer at all.
    Unknown,
}

impl SenderIdentity {
    /// The identity a delivery-time lookup may match. Only a cryptographically
    /// pinned peer is authorization-bearing; alias pseudonyms are diagnostics.
    pub fn peer_id(&self) -> Option<&PeerId> {
        match self {
            Self::Pinned { peer_id, .. } => Some(peer_id),
            Self::Unpinned { .. }
            | Self::InvalidPin
            | Self::DeclaredMismatch { .. }
            | Self::Unknown => None,
        }
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Pinned { .. })
    }
}

/// A per-sender override, resolved to an identity and clamped through the team
/// lattice before it can become a delivery-time answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderPolicy {
    pub alias: String,
    pub identity: SenderIdentity,
    pub response_mode: Option<Resolved<ResponseMode>>,
    pub notification: Option<Resolved<NotificationUrgency>>,
    pub auto_response: Option<String>,
    /// Per-message-type keys inside this sender's block that parsed but cannot
    /// resolve (`DF-18-3b-MSGTYPE`).
    pub deferred_types: Vec<String>,
}

/// A key that parsed but has no enforcement path yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredKey {
    pub key: String,
    pub file: String,
    /// The deferred-work id that owns it.
    pub deferral: String,
}

/// More than one alias resolved to the same pinned identity. No policy for that
/// identity is published until the operator removes the ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderIdentityConflict {
    pub peer_id: PeerId,
    pub aliases: Vec<String>,
}

/// A `[team.transparency]` key whose behaviour is welded to `enforced`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransparencyInvariant {
    pub key: String,
    pub configured: bool,
    pub enforced: bool,
    pub file: String,
}

/// The one resolved answer both shells render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePolicy {
    /// Merged with `max` — binds upward.
    pub urgency: Resolved<NotificationUrgency>,
    /// Merged with `min` — binds downward.
    pub automation: Resolved<ResponseMode>,
    /// Not merged.
    pub sharing: SharingBreadth,
    pub digest_interval_minutes: u32,
    /// Whether a team file participated at all.
    pub team_file_present: bool,
    /// Per-type keys parsed from either file, none of them enforceable today.
    pub deferred_overrides: Vec<DeferredKey>,
    /// `[team.transparency]` keys, never merged.
    pub transparency_invariants: Vec<TransparencyInvariant>,
    /// Per-sender overrides, keyed on identity.
    pub sender_overrides: Vec<SenderPolicy>,
    /// Ambiguous identity bindings excluded from `sender_overrides`.
    pub sender_conflicts: Vec<SenderIdentityConflict>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_mode_orders_by_autonomy() {
        assert!(ResponseMode::NotifyAndWait < ResponseMode::NotifyAndDraft);
        assert!(ResponseMode::NotifyAndDraft < ResponseMode::NotifyAndAuto);
        assert_eq!(
            ResponseMode::NotifyAndWait.min(ResponseMode::NotifyAndAuto),
            ResponseMode::NotifyAndWait
        );
    }

    /// ADR-18-3b-01 D3 — `digest` is quieter than `queue`, so it sorts below it.
    #[test]
    fn urgency_orders_digest_below_queue() {
        assert!(NotificationUrgency::Digest < NotificationUrgency::Queue);
        assert!(NotificationUrgency::Queue < NotificationUrgency::Immediate);
        assert_eq!(
            NotificationUrgency::Digest.max(NotificationUrgency::Immediate),
            NotificationUrgency::Immediate
        );
    }

    #[test]
    fn omitted_defaults_serialize_without_fabricating_authorship() {
        assert_eq!(
            toml::to_string(&IndividualDefaults::default()).unwrap(),
            "digest_interval_minutes = 15\n"
        );
    }

    #[test]
    fn omitted_values_resolve_to_the_fail_closed_pair() {
        let defaults = IndividualDefaults::default();
        assert_eq!(
            defaults.response_mode.unwrap_or_default(),
            ResponseMode::NotifyAndWait
        );
        assert_eq!(
            defaults.notification.unwrap_or_default(),
            NotificationUrgency::Queue
        );
        assert_eq!(
            defaults.digest_interval_minutes,
            DEFAULT_DIGEST_INTERVAL_MINUTES
        );
    }

    #[test]
    fn team_transparency_defaults_match_shipped_product_behaviour() {
        let transparency = TeamTransparency::default();
        assert!(transparency.retract_always_available);
        assert!(transparency.transparency_log_visible_to_self);
        assert!(!transparency.transparency_log_visible_to_others);
        assert!(transparency.auto_response_always_marked);
        assert_eq!(transparency.keys().len(), 4);
    }

    #[test]
    fn team_overrides_reports_only_configured_per_type_keys() {
        let overrides = TeamOverrides {
            bug_reports: Some(NotificationUrgency::Immediate),
            status_request_response: Some(ResponseMode::NotifyAndAuto),
            // A displayed norm, not an unenforced per-type key.
            status_detail_minimum: Some("story-and-blockers".to_owned()),
            ..TeamOverrides::default()
        };
        assert_eq!(
            overrides.configured_keys(),
            vec!["bug_reports", "status_request_response"]
        );
        assert!(TeamOverrides::default().configured_keys().is_empty());
    }

    // ── SenderOverride: the scalar/table split the hand-written serde exists for ──

    #[test]
    fn sender_override_keeps_unknown_tables_as_per_type_overrides() {
        let parsed: SenderOverride = toml::from_str(
            r#"
            response_mode = "notify-and-draft"
            notification = "immediate"

            [story_assignment]
            response_mode = "notify-and-auto"
            auto_response = "Received."
            "#,
        )
        .expect("nested per-type tables parse");
        assert_eq!(parsed.response_mode, Some(ResponseMode::NotifyAndDraft));
        assert_eq!(parsed.per_type.len(), 1);
        assert_eq!(
            parsed.per_type["story_assignment"].response_mode,
            Some(ResponseMode::NotifyAndAuto)
        );
    }

    /// The typo case. An unknown **scalar** must be an error — swallowing it as a
    /// message type would hide a misconfiguration a human wrote seconds ago.
    #[test]
    fn sender_override_rejects_an_unknown_scalar_key() {
        let error = toml::from_str::<SenderOverride>("respons_mode = \"notify-and-auto\"\n")
            .expect_err("a misspelled scalar must not be accepted");
        let rendered = error.to_string();
        assert!(
            rendered.contains("respons_mode"),
            "error must name the offending key, got: {rendered}"
        );
    }

    #[test]
    fn sender_override_rejects_an_unknown_response_mode_variant() {
        let error = toml::from_str::<SenderOverride>("response_mode = \"notify-and-yolo\"\n")
            .expect_err("an unrecognised response_mode must be an error, not a default");
        assert!(error.to_string().contains("notify-and-yolo"));
    }

    /// Scalars must precede tables or TOML serialization fails outright — the
    /// reason `Serialize` is hand-written rather than derived with `flatten`.
    #[test]
    fn sender_override_round_trips_scalars_and_tables() {
        let mut per_type = BTreeMap::new();
        per_type.insert(
            "status_request".to_owned(),
            MessageTypeOverride {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Queue),
                auto_response: None,
            },
        );
        let original = SenderOverride {
            peer_id: Some(
                PeerId::from_public_key(&[1u8; 32])
                    .unwrap()
                    .as_str()
                    .to_owned(),
            ),
            response_mode: Some(ResponseMode::NotifyAndDraft),
            notification: Some(NotificationUrgency::Immediate),
            auto_response: Some("Received.".to_owned()),
            per_type,
        };
        let text = toml::to_string(&original).expect("scalars emit before tables");
        assert_eq!(
            toml::from_str::<SenderOverride>(&text).expect("round-trip parses"),
            original
        );
    }
}
