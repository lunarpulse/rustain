//! Self-update subsystem (Story 13.3a, FR103).
//!
//! `rustain update` downloads, verifies (signature + checksum), and atomically
//! replaces the running binary. `rustain update --check` is a non-destructive
//! dry-run that always exits 0.
//!
//! Architecture: port-based, NOT the `self_update` crate.
//! - `SelfUpdatePort` (network): latest-release query + asset download.
//! - `BinaryReplacerPort` (filesystem): backup + atomic replace + restore.
//! - `verify_release` (pure fn): minisign signature over SHA256SUMS manifest,
//!   then hash binding for the exact target-triple asset.

pub mod client;
pub mod orchestrator;
pub mod replacer;
pub mod trust;
pub mod types;
pub mod verify;
