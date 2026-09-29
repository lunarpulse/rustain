//! Peer transport ticket — the artifact `peer invite` mints and `peer add`
//! imports (Story 18.4b, AC1 / FR157, `CC:103`).
//!
//! # What a ticket is, and what it is not
//!
//! A ticket carries **reachability plus a candidate identity, never trust**
//! (`CC:106`). The trust gate is the operator's explicit `peer add` confirm,
//! not this signature.
//!
//! # The signature is self-certifying, and that is the point
//!
//! The issuer key **is** the key the ticket offers: `peer invite` signs with
//! the local identity key ([`crate::adapters::rap::IdentityKeyStore`] →
//! `AgentSigner`) and embeds that same public key as [`PeerTicket::offered_key`].
//! So the signature proves exactly one thing — the addresses, expiry and
//! suggested name were not altered between minting and import — and **nothing
//! about authority or about who the holder is**. That asymmetry is why
//! `CC:106` makes the fingerprint confirm the trust gate: a ticket anyone can
//! mint for their own key is worth exactly the out-of-band comparison the
//! operator performs on the fingerprint.
//!
//! # One identity derivation, one key encoding (ruling A6)
//!
//! The offered key is a [`PinnedKey`], the same shape `.rustain/p2p.json`
//! stores under `pinnedKey`: `x` is unpadded base64url of the 32 raw Ed25519
//! public-key bytes. [`PinnedKey::peer_id`] stays the **sole** derivation from
//! key to [`PeerId`]; this module never builds a `PeerId` by hand.
//!
//! # Addresses stay opaque (NFR74)
//!
//! [`PeerTicket::addresses`] is a list of opaque byte strings — the same bytes
//! `crate::domain::ports::PeerAddress` wraps. They are reachability hints and
//! never an identity, so nothing in this module compares, parses or derives
//! anything from them. The domain model deliberately holds `Vec<Vec<u8>>`
//! rather than the port type, so no transport type reaches `domain::models`.
//!
//! # One encoding, which is also the signing input
//!
//! The blob is base64url of a fixed-layout, length-prefixed byte string
//! followed by the 64-byte signature, and the signature covers exactly those
//! same bytes prefixed by a domain tag. There is deliberately no second
//! serializer: a JSON envelope beside the signing input would be a second
//! encoding of one ticket and a second thing to keep canonical. Decoding
//! re-encodes and compares, so a body that parses but does not round-trip is
//! refused rather than accepted with trailing bytes.
//!
//! Raw key bytes rather than the 43-character base64url `x`, one-byte string
//! lengths and two-byte address lengths keep a minimal ticket near 108 bytes
//! (≈144 base64url characters). That is not micro-optimisation: it is what
//! decides whether AC2's QR fits inside a terminal at all.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use thiserror::Error;

// Imported through the `domain::models` re-export rather than the defining
// module path: `tests/conformance.rs::test_domain_no_forbidden_crate_imports`
// forbids the token `a2a` in any `src/domain` import line, and `PinnedKey` is
// declared beside the A2A peer spec. `p2p_peer_spec.rs` imports it the same way.
use crate::domain::models::{A2aPeerSpecError, Ed25519Sig, PeerId, PinnedKey};

/// Domain separation for the signature. Never travels on the wire.
const DOMAIN_TAG: &[u8] = b"rustain.peerticket.";
const FORMAT_VERSION: u8 = 1;
const SIGNATURE_LEN: usize = 64;

/// Wire prefix on the copyable blob. Present so a paste of the wrong thing
/// fails as [`PeerTicketError::Malformed`] with a reason instead of as a
/// base64 decode error deep inside the parser.
pub const PEER_TICKET_PREFIX: &str = "rustain-peer1.";

/// The JWK `alg` value the one supported algorithm serializes as.
const EDDSA_LABEL: &str = "EdDSA";

/// Number of head and tail columns a short fingerprint renders.
///
/// 12 is the shipped cap for key material in a row (`transparency_panel.rs`).
/// A 12-column head…tail form is a **recognition** aid; comparison uses
/// `peer show`, which prints both keys whole.
pub const PEER_FINGERPRINT_COLUMNS: usize = 12;

/// The constant hex prefix every [`PeerId`] carries: the multihash header for
/// "sha2-256, 32-byte digest" (`0x12`, `0x20`).
///
/// It is identical for every peer in existence, so a head…tail form over the
/// raw id spends four of its head columns on a constant and leaves one
/// discriminating character at the head. [`peer_fingerprint`] elides it.
pub const PEER_ID_MULTIHASH_PREFIX: &str = "1220";

/// The short, comparable form of a peer identity.
///
/// Head…tail over the **digest**, with the constant multihash header elided, so
/// all twelve columns discriminate. ⛔ It is a recognition aid: `peer show`
/// prints the whole id, header included, and that is what a comparison uses.
#[must_use]
pub fn peer_fingerprint(peer_id: &PeerId) -> String {
    let hex = peer_id.as_str();
    let digest = hex.strip_prefix(PEER_ID_MULTIHASH_PREFIX).unwrap_or(hex);
    short_fingerprint(digest, PEER_FINGERPRINT_COLUMNS)
}

/// An expiring, issuer-signed reachability artifact for one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerTicket {
    /// The peer's Ed25519 public key. Also the issuer key — see the module
    /// docs on self-certification.
    pub offered_key: PinnedKey,
    /// Opaque reachability hints (NFR74). Empty is legitimate and is reported,
    /// never fabricated.
    pub addresses: Vec<Vec<u8>>,
    /// Expiry, unix seconds.
    pub not_after: i64,
    /// A name the issuer suggests. **Advisory only**: the importing operator
    /// supplies the alias that becomes the `p2p.json` map key, because a
    /// remote-chosen key in a local config file is both a spoofing vector and
    /// a config write the operator never authored.
    pub suggested_name: Option<String>,
    /// Ed25519 signature over the domain tag followed by the canonical wire
    /// body.
    pub signature: Ed25519Sig,
}

impl PeerTicket {
    /// Mint a ticket, signing with a caller-supplied Ed25519 signer.
    ///
    /// The signing key never enters this module. `peer invite` holds the local
    /// identity through `AgentSigner`, which owns its key and exposes only a
    /// detached-signing method — so the shipped one-identity seam stays intact
    /// and no second key loader appears on this path.
    ///
    /// `not_after` is unix seconds. `addresses` are opaque bytes supplied by the
    /// caller; this function neither invents nor validates them.
    pub fn mint_with(
        public_key: &[u8],
        addresses: Vec<Vec<u8>>,
        not_after: i64,
        suggested_name: Option<String>,
        sign: impl FnOnce(&[u8]) -> Vec<u8>,
    ) -> Result<Self, PeerTicketError> {
        if public_key.len() != 32 {
            return Err(PeerTicketError::Malformed {
                reason: format!(
                    "a ticket key is 32 bytes, not {} — this is a local identity fault, not a \
                     ticket fault",
                    public_key.len()
                ),
            });
        }
        let x = URL_SAFE_NO_PAD.encode(public_key);
        let offered_key = PinnedKey::parse(EDDSA_LABEL, x, None).map_err(pin_error)?;
        let mut ticket = Self {
            offered_key,
            addresses,
            not_after,
            suggested_name,
            signature: Ed25519Sig(Vec::new()),
        };
        let signature = sign(&ticket.signing_input()?);
        ticket.signature = Ed25519Sig(signature);
        Ok(ticket)
    }

    /// Mint with an `ed25519_dalek` signing key directly.
    pub fn mint(
        signing_key: &SigningKey,
        addresses: Vec<Vec<u8>>,
        not_after: i64,
        suggested_name: Option<String>,
    ) -> Result<Self, PeerTicketError> {
        Self::mint_with(
            &signing_key.verifying_key().to_bytes(),
            addresses,
            not_after,
            suggested_name,
            |message| signing_key.sign(message).to_bytes().to_vec(),
        )
    }

    /// The peer identity this ticket offers, through the one derivation (A6).
    pub fn peer_id(&self) -> Result<PeerId, PeerTicketError> {
        self.offered_key.peer_id().map_err(pin_error)
    }

    /// The canonical wire body: exactly what the blob carries minus the
    /// trailing signature, and exactly what the signature covers once prefixed
    /// by [`DOMAIN_TAG`].
    ///
    /// The layout is fixed-width where it can be and explicitly length-prefixed
    /// where it cannot, so **one ticket has exactly one encoding**. There is no
    /// second serializer to drift from this one: the signing input and the wire
    /// form are the same bytes.
    ///
    /// ```text
    /// 1  byte   format version
    /// 32 bytes  raw Ed25519 public key
    /// 8  bytes  not_after, i64 big-endian
    /// 1  byte   kid length      + kid bytes
    /// 1  byte   name length     + name bytes
    /// 1  byte   address count   + per address: 2-byte big-endian length + bytes
    /// ```
    ///
    /// The blob is base64url of `<this> || <64-byte signature>`. Raw key bytes
    /// rather than the 43-character base64url `x` keeps the encoding compact,
    /// which is what decides whether AC2's QR can fit a terminal at all.
    fn wire_body(&self) -> Result<Vec<u8>, PeerTicketError> {
        let key = self.raw_public_key()?;
        let kid = self.offered_key.kid.as_deref().unwrap_or("");
        let name = self.suggested_name.as_deref().unwrap_or("");
        let mut out = Vec::with_capacity(64 + kid.len() + name.len());
        out.push(FORMAT_VERSION);
        out.extend_from_slice(&key);
        out.extend_from_slice(&self.not_after.to_be_bytes());
        push_short_str(&mut out, kid, "key id")?;
        push_short_str(&mut out, name, "suggested name")?;
        if self.addresses.len() > u8::MAX as usize {
            return Err(PeerTicketError::Malformed {
                reason: format!(
                    "a ticket carries at most {} addresses, not {}",
                    u8::MAX,
                    self.addresses.len()
                ),
            });
        }
        out.push(self.addresses.len() as u8);
        for address in &self.addresses {
            let len: u16 = address
                .len()
                .try_into()
                .map_err(|_| PeerTicketError::Malformed {
                    reason: format!(
                        "one address is {} bytes, past the {} the format carries",
                        address.len(),
                        u16::MAX
                    ),
                })?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(address);
        }
        Ok(out)
    }

    /// Canonical bytes the issuer signature covers: the domain tag then the
    /// wire body. The tag is domain separation only and never travels.
    fn signing_input(&self) -> Result<Vec<u8>, PeerTicketError> {
        let body = self.wire_body()?;
        let mut out = Vec::with_capacity(DOMAIN_TAG.len() + body.len());
        out.extend_from_slice(DOMAIN_TAG);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// The 32 raw Ed25519 public-key bytes behind the offered key.
    fn raw_public_key(&self) -> Result<[u8; 32], PeerTicketError> {
        URL_SAFE_NO_PAD
            .decode(self.offered_key.x.as_bytes())
            .map_err(|error| PeerTicketError::Malformed {
                reason: format!("ticket key is not valid base64url ({error})"),
            })?
            .try_into()
            .map_err(|_| PeerTicketError::Malformed {
                reason: "ticket key is not 32 bytes".to_owned(),
            })
    }

    /// Encode to the copyable text blob.
    pub fn encode(&self) -> Result<String, PeerTicketError> {
        let mut bytes = self.wire_body()?;
        if self.signature.as_bytes().len() != SIGNATURE_LEN {
            return Err(PeerTicketError::Malformed {
                reason: format!(
                    "ticket signature is {} bytes, not {SIGNATURE_LEN}",
                    self.signature.as_bytes().len()
                ),
            });
        }
        bytes.extend_from_slice(self.signature.as_bytes());
        Ok(format!(
            "{PEER_TICKET_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(&bytes)
        ))
    }

    /// Decode, check the issuer signature, then check the expiry.
    ///
    /// The three refusals are distinct by construction and the order is
    /// deliberate: an unchecked field is never used to refuse, so a ticket
    /// that is *both* altered and expired reports the signature failure. Each
    /// single fault therefore has exactly one reason:
    ///
    /// | fault | reason |
    /// |---|---|
    /// | wrong prefix, bad base64, short or overlong body, bad key | [`PeerTicketError::Malformed`] |
    /// | contents altered after minting | [`PeerTicketError::BadIssuerSignature`] |
    /// | intact but past `not_after` | [`PeerTicketError::Expired`] |
    pub fn decode(blob: &str, now_unix: i64) -> Result<Self, PeerTicketError> {
        let body = blob
            .trim()
            .strip_prefix(PEER_TICKET_PREFIX)
            .ok_or_else(|| PeerTicketError::Malformed {
                reason: format!(
                    "not a peer ticket — expected it to start with {PEER_TICKET_PREFIX}"
                ),
            })?;
        let bytes = URL_SAFE_NO_PAD.decode(body.as_bytes()).map_err(|error| {
            PeerTicketError::Malformed {
                reason: format!("ticket body is not valid base64url ({error})"),
            }
        })?;
        if bytes.len() < SIGNATURE_LEN {
            return Err(PeerTicketError::Malformed {
                reason: "ticket body is too short to carry a signature".to_owned(),
            });
        }
        let (wire, signature_bytes) = bytes.split_at(bytes.len() - SIGNATURE_LEN);
        let ticket = Self::parse_wire(wire, Ed25519Sig(signature_bytes.to_vec()))?;
        // Re-encoding must reproduce the input exactly. A body that parses but
        // does not round-trip carries trailing or non-canonical bytes, and a
        // second encoding of one ticket is a second thing to sign.
        if ticket.wire_body()?.as_slice() != wire {
            return Err(PeerTicketError::Malformed {
                reason: "ticket body is not canonically encoded".to_owned(),
            });
        }
        ticket.check_issuer_signature()?;
        ticket.check_expiry(now_unix)?;
        Ok(ticket)
    }

    /// Refuse a ticket that is past its signed expiry.
    ///
    /// Callers must invoke this again after any human confirmation: decoding
    /// before an unbounded prompt is not permission to import after expiry.
    pub fn check_expiry(&self, now_unix: i64) -> Result<(), PeerTicketError> {
        if now_unix > self.not_after {
            return Err(PeerTicketError::Expired {
                not_after: self.not_after,
            });
        }
        Ok(())
    }

    fn parse_wire(wire: &[u8], signature: Ed25519Sig) -> Result<Self, PeerTicketError> {
        let mut cursor = Cursor { bytes: wire, at: 0 };
        let version = cursor.take_u8()?;
        if version != FORMAT_VERSION {
            return Err(PeerTicketError::Malformed {
                reason: format!("ticket format version {version} is not supported by this build"),
            });
        }
        let key = cursor.take(32)?;
        let not_after = i64::from_be_bytes(cursor.take(8)?.try_into().map_err(|_| {
            PeerTicketError::Malformed {
                reason: "ticket expiry is truncated".to_owned(),
            }
        })?);
        let kid = cursor.take_short_str("key id")?;
        let name = cursor.take_short_str("suggested name")?;
        let address_count = cursor.take_u8()? as usize;
        let mut addresses = Vec::with_capacity(address_count);
        for _ in 0..address_count {
            let len = u16::from_be_bytes(cursor.take(2)?.try_into().map_err(|_| {
                PeerTicketError::Malformed {
                    reason: "ticket address length is truncated".to_owned(),
                }
            })?);
            addresses.push(cursor.take(len as usize)?.to_vec());
        }
        let offered_key = PinnedKey::parse(
            EDDSA_LABEL,
            URL_SAFE_NO_PAD.encode(key),
            (!kid.is_empty()).then_some(kid),
        )
        .map_err(pin_error)?;
        Ok(Self {
            offered_key,
            addresses,
            not_after,
            suggested_name: (!name.is_empty()).then_some(name),
            signature,
        })
    }

    /// Check the issuer signature against the key the ticket itself offers.
    fn check_issuer_signature(&self) -> Result<(), PeerTicketError> {
        let issuer = VerifyingKey::from_bytes(&self.raw_public_key()?).map_err(|_| {
            PeerTicketError::Malformed {
                reason: "ticket key is not a usable Ed25519 key".to_owned(),
            }
        })?;
        let signature_bytes: [u8; SIGNATURE_LEN] =
            self.signature
                .as_bytes()
                .try_into()
                .map_err(|_| PeerTicketError::Malformed {
                    reason: format!(
                        "ticket signature is {} bytes, not {SIGNATURE_LEN}",
                        self.signature.as_bytes().len()
                    ),
                })?;
        issuer
            .verify_strict(
                &self.signing_input()?,
                &Signature::from_bytes(&signature_bytes),
            )
            .map_err(|_| PeerTicketError::BadIssuerSignature)
    }
}

/// Distinguishable refusals. `epics.md:8153` P11 — every refusal carries a
/// typed reason, and *expired* must never collapse into *altered*.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerTicketError {
    #[error("this ticket is malformed: {reason}. Nothing was changed.")]
    Malformed { reason: String },
    #[error("this ticket expired. Ask for a fresh one; nothing was changed.")]
    Expired { not_after: i64 },
    #[error(
        "this ticket's contents do not match the signature its issuer made — it was altered after it was minted. Nothing was changed."
    )]
    BadIssuerSignature,
}

/// Render key material head…tail so **both ends stay checkable**.
///
/// Head-only truncation is forgeable at the tail: two keys sharing a prefix
/// read as equal. `max_columns` counts the rendered characters including the
/// separating ellipsis.
#[must_use]
pub fn short_fingerprint(full: &str, max_columns: usize) -> String {
    if full.chars().count() <= max_columns {
        return full.to_owned();
    }
    if max_columns <= 1 {
        return "…".to_owned();
    }
    let head_len = (max_columns - 1) / 2;
    let tail_len = max_columns - 1 - head_len;
    let chars: Vec<char> = full.chars().collect();
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
}

fn pin_error(error: A2aPeerSpecError) -> PeerTicketError {
    PeerTicketError::Malformed {
        reason: format!("ticket key is unusable ({error})"),
    }
}

/// Append a one-byte-length-prefixed UTF-8 field.
fn push_short_str(out: &mut Vec<u8>, value: &str, field: &str) -> Result<(), PeerTicketError> {
    let len: u8 = value
        .len()
        .try_into()
        .map_err(|_| PeerTicketError::Malformed {
            reason: format!(
                "the {field} is {} bytes, past the {} the format carries",
                value.len(),
                u8::MAX
            ),
        })?;
    out.push(len);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

/// A bounds-checked forward reader over the wire body. Every overrun is a
/// [`PeerTicketError::Malformed`] with a reason, never a panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], PeerTicketError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| PeerTicketError::Malformed {
                reason: "ticket body length overflowed".to_owned(),
            })?;
        if end > self.bytes.len() {
            return Err(PeerTicketError::Malformed {
                reason: "ticket body is truncated".to_owned(),
            });
        }
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn take_u8(&mut self) -> Result<u8, PeerTicketError> {
        Ok(self.take(1)?[0])
    }

    fn take_short_str(&mut self, field: &str) -> Result<String, PeerTicketError> {
        let len = self.take_u8()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| PeerTicketError::Malformed {
            reason: format!("the {field} is not valid UTF-8"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_ed25519_public_keys_do_not_validate_ticket_signatures() {
        use ed25519_dalek::Verifier as _;

        // Compressed Edwards identity. With R=identity and s=0, dalek's
        // permissive verifier accepts this signature for every message; strict
        // verification must reject the small-order public key.
        let mut identity = [0_u8; 32];
        identity[0] = 1;
        let offered_key = PinnedKey::parse(EDDSA_LABEL, URL_SAFE_NO_PAD.encode(identity), None)
            .expect("identity encoding");
        let mut signature = [0_u8; SIGNATURE_LEN];
        signature[0] = 1;
        let ticket = PeerTicket {
            offered_key,
            addresses: Vec::new(),
            not_after: 2_000_000_000,
            suggested_name: None,
            signature: Ed25519Sig(signature.to_vec()),
        };
        let issuer = VerifyingKey::from_bytes(&identity).expect("weak public key parses");
        let signature = Signature::from_bytes(&signature);
        assert!(
            issuer
                .verify(&ticket.signing_input().expect("input"), &signature)
                .is_ok(),
            "fixture must catch a regression to permissive verification"
        );

        let blob = ticket.encode().expect("encode");
        assert!(matches!(
            PeerTicket::decode(&blob, 1_900_000_000),
            Err(PeerTicketError::BadIssuerSignature)
        ));
    }
}
