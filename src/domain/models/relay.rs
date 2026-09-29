//! Relay-mode value types (Story 18.4c, FR159 / FR159-a).
//!
//! # Three modes, ⛔ never a tier
//!
//! `disabled`, `default` (the relay list n0 operates) and `configured` (relays
//! the operator names). All three are available to every operator: a mode is a
//! choice the operator makes and can change. ⛔ Nothing here is gated on a
//! licence, edition, plan or build flag, and no shipped string names one.
//!
//! # What a relay is, stated once (FR159-a)
//!
//! iroh's relay is a **forward-only conduit** for an end-to-end-encrypted QUIC
//! session: no store, no queue. What it observes is the **social graph** — which
//! endpoint exchanged traffic with which, when, and how much — ⛔ never bodies.
//! ⛔ And no string in this tree may say the relay *cannot* read anything: this
//! cut tests no confidentiality property and may claim none.
//!
//! # The relay set is configuration, ⛔ never peer-supplied data (D13)
//!
//! A relay URL that arrives inside a peer's ticket is a **claim**; this host's
//! configured set is the **fact**. The claim is recorded and rendered always,
//! and dialed only on membership — so a stranger's ticket can never add an
//! outbound destination to this process. ⚑ `disabled` therefore matches nothing:
//! an empty set is not a wildcard, and the strictest mode must be the strictest.
//!
//! # Boundary (NFR74)
//!
//! Nothing here is an iroh type. A relay URL is carried as its canonical
//! `url::Url` text, so a build with no transport adapter at all can still read
//! the operator's configuration and still refuse a relay they never configured.

use std::collections::BTreeSet;

/// How many relays one `configured` mode may name.
///
/// Reach, not authority: a handful of relays is redundancy, and a list past this
/// is a configuration mistake worth naming rather than composing.
pub const MAX_CONFIGURED_RELAYS: usize = 8;

/// Which relay this host may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMode {
    /// No relay at all. Directly-addressable peers only.
    Disabled,
    /// The relay list n0 operates and publishes.
    ///
    /// ⚠ Named for who runs it, because AC9's disclosure has to say so: an
    /// operator on this mode is handing their social graph to a vendor.
    N0Default,
    /// Relays the operator named, as canonical URLs.
    Configured { urls: Vec<String> },
}

impl RelayMode {
    /// Whether a third party carries traffic under this mode.
    ///
    /// The disclosure (AC9) exists exactly where this is true, ⛔ and nowhere
    /// else: on a `disabled` host nothing third-party is carrying anything.
    #[must_use]
    pub fn uses_a_relay(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// What `.rustain/relay.json` said — and, when it said nothing readable, that
/// this host degraded rather than refused.
///
/// ⚑ `Malformed` is a **distinct state from `Absent`**, and that is not
/// cosmetic. Both compose the same endpoint, so without the distinction an
/// operator degrades into a mode indistinguishable from the one they chose and
/// never learns their configuration is broken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayConfigState {
    /// No file. `disabled`, chosen by omission — the shipped default.
    Absent,
    /// The file read, and named this mode.
    Present(RelayMode),
    /// The file did not read. This host composes `disabled` and says so.
    ///
    /// ⚑ Degrade, ⛔ do not refuse. The loud-failure precedent covers intent
    /// that is **known and unhonorable**; a malformed file's intent is
    /// **unreadable**, and you cannot ignore an intent you never parsed.
    /// Refusing would turn one corrupt byte into a denial of service on the
    /// whole peer transport, while degrading costs reach only — and `disabled`
    /// is the mode that contacts nobody, so corrupting the file wins an
    /// attacker nothing.
    Malformed { reason: String },
}

impl RelayConfigState {
    /// The mode this host actually composes.
    #[must_use]
    pub fn mode(&self) -> RelayMode {
        match self {
            Self::Present(mode) => mode.clone(),
            Self::Absent | Self::Malformed { .. } => RelayMode::Disabled,
        }
    }

    /// Why `disabled` is a fallback here rather than a choice.
    #[must_use]
    pub fn degraded_reason(&self) -> Option<&str> {
        match self {
            Self::Malformed { reason } => Some(reason.as_str()),
            Self::Absent | Self::Present(_) => None,
        }
    }
}

/// The relay hosts this process may contact.
///
/// It is exactly the operator's configuration, expanded to canonical URLs — for
/// `configured` the URLs they wrote, for the n0 default the list that mode
/// resolves to, and for `disabled` nothing at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelaySet(BTreeSet<String>);

impl RelaySet {
    /// The set that matches nothing, which is what `disabled` means.
    #[must_use]
    pub fn empty() -> Self {
        Self(BTreeSet::new())
    }

    /// Whether this host configured that relay.
    ///
    /// `canonical` must already have been through
    /// [`crate::domain::services::peer_reach_filter::canonical_relay_url`]:
    /// comparing raw text is how a parser differential opens.
    #[must_use]
    pub fn contains(&self, canonical: &str) -> bool {
        self.0.contains(canonical)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The URLs, in canonical order, for rendering and for structural checks.
    #[must_use]
    pub fn urls(&self) -> Vec<&str> {
        self.0.iter().map(String::as_str).collect()
    }
}

impl FromIterator<String> for RelaySet {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_file_and_a_broken_file_compose_the_same_mode_and_stay_distinguishable() {
        let absent = RelayConfigState::Absent;
        let broken = RelayConfigState::Malformed {
            reason: "invalid JSON".to_owned(),
        };
        assert_eq!(absent.mode(), RelayMode::Disabled);
        assert_eq!(broken.mode(), RelayMode::Disabled);
        assert_eq!(absent.degraded_reason(), None);
        assert_eq!(broken.degraded_reason(), Some("invalid JSON"));
    }

    #[test]
    fn the_disclosure_fires_on_every_mode_that_hands_traffic_to_a_third_party() {
        assert!(!RelayMode::Disabled.uses_a_relay());
        assert!(RelayMode::N0Default.uses_a_relay());
        assert!(
            RelayMode::Configured {
                urls: vec!["https://relay.example/".to_owned()]
            }
            .uses_a_relay()
        );
    }

    /// ⚑ An empty set is not a wildcard. The strictest mode is the strictest.
    #[test]
    fn the_empty_relay_set_matches_nothing() {
        assert!(!RelaySet::empty().contains("https://relay.example/"));
        assert!(RelaySet::empty().is_empty());
    }

    #[test]
    fn membership_is_exact_over_canonical_urls() {
        let set: RelaySet = ["https://relay.example/".to_owned()].into_iter().collect();
        assert!(set.contains("https://relay.example/"));
        assert!(!set.contains("https://relay.example"));
        assert!(!set.contains("https://other.example/"));
        assert_eq!(set.len(), 1);
    }
}
