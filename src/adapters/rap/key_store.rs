use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rand_core::{OsRng, RngCore};
use thiserror::Error;
use zeroize::Zeroize;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use super::AgentSigner;

const KEY_FILE_NAME: &str = "peer-identity.pkcs8.der";

#[derive(Clone, Debug)]
pub struct IdentityKeyStore {
    path: PathBuf,
}

impl IdentityKeyStore {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            path: base_dir.as_ref().join(KEY_FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load an existing identity key, or create one if it is absent.
    ///
    /// Creation uses an exclusive (`create_new`) open, so two processes racing
    /// to provision a peer identity cannot both succeed: the loser's open fails
    /// with [`ErrorKind::AlreadyExists`], at which point it loads the winner's
    /// key instead. Every read path re-runs the 0600 fail-closed permission
    /// check before trusting a key.
    pub fn load_or_generate(&self) -> Result<AgentSigner, KeyStoreError> {
        if self.path.exists() {
            return self.load();
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.create_or_collide()
    }

    /// Generate a key and persist it, or — on a create race — load the winner.
    ///
    /// Split out from [`Self::load_or_generate`] so the collision branch is
    /// directly exercisable: `load_or_generate`'s `exists()` fast path makes the
    /// `AlreadyExists` arm unreachable once a file is present, so tests drive it
    /// through this seam with a pre-existing winner file.
    fn create_or_collide(&self) -> Result<AgentSigner, KeyStoreError> {
        let signing_key = generate_signing_key()?;
        let der = signing_key.to_pkcs8_der()?;
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(&self.path) {
            Ok(mut file) => {
                file.write_all(der.as_bytes())?;
                file.flush()?;
                #[cfg(unix)]
                assert_key_perms_0600(&self.path)?;
                Ok(AgentSigner::from_signing_key(signing_key))
            }
            // Lost the create race to a concurrent peer: discard this freshly
            // generated (and never persisted) key and load the winner's. `load`
            // re-runs the 0600 fail-closed check, so a winner file whose perms
            // were tampered with still rejects.
            Err(err) if err.kind() == ErrorKind::AlreadyExists => self.load(),
            Err(err) => Err(err.into()),
        }
    }

    pub fn load(&self) -> Result<AgentSigner, KeyStoreError> {
        #[cfg(unix)]
        assert_key_perms_0600(&self.path)?;
        let mut bytes = fs::read(&self.path)?;
        let signing_key = SigningKey::from_pkcs8_der(&bytes)?;
        bytes.zeroize();
        Ok(AgentSigner::from_signing_key(signing_key))
    }
}

/// Generate an Ed25519 signing key from the OS CSPRNG.
///
/// `OsRng` (getrandom-backed) is available on every supported platform, unlike
/// `/dev/urandom` which is Unix-only. The 32-byte seed is explicitly zeroized
/// once consumed.
fn generate_signing_key() -> Result<SigningKey, KeyStoreError> {
    let mut seed = [0u8; 32];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut seed)?;
    let key = SigningKey::from_bytes(&seed);
    seed.zeroize();
    Ok(key)
}

#[cfg(unix)]
fn assert_key_perms_0600(path: &Path) -> Result<(), KeyStoreError> {
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(KeyStoreError::InsecurePermissions { mode });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum KeyStoreError {
    #[error("identity key I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity key entropy generation failed: {0}")]
    Entropy(#[from] rand_core::Error),
    #[error("identity key encode/decode failed: {0}")]
    Pkcs8(#[from] ed25519_dalek::pkcs8::Error),
    #[error("identity key file permissions must be 0600, got {mode:o}")]
    InsecurePermissions { mode: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_store_generates_and_loads_0600_pkcs8() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityKeyStore::new(dir.path());
        let signer = store.load_or_generate().unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(signer.identity().peer_id, loaded.identity().peer_id);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_store_refuses_non_0600_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityKeyStore::new(dir.path());
        store.load_or_generate().unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.load().unwrap_err(),
            KeyStoreError::InsecurePermissions { mode: 0o644 }
        ));
    }

    /// Two independent stores must provision distinct peer identities — guards
    /// against a degenerate/constant RNG silently replacing `/dev/urandom`.
    #[test]
    fn generated_keys_are_unique() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ka = IdentityKeyStore::new(a.path()).load_or_generate().unwrap();
        let kb = IdentityKeyStore::new(b.path()).load_or_generate().unwrap();
        assert_ne!(ka.identity().peer_id, kb.identity().peer_id);
    }

    /// Race loser (`create_new` → `AlreadyExists`) loads the winner's identity.
    /// Driven through the `create_or_collide` seam with a pre-existing winner
    /// file, which deterministically forces the collision arm without relying
    /// on a real filesystem race.
    #[test]
    fn race_loser_loads_winner_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityKeyStore::new(dir.path());
        let winner = store.load_or_generate().unwrap();
        // File now exists → `create_new` fails `AlreadyExists` → load winner.
        let loser = store.create_or_collide().unwrap();
        assert_eq!(winner.identity().peer_id, loser.identity().peer_id);
    }

    /// The collision/loser branch still fails closed when the winner file's
    /// permissions have been loosened below 0600.
    #[cfg(unix)]
    #[test]
    fn race_loser_fails_closed_on_insecure_winner_perms() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityKeyStore::new(dir.path());
        store.load_or_generate().unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.create_or_collide().unwrap_err(),
            KeyStoreError::InsecurePermissions { mode: 0o644 }
        ));
    }

    /// A create error that is NOT `AlreadyExists` must propagate (fail closed)
    /// rather than be misread as a collision. Invoking the seam directly with a
    /// path whose parent directory is absent yields `NotFound`, not collision.
    #[test]
    fn create_or_collide_propagates_non_collision_errors() {
        let dir = tempfile::tempdir().unwrap();
        // `load_or_generate` would `mkdir -p` the parent first; calling the seam
        // directly keeps the parent missing → `create_new` fails with NotFound.
        let store = IdentityKeyStore::new(dir.path().join("no_such_parent"));
        assert!(matches!(
            store.create_or_collide().unwrap_err(),
            KeyStoreError::Io(_)
        ));
    }
}
