//! Session write scratch (D-24 B): the guard every write temporary lives under, and the
//! startup sweep that reclaims only provably abandoned scratch.
//!
//! # What a sweep may remove
//!
//! Only an exact, regular file with one of these names, found in the configured sessions root
//! or in an immediate, non-symlink session directory (`<root>/<id>/`):
//!
//! * `.rustain-<pid>-<seq>.tmp`: the tagged temporary of [`FileSystemStorage::atomic_write`]
//!   (root and session directory alike);
//! * legacy `<id>.meta.json.tmp`, `<id>.session.json.tmp` (root) and `conversation.json.tmp`,
//!   `meta.json.tmp`, `checkpoints.json.tmp`, `rewind_txn.json.tmp` (session directory), fixed
//!   names that only a pre-D-24 binary still emits.
//!
//! Anything else (the authority directory, `snapshots/`, images, a malformed name, a symlink,
//! a directory) is left alone.
//!
//! # The proof for each name class
//!
//! **Tagged names.** The pid in the name is only a label, never evidence. Every creator holds a
//! shared `flock` on the root's per-label guard `<root>/.rustain-authority/.scratch-<pid>.guard`
//! for its whole span: from before it creates the temporary (exclusively, never reusing an
//! existing name) until it has renamed or removed it. The sweep takes that guard *exclusively and
//! without waiting* before it touches any name carrying the label. Holding it proves no creator
//! of that label is between create and rename/cleanup, so a name still present belongs to a
//! writer that died (a held lock does not survive its process). A live writer, a reused pid whose
//! owner is mid-write, another pid namespace, or an unreadable guard all leave the guard busy or
//! unavailable, and the candidate is retained. No PID probe is involved.
//!
//! **Legacy fixed names.** They carry the session id, so the sweep takes that session's D-24 A
//! authority without waiting (creating its lock file if the session never had one). While it is
//! held no upgraded writer can create, write or rename anything of that session. A busy
//! authority retains the candidate. An old binary takes neither lock: the owner-ruled rollout
//! prerequisite (old writers quiesced and not relaunched before a root is opened to upgraded
//! writers) is what proves no old writer uses the name; the code cannot prove it.
//!
//! The directory and the file identity are pinned through `unlink`: the directory is opened with
//! `O_DIRECTORY | O_NOFOLLOW` relative to the root descriptor, the candidate is `fstatat`ed
//! without following links, must be regular and unchanged since the scan, and is removed with
//! `unlinkat` on that descriptor.

use std::path::{Path, PathBuf};

use crate::domain::errors::StorageError;

/// Why a candidate was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retain {
    /// The label's scratch guard is held: its creator may still be writing.
    LiveWriter,
    /// The session's authority is held: an upgraded writer owns the session right now.
    SessionInUse,
    /// The name no longer resolves to the file the scan saw.
    Replaced,
    /// The name matches but the entry is not a regular file (symlink, directory, ...).
    NotRegular,
    /// The proof could not be obtained (I/O error opening the guard or the authority).
    Unprovable,
}

/// What one sweep did.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Exact paths unlinked.
    pub removed: Vec<PathBuf>,
    /// Candidates kept, with the reason.
    pub retained: Vec<(PathBuf, Retain)>,
}

#[cfg(unix)]
pub(super) use imp::{Guard, hold_creator};
#[cfg(not(unix))]
pub(super) use stub::{Guard, hold_creator};

/// Reclaim abandoned scratch under `root`. A missing root has nothing to reclaim and is not
/// created.
#[cfg(unix)]
pub(super) async fn sweep(root: &Path) -> Result<SweepReport, StorageError> {
    imp::sweep(root).await
}

/// Reclaim abandoned scratch under `root` (not supported off Unix: nothing is removed).
#[cfg(not(unix))]
pub(super) async fn sweep(_root: &Path) -> Result<SweepReport, StorageError> {
    Ok(SweepReport::default())
}

#[cfg(not(unix))]
mod stub {
    use std::path::Path;

    pub(crate) struct Guard;

    pub(crate) fn hold_creator(_root: &Path) -> std::io::Result<Guard> {
        Ok(Guard)
    }
}

/// Take a label's guard exclusively, as the sweeper does (tests only).
#[cfg(all(test, unix))]
pub(super) fn hold_exclusive_for_test(
    root: &Path,
    pid: u32,
) -> std::io::Result<Option<imp::Guard>> {
    imp::hold(root, pid, imp::Hold::TryExclusive)
}

/// Test-only pause point between the temporary's write and its rename, keyed by the canonical
/// sessions root so parallel tests never share one.
#[cfg(test)]
pub(super) mod hook {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock};

    type Hook = Arc<dyn Fn(&Path) + Send + Sync>;
    static ARMED: LazyLock<Mutex<HashMap<PathBuf, Hook>>> = LazyLock::new(Default::default);

    /// Run `hook(tmp)` for every temporary written under `root` until [`disarm`].
    pub(crate) fn arm(root: &Path, hook: impl Fn(&Path) + Send + Sync + 'static) {
        let root = std::fs::canonicalize(root).expect("root exists before arming");
        ARMED.lock().insert(root, Arc::new(hook));
    }

    pub(crate) fn disarm(root: &Path) {
        if let Ok(root) = std::fs::canonicalize(root) {
            ARMED.lock().remove(&root);
        }
    }

    /// Called by the writer with its temporary written and not yet renamed.
    pub(crate) fn fire(root: &Path, tmp: &Path) {
        let Ok(root) = std::fs::canonicalize(root) else {
            return;
        };
        let hook = ARMED.lock().get(&root).cloned();
        if let Some(hook) = hook {
            hook(tmp);
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use super::{Retain, SweepReport};
    use crate::adapters::filesystem::session_authority::{self, AUTHORITY_DIR, Opened};
    use crate::domain::errors::StorageError;

    /// Bounded retries when a guard file is replaced between open and lock.
    const GUARD_RETRIES: usize = 16;

    /// A held scratch guard (the `flock` ends when this drops).
    pub(crate) struct Guard {
        _file: File,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Hold {
        /// A creator: waits for a sweeper that has the label.
        Shared,
        /// A sweeper: gives up at once if any creator of the label is in its span.
        TryExclusive,
    }

    fn guard_path(root: &Path, pid: u32) -> PathBuf {
        root.join(AUTHORITY_DIR)
            .join(format!(".scratch-{pid}.guard"))
    }

    /// Lock the guard of `pid`'s label under `root`. `Ok(None)` only for a refused
    /// [`Hold::TryExclusive`]. The locked inode is re-checked against the name afterwards, so
    /// a guard removed and recreated meanwhile is never trusted.
    pub(super) fn hold(root: &Path, pid: u32, how: Hold) -> io::Result<Option<Guard>> {
        let path = guard_path(root, pid);
        for _ in 0..GUARD_RETRIES {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let op = match how {
                Hold::Shared => libc::LOCK_SH,
                Hold::TryExclusive => libc::LOCK_EX | libc::LOCK_NB,
            };
            loop {
                // SAFETY: the descriptor is valid and owned by `file` for the whole call.
                if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EWOULDBLOCK) if how == Hold::TryExclusive => return Ok(None),
                    _ => return Err(error),
                }
            }
            let held = file.metadata()?;
            match std::fs::symlink_metadata(&path) {
                Ok(named) if named.dev() == held.dev() && named.ino() == held.ino() => {
                    return Ok(Some(Guard { _file: file }));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("scratch guard kept being replaced"))
    }

    /// The shared guard of this process's label for one create..rename/cleanup span.
    pub(crate) fn hold_creator(root: &Path) -> io::Result<Guard> {
        hold(root, std::process::id(), Hold::Shared)?
            .ok_or_else(|| io::Error::other("shared scratch guard refused"))
    }

    // ── names ────────────────────────────────────────────────────────────────

    /// Canonical decimal digits only (no sign, no leading zero, no whitespace).
    fn canonical_decimal(text: &str) -> bool {
        !text.is_empty()
            && text.bytes().all(|b| b.is_ascii_digit())
            && (text == "0" || !text.starts_with('0'))
    }

    /// `.rustain-<pid>-<seq>.tmp` with a valid positive pid and a decimal sequence.
    pub(super) fn parse_tagged(name: &str) -> Option<u32> {
        let body = name.strip_prefix(".rustain-")?.strip_suffix(".tmp")?;
        let (pid, seq) = body.split_once('-')?;
        if !canonical_decimal(pid) || !canonical_decimal(seq) {
            return None;
        }
        seq.parse::<u64>().ok()?;
        let pid = pid.parse::<u32>().ok()?;
        (1..=i32::MAX as u32).contains(&pid).then_some(pid)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Kind {
        Tagged(u32),
        /// A fixed legacy name owned by this session id.
        Legacy(String),
    }

    fn valid_id(id: &str) -> bool {
        crate::infrastructure::utils::sanitize_id(id).is_ok()
    }

    /// A name found directly in the sessions root.
    pub(super) fn classify_root(name: &str) -> Option<Kind> {
        if let Some(pid) = parse_tagged(name) {
            return Some(Kind::Tagged(pid));
        }
        let id = name
            .strip_suffix(".meta.json.tmp")
            .or_else(|| name.strip_suffix(".session.json.tmp"))?;
        valid_id(id).then(|| Kind::Legacy(id.to_string()))
    }

    /// A name found in the session directory of `id`.
    pub(super) fn classify_in_dir(id: &str, name: &str) -> Option<Kind> {
        if let Some(pid) = parse_tagged(name) {
            return Some(Kind::Tagged(pid));
        }
        matches!(
            name,
            "conversation.json.tmp"
                | "meta.json.tmp"
                | "checkpoints.json.tmp"
                | "rewind_txn.json.tmp"
        )
        .then(|| Kind::Legacy(id.to_string()))
    }

    // ── descriptors ──────────────────────────────────────────────────────────

    fn cstring(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// Open a directory. `parent` `None` resolves `path` from the process (the configured root
    /// may itself be a symlink alias); otherwise `path` is one component below `parent` and a
    /// symlink is refused.
    fn open_dir(parent: Option<&OwnedFd>, path: &CStr) -> io::Result<OwnedFd> {
        let (dirfd, flags) = match parent {
            None => (libc::AT_FDCWD, libc::O_RDONLY | libc::O_DIRECTORY),
            Some(fd) => (
                fd.as_raw_fd(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            ),
        };
        // SAFETY: `path` is a valid NUL-terminated string and `dirfd` is valid or AT_FDCWD.
        let fd = unsafe { libc::openat(dirfd, path.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by `openat` and is owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn stat_at(dir: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
        // SAFETY: a zeroed `stat` is a valid out-parameter; `name` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn file_type(st: &libc::stat) -> libc::mode_t {
        st.st_mode & libc::S_IFMT
    }

    // `dev_t` and `ino_t` widths differ per platform (macOS: 32-bit `dev_t`); the casts are
    // no-ops on Linux x86_64 only.
    #[allow(clippy::unnecessary_cast)]
    fn identity(st: &libc::stat) -> (u64, u64) {
        (st.st_dev as u64, st.st_ino as u64)
    }

    /// Entry names of an open directory (without `.` and `..`).
    fn list(dir: &OwnedFd) -> io::Result<Vec<Vec<u8>>> {
        // SAFETY: `fdopendir` takes ownership of the duplicate, which `closedir` releases.
        let dup = unsafe { libc::dup(dir.as_raw_fd()) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(dup) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(dup) };
            return Err(error);
        }
        let mut names = Vec::new();
        loop {
            // SAFETY: `stream` is a live DIR; the entry is read before the next call.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(name.to_vec());
            }
        }
        unsafe { libc::closedir(stream) };
        Ok(names)
    }

    // ── scan ─────────────────────────────────────────────────────────────────

    /// One exact scratch file the scan saw.
    #[derive(Debug, Clone)]
    pub(super) struct Candidate {
        /// `None`: directly in the root; `Some(id)`: in `<root>/<id>/`.
        dir: Option<String>,
        name: String,
        ident: (u64, u64),
    }

    impl Candidate {
        fn path(&self, root: &Path) -> PathBuf {
            match &self.dir {
                None => root.join(&self.name),
                Some(dir) => root.join(dir).join(&self.name),
            }
        }
    }

    #[derive(Default)]
    pub(super) struct Plan {
        pub(super) tagged: BTreeMap<u32, Vec<Candidate>>,
        pub(super) legacy: BTreeMap<String, Vec<Candidate>>,
    }

    fn consider(
        plan: &mut Plan,
        report: &mut SweepReport,
        root: &Path,
        dir: Option<&str>,
        dirfd: &OwnedFd,
        name: &str,
        kind: Option<Kind>,
    ) {
        let Some(kind) = kind else { return };
        let candidate_path = match dir {
            None => root.join(name),
            Some(dir) => root.join(dir).join(name),
        };
        let Ok(cname) = cstring(name.as_bytes()) else {
            return;
        };
        let Ok(st) = stat_at(dirfd, &cname) else {
            return;
        };
        if file_type(&st) != libc::S_IFREG {
            report.retained.push((candidate_path, Retain::NotRegular));
            return;
        }
        let candidate = Candidate {
            dir: dir.map(str::to_string),
            name: name.to_string(),
            ident: identity(&st),
        };
        match kind {
            Kind::Tagged(pid) => plan.tagged.entry(pid).or_default().push(candidate),
            Kind::Legacy(id) => plan.legacy.entry(id).or_default().push(candidate),
        }
    }

    /// Enumerate the exact candidates in the root and its immediate session directories.
    pub(super) fn scan(root: &Path) -> io::Result<Option<(Plan, SweepReport)>> {
        let rootc = cstring(root.as_os_str().as_bytes())?;
        let root_fd = match open_dir(None, &rootc) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut plan = Plan::default();
        let mut report = SweepReport::default();
        for raw in list(&root_fd)? {
            let Ok(name) = std::str::from_utf8(&raw) else {
                continue;
            };
            if name == AUTHORITY_DIR {
                continue;
            }
            let Ok(cname) = cstring(&raw) else { continue };
            let Ok(st) = stat_at(&root_fd, &cname) else {
                continue;
            };
            match file_type(&st) {
                libc::S_IFDIR if valid_id(name) => {
                    // A symlink or a directory swapped in meanwhile fails the NOFOLLOW open.
                    let Ok(id_fd) = open_dir(Some(&root_fd), &cname) else {
                        continue;
                    };
                    let Ok(entries) = list(&id_fd) else { continue };
                    for entry in entries {
                        let Ok(inner) = std::str::from_utf8(&entry) else {
                            continue;
                        };
                        let kind = classify_in_dir(name, inner);
                        consider(
                            &mut plan,
                            &mut report,
                            root,
                            Some(name),
                            &id_fd,
                            inner,
                            kind,
                        );
                    }
                }
                // A directory whose name is not a session id: only a scratch-looking name is
                // worth a diagnostic (it is never entered).
                _ => consider(
                    &mut plan,
                    &mut report,
                    root,
                    None,
                    &root_fd,
                    name,
                    classify_root(name),
                ),
            }
        }
        Ok(Some((plan, report)))
    }

    // ── removal ──────────────────────────────────────────────────────────────

    enum Outcome {
        Removed,
        Gone,
        Retained(Retain),
    }

    /// Unlink one candidate through descriptors pinned under `root_fd`, after re-checking that
    /// it is still the same regular file the scan saw.
    fn remove_pinned(root_fd: &OwnedFd, candidate: &Candidate) -> Outcome {
        let owned;
        let dir: &OwnedFd = match &candidate.dir {
            None => root_fd,
            Some(id) => {
                let Ok(idc) = cstring(id.as_bytes()) else {
                    return Outcome::Retained(Retain::Replaced);
                };
                match open_dir(Some(root_fd), &idc) {
                    Ok(fd) => {
                        owned = fd;
                        &owned
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Outcome::Gone,
                    Err(_) => return Outcome::Retained(Retain::Replaced),
                }
            }
        };
        let Ok(name) = cstring(candidate.name.as_bytes()) else {
            return Outcome::Retained(Retain::Replaced);
        };
        match stat_at(dir, &name) {
            Ok(st) if file_type(&st) != libc::S_IFREG => {
                return Outcome::Retained(Retain::NotRegular);
            }
            Ok(st) if identity(&st) != candidate.ident => {
                return Outcome::Retained(Retain::Replaced);
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Outcome::Gone,
            Err(_) => return Outcome::Retained(Retain::Unprovable),
        }
        // SAFETY: `dir` is a valid directory descriptor and `name` a NUL-terminated component.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            Outcome::Removed
        } else if io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            Outcome::Gone
        } else {
            Outcome::Retained(Retain::Unprovable)
        }
    }

    pub(super) fn remove_all(root: &Path, items: &[Candidate], report: &mut SweepReport) {
        let Ok(rootc) = cstring(root.as_os_str().as_bytes()) else {
            return;
        };
        let Ok(root_fd) = open_dir(None, &rootc) else {
            for item in items {
                report.retained.push((item.path(root), Retain::Unprovable));
            }
            return;
        };
        for item in items {
            match remove_pinned(&root_fd, item) {
                Outcome::Removed => report.removed.push(item.path(root)),
                Outcome::Gone => {}
                Outcome::Retained(why) => report.retained.push((item.path(root), why)),
            }
        }
    }

    /// Tagged names: reclaim a label's names only under its exclusive guard.
    fn sweep_tagged(root: &Path, groups: BTreeMap<u32, Vec<Candidate>>, report: &mut SweepReport) {
        for (pid, items) in groups {
            match hold(root, pid, Hold::TryExclusive) {
                Ok(Some(_guard)) => remove_all(root, &items, report),
                Ok(None) => {
                    for item in &items {
                        report.retained.push((item.path(root), Retain::LiveWriter));
                    }
                }
                Err(e) => {
                    tracing::warn!(pid, "scratch sweep: guard unavailable: {e}");
                    for item in &items {
                        report.retained.push((item.path(root), Retain::Unprovable));
                    }
                }
            }
        }
    }

    /// `.scratch-<pid>.guard` -> `pid`.
    fn guard_pid(name: &str) -> Option<u32> {
        let pid = name.strip_prefix(".scratch-")?.strip_suffix(".guard")?;
        canonical_decimal(pid).then(|| pid.parse().ok()).flatten()
    }

    /// Drop the guard files of labels nobody holds. A guard is created for every writing process
    /// and would otherwise pile up one empty file per process that ever wrote. Taking the label
    /// exclusively first means no creator is in its span; a creator that had already opened the
    /// file waits for the lock, then finds the name gone and recreates it (`hold` re-checks the
    /// locked inode against the name). This process's own label stays.
    fn reap_guards(root: &Path) {
        let own = std::process::id();
        let Ok(entries) = std::fs::read_dir(root.join(AUTHORITY_DIR)) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(guard_pid) else {
                continue;
            };
            if pid != own
                && let Ok(Some(_held)) = hold(root, pid, Hold::TryExclusive)
            {
                let _ = std::fs::remove_file(guard_path(root, pid));
            }
        }
    }

    pub(super) async fn sweep(root: &Path) -> Result<SweepReport, StorageError> {
        let io_err = |what: &str, e: &dyn std::fmt::Display| {
            StorageError::IoError(format!("scratch sweep {what}: {e}"))
        };
        let scan_root = root.to_path_buf();
        let scanned = tokio::task::spawn_blocking(move || scan(&scan_root))
            .await
            .map_err(|e| io_err("scan task", &e))?
            .map_err(|e| io_err("scan", &e))?;
        let Some((plan, mut report)) = scanned else {
            return Ok(SweepReport::default());
        };

        let tagged_root = root.to_path_buf();
        let tagged = plan.tagged;
        let mut report = tokio::task::spawn_blocking(move || {
            sweep_tagged(&tagged_root, tagged, &mut report);
            reap_guards(&tagged_root);
            report
        })
        .await
        .map_err(|e| io_err("tagged task", &e))?;

        for (id, items) in plan.legacy {
            match session_authority::try_acquire(root, &id).await {
                Ok(Opened::Held(held)) => {
                    let remove_root = root.to_path_buf();
                    let partial = tokio::task::spawn_blocking(move || {
                        let mut partial = SweepReport::default();
                        remove_all(&remove_root, &items, &mut partial);
                        partial
                    })
                    .await
                    .map_err(|e| io_err("legacy task", &e))?;
                    drop(held);
                    report.removed.extend(partial.removed);
                    report.retained.extend(partial.retained);
                }
                Ok(Opened::Busy) => {
                    for item in &items {
                        report
                            .retained
                            .push((item.path(root), Retain::SessionInUse));
                    }
                }
                Ok(Opened::Absent) | Err(_) => {
                    for item in &items {
                        report.retained.push((item.path(root), Retain::Unprovable));
                    }
                }
            }
        }
        Ok(report)
    }
}

#[cfg(all(test, unix))]
mod name_tests {
    use super::imp::{Kind, classify_in_dir, classify_root, parse_tagged};

    #[test]
    fn only_canonical_tagged_names_parse() {
        assert_eq!(parse_tagged(".rustain-4242-0.tmp"), Some(4242));
        assert_eq!(parse_tagged(".rustain-1-18446744073709551615.tmp"), Some(1));
        for bad in [
            ".rustain-0-1.tmp",
            ".rustain-007-1.tmp",
            ".rustain-5-01.tmp",
            ".rustain--5-1.tmp",
            ".rustain-+5-1.tmp",
            ".rustain-5-.tmp",
            ".rustain--1.tmp",
            ".rustain-5-1-2.tmp",
            ".rustain-5-1.tmp.bak",
            "rustain-5-1.tmp",
            ".rustain-2147483648-1.tmp",
            ".rustain-5-18446744073709551616.tmp",
            ".rustain-x-1.tmp",
            ".rustain-5-1.TMP",
        ] {
            assert_eq!(parse_tagged(bad), None, "{bad}");
        }
    }

    #[test]
    fn legacy_names_are_exact_and_carry_a_valid_id() {
        assert_eq!(
            classify_root("abc-1_x.meta.json.tmp"),
            Some(Kind::Legacy("abc-1_x".into()))
        );
        assert_eq!(
            classify_root("abc.session.json.tmp"),
            Some(Kind::Legacy("abc".into()))
        );
        for bad in [
            "abc.meta.json",
            "abc.json.tmp",
            ".meta.json.tmp",
            "a.b.meta.json.tmp",
            "a b.session.json.tmp",
            "conversation.json.tmp",
            "notes.tmp",
        ] {
            assert_eq!(classify_root(bad), None, "{bad}");
        }
        for fixed in [
            "conversation.json.tmp",
            "meta.json.tmp",
            "checkpoints.json.tmp",
            "rewind_txn.json.tmp",
        ] {
            assert_eq!(
                classify_in_dir("sid", fixed),
                Some(Kind::Legacy("sid".into()))
            );
        }
        assert_eq!(classify_in_dir("sid", "snapshot.tmp"), None);
        assert_eq!(classify_in_dir("sid", "abc.meta.json.tmp"), None);
        assert_eq!(
            classify_in_dir("sid", ".rustain-9-3.tmp"),
            Some(Kind::Tagged(9))
        );
    }
}

/// The pinned unlink re-checks identity and type at the moment of removal.
#[cfg(all(test, unix))]
mod pinned_removal_tests {
    use super::imp::{remove_all, scan};
    use super::{Retain, SweepReport};

    fn candidates(root: &std::path::Path) -> Vec<super::imp::Candidate> {
        let (plan, report) = scan(root).unwrap().unwrap();
        assert!(report.retained.is_empty());
        plan.tagged.into_values().flatten().collect()
    }

    #[test]
    fn a_file_replaced_after_the_scan_is_kept() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let orphan = root.join(".rustain-424242-1.tmp");
        std::fs::write(&orphan, b"orphan").unwrap();
        let items = candidates(root);
        assert_eq!(items.len(), 1);
        // A different file takes the name while the scanned one still exists.
        let other = root.join("other");
        std::fs::write(&other, b"someone else's bytes").unwrap();
        std::fs::rename(&other, &orphan).unwrap();
        let mut report = SweepReport::default();
        remove_all(root, &items, &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(report.retained, vec![(orphan.clone(), Retain::Replaced)]);
        assert_eq!(std::fs::read(&orphan).unwrap(), b"someone else's bytes");
    }

    #[test]
    fn a_symlink_swapped_in_after_the_scan_is_never_followed_or_unlinked() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();
        let orphan = root.join(".rustain-424242-1.tmp");
        std::fs::write(&orphan, b"orphan").unwrap();
        let items = candidates(&root);
        std::fs::remove_file(&orphan).unwrap();
        std::os::unix::fs::symlink(&outside, &orphan).unwrap();
        let mut report = SweepReport::default();
        remove_all(&root, &items, &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(report.retained, vec![(orphan.clone(), Retain::NotRegular)]);
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        assert!(
            std::fs::symlink_metadata(&orphan)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_session_directory_swapped_for_a_symlink_is_not_entered() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(root.join("s1")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(".rustain-424242-1.tmp"), b"theirs").unwrap();
        std::fs::write(root.join("s1").join(".rustain-424242-1.tmp"), b"orphan").unwrap();
        let items = candidates(&root);
        assert_eq!(items.len(), 1);
        std::fs::remove_dir_all(root.join("s1")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("s1")).unwrap();
        let mut report = SweepReport::default();
        remove_all(&root, &items, &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(
            std::fs::read(elsewhere.join(".rustain-424242-1.tmp")).unwrap(),
            b"theirs"
        );
    }
}
