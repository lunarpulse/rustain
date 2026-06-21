use serde::{Deserialize, Serialize};

/// RAP peer identity placeholder for R2 signing.
///
/// R1 pins this as text so the canonical byte layout is stable before crypto
/// lands. The string is expected to be a multihash-style peer id with an
/// explicit codec/hash prefix (for example, a future `ed25519-pub` multihash
/// prefix), but R1 does not parse or verify it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PeerId(pub String);

/// Ed25519 signature placeholder for R2.
///
/// Stored as `Vec<u8>` rather than `[u8; 64]` so serde support is available
/// without an extra dependency. R1 never mints signed tokens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ed25519Sig(pub Vec<u8>);
