//! Story 19.18c — the per-item capability.
//!
//! A sender tags every item it sends with a value only it can re-derive:
//! `URL_SAFE_NO_PAD(blake3::keyed_hash(sub, messageId))`, where `sub` is a
//! `derive_key` subkey of a dedicated 32-byte key file (`D1`, `P2`). The
//! recipient hashes the value at ingress and keeps only `sha256(value)`; an
//! API-key caller is later served the item's thread, follow-up reference,
//! retract, `tasks/get` and `tasks/cancel` only when it presents the value
//! again. A co-holder of the same API key cannot derive it.
//!
//! ⛔ The key never leaves this module except as a derivation; neither the key
//! nor a derived value has a `Display`, and their `Debug` prints no bytes.
//! ⛔ The capability is an authorization factor, never part of an address
//! (`A1`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// The key file's name inside the per-user data directory (`D1`).
pub const ITEM_CAPABILITY_KEY_FILE: &str = "item-capability.key";

/// The key file's exact length. ⛔ Never padded, never truncated.
pub const ITEM_CAPABILITY_KEY_BYTES: usize = 32;

/// The wire length of one capability: 32 bytes, URL-safe base64, unpadded.
pub const ITEM_CAPABILITY_WIRE_CHARS: usize = 43;

/// Story 19.18c `P2` — the `derive_key` context, frozen with the v1 carrier.
/// A new capability format ships under a new metadata key, never a new
/// context behind the old one.
const DERIVE_CONTEXT: &str = "rustain 2026-09-29 item-capability v1";

/// This host's item-capability key: only the `derive_key` subkey is held.
pub struct ItemCapabilityKey {
    subkey: Zeroizing<[u8; 32]>,
}

impl ItemCapabilityKey {
    /// Derive the v1 subkey from the key file's 32 bytes.
    #[must_use]
    pub fn from_file_bytes(root: &[u8; ITEM_CAPABILITY_KEY_BYTES]) -> Self {
        Self {
            subkey: Zeroizing::new(blake3::derive_key(DERIVE_CONTEXT, root)),
        }
    }
}

impl std::fmt::Debug for ItemCapabilityKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ItemCapabilityKey(..)")
    }
}

/// One derived capability, in its wire form.
#[derive(Clone, PartialEq, Eq)]
pub struct ItemCapability(Zeroizing<String>);

impl ItemCapability {
    /// The 43-character wire value, for the one carrier that sends it.
    #[must_use]
    pub fn as_wire_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ItemCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ItemCapability(..)")
    }
}

/// The capability of the item a `message/send` with `message_id` creates.
#[must_use]
pub fn derive_item_capability(key: &ItemCapabilityKey, message_id: &str) -> ItemCapability {
    let mac = blake3::keyed_hash(&key.subkey, message_id.as_bytes());
    ItemCapability(Zeroizing::new(URL_SAFE_NO_PAD.encode(mac.as_bytes())))
}

/// A capability a caller presented, already reduced to `sha256(decoded)`.
/// ⛔ The raw value is dropped at parse time and never stored or logged.
#[derive(Clone, Copy)]
pub struct PresentedCapability([u8; 32]);

impl PresentedCapability {
    /// The lowercase hex digest a recipient item stores (`A13`).
    #[must_use]
    pub fn stored_hash(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for PresentedCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PresentedCapability(..)")
    }
}

/// A capability carrier that is present but not a canonical v1 value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedCapability;

/// Parse one optional carrier (`A7` step 1): absent or `null` is `None`; a
/// 43-character canonical `URL_SAFE_NO_PAD` string decoding to 32 bytes is
/// `Some`; anything else is malformed.
///
/// # Errors
///
/// [`MalformedCapability`] for a present value that is not a canonical v1
/// capability.
pub fn parse_presented_capability(
    value: Option<&serde_json::Value>,
) -> Result<Option<PresentedCapability>, MalformedCapability> {
    let text = match value {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(text)) => text,
        Some(_) => return Err(MalformedCapability),
    };
    if text.len() != ITEM_CAPABILITY_WIRE_CHARS {
        return Err(MalformedCapability);
    }
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(text.as_bytes())
            .map_err(|_| MalformedCapability)?,
    );
    if decoded.len() != 32 {
        return Err(MalformedCapability);
    }
    Ok(Some(PresentedCapability(Sha256::digest(&*decoded).into())))
}

/// The one compare between a stored hash and a presented capability. A stored
/// hash that does not decode to 32 bytes never matches (fail closed, `A14`).
/// `ct_eq` over the two digests is hygiene, not a claimed property (`P9`).
#[must_use]
pub fn capability_matches(stored_hash: &str, presented: &PresentedCapability) -> bool {
    let mut stored = [0u8; 32];
    if hex::decode_to_slice(stored_hash, &mut stored).is_err() {
        return false;
    }
    stored.ct_eq(&presented.0).into()
}

/// Whether a presented capability matches an item's stored hash — computed
/// once, independent of the caller's trust (`P7`). `false` when either side
/// is absent.
#[must_use]
pub fn presented_matches(
    stored_hash: Option<&str>,
    presented: Option<&PresentedCapability>,
) -> bool {
    match (stored_hash, presented) {
        (Some(stored), Some(presented)) => capability_matches(stored, presented),
        _ => false,
    }
}

/// Why the key file could not be used. Every message names the item
/// capability key, never its bytes.
#[derive(Debug, thiserror::Error)]
pub enum ItemCapabilityKeyError {
    #[error("item capability key I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("item capability key entropy generation failed: {0}")]
    Entropy(#[from] rand_core::Error),
    #[error("item capability key file must be exactly 32 bytes, found {len}")]
    WrongLength { len: usize },
    #[error("item capability key file permissions must be 0600, got {mode:o}")]
    InsecurePermissions { mode: u32 },
}

/// The dedicated key file (`A5`). Mirrors `IdentityKeyStore`, but creates the
/// file atomically with a parent-directory sync (`P3`) and never writes an
/// existing file.
#[derive(Clone, Debug)]
pub struct ItemCapabilityKeyStore {
    path: PathBuf,
}

impl ItemCapabilityKeyStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(ITEM_CAPABILITY_KEY_FILE),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the key, or generate it when the file is absent. ⛔ A file that
    /// exists but cannot be loaded is an error, never a reason to write.
    ///
    /// # Errors
    ///
    /// Any I/O, length, permission or entropy failure.
    pub fn load_or_generate(&self) -> Result<ItemCapabilityKey, ItemCapabilityKeyError> {
        if self.path.exists() {
            return self.load();
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.create_or_collide()
    }

    /// Generate and persist a key, or — when another creator won the race —
    /// load the winner's. Split out so the collision arm is directly
    /// exercisable (the `IdentityKeyStore::create_or_collide` seam).
    fn create_or_collide(&self) -> Result<ItemCapabilityKey, ItemCapabilityKeyError> {
        let mut root = Zeroizing::new([0u8; ITEM_CAPABILITY_KEY_BYTES]);
        OsRng.try_fill_bytes(&mut *root)?;
        match crate::infrastructure::transparency::create_private_noclobber(
            &self.path,
            &*root,
            ".item-capability-",
        )? {
            crate::infrastructure::transparency::PrivateCreate::Created => {
                Ok(ItemCapabilityKey::from_file_bytes(&root))
            }
            crate::infrastructure::transparency::PrivateCreate::AlreadyExists => self.load(),
        }
    }

    /// Load an existing key, refusing any mode but 0600 (unix) and any length
    /// but 32 bytes.
    ///
    /// # Errors
    ///
    /// Any I/O, length or permission failure.
    pub fn load(&self) -> Result<ItemCapabilityKey, ItemCapabilityKeyError> {
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&self.path)?.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(ItemCapabilityKeyError::InsecurePermissions { mode });
            }
        }
        let bytes = Zeroizing::new(std::fs::read(&self.path)?);
        let root: &[u8; ITEM_CAPABILITY_KEY_BYTES] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| ItemCapabilityKeyError::WrongLength { len: bytes.len() })?;
        Ok(ItemCapabilityKey::from_file_bytes(root))
    }
}

/// What a composition root hands `A2aEgress::compose` (`PQ1`).
#[derive(Clone, Debug)]
pub enum ItemCapabilityKeyState {
    /// The key loaded (or was generated): every send carries a capability.
    Ready(Arc<ItemCapabilityKey>),
    /// A key file exists but cannot be used, generation failed, or the data
    /// directory could not be resolved: sends to credentialed peers are
    /// refused with a repair sentence; uncredentialed peers send legacy.
    Unusable { path: String, cause: String },
    /// No capability at all — only test harnesses that prove pre-19.18c
    /// behaviour. ⛔ The roots never construct it.
    Disabled,
}

/// The literal a refusal names when the data directory itself failed.
const DATA_DIR_UNRESOLVED: &str = "the rustain data directory";

/// The one loader both composition roots call (`P5`). A missing file is
/// generated; any failure logs one warning naming the path and cause and
/// yields [`ItemCapabilityKeyState::Unusable`]. ⛔ Never a compose error and
/// ⛔ never a regeneration over an existing file (`A4`).
#[must_use]
pub fn load_item_capability_key(data_dir: anyhow::Result<PathBuf>) -> ItemCapabilityKeyState {
    let dir = match data_dir {
        Ok(dir) => dir,
        Err(error) => {
            let cause = error.to_string();
            tracing::warn!(path = DATA_DIR_UNRESOLVED, %cause, "A2A item capability key unusable");
            return ItemCapabilityKeyState::Unusable {
                path: DATA_DIR_UNRESOLVED.to_owned(),
                cause,
            };
        }
    };
    let store = ItemCapabilityKeyStore::new(dir);
    match store.load_or_generate() {
        Ok(key) => ItemCapabilityKeyState::Ready(Arc::new(key)),
        Err(error) => {
            let path = store.path().display().to_string();
            let cause = error.to_string();
            tracing::warn!(%path, %cause, "A2A item capability key unusable");
            ItemCapabilityKeyState::Unusable { path, cause }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key(byte: u8) -> ItemCapabilityKey {
        ItemCapabilityKey::from_file_bytes(&[byte; ITEM_CAPABILITY_KEY_BYTES])
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// `K01` — the store: generate 0600/32 B, reload the identical key, refuse
    /// a loosened mode and a wrong length, never rewrite an existing file, and
    /// the race loser loads the winner's key.
    #[test]
    fn k01_the_key_store_generates_once_and_refuses_what_it_cannot_trust() {
        let dir = tempfile::tempdir().expect("dir");
        let store = ItemCapabilityKeyStore::new(dir.path());
        let first = store.load_or_generate().expect("generate");
        let bytes = std::fs::read(store.path()).expect("key file");
        assert_eq!(bytes.len(), ITEM_CAPABILITY_KEY_BYTES);
        #[cfg(unix)]
        assert_eq!(mode(store.path()), 0o600);
        let modified = std::fs::metadata(store.path())
            .expect("metadata")
            .modified()
            .expect("mtime");
        let second = store.load_or_generate().expect("reload");
        assert_eq!(
            derive_item_capability(&first, "rustain-1").as_wire_str(),
            derive_item_capability(&second, "rustain-1").as_wire_str(),
            "a second load returns the identical key"
        );
        assert_eq!(std::fs::read(store.path()).expect("key file"), bytes);
        assert_eq!(
            std::fs::metadata(store.path())
                .expect("metadata")
                .modified()
                .expect("mtime"),
            modified,
            "an existing valid file is never rewritten"
        );

        // The race loser: `create_private_noclobber` answers `AlreadyExists`
        // and the winner's key is loaded, driven through the seam.
        let loser = store
            .create_or_collide()
            .expect("collision loads the winner");
        assert_eq!(
            derive_item_capability(&loser, "rustain-1").as_wire_str(),
            derive_item_capability(&first, "rustain-1").as_wire_str()
        );
        assert_eq!(std::fs::read(store.path()).expect("key file"), bytes);

        for len in [31usize, 33] {
            let dir = tempfile::tempdir().expect("dir");
            let store = ItemCapabilityKeyStore::new(dir.path());
            std::fs::write(store.path(), vec![7u8; len]).expect("plant");
            #[cfg(unix)]
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
            assert!(matches!(
                store.load_or_generate(),
                Err(ItemCapabilityKeyError::WrongLength { len: found }) if found == len
            ));
            assert_eq!(
                std::fs::read(store.path()).expect("file"),
                vec![7u8; len],
                "a refused file is never overwritten"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn k01_a_loosened_mode_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().expect("dir");
        let store = ItemCapabilityKeyStore::new(dir.path());
        store.load_or_generate().expect("generate");
        let bytes = std::fs::read(store.path()).expect("key file");
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(matches!(
            store.load_or_generate(),
            Err(ItemCapabilityKeyError::InsecurePermissions { mode: 0o644 })
        ));
        assert!(matches!(
            store.create_or_collide(),
            Err(ItemCapabilityKeyError::InsecurePermissions { mode: 0o644 })
        ));
        assert_eq!(std::fs::read(store.path()).expect("key file"), bytes);
    }

    /// `K02` (derivation half) — 43 URL-safe characters; distinct per key and
    /// per message id; and a known-answer vector that pins the `derive_key`
    /// context (`P2`, `M29`).
    #[test]
    fn k02_the_derivation_is_keyed_domain_separated_and_pinned() {
        let key = fixed_key(1);
        let capability = derive_item_capability(&key, "rustain-1-0000000000000000");
        let wire = capability.as_wire_str();
        assert_eq!(wire.len(), ITEM_CAPABILITY_WIRE_CHARS);
        assert!(
            wire.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "{wire}"
        );
        assert_ne!(
            wire,
            derive_item_capability(&fixed_key(2), "rustain-1-0000000000000000").as_wire_str()
        );
        assert_ne!(
            wire,
            derive_item_capability(&key, "rustain-2-0000000000000000").as_wire_str()
        );
        assert_eq!(
            wire, KNOWN_ANSWER,
            "the v1 derivation (context + keyed hash) is frozen"
        );
        assert_eq!(
            format!("{key:?} {capability:?}"),
            "ItemCapabilityKey(..) ItemCapability(..)"
        );
    }

    /// `derive(key = [1; 32], "rustain-1-0000000000000000")` under the v1
    /// context, computed independently with the reference `blake3` Python
    /// binding (`derive_key_context=…`, then `key=subkey`), unpadded URL-safe.
    const KNOWN_ANSWER: &str = "ZZSlKP-e-zSi_doJMGn5DmP2A3Kd_6wrNy0MgbXkM2U";

    /// `K14` (compare half) — match, a mismatch at byte 0 and at byte 31, and
    /// an undecodable stored hash that never matches.
    #[test]
    fn k14_the_one_compare_matches_exactly_and_fails_closed() {
        let wire = derive_item_capability(&fixed_key(3), "rustain-9-0000000000000000");
        let presented =
            parse_presented_capability(Some(&serde_json::Value::from(wire.as_wire_str())))
                .expect("well formed")
                .expect("present");
        let stored = presented.stored_hash();
        assert!(capability_matches(&stored, &presented));
        for index in [0usize, 31] {
            let mut digest = [0u8; 32];
            hex::decode_to_slice(&stored, &mut digest).expect("hex");
            digest[index] ^= 0x01;
            assert!(
                !capability_matches(&hex::encode(digest), &presented),
                "byte {index}"
            );
        }
        for broken in ["zz", "", &stored[..62]] {
            assert!(!capability_matches(broken, &presented), "{broken:?}");
        }
        assert!(!presented_matches(None, Some(&presented)));
        assert!(!presented_matches(Some(&stored), None));
    }

    /// The parse half of `A7` step 1 (the served probes are `K04`).
    #[test]
    fn a_malformed_carrier_is_refused_and_null_is_absent() {
        let wire = derive_item_capability(&fixed_key(4), "rustain-4-0000000000000000");
        let text = wire.as_wire_str().to_owned();
        assert!(parse_presented_capability(None).expect("absent").is_none());
        assert!(
            parse_presented_capability(Some(&serde_json::Value::Null))
                .expect("null")
                .is_none()
        );
        assert!(
            parse_presented_capability(Some(&serde_json::Value::from(text.as_str())))
                .expect("valid")
                .is_some()
        );
        // 43 symbols carry 258 bits for 256: the last symbol's two low bits
        // must be zero. `B` (1) never is, so this is a non-canonical value.
        let mut noncanonical = text.clone();
        noncanonical.pop();
        noncanonical.push('B');
        for bad in [
            serde_json::Value::from(&text[..42]),
            serde_json::Value::from(format!("{text}A")),
            serde_json::Value::from(format!("{}+", &text[..42])),
            serde_json::Value::from(format!("{}/", &text[..42])),
            serde_json::Value::from(format!("{}=", &text[..42])),
            serde_json::json!(7),
            serde_json::json!({ "value": text }),
        ] {
            assert_eq!(
                parse_presented_capability(Some(&bad)).map(|parsed| parsed.is_some()),
                Err(MalformedCapability),
                "{bad}"
            );
        }
        assert_eq!(
            parse_presented_capability(Some(&serde_json::Value::from(noncanonical)))
                .map(|parsed| parsed.is_some()),
            Err(MalformedCapability),
            "a final symbol with non-zero trailing bits is refused"
        );
    }

    /// The loader helper both roots call: a missing file is generated, an
    /// unusable one yields `Unusable` naming the path, and a data-directory
    /// failure names the directory literal.
    #[test]
    fn the_loader_generates_or_reports_unusable() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(matches!(
            load_item_capability_key(Ok(dir.path().to_path_buf())),
            ItemCapabilityKeyState::Ready(_)
        ));
        std::fs::write(dir.path().join(ITEM_CAPABILITY_KEY_FILE), [0u8; 31]).expect("plant");
        let ItemCapabilityKeyState::Unusable { path, cause } =
            load_item_capability_key(Ok(dir.path().to_path_buf()))
        else {
            panic!("a 31-byte file is unusable");
        };
        assert!(path.ends_with(ITEM_CAPABILITY_KEY_FILE), "{path}");
        assert!(cause.contains("item capability key"), "{cause}");
        let ItemCapabilityKeyState::Unusable { path, .. } =
            load_item_capability_key(Err(anyhow::anyhow!("no home")))
        else {
            panic!("a data-directory failure is unusable");
        };
        assert_eq!(path, DATA_DIR_UNRESOLVED);
    }
}
