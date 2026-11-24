//! D-24 B keystone K5 at the real filesystem port: the startup scratch sweep removes exactly the
//! orphans a dead writer left, and never a live writer's temporary file. Writers are real
//! `FileSystemStorage` writes parked at their scratch file by the writer's own pause point
//! (`scratch::hook`), in this process or in a child process that is stopped and reaped.
//! Everything runs over a `TempDir`; nothing is simulated by pid or timestamp.

use super::scratch::{self, Retain, hook};
use super::session_authority::{self as authority, AUTHORITY_DIR, Opened};
use super::*;
use crate::domain::models::{ChatMessage, MessageRole};
use crate::domain::ports::StoragePort;
use std::io::{BufRead, Write};
use std::os::unix::fs::MetadataExt;
use tempfile::TempDir;

/// A pid whose guard nobody holds: tagged names carrying it are provable orphans.
const DEAD_PID: u32 = 424_242;

fn conv(id: &str) -> Conversation {
    let mut c = Conversation {
        id: id.into(),
        title: "t".into(),
        created_at: 1,
        updated_at: 1,
        ..Default::default()
    };
    c.messages.push(ChatMessage {
        id: "u1".into(),
        role: MessageRole::User,
        content: "hello".into(),
        ..Default::default()
    });
    c
}

fn sessions(dir: &TempDir) -> PathBuf {
    dir.path().join("sessions")
}

fn store(dir: &TempDir) -> FileSystemStorage {
    FileSystemStorage::with_workspace_root(sessions(dir), dir.path().to_path_buf())
}

/// Every file under `root` with its bytes (the "nothing else changed" oracle).
fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                walk(base, &p, out);
            } else if meta.file_type().is_symlink() {
                out.push((rel, b"<symlink>".to_vec()));
            } else {
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn tagged(pid: u32, seq: u64) -> String {
    format!(".rustain-{pid}-{seq}.tmp")
}

// ── two real processes ──────────────────────────────────────────────────────────

/// Child half: inert unless a parent launched it. It performs a real write and parks it between
/// the scratch file's write and its rename until the parent says go.
#[test]
fn child_role() {
    let Some(role) = crate::infrastructure::utils::env_var_trimmed("D24B_ROLE") else {
        return;
    };
    let root =
        PathBuf::from(crate::infrastructure::utils::env_var_trimmed("D24B_SESSIONS").unwrap());
    let rel = crate::infrastructure::utils::env_var_trimmed("D24B_REL").unwrap_or_default();
    let say = |line: String| {
        println!("D24B:{line}");
        std::io::stdout().flush().unwrap();
    };
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let authority_dir = root.join(AUTHORITY_DIR);
    let announce = say;
    hook::arm(&root, move |tmp| {
        // Park only the first transcript-side scratch file, not the authority record's.
        if tmp.starts_with(&authority_dir) || !first.swap(false, Ordering::SeqCst) {
            return;
        }
        announce(format!("PARKED {}", tmp.display()));
        let mut gate = String::new();
        std::io::stdin().lock().read_line(&mut gate).unwrap();
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        match role.as_str() {
            // A real upgraded session save (flat layout, root scratch).
            "save" => {
                let s = FileSystemStorage::new(root.clone());
                s.save_conversation(&conv(&rel)).await.unwrap();
            }
            // The real atomic write at an exact target (session directory scratch).
            "direct" => {
                let target = root.join(&rel);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                FileSystemStorage::atomic_write(&root, target, b"{\"from\":\"child\"}".to_vec())
                    .await
                    .unwrap();
            }
            other => panic!("unknown role {other}"),
        }
    });
    println!("D24B:DONE");
}

struct Child {
    process: std::process::Child,
    lines: std::io::BufReader<std::process::ChildStdout>,
}

impl Child {
    fn start(role: &str, sessions: &Path, rel: &str) -> Self {
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "adapters::filesystem::scratch_tests::child_role",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("D24B_ROLE", role)
            .env("D24B_SESSIONS", sessions)
            .env("D24B_REL", rel)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let lines = std::io::BufReader::new(process.stdout.take().unwrap());
        Self { process, lines }
    }

    /// The next `D24B:` line the child printed (libtest chatter is skipped).
    fn say(&mut self) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "the child exited before answering"
            );
            if let Some((_, rest)) = line.trim().split_once("D24B:") {
                return rest.to_string();
            }
        }
    }

    /// Start the child and wait, structurally, until its write is parked at the scratch file.
    fn parked(role: &str, sessions: &Path, rel: &str) -> (Self, PathBuf) {
        let mut child = Self::start(role, sessions, rel);
        let line = child.say();
        let tmp = PathBuf::from(line.strip_prefix("PARKED ").expect("child parks first"));
        assert!(tmp.is_file(), "the scratch file exists while parked");
        (child, tmp)
    }

    fn go(&mut self) {
        let stdin = self.process.stdin.as_mut().unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Stop the child mid-write and reap it: the kernel releases every lock it held.
    fn kill_and_reap(mut self) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
    }

    fn finish(mut self) {
        assert_eq!(self.say(), "DONE");
        drop(self.process.stdin.take());
        assert!(self.process.wait().unwrap().success());
    }
}

/// The scratch guard files under `root` other than this process's own, by name.
fn guards(root: &Path) -> Vec<String> {
    let own = format!(".scratch-{}.guard", std::process::id());
    let mut names: Vec<String> = std::fs::read_dir(root.join(AUTHORITY_DIR))
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".scratch-") && *name != own)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn ino(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).unwrap().ino()
}

fn retained(report: &scratch::SweepReport, path: &Path) -> Option<Retain> {
    report
        .retained
        .iter()
        .find(|(p, _)| p == path)
        .map(|(_, why)| *why)
}

/// A real writer stopped and reaped between scratch write and rename leaves an orphan the next
/// startup removes: in the root, in a committed session directory and in an incomplete one.
#[tokio::test]
async fn a_dead_writers_orphans_are_removed_from_the_root_and_every_session_directory() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    let s = store(&dir);
    s.save_conversation(&conv("kept")).await.unwrap();
    std::fs::create_dir_all(root.join("legit")).unwrap();
    std::fs::write(root.join("legit").join("conversation.json"), b"committed").unwrap();
    std::fs::write(root.join("legit").join("meta.json"), b"committed meta").unwrap();
    std::fs::create_dir_all(root.join("half")).unwrap();

    let (flat, in_root) = Child::parked("save", &root, "crashed");
    let (legit, in_legit) = Child::parked("direct", &root, "legit/meta.json");
    let (half, in_half) = Child::parked("direct", &root, "half/conversation.json");
    assert_eq!(in_root.parent().unwrap(), root);
    assert_eq!(in_legit.parent().unwrap(), root.join("legit"));
    assert_eq!(in_half.parent().unwrap(), root.join("half"));
    flat.kill_and_reap();
    legit.kill_and_reap();
    half.kill_and_reap();
    let committed_before: Vec<_> = tree(&root)
        .into_iter()
        .filter(|(rel, _)| !rel.contains(".tmp") && !rel.starts_with(AUTHORITY_DIR))
        .collect();

    let report = s.sweep_scratch().await.unwrap();

    let mut removed = report.removed.clone();
    removed.sort();
    let mut want = vec![in_root.clone(), in_legit.clone(), in_half.clone()];
    want.sort();
    assert_eq!(removed, want, "exactly the three orphans, nothing else");
    assert!(report.retained.is_empty(), "{:?}", report.retained);
    assert!(
        guards(&root).is_empty(),
        "the dead writers' guard files are reclaimed with their orphans: {:?}",
        guards(&root)
    );
    for orphan in [&in_root, &in_legit, &in_half] {
        assert!(!orphan.exists());
    }
    let committed_after: Vec<_> = tree(&root)
        .into_iter()
        .filter(|(rel, _)| !rel.contains(".tmp") && !rel.starts_with(AUTHORITY_DIR))
        .collect();
    assert_eq!(
        committed_after, committed_before,
        "accepted history is byte-identical"
    );
    // The store still works after a sweep, and a second sweep finds nothing.
    s.save_conversation(&conv("later")).await.unwrap();
    assert!(s.sweep_scratch().await.unwrap().removed.is_empty());
}

/// A writer that is alive and parked at its scratch file keeps it (same path, same file, same
/// bytes) through a concurrent startup sweep and then renames it; a different writer stopped at
/// the same boundary is reclaimed by that same sweep.
#[tokio::test]
async fn a_live_writers_scratch_survives_the_sweep_and_its_rename_still_succeeds() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    std::fs::create_dir_all(&root).unwrap();
    let s = store(&dir);

    let (mut live, live_tmp) = Child::parked("save", &root, "live-session");
    let (dead, dead_tmp) = Child::parked("direct", &root, "dead/conversation.json");
    dead.kill_and_reap();
    let bytes = std::fs::read(&live_tmp).unwrap();
    let identity = ino(&live_tmp);

    let report = s.sweep_scratch().await.unwrap();

    assert_eq!(report.removed, vec![dead_tmp.clone()], "only the dead one");
    assert_eq!(retained(&report, &live_tmp), Some(Retain::LiveWriter));
    assert_eq!(ino(&live_tmp), identity, "the very same file");
    assert_eq!(
        guards(&root),
        vec![format!(".scratch-{}.guard", live.process.id())],
        "the live writer's guard stays, the dead one's is gone"
    );
    assert_eq!(std::fs::read(&live_tmp).unwrap(), bytes);

    live.go();
    live.finish();
    assert!(!live_tmp.exists(), "renamed into place");
    let loaded = s.load_conversation("live-session").await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 1, "the parked save committed");
}

/// The sweeping process can be its own live writer's neighbour: a write of this very process
/// parked at its scratch file is kept, then completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn this_processs_own_parked_write_is_kept_and_completes() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    std::fs::create_dir_all(&root).unwrap();
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<PathBuf>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = parking_lot::Mutex::new(release_rx);
    let reached_tx = parking_lot::Mutex::new(reached_tx);
    hook::arm(&root, move |tmp| {
        reached_tx.lock().send(tmp.to_path_buf()).unwrap();
        release_rx.lock().recv().unwrap();
    });
    let target = root.join("own.session.json");
    let (write_root, write_target) = (root.clone(), target.clone());
    let write = tokio::spawn(async move {
        FileSystemStorage::atomic_write(&write_root, write_target, b"own bytes".to_vec()).await
    });
    let tmp = tokio::task::spawn_blocking(move || reached_rx.recv().unwrap())
        .await
        .unwrap();
    hook::disarm(&root);

    let report = store(&dir).sweep_scratch().await.unwrap();

    assert!(report.removed.is_empty());
    assert_eq!(retained(&report, &tmp), Some(Retain::LiveWriter));
    assert_eq!(std::fs::read(&tmp).unwrap(), b"own bytes");
    release_tx.send(()).unwrap();
    write.await.unwrap().unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"own bytes");
    assert!(!tmp.exists());
}

/// A writer never reuses or truncates an existing scratch name (a reused pid meets an orphan of
/// its predecessor): creation is exclusive and moves to the next name.
#[test]
fn a_writer_never_reuses_an_existing_scratch_name() {
    let dir = TempDir::new().unwrap();
    let orphan = dir.path().join(tagged(DEAD_PID, 0));
    std::fs::write(&orphan, b"predecessor's bytes").unwrap();
    let mut names = vec![orphan.clone(), dir.path().join(tagged(DEAD_PID, 1))].into_iter();
    let (tmp, mut file) =
        FileSystemStorage::create_scratch_named(|| names.next().unwrap()).unwrap();
    file.write_all(b"mine").unwrap();
    assert_eq!(tmp, dir.path().join(tagged(DEAD_PID, 1)));
    assert_eq!(std::fs::read(&orphan).unwrap(), b"predecessor's bytes");
}

/// Everything that is not an exact regular scratch file in a trusted location stays, byte for
/// byte: symlinks, directories, malformed names, other classes of file, nested trees, the
/// authority directory, a symlinked session directory and an invalid session directory name.
#[tokio::test]
async fn unknown_malformed_replaced_and_foreign_entries_are_retained() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    std::fs::create_dir_all(root.join("legit").join("images")).unwrap();
    std::fs::create_dir_all(root.join("legit").join("snapshots")).unwrap();
    std::fs::create_dir_all(root.join(AUTHORITY_DIR)).unwrap();
    std::fs::create_dir_all(root.join("not valid")).unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join(tagged(DEAD_PID, 5)), b"theirs").unwrap();

    std::os::unix::fs::symlink(&outside, root.join(tagged(DEAD_PID, 1))).unwrap();
    std::fs::create_dir_all(root.join(tagged(DEAD_PID, 2))).unwrap();
    std::fs::write(root.join(tagged(DEAD_PID, 2)).join("inside"), b"inside").unwrap();
    for malformed in [
        ".rustain-007-3.tmp",
        ".rustain-424242-4.tmp.bak",
        ".rustain-0-4.tmp",
        "rustain-424242-4.tmp",
        "notes.tmp",
        "a.b.meta.json.tmp",
        "a b.session.json.tmp",
    ] {
        std::fs::write(root.join(malformed), b"keep me").unwrap();
    }
    std::os::unix::fs::symlink(&elsewhere, root.join("linked")).unwrap();
    std::fs::write(
        root.join("not valid").join(tagged(DEAD_PID, 6)),
        b"invalid dir",
    )
    .unwrap();
    std::fs::write(
        root.join("legit").join("images").join(tagged(DEAD_PID, 7)),
        b"nested",
    )
    .unwrap();
    std::fs::write(
        root.join("legit").join("snapshots").join("5_abc.tmp"),
        b"snap",
    )
    .unwrap();
    std::fs::write(
        root.join("legit")
            .join("snapshots")
            .join(tagged(DEAD_PID, 8)),
        b"snap2",
    )
    .unwrap();
    std::fs::write(
        root.join(AUTHORITY_DIR).join(tagged(DEAD_PID, 9)),
        b"authority",
    )
    .unwrap();
    std::fs::write(root.join("legit").join("notes.tmp"), b"unrelated").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("legit").join("conversation.json.tmp")).unwrap();
    let before = tree(dir.path());

    let report = store(&dir).sweep_scratch().await.unwrap();

    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert_eq!(
        retained(&report, &root.join(tagged(DEAD_PID, 1))),
        Some(Retain::NotRegular)
    );
    assert_eq!(
        retained(&report, &root.join(tagged(DEAD_PID, 2))),
        Some(Retain::NotRegular)
    );
    assert_eq!(
        retained(&report, &root.join("legit").join("conversation.json.tmp")),
        Some(Retain::NotRegular)
    );
    assert_eq!(tree(dir.path()), before, "no byte, name or link changed");
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
}

/// Legacy fixed names (a pre-D-24 binary's) are reclaimed per session under that session's
/// authority: an idle session loses them; a session an upgraded writer holds right now keeps them.
#[tokio::test]
async fn legacy_fixed_names_follow_the_session_authority() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    let s = store(&dir);
    std::fs::create_dir_all(root.join("old2")).unwrap();
    std::fs::create_dir_all(root.join("old3")).unwrap(); // incomplete first migration
    std::fs::create_dir_all(root.join("busy2")).unwrap();
    std::fs::write(root.join("old1.meta.json"), b"{}").unwrap();
    std::fs::write(root.join("old2").join("conversation.json"), b"committed").unwrap();
    let idle = [
        root.join("old1.meta.json.tmp"),
        root.join("old1.session.json.tmp"),
        root.join("old2").join("conversation.json.tmp"),
        root.join("old2").join("meta.json.tmp"),
        root.join("old2").join("checkpoints.json.tmp"),
        root.join("old2").join("rewind_txn.json.tmp"),
        root.join("old3").join("meta.json.tmp"),
    ];
    let busy = [
        root.join("busy1.meta.json.tmp"),
        root.join("busy2").join("conversation.json.tmp"),
    ];
    for path in idle.iter().chain(&busy) {
        std::fs::write(path, b"legacy scratch").unwrap();
    }
    // Two upgraded writers' sessions: an upgraded writer holds both authorities.
    let held1 = authority::acquire(&root, "busy1").await.unwrap();
    let held2 = authority::acquire(&root, "busy2").await.unwrap();

    let report = s.sweep_scratch().await.unwrap();

    let mut removed = report.removed.clone();
    removed.sort();
    let mut want = idle.to_vec();
    want.sort();
    assert_eq!(removed, want);
    for path in &busy {
        assert!(path.exists(), "{}", path.display());
        assert_eq!(retained(&report, path), Some(Retain::SessionInUse));
    }
    assert_eq!(
        std::fs::read(root.join("old2").join("conversation.json")).unwrap(),
        b"committed"
    );
    // Once the writer lets go, the next startup reclaims them.
    drop((held1, held2));
    let second = s.sweep_scratch().await.unwrap();
    assert_eq!(second.removed.len(), busy.len());
    assert!(busy.iter().all(|p| !p.exists()));
}

/// Tagged names answer to the scratch guard alone: a session an upgraded writer holds does not
/// make a dead writer's tagged orphan live.
#[tokio::test]
async fn a_held_session_does_not_keep_a_dead_writers_tagged_orphan_alive() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    std::fs::create_dir_all(root.join("busy")).unwrap();
    let orphan = root.join("busy").join(tagged(DEAD_PID, 3));
    std::fs::write(&orphan, b"orphan").unwrap();
    let held = authority::acquire(&root, "busy").await.unwrap();
    let report = store(&dir).sweep_scratch().await.unwrap();
    assert_eq!(report.removed, vec![orphan]);
    drop(held);
}

/// Non-writing observations never sweep, and a missing root is neither created nor an error.
#[tokio::test]
async fn reads_and_previews_do_not_sweep_and_a_missing_root_stays_missing() {
    let dir = TempDir::new().unwrap();
    let root = sessions(&dir);
    let s = store(&dir);
    let report = s.sweep_scratch().await.unwrap();
    assert!(report.removed.is_empty() && report.retained.is_empty());
    assert!(!root.exists(), "a sweep never creates the root");

    s.save_conversation(&conv("one")).await.unwrap();
    std::fs::write(root.join(tagged(DEAD_PID, 1)), b"orphan").unwrap();
    std::fs::create_dir_all(root.join("one-dir")).unwrap();
    std::fs::write(root.join("one-dir").join(tagged(DEAD_PID, 2)), b"orphan").unwrap();
    let before = tree(&root);
    assert!(matches!(
        authority::try_acquire_existing(&root, "never")
            .await
            .unwrap(),
        Opened::Absent
    ));
    let _ = s.list_conversations_read_only().await.unwrap();
    let _ = s.load_conversation("one").await.unwrap();
    let _ = s.load_conversation("absent").await.unwrap();
    assert_eq!(tree(&root), before, "observation leaves every file alone");
    // The port entry sweeps and reports nothing back.
    StoragePort::sweep_session_scratch(&s).await;
    assert!(!root.join(tagged(DEAD_PID, 1)).exists());
    assert!(!root.join("one-dir").join(tagged(DEAD_PID, 2)).exists());
}

/// The sweeper takes a label's guard exclusively: while it holds it, a creator of that label
/// cannot enter its span (a shared lock is refused), and once it lets go the creator can.
#[test]
fn a_creator_cannot_enter_while_a_sweeper_holds_its_label() {
    use std::os::fd::AsRawFd;
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let pid = std::process::id();
    let guard_path = root
        .join(AUTHORITY_DIR)
        .join(format!(".scratch-{pid}.guard"));
    // A non-blocking shared attempt: `true` when a creator could enter now.
    let creator_could_enter = || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&guard_path)
            .unwrap();
        // SAFETY: valid descriptor owned by `file` for the call.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) == 0 }
    };
    let sweeper = scratch::hold_exclusive_for_test(&root, pid)
        .unwrap()
        .expect("nobody holds this label");
    assert!(!creator_could_enter());
    assert!(
        scratch::hold_exclusive_for_test(&root, pid)
            .unwrap()
            .is_none(),
        "a second sweeper of the label is refused too"
    );
    drop(sweeper);
    assert!(creator_could_enter());
    drop(scratch::hold_creator(&root).unwrap());
}
