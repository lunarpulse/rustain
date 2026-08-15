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

use serde::Deserialize;

use crate::domain::models::PeerId;
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
    /// Port 0 names no listener.
    UnusablePort { rendered: String },
    /// A relay URL past [`MAX_RELAY_URL_BYTES`] or not an https URL.
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
            Self::UnusablePort { rendered } => format!(
                "Refusing the network address {rendered}: port 0 names no listener. Nothing was \
                 written."
            ),
            Self::RelayUrlRejected => "Refusing the network address: its relay URL is not an \
                 https URL this host will keep. Nothing was written."
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBundle {
    id: String,
    addrs: Vec<WireTransport>,
}

/// The wire shape of one transport address. An externally tagged enum with no
/// catch-all: the custom variant and any variant a newer build introduces fail
/// the decode, which is the refusal D15 requires.
#[derive(Debug, Deserialize)]
enum WireTransport {
    Relay(String),
    Ip(String),
}

/// Decode one bundle's transports, or say why not.
fn decode_bundle(bytes: &[u8], expected: &PeerId) -> Result<Vec<ReachTransport>, ReachRefusal> {
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
    bundle
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
            WireTransport::Relay(url) => {
                if url.len() > MAX_RELAY_URL_BYTES || !url.starts_with("https://") {
                    Err(ReachRefusal::RelayUrlRejected)
                } else {
                    Ok(ReachTransport::Relay(url.clone()))
                }
            }
        })
        .collect()
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

/// The host an `https://` relay URL names, when it is an IP literal.
///
/// A hostname is returned as `None`: import time cannot know where a name will
/// resolve at dial time, so the consent card's rendered URL is the check for
/// those. An IP literal, though, is checkable right now — and a relay URL is
/// the same dial instruction a socket is.
fn relay_ip_literal(url: &str) -> Option<IpAddr> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split('/').next()?;
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, _) = bracketed.split_once(']')?;
        return host.parse::<std::net::Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let host = authority.split(':').next()?;
    host.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4)
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
    let transports = decode_bundle(bundle, expected)?;
    if transports.is_empty() {
        return Err(ReachRefusal::NoTransportAddress);
    }
    if transports.len() > MAX_TRANSPORT_ADDRESSES {
        return Err(ReachRefusal::TooManyTransportAddresses {
            count: transports.len(),
        });
    }
    for transport in &transports {
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
                    return Err(ReachRefusal::LocalNetworkAddress {
                        rendered: transport.rendered(),
                    });
                }
            }
        }
    }
    PeerAddress::from_bytes(bundle.clone())
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

/// How many transport addresses a ticket's bundles name in total.
///
/// ⚠ This counts addresses **inside** the bundle, not vector elements. The
/// vector length is the number of endpoints; the operator is being told how many
/// ways one endpoint can be reached, and a bundle holding three renders three.
#[must_use]
pub fn transport_address_count(addresses: &[Vec<u8>]) -> usize {
    addresses
        .iter()
        .map(
            |bundle| match serde_json::from_slice::<WireBundle>(bundle) {
                Ok(bundle) => bundle.addrs.len(),
                // An unreadable bundle still names something; reporting 0 would
                // claim the ticket carries no address when it carries one this host
                // is unable to decode.
                Err(_) => 1,
            },
        )
        .sum()
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
                    Err(ReachRefusal::LocalNetworkAddress { .. })
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
