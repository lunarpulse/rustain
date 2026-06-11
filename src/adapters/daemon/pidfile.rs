//! Daemon PID file (Story 12.1a Task 5) — atomic write, liveness probe, and the
//! already-running guard (AC-12-1a-9).
//!
//! The PID file is workspace-scoped (`{workspace}/.rustain/daemon.pid`, see
//! `infrastructure::paths::daemon_pid_path`) and records the socket + workspace
//! paths so `status`/`stop`/attach(12.2) read them rather than re-deriving
//! (AC-12-1a-8). It doubles as the readiness marker: the detached child writes it
//! as the last step before entering the lifecycle loop, and the parent `start`
//! polls for it (NFR47 ≤ 3s).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// On-disk daemon record. TOML-encoded (the `toml` crate is already a dep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonPidFile {
    /// OS process id of the running daemon.
    pub pid: u32,
    /// Resolved Unix socket path (`{data_dir}/daemons/<hash>.sock`).
    pub socket_path: PathBuf,
    /// Canonical workspace the daemon operates in.
    pub workspace: PathBuf,
    /// Unix seconds at daemon start — `status` derives uptime from this.
    pub started_at_unix: u64,
    /// Active profile name at start (shown by `status`).
    pub profile: String,
    /// Self-authored lineage nonce (Story 12.1b AC-12-1b-8, D-1). A random hex token
    /// written at daemon start so the ownership question becomes "did *I* write this
    /// PID file for *this* daemon lineage?" — zero-dependency PID-recycle hardening.
    /// `#[serde(default)]` keeps 12.1a PID files (no nonce) parseable; an empty nonce
    /// is treated as un-hardened and falls back to the platform liveness checks.
    #[serde(default)]
    pub nonce: String,
    /// Boot id at daemon start (Story 12.1b AC-12-1b-8). On Linux this is
    /// `/proc/sys/kernel/random/boot_id`, which the kernel regenerates every boot.
    /// A recorded boot id that differs from the current one means the PID file
    /// predates a reboot — the kernel has since reset the PID space, so the recorded
    /// PID cannot be our daemon. `None` on platforms without a boot id (skips the
    /// check). `#[serde(default)]` for 12.1a back-compat.
    #[serde(default)]
    pub boot_id: Option<String>,
}

impl DaemonPidFile {
    /// Atomically write the PID file: write a sibling temp file then rename, so a
    /// reader never observes a half-written file (Task 5).
    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        let body = toml::to_string(self).context("serializing daemon PID file")?;
        let tmp = path.with_extension("pid.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, body.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read + parse the PID file. Errors on missing/unparseable file.
    pub fn read(path: &Path) -> Result<Self> {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Liveness probe (Task 5). Returns true only when the process is a *live*
/// daemon — a **zombie** (terminated but not yet reaped) counts as dead.
///
/// This zombie distinction matters: `start` re-execs a detached child and then
/// exits, so the daemon is orphaned. When it later exits (via `stop`) it may sit
/// as a zombie until its reaper collects it — and a bare `kill(pid, 0)` returns
/// success for a zombie, which would make `stop` spin until its SIGKILL deadline
/// and `status` report a dead daemon as running. On Linux we read
/// `/proc/<pid>/stat` and treat state `Z` as dead; elsewhere we fall back to the
/// `kill(pid, 0)` probe (macOS is P1; its reaping is handled by the OS).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                // Format: `pid (comm) state ...`. `comm` may contain spaces and
                // parens, so the state char is the first token AFTER the last ')'.
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next());
                !matches!(state, Some("Z") | None)
            }
            // No /proc entry → the PID does not exist → dead.
            Err(_) => false,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // AC-12-1d-4: detect zombie on macOS via `ps` so `stop` doesn't spin to the
        // 5s SIGKILL deadline. A zombie reports alive to kill(pid,0), but `ps` stat
        // starts with 'Z'.
        if let Some(stat) = macos_process_stat(pid) {
            if stat.starts_with('Z') {
                return false;
            }
        }
        // Not a zombie (or ps couldn't resolve it). kill(0) is the final probe.
        // EPERM on another user's process means we can't verify ownership → dead.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Generate a fresh lineage nonce (Story 12.1b AC-12-1b-8). 16 random bytes from
/// the OS CSPRNG (`/dev/urandom`) rendered as hex — **zero new dependency** (no
/// `rand`/`getrandom`); `/dev/urandom` exists on every Unix we support. Falls back
/// to a blake3 digest of (pid, wall-clock nanos) only if `/dev/urandom` is somehow
/// unreadable, which is still unique enough for a lineage marker.
pub fn generate_nonce() -> String {
    let mut buf = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok()
    {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{}", std::process::id(), nanos);
    blake3::hash(seed.as_bytes()).to_hex()[..32].to_string()
}

/// Current boot id, or `None` where the platform doesn't expose one. On Linux the
/// kernel regenerates `/proc/sys/kernel/random/boot_id` on every boot, so it is a
/// reliable "is this the same boot?" signal (Story 12.1b AC-12-1b-8).
#[cfg(target_os = "linux")]
pub fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "macos")]
pub fn current_boot_id() -> Option<String> {
    // kern.boottime returns a timeval; we hash it into a stable string
    // that changes on every reboot — same semantics as Linux's boot_id.
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let rc = unsafe {
        libc::sysctl(
            mib.as_ptr() as *mut _,
            2,
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(format!("{}-{}", tv.tv_sec, tv.tv_usec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn current_boot_id() -> Option<String> {
    None
}

/// Environment variable the parent `start` injects into the detached child so the
/// live daemon process *carries* its own lineage nonce, observable via
/// `/proc/<pid>/environ` (Story 12.1c P1). This is what makes the nonce load-bearing
/// for ownership: without it the nonce would be write-only.
pub const DAEMON_NONCE_ENV: &str = "RUSTAIN_DAEMON_NONCE";

/// Query `ps -p <pid> -o stat=` for the process state. Returns `None` if the
/// process is gone or `ps` is unavailable.
#[cfg(target_os = "macos")]
fn macos_process_stat(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok().map(|s| s.trim().to_string())
}

/// Query `ps -p <pid> -o comm=` for the process command name. The basename is
/// returned to match Linux `/proc/<pid>/comm` semantics.
#[cfg(target_os = "macos")]
fn macos_process_comm(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let name = s.trim();
    std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str().map(String::from))
        .or_else(|| Some(name.to_string()))
}

/// Read the raw `KERN_PROCARGS2` buffer for `pid`. Returns `None` on sysctl failure.
#[cfg(target_os = "macos")]
fn macos_procargs2_raw(pid: u32) -> Option<Vec<u8>> {
    let mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut retries = 3;
    let mut len: usize = 0;
    loop {
        let rc = unsafe {
            libc::sysctl(
                mib.as_ptr() as *mut _,
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        let mut out_len = len;
        let rc = unsafe {
            libc::sysctl(
                mib.as_ptr() as *mut _,
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut out_len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buf.truncate(out_len);
            return Some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
        retries -= 1;
        if retries == 0 {
            return None;
        }
        len = 0;
    }
}

/// The per-process facts ownership needs, behind a trait so the predicate is
/// unit-testable without real `/proc` (Story 12.1c P1 — Murat/Amelia).
#[cfg(unix)]
trait ProcInspector {
    /// Current boot id, or `None` where unavailable.
    fn current_boot_id(&self) -> Option<String>;
    /// Can we introspect arbitrary processes at all? `false` on platforms without
    /// `/proc` (macOS) — the comm/argv fallback is then unavailable.
    fn introspectable(&self) -> bool;
    /// `RUSTAIN_DAEMON_NONCE` from `pid`'s environment, if present.
    fn env_nonce(&self, pid: u32) -> Option<String>;
    /// The process command name (`/proc/<pid>/comm`), trimmed.
    fn comm(&self, pid: u32) -> Option<String>;
    /// The process argv (`/proc/<pid>/cmdline`, NUL-split).
    fn cmdline_args(&self, pid: u32) -> Vec<String>;
    /// Expected daemon command name (basename of our own executable, e.g. `rustain`).
    fn expected_comm(&self) -> String;
}

/// Production inspector backed by `/proc` (Linux) / nothing (macOS).
#[cfg(unix)]
struct RealProc;

#[cfg(unix)]
impl ProcInspector for RealProc {
    fn current_boot_id(&self) -> Option<String> {
        current_boot_id()
    }
    fn introspectable(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "macos"))
    }

    #[cfg(target_os = "linux")]
    fn env_nonce(&self, pid: u32) -> Option<String> {
        let body = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let prefix = format!("{DAEMON_NONCE_ENV}=");
        body.split(|b| *b == 0)
            .filter_map(|kv| std::str::from_utf8(kv).ok())
            .find_map(|kv| kv.strip_prefix(&prefix).map(|v| v.to_string()))
            .filter(|v| !v.is_empty())
    }
    #[cfg(target_os = "macos")]
    fn env_nonce(&self, pid: u32) -> Option<String> {
        let buf = macos_procargs2_raw(pid)?;
        let pa = super::procargs::parse_procargs2(&buf)?;
        let prefix = format!("{DAEMON_NONCE_ENV}=");
        pa.env
            .iter()
            .find_map(|kv| kv.strip_prefix(&prefix).map(|v| v.to_string()))
            .filter(|v| !v.is_empty())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn env_nonce(&self, _pid: u32) -> Option<String> {
        None
    }
    #[cfg(target_os = "linux")]
    fn comm(&self, pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn comm(&self, _pid: u32) -> Option<String> {
        None
    }
    #[cfg(target_os = "macos")]
    fn comm(&self, pid: u32) -> Option<String> {
        macos_process_comm(pid)
    }

    #[cfg(target_os = "linux")]
    fn cmdline_args(&self, pid: u32) -> Vec<String> {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|a| !a.is_empty())
                    .filter_map(|a| std::str::from_utf8(a).ok().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    fn cmdline_args(&self, pid: u32) -> Vec<String> {
        macos_procargs2_raw(pid)
            .and_then(|buf| super::procargs::parse_procargs2(&buf))
            .map(|pa| pa.argv)
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn cmdline_args(&self, _pid: u32) -> Vec<String> {
        Vec::new()
    }

    fn expected_comm(&self) -> String {
        // Both Linux TASK_COMM_LEN (15) and macOS MAXCOMLEN (16) truncate.
        // Use the shorter limit so exact-comm matching works on both.
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(|n| n.chars().take(15).collect())
            .unwrap_or_else(|| "rustain".to_string())
    }
}

/// Ownership check (Story 12.1b AC-12-1b-8, hardened in 12.1c P1; resolves 12.1a
/// Review-Defer D-1). Called only when `process_alive(pf.pid)` is already true:
/// decides whether the live process recorded in `pf` actually belongs to OUR rustain
/// daemon lineage. After a crash the dead PID can be recycled by an unrelated
/// process; without this check `stop` would SIGTERM an innocent process and the
/// guard would report it "Running".
#[cfg(unix)]
fn process_is_ours(pf: &DaemonPidFile) -> bool {
    ownership_ok(pf, &RealProc)
}

/// The ownership predicate, parameterised over a [`ProcInspector`] for testing.
///
/// Precedence (Story 12.1c P1 — party-mode consensus, decide per long-term
/// correctness):
///  1. **boot-id hard veto** — a recorded boot id differing from the current one
///     means the PID file predates a reboot; the PID space has reset, so this PID is
///     categorically not ours. Wins over every other signal.
///  2. **definitive: injected nonce** — if the PID file carries a nonce AND the live
///     process exposes a `RUSTAIN_DAEMON_NONCE`, they must match. A recycled foreign
///     process does not carry our nonce, so this is the strong same-boot gate for the
///     standalone `start`/`stop` path (the parent injects the nonce into the child).
///  3. **fallback (supervised daemon)** — systemd/launchd start the daemon without
///     the injected env var, so the nonce is absent; require the live process to
///     actually BE a rustain daemon: `comm` EXACTLY the executable name (not a
///     substring — `vim rustain/x.rs` / `tail rustain.log` must NOT match) AND an
///     argv daemon-body token (`__run` / `--foreground`). Unavailable off Linux
///     (`introspectable() == false`) → accept (documented macOS residual).
#[cfg(unix)]
fn ownership_ok(pf: &DaemonPidFile, p: &impl ProcInspector) -> bool {
    // 1. boot-id hard veto.
    if let (Some(recorded), Some(current)) = (pf.boot_id.as_deref(), p.current_boot_id().as_deref())
    {
        if recorded != current {
            return false;
        }
    }

    // 2. Definitive nonce match (standalone path): our parent injected the nonce into
    //    the child's environment, so a genuine instance echoes it; a recycled foreign
    //    PID does not.
    if !pf.nonce.is_empty() {
        if let Some(env_nonce) = p.env_nonce(pf.pid) {
            return env_nonce == pf.nonce;
        }
    }

    // 3. Fallback for the supervised daemon (no injected env nonce).
    if !p.introspectable() {
        // No way to introspect (macOS) — boot-id passed and no nonce to disprove
        // ownership; accept. Residual same-boot recycle is the documented nonce-
        // collision risk (AC-12-1b-8).
        return true;
    }
    let comm_ok = p
        .comm(pf.pid)
        .is_some_and(|c| c == p.expected_comm() || c == "rustain");
    if !comm_ok {
        return false;
    }
    let args = p.cmdline_args(pf.pid);
    let has_daemon = args.iter().any(|a| a == "daemon");
    let has_body = args.iter().any(|a| a == "__run" || a == "--foreground");
    has_daemon && has_body
}

/// Result of the already-running guard (AC-12-1a-9).
#[derive(Debug)]
pub enum GuardOutcome {
    /// PID file exists AND the process is alive — refuse to start.
    Running(DaemonPidFile),
    /// PID file exists but the process is dead (or the file is unreadable) —
    /// reclaim it and proceed (do not require manual cleanup).
    Stale,
    /// No PID file — free to start.
    Free,
}

/// Inspect the PID file to decide whether a daemon is already running for this
/// workspace (AC-12-1a-9). An unreadable file is treated as `Stale` (a corrupt
/// leftover should never block a fresh start).
#[cfg(unix)]
pub fn check_running(pid_path: &Path) -> GuardOutcome {
    if !pid_path.exists() {
        return GuardOutcome::Free;
    }
    match DaemonPidFile::read(pid_path) {
        // Alive AND verified as our daemon lineage → genuinely Running.
        Ok(ref pf) if process_alive(pf.pid) && process_is_ours(pf) => {
            GuardOutcome::Running(pf.clone())
        }
        // Alive but a recycled/foreign PID (ownership mismatch — D-1), a dead PID,
        // or an unreadable file → Stale (reclaimable; never SIGTERM an innocent PID).
        _ => GuardOutcome::Stale,
    }
}

/// Remove the PID file, ignoring "not found".
pub fn remove(pid_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sample(pid: u32) -> DaemonPidFile {
        DaemonPidFile {
            pid,
            socket_path: PathBuf::from("/tmp/x.sock"),
            workspace: PathBuf::from("/ws"),
            started_at_unix: 1_700_000_000,
            profile: "coding".into(),
            nonce: generate_nonce(),
            boot_id: current_boot_id(),
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        let pf = sample(4242);
        pf.write_atomic(&path).unwrap();
        assert!(path.exists());
        assert_eq!(DaemonPidFile::read(&path).unwrap(), pf);
    }

    #[test]
    fn nonce_is_unique_and_hex() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b, "two nonces must differ");
        assert!(!a.is_empty());
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn legacy_pidfile_without_nonce_or_boot_id_parses() {
        // 12.1a-shaped PID file (no nonce/boot_id) must still deserialize via
        // #[serde(default)] — back-compat across the upgrade.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        std::fs::write(
            &path,
            "pid = 4242\n\
             socket_path = \"/tmp/x.sock\"\n\
             workspace = \"/ws\"\n\
             started_at_unix = 1700000000\n\
             profile = \"coding\"\n",
        )
        .unwrap();
        let pf = DaemonPidFile::read(&path).expect("legacy pid file must parse");
        assert_eq!(pf.pid, 4242);
        assert_eq!(pf.nonce, "");
        assert_eq!(pf.boot_id, None);
    }

    #[test]
    fn current_process_is_alive_and_garbage_pid_is_not() {
        assert!(process_alive(std::process::id()));
        // PID 0 is "every process in the group" for kill(2); use a very high,
        // almost-certainly-unused PID for the dead case.
        assert!(!process_alive(0x7FFF_FFF0));
    }

    #[test]
    fn guard_free_then_dead_then_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        assert!(matches!(check_running(&path), GuardOutcome::Free));

        // Dead PID → Stale.
        sample(0x7FFF_FFF0).write_atomic(&path).unwrap();
        assert!(matches!(check_running(&path), GuardOutcome::Stale));

        // Corrupt file → Stale (never blocks a fresh start).
        std::fs::write(&path, "not toml at all").unwrap();
        assert!(matches!(check_running(&path), GuardOutcome::Stale));
    }

    // ── Ownership / PID-recycle hardening (AC-12-1b-8, Task 6) ────────────────
    //
    // These two cases are the heart of D-1. They are Linux-only: Linux is the one
    // platform where we can introspect an arbitrary live PID with zero deps
    // (`/proc`), and where `current_boot_id()` returns a real value to diff. On
    // macOS the residual same-boot recycle is the documented nonce-collision risk.

    /// (a) A live PID that is NOT a rustain process must read as `Stale`, never
    /// `Running` — so `stop` can't SIGTERM an innocent recycled PID and the guard
    /// can't report it running. We spawn a real non-rustain child (`sleep`) and point
    /// the PID file at it; its `/proc/<pid>/comm` is `sleep`, not `rustain`.
    /// (The lib test binary itself is named `rustain-<hash>`, so it would *pass* the
    /// comm check — hence a separate child is required.)
    #[cfg(target_os = "linux")]
    #[test]
    fn live_non_rustain_pid_is_stale_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // Boot id matches the current boot (so the boot-id check passes), the PID is
        // alive, but it is a `sleep` process → ownership (comm) check rejects it.
        let mut pf = sample(child.id());
        pf.boot_id = current_boot_id();
        pf.write_atomic(&path).unwrap();
        let outcome = check_running(&path);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            matches!(outcome, GuardOutcome::Stale),
            "a live non-rustain PID must be Stale (D-1 ownership), not Running"
        );
    }

    /// (b) A PID file whose recorded boot id predates a reboot (here: a bogus boot
    /// id that can't match the current one) must read as `Stale` even though the PID
    /// is alive — the kernel reset the PID space, so this PID can't be our daemon.
    #[cfg(target_os = "linux")]
    #[test]
    fn boot_id_mismatch_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        let mut pf = sample(std::process::id());
        pf.boot_id = Some("00000000-0000-0000-0000-000000000000".to_string());
        pf.write_atomic(&path).unwrap();
        assert!(
            matches!(check_running(&path), GuardOutcome::Stale),
            "a boot-id mismatch must be Stale (pre-reboot PID file)"
        );
    }

    // ── ownership_ok predicate (Story 12.1c P1) — deterministic, fake /proc ───────
    //
    // The recycle/false-positive matrix the substring predicate got wrong. Driven by
    // a fake inspector so every branch is exercised without real processes.

    struct FakeProc {
        boot: Option<String>,
        introspectable: bool,
        env_nonce: Option<String>,
        comm: Option<String>,
        argv: Vec<String>,
        expected_comm: String,
    }

    impl Default for FakeProc {
        fn default() -> Self {
            FakeProc {
                boot: Some("boot-current".into()),
                introspectable: true,
                env_nonce: None,
                comm: Some("rustain".into()),
                argv: vec!["rustain".into(), "daemon".into(), "__run".into()],
                expected_comm: "rustain".into(),
            }
        }
    }

    impl ProcInspector for FakeProc {
        fn current_boot_id(&self) -> Option<String> {
            self.boot.clone()
        }
        fn introspectable(&self) -> bool {
            self.introspectable
        }
        fn env_nonce(&self, _pid: u32) -> Option<String> {
            self.env_nonce.clone()
        }
        fn comm(&self, _pid: u32) -> Option<String> {
            self.comm.clone()
        }
        fn cmdline_args(&self, _pid: u32) -> Vec<String> {
            self.argv.clone()
        }
        fn expected_comm(&self) -> String {
            self.expected_comm.clone()
        }
    }

    fn pf_with(nonce: &str, boot: Option<&str>) -> DaemonPidFile {
        DaemonPidFile {
            pid: 4242,
            socket_path: PathBuf::from("/tmp/x.sock"),
            workspace: PathBuf::from("/ws"),
            started_at_unix: 1_700_000_000,
            profile: "coding".into(),
            nonce: nonce.into(),
            boot_id: boot.map(str::to_string),
        }
    }

    /// THE regression (old D6): a recycled PID landed on a non-daemon process whose
    /// argv contains "rustain" (e.g. `vim rustain/src/x.rs`) must NOT be ours — the
    /// substring predicate wrongly matched it, causing `stop` to SIGTERM an innocent.
    #[test]
    fn vim_editing_rustain_files_is_not_ours() {
        let pf = pf_with("abc", Some("boot-current"));
        let fake = FakeProc {
            env_nonce: None,
            comm: Some("vim".into()),
            argv: vec!["vim".into(), "rustain/src/main.rs".into()],
            ..Default::default()
        };
        assert!(
            !ownership_ok(&pf, &fake),
            "vim editing rustain files is NOT our daemon"
        );
    }

    #[test]
    fn tail_of_rustain_log_is_not_ours() {
        let pf = pf_with("abc", Some("boot-current"));
        let fake = FakeProc {
            comm: Some("tail".into()),
            argv: vec!["tail".into(), "-f".into(), "rustain.log".into()],
            ..Default::default()
        };
        assert!(!ownership_ok(&pf, &fake));
    }

    /// boot-id mismatch WINS even when comm + argv say "rustain daemon __run".
    #[test]
    fn boot_id_mismatch_beats_matching_comm() {
        let pf = pf_with("abc", Some("boot-OLD"));
        let fake = FakeProc {
            boot: Some("boot-current".into()),
            comm: Some("rustain".into()),
            argv: vec!["rustain".into(), "daemon".into(), "__run".into()],
            ..Default::default()
        };
        assert!(
            !ownership_ok(&pf, &fake),
            "pre-reboot PID file is never ours"
        );
    }

    /// Definitive nonce: env nonce present but mismatched → not ours (even if it
    /// otherwise looks like a daemon).
    #[test]
    fn env_nonce_mismatch_is_not_ours() {
        let pf = pf_with("file-nonce", Some("boot-current"));
        let fake = FakeProc {
            env_nonce: Some("different-nonce".into()),
            ..Default::default()
        };
        assert!(!ownership_ok(&pf, &fake));
    }

    /// Definitive nonce: env nonce present and matches → ours (the standalone path).
    #[test]
    fn env_nonce_match_is_ours() {
        let pf = pf_with("the-nonce", Some("boot-current"));
        let fake = FakeProc {
            env_nonce: Some("the-nonce".into()),
            // comm/argv deliberately bogus to prove the nonce match is definitive.
            comm: Some("anything".into()),
            argv: vec!["whatever".into()],
            ..Default::default()
        };
        assert!(ownership_ok(&pf, &fake));
    }

    /// Supervised daemon (no injected env nonce): exact comm + daemon argv token → ours.
    #[test]
    fn supervised_daemon_without_env_nonce_is_ours() {
        let pf = pf_with("file-nonce", Some("boot-current"));
        let fake = FakeProc {
            env_nonce: None,
            comm: Some("rustain".into()),
            argv: vec![
                "rustain".into(),
                "daemon".into(),
                "start".into(),
                "--foreground".into(),
            ],
            ..Default::default()
        };
        assert!(ownership_ok(&pf, &fake));
    }

    /// Supervised fallback requires a daemon BODY token — a foreign process literally
    /// named `rustain` but NOT running the daemon body is not ours.
    #[test]
    fn rustain_named_process_without_daemon_token_is_not_ours() {
        let pf = pf_with("file-nonce", Some("boot-current"));
        let fake = FakeProc {
            env_nonce: None,
            comm: Some("rustain".into()),
            argv: vec!["rustain".into(), "--help".into()],
            ..Default::default()
        };
        assert!(!ownership_ok(&pf, &fake));
    }

    /// Non-introspectable platform (macOS): boot ok + no disproving nonce → accept
    /// (documented residual).
    #[test]
    fn non_introspectable_platform_accepts_after_boot_and_nonce() {
        let pf = pf_with("file-nonce", Some("boot-current"));
        let fake = FakeProc {
            introspectable: false,
            env_nonce: None,
            ..Default::default()
        };
        assert!(ownership_ok(&pf, &fake));
    }

    // ── macOS child-spawn introspection (AC-12-1d-3c) — NOT #[ignore] ────────────
    //
    // Spawns a child with known argv + RUSTAIN_DAEMON_NONCE, introspects via
    // the real sysctl path + pure parser round-trip, and asserts the result.

    /// Positive case: child carries a nonce → RealProc reads it back via KERN_PROCARGS2.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_child_introspection_positive() {
        let nonce = "test-nonce-abc123";
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .env(DAEMON_NONCE_ENV, nonce)
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        // Give the child a moment to start.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let inspector = RealProc;

        // comm should be "sleep"
        let comm = inspector.comm(pid);
        assert!(
            comm.as_deref() == Some("sleep"),
            "expected comm='sleep', got {:?}",
            comm
        );

        // env_nonce round-trip through KERN_PROCARGS2 + parse_procargs2
        let env = inspector.env_nonce(pid);
        assert_eq!(
            env.as_deref(),
            Some(nonce),
            "KERN_PROCARGS2 round-trip must recover the injected nonce"
        );

        // cmdline_args round-trip
        let args = inspector.cmdline_args(pid);
        assert!(
            args.iter().any(|a| a == "60"),
            "cmdline must contain the sleep argument '60', got {:?}",
            args
        );
        assert!(
            args.first().map(|s| s.as_str()) == Some("sleep"),
            "argv[0] must be 'sleep', got {:?}",
            args
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Negative case: child without a nonce → env_nonce returns None.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_child_introspection_negative() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let inspector = RealProc;
        assert!(
            inspector.env_nonce(pid).is_none(),
            "a child without RUSTAIN_DAEMON_NONCE must return None"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// macOS zombie detection (AC-12-1d-4): a zombie must report as not alive.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_zombie_process_is_not_alive() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        // Do NOT call child.wait() — let the child become a zombie.
        // The child will exit immediately (it's `true`), but we haven't reaped it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let is_zombie = loop {
            if let Some(stat) = macos_process_stat(pid) {
                if stat.starts_with('Z') {
                    break true;
                }
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(is_zombie, "child did not become zombie within timeout");
        // On macOS, `ps` state 'Z' means the process is a zombie.
        assert!(
            !process_alive(pid),
            "a zombie process must report as NOT alive on macOS"
        );
        let _ = child.wait(); // reap
    }
}
