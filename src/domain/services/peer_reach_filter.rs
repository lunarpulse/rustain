//! The import filter for ticket-borne reach (Story 18.4d, D4 + D15).
//!
//! # Why a filter exists at all
//!
//! A ticket's address list is **remote-supplied bytes**. The ticket format allows
//! 255 elements of 65,535 bytes each, one address bundle holds an unbounded set
//! of transport addresses, and the transport dials *every* address in the bundle
//! it is given. Without a filter, one pasted ticket becomes an attacker-directed
//! burst of connection attempts from inside the operator's own network position —
//! loopback services, link-local metadata endpoints, site-local ranges, arbitrary
//! ports. Bounding the outer vector alone does nothing about that: it bounds the
//! wrong dimension.
//!
//! So this module refuses, at the door and before anything is persisted:
//!
//! * more than one bundle (a bundle is one endpoint; two is not a union),
//! * a bundle whose encoded length is past [`MAX_REACH_BUNDLE_BYTES`],
//! * a bundle whose identifier does not derive the [`PeerId`] the ticket offers,
//! * more than [`MAX_TRANSPORT_ADDRESSES`] addresses inside the bundle,
//! * a transport variant this build does not recognise, including the custom
//!   variant whose payload is documented as neither validated nor size-limited,
//! * loopback, unspecified, multicast, link-local, site-local and broadcast
//!   sockets unless the operator opts in **per import**, and port 0 always.
//!
//! # The local-address opt-in is real, not theoretical
//!
//! Two hosts on one machine reach each other over loopback or a site-local
//! address, so the demo needs the opt-in. It is therefore a named per-import flag
//! on `peer add` and never a stored default. ⛔ It is not a confirm bypass: the
//! fingerprint confirm still happens, and there is still no flag that skips it.
//!
//! # Ungated on purpose
//!
//! `peer add` is available in a build with no transport adapter, so this filter
//! decodes the reach payload with its own mirror of the encoding rather than the
//! adapter's types. The mirror is not a guess: a lane test decodes a real bound
//! endpoint's address through it, so a shape change in the adapter fails here
//! instead of being discovered by a refused dial.

use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::domain::models::{PeerId, RelaySet};
use crate::domain::ports::PeerAddress;

/// Longest encoded address bundle this host will import.
///
/// Eight addresses plus an identifier fit in a few hundred bytes; four kilobytes
/// leaves generous headroom while refusing the 65,535-byte element the ticket
/// format would otherwise carry.
pub const MAX_REACH_BUNDLE_BYTES: usize = 4 * 1024;

/// Most transport addresses one imported bundle may name.
pub const MAX_TRANSPORT_ADDRESSES: usize = 8;

/// Longest relay URL an imported bundle may name.
pub const MAX_RELAY_URL_BYTES: usize = 256;

/// Why an import refused to record reach. Every arm writes nothing.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachRefusal {
    /// More than one address bundle. ⛔ Not "take the first": a ticket that
    /// names two endpoints is a ticket this host does not understand.
    TooManyBundles { count: usize },
    /// The bundle is longer than [`MAX_REACH_BUNDLE_BYTES`].
    Oversize { bytes: usize },
    /// The bundle did not decode as an address bundle.
    Undecodable { reason: String },
    /// The bundle names an endpoint that does not derive the offered key.
    KeyMismatch,
    /// The bundle names no transport address at all.
    NoTransportAddress,
    /// The bundle names more than [`MAX_TRANSPORT_ADDRESSES`] addresses.
    TooManyTransportAddresses { count: usize },
    /// A socket this host will not dial without the per-import opt-in.
    LocalNetworkAddress { rendered: String },
    /// A relay URL naming this machine or this local network.
    ///
    /// ⚑ Distinct from [`Self::LocalNetworkAddress`] because the remedy differs:
    /// `--allow-local-addresses` admits a local *socket* for two hosts that
    /// really are here, but it cannot make a relay dialable — D13 membership
    /// still decides that. Prescribing the flag here would send the operator to
    /// a remedy that cannot work.
    LocalRelayAddress { rendered: String },
    /// Port 0 names no listener.
    UnusablePort { rendered: String },
    /// A relay URL past [`MAX_RELAY_URL_BYTES`], not an https URL, carrying
    /// credentials, or naming port 0.
    RelayUrlRejected,
}

impl ReachRefusal {
    /// One operator-facing sentence naming the reason. ⛔ It never speculates
    /// about intent: a bad bundle and a hostile bundle read the same on the wire.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::TooManyBundles { count } => format!(
                "Refusing the network address: this ticket names {count} endpoints and a ticket \
                 names one. Nothing was written."
            ),
            Self::Oversize { bytes } => format!(
                "Refusing the network address: it is {bytes} bytes, past the \
                 {MAX_REACH_BUNDLE_BYTES} this host imports. Nothing was written."
            ),
            // ⚠ `reason` is serde's own message over remote-supplied bytes, so
            // it can quote what a stranger wrote. It is sanitized here rather
            // than at each render site: "audited clean once" is not an
            // invariant, the sanitizer is.
            Self::Undecodable { reason } => format!(
                "Refusing the network address: it did not read as an address ({}). Nothing \
                 was written.",
                crate::domain::services::transparency::sanitize_disclosable(
                    reason,
                    crate::domain::services::transparency::MAX_SUMMARY_BYTES,
                )
            ),
            Self::KeyMismatch => "Refusing the network address: it names a different endpoint \
                 than the key this ticket offers. Nothing was written."
                .to_owned(),
            Self::NoTransportAddress => "Refusing the network address: it names no address at \
                 all. Nothing was written."
                .to_owned(),
            Self::TooManyTransportAddresses { count } => format!(
                "Refusing the network address: it names {count} addresses, past the \
                 {MAX_TRANSPORT_ADDRESSES} this host imports. Nothing was written."
            ),
            Self::LocalNetworkAddress { rendered } => format!(
                "Refusing the network address {rendered}: it points into this machine or this \
                 local network, and dialing it would aim this host at its own side of the \
                 network. Nothing was written. Re-run with --allow-local-addresses if both \
                 hosts really are here."
            ),
            // ⛔ No --allow-local-addresses prescription here: the flag admits
            // a local *socket* for two hosts that really are here, but it
            // cannot make a relay dialable — D13 membership decides that, so
            // the flag would be a remedy that cannot work. The sentence stays
            // a posture about this host.
            Self::LocalRelayAddress { rendered } => format!(
                "Refusing the network address {rendered}: its relay URL points into this \
                 machine or this local network, and this host keeps no record of it. Nothing \
                 was written."
            ),
            Self::UnusablePort { rendered } => format!(
                "Refusing the network address {rendered}: port 0 names no listener. Nothing was \
                 written."
            ),
            // ⛔ A posture, never a verdict about the peer: a relay this host
            // will not keep says something about this host's configuration, and
            // the peer that named it did nothing wrong.
            Self::RelayUrlRejected => "Refusing the network address: its relay URL is not an \
                 https URL this host will keep — this peer names a relay this host does not \
                 use. Nothing was written."
                .to_owned(),
        }
    }
}

/// A recognised transport address, in this module's own vocabulary.
///
/// ⛔ Deliberately not the adapter's enum: this type exists so a build with no
/// transport adapter can still refuse a hostile bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachTransport {
    Ip(SocketAddr),
    Relay(String),
}

impl ReachTransport {
    /// How one address renders on the confirm card.
    #[must_use]
    pub fn rendered(&self) -> String {
        match self {
            Self::Ip(socket) => format!("ip {socket}"),
            Self::Relay(url) => format!("relay {url}"),
        }
    }
}

// ── The mirror of the adapter's encoding ────────────────────────────────────

/// The wire shape of one address bundle: an endpoint identifier and its
/// addresses. `deny_unknown_fields` is fail-closed on purpose — a bundle this
/// build cannot fully read is one it must not dial.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBundle {
    id: String,
    addrs: Vec<WireTransport>,
}

/// The wire shape of one transport address. An externally tagged enum with no
/// catch-all: the custom variant and any variant a newer build introduces fail
/// the decode, which is the refusal D15 requires.
///
/// ⚠ It serialises as well as deserialises, because [`dialable_reach`] rebuilds
/// a bundle with the relay addresses this host may not dial removed. ⛔ The
/// rebuild is skipped entirely when nothing was dropped, so an unfiltered
/// bundle stays byte-identical to what the operator imported.
#[derive(Debug, Serialize, Deserialize)]
enum WireTransport {
    Relay(String),
    Ip(String),
}

/// One bundle, fully validated: the endpoint identifier as offered, and every
/// transport in the **canonical** form this host stores, renders and compares.
///
/// ⚑ `transports` is canonical, so persisting a rebuild of this — and not the
/// offered bytes — is what makes the stored, rendered and compared value **one
/// canonical form**. The identifier stays exactly as the operator's ticket
/// carried it; it is validated against `expected`, never re-spelled.
struct ValidatedBundle {
    id: String,
    transports: Vec<ReachTransport>,
}

/// Decode one bundle's transports, or say why not.
fn decode_bundle(bytes: &[u8], expected: &PeerId) -> Result<ValidatedBundle, ReachRefusal> {
    if bytes.len() > MAX_REACH_BUNDLE_BYTES {
        return Err(ReachRefusal::Oversize { bytes: bytes.len() });
    }
    let bundle: WireBundle =
        serde_json::from_slice(bytes).map_err(|error| ReachRefusal::Undecodable {
            reason: error.to_string(),
        })?;
    let key = decode_endpoint_key(&bundle.id)?;
    // One derivation, the shipped one: `PeerId::from_public_key` is what the
    // adapter's own bind check derives through too, so this is the same rule
    // applied earlier, not a second rule.
    let derived = PeerId::from_public_key(&key).map_err(|_| ReachRefusal::KeyMismatch)?;
    if &derived != expected {
        return Err(ReachRefusal::KeyMismatch);
    }
    // ⚑ Every transport is canonicalised on the way in, so the confirm card
    // shows the operator one string while the membership test (D13) compares
    // the same one.
    let transports = bundle
        .addrs
        .iter()
        .map(|wire| match wire {
            WireTransport::Ip(text) => {
                text.parse::<SocketAddr>()
                    .map(ReachTransport::Ip)
                    .map_err(|error| ReachRefusal::Undecodable {
                        reason: error.to_string(),
                    })
            }
            WireTransport::Relay(url) => canonical_relay_url(url)
                .map(ReachTransport::Relay)
                .ok_or(ReachRefusal::RelayUrlRejected),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ValidatedBundle {
        id: bundle.id,
        transports,
    })
}

/// Decode the 64-character lowercase-hex endpoint identifier the encoding uses.
fn decode_endpoint_key(id: &str) -> Result<[u8; 32], ReachRefusal> {
    if id.len() != 64 {
        return Err(ReachRefusal::KeyMismatch);
    }
    let mut key = [0u8; 32];
    for (slot, pair) in key.iter_mut().zip(id.as_bytes().chunks(2)) {
        let hi = hex_nibble(pair[0]).ok_or(ReachRefusal::KeyMismatch)?;
        let lo = hex_nibble(pair[1]).ok_or(ReachRefusal::KeyMismatch)?;
        *slot = (hi << 4) | lo;
    }
    Ok(key)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

// ── The policy ──────────────────────────────────────────────────────────────

/// Whether this address points at this machine or this local network.
#[must_use]
fn is_local_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_local_v4(ip),
        IpAddr::V6(ip) => {
            // An IPv4-mapped IPv6 literal (`::ffff:127.0.0.1`) is the IPv4
            // address it embeds wearing a longer coat: the dial still lands on
            // the same loopback or link-local service, so the v4 rule decides.
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_local_v4(&v4);
            }
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // Unique-local fc00::/7, link-local fe80::/10 and site-local
                // fec0::/10 (deprecated, still not a stranger's to name). All
                // three predicates are unstable, so the masks are written out.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
        }
    }
}

fn is_local_v4(ip: &std::net::Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_private()
        || ip.is_link_local()
        // Carrier-grade NAT, 100.64.0.0/10. `Ipv4Addr::is_shared` is
        // unstable, so the mask is written out.
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 0x40)
}

/// Whether this socket points at this machine or this local network.
#[must_use]
pub fn is_local_network(socket: &SocketAddr) -> bool {
    is_local_ip(&socket.ip())
}

/// The canonical form of a relay URL, or `None` when it is not one this host
/// will keep.
///
/// # One parser, and it is iroh's
///
/// `RelayUrl` is `Arc<url::Url>` whose `FromStr` delegates to `Url::from_str`,
/// so parsing here with the same WHATWG parser makes this host's canonical text
/// and iroh's `RelayUrl` equality **the same relation**. ⛔ The hand-parse this
/// replaced split only on `/` and `:` with a strict dotted-quad test, while
/// `url::Url` strips userinfo, ignores `?` and `#`, and normalises
/// `2130706433`, `0x7f.0.0.1` and `127.1` — a parser differential in which one
/// side decides and the other renders.
///
/// ⚠ The https-only bound and the length bound are **rustain's, not iroh's**:
/// `RelayUrl` checks no scheme and does not even require a host.
#[must_use]
pub fn canonical_relay_url(url: &str) -> Option<String> {
    if url.len() > MAX_RELAY_URL_BYTES {
        return None;
    }
    let parsed = Url::parse(url).ok()?;
    if parsed.scheme() != "https" || parsed.host().is_none() {
        return None;
    }
    // ⛔ No credentials. A userinfo prefix survives `Url::as_str()` intact, and
    // this canonical string is what flows into the self reach record, minted
    // tickets and the ping path line — publishing it would hand every ticket
    // recipient the operator's password.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    // ⛔ Port 0 names no listener, exactly as it does for a socket
    // (`ReachRefusal::UnusablePort`). Accepting it would compose a relay
    // configuration that can never be established.
    if parsed.port() == Some(0) {
        return None;
    }
    let canonical = parsed.as_str().to_owned();
    // ⛔ Canonicalising must not smuggle a longer string past the bound the
    // caller was promised: percent-encoding and IDNA both grow the text.
    (canonical.len() <= MAX_RELAY_URL_BYTES).then_some(canonical)
}

/// The host an `https://` relay URL names, when it is an IP literal.
///
/// A hostname is returned as `None`: import time cannot know where a name will
/// resolve at dial time. ⚑ Since **D13** that is no longer load-bearing for
/// safety — an unconfigured relay is never dialed whatever it resolves to — but
/// the check still keeps a local-network literal out of the recorded value
/// unless the operator opted in, exactly as it does for a socket.
fn relay_ip_literal(url: &str) -> Option<IpAddr> {
    match Url::parse(url).ok()?.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

/// The filtered, importable reach a ticket carries.
///
/// `Ok(None)` is the honest empty case: a ticket may legitimately carry no
/// address, and that is neither an error nor a claim of unreachability.
///
/// # Errors
///
/// Returns the named [`ReachRefusal`]. Every arm means **nothing is persisted**.
pub fn imported_reach(
    addresses: &[Vec<u8>],
    expected: &PeerId,
    allow_local: bool,
) -> Result<Option<PeerAddress>, ReachRefusal> {
    let [bundle] = addresses else {
        if addresses.is_empty() {
            return Ok(None);
        }
        return Err(ReachRefusal::TooManyBundles {
            count: addresses.len(),
        });
    };
    let validated = decode_bundle(bundle, expected)?;
    let transports = &validated.transports;
    if transports.is_empty() {
        return Err(ReachRefusal::NoTransportAddress);
    }
    if transports.len() > MAX_TRANSPORT_ADDRESSES {
        return Err(ReachRefusal::TooManyTransportAddresses {
            count: transports.len(),
        });
    }
    for transport in transports {
        match transport {
            ReachTransport::Ip(socket) => {
                if socket.port() == 0 {
                    return Err(ReachRefusal::UnusablePort {
                        rendered: transport.rendered(),
                    });
                }
                if !allow_local && is_local_network(socket) {
                    return Err(ReachRefusal::LocalNetworkAddress {
                        rendered: transport.rendered(),
                    });
                }
            }
            ReachTransport::Relay(url) => {
                if !allow_local && relay_ip_literal(url).is_some_and(|ip| is_local_ip(&ip)) {
                    // ⛔ `LocalRelayAddress`, not `LocalNetworkAddress`: the
                    // socket remedy (`--allow-local-addresses`) cannot make a
                    // relay dialable, so its sentence must not prescribe it.
                    return Err(ReachRefusal::LocalRelayAddress {
                        rendered: transport.rendered(),
                    });
                }
            }
        }
    }
    // ⚑ Persist the canonical rebuild, ⛔ not the offered bytes. The operator
    // may keep a differently-spelled URL in their ticket than this host stores;
    // the stored, rendered and compared form must be one string, or the confirm
    // card shows one value while the membership test (D13) compares another.
    // The identifier is carried exactly as offered — it is validated, never
    // re-spelled.
    let canonical = WireBundle {
        id: validated.id,
        addrs: transports
            .iter()
            .map(|transport| match transport {
                ReachTransport::Ip(socket) => WireTransport::Ip(socket.to_string()),
                ReachTransport::Relay(url) => WireTransport::Relay(url.clone()),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&canonical).map_err(|error| ReachRefusal::Undecodable {
        reason: error.to_string(),
    })?;
    PeerAddress::from_bytes(bytes)
        .map(Some)
        .map_err(|error| ReachRefusal::Undecodable {
            reason: error.to_string(),
        })
}

/// The transport addresses a stored bundle names, for rendering only.
///
/// Best effort by design: this is a display helper, so an unreadable bundle
/// yields an explicit unknown entry rather than silently rendering zero
/// addresses for a bundle that exists.
#[must_use]
pub fn describe_reach(address: &PeerAddress) -> Vec<String> {
    rendered_transports(address.as_bytes())
}

/// Render one bundle's addresses, or one explicit unknown entry.
fn rendered_transports(bundle: &[u8]) -> Vec<String> {
    match serde_json::from_slice::<WireBundle>(bundle) {
        Ok(bundle) => bundle
            .addrs
            .iter()
            .map(|wire| match wire {
                WireTransport::Ip(text) => format!("ip {text}"),
                WireTransport::Relay(url) => format!("relay {url}"),
            })
            .collect(),
        Err(_) => vec!["an address this build could not read".to_owned()],
    }
}

/// How many transport addresses a ticket's bundles name, **by kind**.
///
/// ⚠ This counts addresses **inside** the bundle, not vector elements. The
/// vector length is the number of endpoints; the operator is being told how many
/// ways one endpoint can be reached, and a bundle holding three renders three.
///
/// ⚑ The kinds are separated because they were not, and the confirm card — *the
/// one human checkpoint* — therefore called a relay address **direct**. That
/// was latent only while nothing minted relay tickets; Story 18.4c mints them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReachKindCounts {
    /// Addresses that name a socket this host would dial directly.
    pub direct: usize,
    /// Addresses that name a relay: a third party would carry the traffic.
    pub relay: usize,
    /// Bundles this build could not read. ⛔ Never counted as either kind, and
    /// ⛔ never counted as zero: a bundle that exists names something.
    pub unreadable: usize,
}

impl ReachKindCounts {
    /// Every address named, whatever its kind.
    #[must_use]
    pub fn total(self) -> usize {
        self.direct + self.relay + self.unreadable
    }
}

/// The per-kind census of a ticket's bundles.
#[must_use]
pub fn transport_address_kinds(addresses: &[Vec<u8>]) -> ReachKindCounts {
    let mut counts = ReachKindCounts::default();
    for bundle in addresses {
        match serde_json::from_slice::<WireBundle>(bundle) {
            Ok(bundle) => {
                for wire in &bundle.addrs {
                    match wire {
                        WireTransport::Ip(_) => counts.direct += 1,
                        WireTransport::Relay(_) => counts.relay += 1,
                    }
                }
            }
            // An unreadable bundle still names something; reporting 0 would
            // claim the ticket carries no address when it carries one this host
            // is unable to decode.
            Err(_) => counts.unreadable += 1,
        }
    }
    counts
}

/// How many transport addresses a ticket's bundles name in total.
#[must_use]
pub fn transport_address_count(addresses: &[Vec<u8>]) -> usize {
    transport_address_kinds(addresses).total()
}

/// What of a stored bundle this host may actually dial (Story 18.4c, D13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialableReach {
    /// The addresses this host will bind with. It may be a strict subset of
    /// what the peer named — the dropped entries stay on file and stay
    /// rendered.
    Dialable(PeerAddress),
    /// Everything it named was a relay outside this host's configured set.
    ///
    /// ⛔ Not a verdict about the peer: they named a relay, and this host does
    /// not use it. Both halves of that sentence are about configuration.
    RelayNotConfigured,
    /// It named nothing this host can dial at all.
    Nothing,
}

/// Project a stored reach bundle onto the relays this host configured.
///
/// # Why membership and ⛔ not sanitisation
///
/// The question is not *"how do I make a stranger's relay URL safe to dial?"*
/// but **"why may a stranger's ticket add an outbound destination to this
/// process at all?"** — and before this it could, which breaks zero-phone-home
/// by construction: an operator composes one carefully-chosen relay, imports a
/// ticket, and the host dials someone else's. `UX-DR-PT-11` had already decided
/// it: *an offered relay is a claim; the local mode is a fact.*
///
/// So every constructed bypass — `localhost`, `localtest.me`,
/// `127.0.0.1.nip.io`, `metadata.google.internal`, the octal, IDNA and
/// IPv4-mapped forms — dies here, ⚑ **not because the parser got better, but
/// because none of them is in the operator's set**. It is pure: a set-membership
/// test over operator configuration, ⛔ no DNS, ⛔ no I/O, ⛔ no port minted for
/// one consumer — and therefore ⛔ no TOCTOU window, because nothing is
/// resolved.
///
/// ⚑ `disabled` composes an empty set, which matches nothing. An empty set is
/// ⛔ not a wildcard: the strictest mode must be the strictest.
///
/// **Cost, priced and filed:** two orgs on unshared self-hosted relays cannot
/// reach each other *via relay* (`DF-18-4c-RELAY-SET-NEGOTIATION`); the direct
/// path is unaffected.
#[must_use]
pub fn dialable_reach(address: &PeerAddress, relays: &RelaySet) -> DialableReach {
    let Ok(bundle) = serde_json::from_slice::<WireBundle>(address.as_bytes()) else {
        // ⛔ Not this filter's call. An unreadable bundle names no relay this
        // function can decide about, and the adapter already degrades it at
        // bind with a warning that names the peer. Swallowing it here would
        // delete that sentence and leave the operator with a silently
        // undialable alias instead of a stated one.
        return DialableReach::Dialable(address.clone());
    };
    let mut kept = Vec::with_capacity(bundle.addrs.len());
    let mut dropped_relays = 0usize;
    for wire in bundle.addrs {
        match &wire {
            WireTransport::Ip(_) => kept.push(wire),
            WireTransport::Relay(url) => {
                match canonical_relay_url(url).is_some_and(|canonical| relays.contains(&canonical))
                {
                    true => kept.push(wire),
                    false => dropped_relays += 1,
                }
            }
        }
    }
    if kept.is_empty() {
        return match dropped_relays {
            0 => DialableReach::Nothing,
            _ => DialableReach::RelayNotConfigured,
        };
    }
    if dropped_relays == 0 {
        // Nothing was dropped, so the operator's own bytes are what binds —
        // byte-identical to what a host with no relay configuration at all
        // would have bound with.
        return DialableReach::Dialable(address.clone());
    }
    let filtered = WireBundle {
        id: bundle.id,
        addrs: kept,
    };
    match serde_json::to_vec(&filtered).map_err(|error| error.to_string()) {
        Ok(bytes) => match PeerAddress::from_bytes(bytes) {
            Ok(address) => DialableReach::Dialable(address),
            Err(_) => DialableReach::Nothing,
        },
        Err(_) => DialableReach::Nothing,
    }
}

/// Every transport address a ticket's bundles name, rendered for the confirm
/// card.
///
/// The confirm card is the only human checkpoint before this host dials what a
/// stranger named, and a count cannot tell `203.0.113.7:4433` apart from
/// `169.254.169.254:80`. So the card shows the addresses themselves; the roster,
/// the logs and every non-consent surface stay count-only.
#[must_use]
pub fn describe_ticket_reach(addresses: &[Vec<u8>]) -> Vec<String> {
    addresses
        .iter()
        .flat_map(|bundle| rendered_transports(bundle))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
            .verifying_key()
            .to_bytes()
    }

    fn peer(seed: u8) -> PeerId {
        PeerId::from_public_key(&key(seed)).expect("peer id")
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn bundle(seed: u8, addrs: &[&str]) -> Vec<u8> {
        let addrs = addrs
            .iter()
            .map(|text| format!(r#"{{"Ip":"{text}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"id":"{}","addrs":[{addrs}]}}"#, hex(&key(seed))).into_bytes()
    }

    #[test]
    fn one_bundle_of_routable_sockets_is_imported() {
        let imported = imported_reach(&[bundle(3, &["203.0.113.7:4433"])], &peer(3), false)
            .expect("import")
            .expect("an address");
        assert_eq!(describe_reach(&imported), ["ip 203.0.113.7:4433"]);
        assert_eq!(
            transport_address_count(&[bundle(3, &["203.0.113.7:4433"])]),
            1
        );
    }

    #[test]
    fn no_bundle_is_the_honest_empty_case_and_two_is_a_refusal() {
        assert_eq!(imported_reach(&[], &peer(3), false), Ok(None));
        assert_eq!(
            imported_reach(
                &[
                    bundle(3, &["203.0.113.7:4433"]),
                    bundle(3, &["203.0.113.8:4433"])
                ],
                &peer(3),
                false
            ),
            Err(ReachRefusal::TooManyBundles { count: 2 })
        );
    }

    #[test]
    fn a_bundle_naming_another_endpoint_is_refused() {
        assert_eq!(
            imported_reach(&[bundle(4, &["203.0.113.7:4433"])], &peer(3), false),
            Err(ReachRefusal::KeyMismatch)
        );
    }

    #[test]
    fn the_custom_variant_and_an_unknown_variant_both_fail_the_decode() {
        for variant in [
            r#"{"Custom":{"id":1,"data":{"Heap":[1,2,3]}}}"#,
            r#"{"Quantum":"whatever"}"#,
        ] {
            let raw = format!(r#"{{"id":"{}","addrs":[{variant}]}}"#, hex(&key(3))).into_bytes();
            assert!(
                matches!(
                    imported_reach(&[raw], &peer(3), false),
                    Err(ReachRefusal::Undecodable { .. })
                ),
                "an unrecognised transport variant must refuse: {variant}"
            );
        }
    }

    #[test]
    fn local_network_sockets_need_the_per_import_opt_in() {
        for socket in [
            "127.0.0.1:4433",
            "169.254.169.254:80",
            "10.1.2.3:4433",
            "192.168.1.9:4433",
            "172.16.0.5:4433",
            "0.0.0.0:4433",
            "100.100.0.1:4433",
            "[::1]:4433",
            "[fe80::1]:4433",
            "[fd00::1]:4433",
            "[fec0::1]:4433",
            "[::ffff:127.0.0.1]:4433",
            "[::ffff:169.254.169.254]:80",
            "[::ffff:192.168.1.9]:4433",
        ] {
            assert!(
                matches!(
                    imported_reach(&[bundle(3, &[socket])], &peer(3), false),
                    Err(ReachRefusal::LocalNetworkAddress { .. })
                ),
                "{socket} must need the opt-in"
            );
            // Positive control: the opt-in actually opens the same socket.
            assert!(
                imported_reach(&[bundle(3, &[socket])], &peer(3), true).is_ok(),
                "{socket} must be importable with the opt-in"
            );
        }
    }

    #[test]
    fn port_zero_is_refused_even_with_the_opt_in() {
        assert!(matches!(
            imported_reach(&[bundle(3, &["203.0.113.7:0"])], &peer(3), true),
            Err(ReachRefusal::UnusablePort { .. })
        ));
    }

    #[test]
    fn the_inner_address_cap_bounds_the_dimension_that_gets_dialed() {
        let sockets: Vec<String> = (1..=9).map(|n| format!("203.0.113.{n}:4433")).collect();
        let refs: Vec<&str> = sockets.iter().map(String::as_str).collect();
        assert_eq!(
            imported_reach(&[bundle(3, &refs)], &peer(3), false),
            Err(ReachRefusal::TooManyTransportAddresses { count: 9 })
        );
        // Positive control: exactly the cap is accepted.
        assert!(imported_reach(&[bundle(3, &refs[..8])], &peer(3), false).is_ok());
    }

    #[test]
    fn an_oversize_bundle_is_refused_before_it_is_parsed() {
        let padded = vec![b' '; MAX_REACH_BUNDLE_BYTES + 1];
        assert_eq!(
            imported_reach(&[padded], &peer(3), false),
            Err(ReachRefusal::Oversize {
                bytes: MAX_REACH_BUNDLE_BYTES + 1
            })
        );
    }

    #[test]
    fn the_count_reads_inside_the_bundle_not_the_vector() {
        // Mutant (AC2 e): counting vector elements renders 1 for this ticket.
        let three = bundle(
            3,
            &["203.0.113.7:4433", "203.0.113.8:4433", "203.0.113.9:4433"],
        );
        assert_eq!(transport_address_count(std::slice::from_ref(&three)), 3);
        assert_eq!(
            describe_ticket_reach(std::slice::from_ref(&three)),
            [
                "ip 203.0.113.7:4433",
                "ip 203.0.113.8:4433",
                "ip 203.0.113.9:4433"
            ]
        );
    }

    #[test]
    fn a_relay_url_must_be_https_and_bounded() {
        let ok = format!(
            r#"{{"id":"{}","addrs":[{{"Relay":"https://relay.example./"}}]}}"#,
            hex(&key(3))
        )
        .into_bytes();
        assert!(imported_reach(&[ok], &peer(3), false).is_ok());

        let bad = format!(
            r#"{{"id":"{}","addrs":[{{"Relay":"http://relay.example./"}}]}}"#,
            hex(&key(3))
        )
        .into_bytes();
        assert_eq!(
            imported_reach(&[bad], &peer(3), false),
            Err(ReachRefusal::RelayUrlRejected)
        );
    }

    #[test]
    fn a_relay_url_naming_a_local_ip_literal_needs_the_opt_in() {
        for host in [
            "https://127.0.0.1/",
            "https://169.254.169.254:443/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[fec0::1]/",
        ] {
            let raw = format!(
                r#"{{"id":"{}","addrs":[{{"Relay":"{host}"}}]}}"#,
                hex(&key(3))
            )
            .into_bytes();
            assert!(
                matches!(
                    imported_reach(std::slice::from_ref(&raw), &peer(3), false),
                    // ⚑ `LocalRelayAddress` (18.4c review): same refusal, but
                    // its sentence never prescribes `--allow-local-addresses`,
                    // which cannot make a relay dialable.
                    Err(ReachRefusal::LocalRelayAddress { .. })
                ),
                "{host} must need the opt-in"
            );
            assert!(
                imported_reach(&[raw], &peer(3), true).is_ok(),
                "{host} must be importable with the opt-in"
            );
        }
        // A hostname cannot be resolved at import time; the consent card's
        // rendered URL is the check for those, so it passes the filter.
        let named = format!(
            r#"{{"id":"{}","addrs":[{{"Relay":"https://localhost./"}}]}}"#,
            hex(&key(3))
        )
        .into_bytes();
        assert!(imported_reach(&[named], &peer(3), false).is_ok());
    }
}
