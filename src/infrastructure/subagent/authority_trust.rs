//! Local authority trust and workspace admission (Story Q-6 AC6).
//!
//! # Approved narrow design (conservation only)
//!
//! - **Identity.** One Ed25519 *authority key* per user, generated on first use
//!   and kept in `<data_dir>/authority/signing.key` (mode `0600`), outside every
//!   workspace. Every signature is domain-separated and bound to the workspace's
//!   canonical-path hash, so a record cannot be replayed into another workspace.
//! - **Independent freshness.** `<data_dir>/authority/<workspace>.anchor` stores
//!   the highest durable conservation sequence and authority time, signed with
//!   the same key. The workspace journal cannot move it.
//! - **Lifecycle.** A workspace is *established* the first time it starts with
//!   NO conservation history and NO anchor. After that a restart is admitted only
//!   from a verified, fresh history. History without an anchor (legacy unsigned
//!   state, or an anchor that is gone) is refused: missing recovery data is never
//!   evidence of first initialization.
//! - **Ordering.** Records are journaled first; the anchor is raised afterwards.
//!   A crash between the two leaves the journal ahead of the anchor, which the
//!   next verified start accepts and catches the anchor up to.
//! - **Single writer.** The authority holds an exclusive lease
//!   (`<workspace>.lease`, `flock`) for the life of the process. A second
//!   process over the same workspace is refused rather than allowed to journal a
//!   second, conflicting conservation sequence over the same root budget.
//! - **Legacy.** Unsigned records are refused, not migrated: no existing record
//!   can be verified independently.
//!
//! The wider authenticated-journal design (`DF-18-2-AUTHENTICATED-JOURNAL`)
//! stays open: room, apply and transparency journals keep their
//! trusted-filesystem wording, and nothing here claims an audit trail.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};

use crate::domain::clock::Clock;
use crate::domain::models::{
    AgentId, Budget, CapabilitySet, CapabilityToken, JournalRecord, LedgerConservationRecord,
};
use crate::domain::ports::{AuthorityAnchor, AuthorityTrust, AuthorityTrustError};
use crate::domain::services::authority_ledger::AuthorityLedger;
use crate::domain::services::authority_recovery::RecoveryRefusal;
use crate::infrastructure::subagent::NodeJournal;

const DOMAIN: &[u8] = b"rustain.authority.v1\0";
const KEY_FILE: &str = "signing.key";

fn store_error(reason: &str) -> AuthorityTrustError {
    AuthorityTrustError(reason.to_owned())
}

fn io_error(context: &str, error: &std::io::Error) -> AuthorityTrustError {
    AuthorityTrustError(format!("{context}: {}", error.kind()))
}

/// The filesystem [`AuthorityTrust`]. Holds the single-writer lease.
pub struct LocalAuthorityTrust {
    key: SigningKey,
    workspace_id: String,
    anchor_path: PathBuf,
    ledger_seq: AtomicU64,
    authority_time_ms: AtomicU64,
    /// Exclusive `flock` for the process lifetime; released on drop or exit.
    _lease: std::fs::File,
}

/// A workspace with no anchor yet: it may be established, or refused if it has
/// history.
pub struct UnestablishedAuthority {
    dir: PathBuf,
    workspace_id: String,
    anchor_path: PathBuf,
    lease: std::fs::File,
}

/// The outcome of opening the trust for a workspace.
pub enum TrustOpening {
    Established(LocalAuthorityTrust),
    Unestablished(UnestablishedAuthority),
    /// The anchor or key is present but cannot be verified.
    Unverifiable,
    /// Another live process holds this workspace's authority lease.
    Busy,
}

fn anchor_message(workspace_id: &str, anchor: AuthorityAnchor) -> Vec<u8> {
    let mut message = DOMAIN.to_vec();
    message.extend_from_slice(b"anchor\0");
    message.extend_from_slice(workspace_id.as_bytes());
    message.push(0);
    message.extend_from_slice(&anchor.ledger_seq.to_le_bytes());
    message.extend_from_slice(&anchor.authority_time_ms.to_le_bytes());
    message
}

fn record_message(workspace_id: &str, message: &[u8]) -> Vec<u8> {
    let mut bound = DOMAIN.to_vec();
    bound.extend_from_slice(b"record\0");
    bound.extend_from_slice(workspace_id.as_bytes());
    bound.push(0);
    bound.extend_from_slice(message);
    bound
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn load_key(path: &Path) -> Result<SigningKey, AuthorityTrustError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| io_error("authority key unreadable", &e))?;
    let bytes: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| store_error("authority key is malformed"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn create_key(path: &Path) -> Result<SigningKey, AuthorityTrustError> {
    use rand_core::{OsRng, RngCore};
    use std::io::Write;
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(hex::encode(seed).as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| io_error("authority key write failed", &e))?;
            Ok(SigningKey::from_bytes(&seed))
        }
        // A concurrent establisher created it first: use that key.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_key(path),
        Err(e) => Err(io_error("authority key create failed", &e)),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AnchorFile {
    ledger_seq: u64,
    authority_time_ms: u64,
    sig: String,
}

fn write_anchor(
    key: &SigningKey,
    workspace_id: &str,
    path: &Path,
    anchor: AuthorityAnchor,
) -> Result<(), AuthorityTrustError> {
    use std::io::Write;
    let sig = key.sign(&anchor_message(workspace_id, anchor));
    let body = serde_json::to_vec(&AnchorFile {
        ledger_seq: anchor.ledger_seq,
        authority_time_ms: anchor.authority_time_ms,
        sig: hex::encode(sig.to_bytes()),
    })
    .map_err(|_| store_error("anchor encode failed"))?;
    let tmp = path.with_extension("anchor.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| io_error("anchor write failed", &e))?;
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_error("anchor write failed", &e))?;
    std::fs::rename(&tmp, path).map_err(|e| io_error("anchor replace failed", &e))?;
    if let Some(dir) = path.parent()
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn read_anchor(key: &SigningKey, workspace_id: &str, path: &Path) -> Option<AuthorityAnchor> {
    let file: AnchorFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let anchor = AuthorityAnchor {
        ledger_seq: file.ledger_seq,
        authority_time_ms: file.authority_time_ms,
    };
    let sig = Signature::from_slice(&hex::decode(file.sig).ok()?).ok()?;
    key.verifying_key()
        .verify(&anchor_message(workspace_id, anchor), &sig)
        .ok()?;
    Some(anchor)
}

/// Open the authority trust for `workspace` under `data_dir`.
#[must_use]
pub fn open(data_dir: &Path, workspace: &Path) -> TrustOpening {
    let dir = data_dir.join("authority");
    if create_private_dir(&dir).is_err() {
        return TrustOpening::Unverifiable;
    }
    let workspace_id = crate::infrastructure::paths::workspace_hash(workspace);
    let lease = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{workspace_id}.lease")))
    {
        Ok(file) => file,
        Err(_) => return TrustOpening::Unverifiable,
    };
    match lease.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return TrustOpening::Busy,
        Err(std::fs::TryLockError::Error(_)) => return TrustOpening::Unverifiable,
    }
    let anchor_path = dir.join(format!("{workspace_id}.anchor"));
    if !anchor_path.exists() {
        return TrustOpening::Unestablished(UnestablishedAuthority {
            dir,
            workspace_id,
            anchor_path,
            lease,
        });
    }
    let Ok(key) = load_key(&dir.join(KEY_FILE)) else {
        return TrustOpening::Unverifiable;
    };
    let Some(anchor) = read_anchor(&key, &workspace_id, &anchor_path) else {
        return TrustOpening::Unverifiable;
    };
    TrustOpening::Established(LocalAuthorityTrust {
        key,
        workspace_id,
        anchor_path,
        ledger_seq: AtomicU64::new(anchor.ledger_seq),
        authority_time_ms: AtomicU64::new(anchor.authority_time_ms),
        _lease: lease,
    })
}

impl UnestablishedAuthority {
    /// Establish the authority: create the key if the user has none yet, then
    /// write the initial anchor (sequence 0). Called only for a workspace with no
    /// conservation history.
    pub fn establish(self) -> Result<LocalAuthorityTrust, AuthorityTrustError> {
        let key_path = self.dir.join(KEY_FILE);
        let key = if key_path.exists() {
            load_key(&key_path)?
        } else {
            create_key(&key_path)?
        };
        let anchor = AuthorityAnchor::default();
        write_anchor(&key, &self.workspace_id, &self.anchor_path, anchor)?;
        Ok(LocalAuthorityTrust {
            key,
            workspace_id: self.workspace_id,
            anchor_path: self.anchor_path,
            ledger_seq: AtomicU64::new(0),
            authority_time_ms: AtomicU64::new(0),
            _lease: self.lease,
        })
    }

    /// Where the anchor would live — shown to the operator, never a secret.
    #[must_use]
    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }
}

impl LocalAuthorityTrust {
    /// The anchor file path (not a secret), for operator diagnostics.
    #[must_use]
    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }
}

impl AuthorityTrust for LocalAuthorityTrust {
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.key
            .sign(&record_message(&self.workspace_id, message))
            .to_bytes()
            .to_vec()
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        Signature::from_slice(signature).is_ok_and(|sig| {
            self.key
                .verifying_key()
                .verify(&record_message(&self.workspace_id, message), &sig)
                .is_ok()
        })
    }

    fn anchor(&self) -> AuthorityAnchor {
        AuthorityAnchor {
            ledger_seq: self.ledger_seq.load(Ordering::Acquire),
            authority_time_ms: self.authority_time_ms.load(Ordering::Acquire),
        }
    }

    fn advance_anchor(&self, to: AuthorityAnchor) -> Result<(), AuthorityTrustError> {
        let current = self.anchor();
        let next = AuthorityAnchor {
            ledger_seq: current.ledger_seq.max(to.ledger_seq),
            authority_time_ms: current.authority_time_ms.max(to.authority_time_ms),
        };
        if next == current {
            return Ok(());
        }
        write_anchor(&self.key, &self.workspace_id, &self.anchor_path, next)?;
        self.ledger_seq.store(next.ledger_seq, Ordering::Release);
        self.authority_time_ms
            .store(next.authority_time_ms, Ordering::Release);
        Ok(())
    }
}

/// Why a workspace's authority was not admitted at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// The stored history failed verification or freshness.
    History(RecoveryRefusal),
    /// Another live process holds this workspace's authority lease.
    Busy,
    /// The journal or the trust store could not be read.
    StoreUnavailable,
}

impl AdmissionRefusal {
    /// A safe local diagnostic: a category, never a record, key or value.
    #[must_use]
    pub fn diagnostic(self) -> String {
        let reason = match self {
            Self::History(refusal) => refusal.diagnostic(),
            Self::Busy => "another rustain process holds this workspace's delegation authority",
            Self::StoreUnavailable => "the authority journal or trust store could not be read",
        };
        format!(
            "Delegation authority is unavailable for this workspace ({reason}); delegated \
             work is disabled for this session and no budget is granted."
        )
    }
}

/// The result of [`admit_workspace_authority`].
pub struct AuthorityAdmission {
    pub ledger: Arc<AuthorityLedger>,
    /// The root token the ledger holds: spendable only when admitted.
    pub root: CapabilityToken,
    pub status: AdmissionStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionStatus {
    /// A new authority was established for a workspace with no history.
    FirstInitialization,
    /// A pre-existing authority was admitted from verified, fresh history.
    VerifiedRestart,
    /// Not admitted. The ledger holds an unspendable root.
    Refused(AdmissionRefusal),
}

/// A root that can delegate nothing: zero budget, zero uses, no depth.
fn unspendable_root() -> CapabilityToken {
    CapabilityToken::root(
        AgentId::root(),
        CapabilitySet::from_flags(&[]),
        Budget::ZERO,
        0,
        None,
        Some(0),
    )
}

impl AuthorityAdmission {
    /// A refused admission: the ledger holds an unspendable root, with no
    /// journal sink and no trust, so it can neither grant budget nor write.
    #[must_use]
    pub fn refused(clock: Arc<dyn Clock>, why: AdmissionRefusal) -> Self {
        let root = unspendable_root();
        Self {
            ledger: Arc::new(AuthorityLedger::new(root.clone(), clock)),
            root,
            status: AdmissionStatus::Refused(why),
        }
    }
}

fn refused(clock: Arc<dyn Clock>, why: AdmissionRefusal) -> AuthorityAdmission {
    AuthorityAdmission::refused(clock, why)
}

/// Startup's authority front door: open the workspace trust, read the workspace
/// journal's conservation heads, and either admit the authority (first
/// initialization or a verified restart) or refuse it. Refusal never yields a
/// spendable root, a more permissive head, or trust in unverified time.
pub async fn admit_workspace_authority(
    workspace: &Path,
    data_dir: &Path,
    journal: Arc<NodeJournal>,
    clock: Arc<dyn Clock>,
) -> AuthorityAdmission {
    let records: Vec<LedgerConservationRecord> = match journal.load().await {
        Ok(entries) => entries
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::LedgerConservation(record) => Some(record),
                _ => None,
            })
            .collect(),
        Err(_) => return refused(clock, AdmissionRefusal::StoreUnavailable),
    };
    let (data_dir, workspace) = (data_dir.to_path_buf(), workspace.to_path_buf());
    let Ok(opening) = tokio::task::spawn_blocking(move || open(&data_dir, &workspace)).await else {
        return refused(clock, AdmissionRefusal::StoreUnavailable);
    };

    let trust = match opening {
        TrustOpening::Busy => return refused(clock, AdmissionRefusal::Busy),
        TrustOpening::Unverifiable => {
            return refused(
                clock,
                AdmissionRefusal::History(RecoveryRefusal::UnverifiableAnchor),
            );
        }
        TrustOpening::Unestablished(_) if !records.is_empty() => {
            return refused(
                clock,
                AdmissionRefusal::History(RecoveryRefusal::UnanchoredHistory),
            );
        }
        TrustOpening::Unestablished(pending) => {
            match tokio::task::spawn_blocking(move || pending.establish()).await {
                Ok(Ok(trust)) => (Arc::new(trust), AdmissionStatus::FirstInitialization),
                _ => return refused(clock, AdmissionRefusal::StoreUnavailable),
            }
        }
        TrustOpening::Established(trust) => (Arc::new(trust), AdmissionStatus::VerifiedRestart),
    };
    let (trust, status) = trust;

    let root = CapabilityToken::r1_root(AgentId::root());
    let ledger = AuthorityLedger::new(root.clone(), clock.clone())
        .with_journal_sink(journal as Arc<dyn crate::domain::ports::LedgerJournalSink>)
        .with_authority_trust(trust.clone());
    if status == AdmissionStatus::VerifiedRestart
        && let Err(refusal) = ledger.recover_verified(records, trust.as_ref())
    {
        return refused(clock, AdmissionRefusal::History(refusal));
    }
    AuthorityAdmission {
        ledger: Arc::new(ledger),
        root,
        status,
    }
}
