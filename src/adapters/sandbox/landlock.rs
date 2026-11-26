//! `LandlockSandbox` — Linux Landlock-backed confinement of spawned commands.
//!
//! Gated on `#[cfg(all(target_os = "linux", feature = "sandbox"))]`
//! (ADR-06-04, amended by ADR-23-1-01).
//!
//! # Minimum ABI v3, hard requirement
//!
//! v1 cannot grant `REFER` and v2 cannot handle `TRUNCATE`, so ABI v3
//! (Linux 6.2) is the minimum. The ruleset uses `CompatLevel::HardRequirement`:
//! a kernel that cannot enforce the v3 filesystem rights fails the build of
//! the ruleset instead of being silently skipped (the crate's best-effort
//! default). `network = false` handles TCP bind/connect from ABI v4.
//!
//! # Per spawn, never the rustain process
//!
//! [`SandboxManager::apply`] builds the ruleset in the parent and registers a
//! `pre_exec` closure that restricts the child between `fork` and `exec`.
//! Every descendant inherits the domain for life. The closure only calls
//! `restrict_self`, compares the returned status and builds an `io::Error`
//! from a raw errno: no allocation after `fork`.
//!
//! # Reads and the deny set
//!
//! Landlock grants are a union along the path, so a denied path is carved out
//! by splitting: `READ_DIR | EXECUTE` on `/`, and `READ_FILE` beneath every
//! sibling along each ancestor of a denied path except the next component
//! (entries listed at spawn; symlinks need no rule because Landlock checks the
//! resolved hierarchy).

use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetStatus, Scope,
};
use tokio::process::Command;

use super::{SandboxAdapterKind, SandboxError};
use crate::domain::models::sandbox::SandboxPolicy;
use crate::domain::ports::SandboxManager;

/// The minimum Landlock ABI (Linux 6.2): `REFER` and `TRUNCATE`.
pub const MIN_ABI: u32 = 3;
/// The first ABI that handles TCP bind/connect (Linux 6.7).
pub const NETWORK_ABI: u32 = 4;
/// The first ABI that handles device ioctls (Linux 6.10).
pub const IOCTL_ABI: u32 = 5;
/// The first ABI that scopes signals and abstract Unix sockets (Linux 6.12).
pub const SCOPE_ABI: u32 = 6;

/// The filesystem rights every ruleset handles.
fn handled_fs() -> BitFlags<AccessFs> {
    AccessFs::from_all(ABI::V3)
}

/// Open `path` itself, never a symlink it may have become: `O_PATH |
/// O_NOFOLLOW`, plus `O_DIRECTORY` for a directory.
fn open_nofollow(path: &Path, directory: bool) -> std::io::Result<OwnedFd> {
    let mut flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    Ok(file.into())
}

/// One writable root, opened when the session resolved its sandbox.
struct HeldRoot {
    path: PathBuf,
    fd: OwnedFd,
    directory: bool,
}

/// Landlock-backed sandbox adapter. Rulesets are built per spawn from the
/// writable roots' descriptors held since session start.
pub struct LandlockSandbox {
    abi: u32,
    deny_read: Vec<PathBuf>,
    roots: Vec<HeldRoot>,
    /// Test seam: the `pre_exec` closure reads the restriction as not enforced.
    inject_not_enforced: bool,
}

impl LandlockSandbox {
    /// Probe the running kernel. `Ok(abi)` when Landlock is enabled with
    /// ABI >= 3 and a hard-requirement v3 ruleset can be created (no
    /// restriction is applied); otherwise the reason, in words.
    pub fn probe() -> Result<u32, String> {
        let abi = kernel_abi()?;
        if abi < MIN_ABI {
            return Err(format!(
                "the kernel's Landlock ABI is {abi}, below the required ABI {MIN_ABI}"
            ));
        }
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(handled_fs())
            .and_then(|r| r.create())
            .map_err(|e| format!("a Landlock ABI {MIN_ABI} ruleset cannot be created ({e})"))?;
        Ok(abi)
    }

    /// An adapter for a kernel with Landlock `abi` (from [`Self::probe`]), the
    /// canonical deny set and the canonical writable set `W`. Every root is
    /// opened now with `O_PATH | O_NOFOLLOW` and its descriptor reused by
    /// every spawn: a confined command that later replaces a root (or one of
    /// its parents) with a symlink cannot widen the next spawn (review H2).
    pub fn new(
        abi: u32,
        deny_read: Vec<PathBuf>,
        writable_roots: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let roots = writable_roots
            .iter()
            .map(|path| {
                let directory = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
                let fd = open_nofollow(path, directory).map_err(|e| {
                    SandboxError::RulesetBuildFailed(format!(
                        "cannot open writable root {}: {e}",
                        path.display()
                    ))
                })?;
                Ok(HeldRoot {
                    path: path.clone(),
                    fd,
                    directory,
                })
            })
            .collect::<Result<Vec<_>, SandboxError>>()?;
        Ok(Self {
            abi,
            deny_read,
            roots,
            inject_not_enforced: false,
        })
    }

    /// Test seam: make the in-child check see a not-enforced restriction.
    #[doc(hidden)]
    pub fn with_injected_not_enforced(mut self, inject: bool) -> Self {
        self.inject_not_enforced = inject;
        self
    }

    /// The probed kernel ABI.
    pub fn abi(&self) -> u32 {
        self.abi
    }

    /// Build the ruleset for `policy`. No restriction is applied here.
    fn build_ruleset(&self, policy: &SandboxPolicy) -> Result<RulesetCreated, SandboxError> {
        let (writable_roots, network): (&[PathBuf], bool) = match policy {
            SandboxPolicy::Permissive => {
                return Err(SandboxError::Unsupported {
                    adapter: SandboxAdapterKind::Landlock,
                    reason: "a permissive policy is bound only with the noop adapter".into(),
                });
            }
            SandboxPolicy::ReadOnly { network } => (&[], *network),
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                read_only_paths,
                network,
            } => {
                if let Some(ro) = read_only_paths
                    .iter()
                    .find(|ro| writable_roots.iter().any(|root| ro.starts_with(root)))
                {
                    return Err(SandboxError::Unsupported {
                        adapter: SandboxAdapterKind::Landlock,
                        reason: format!(
                            "read-only path {} lies under a writable root; Landlock grants are a \
                             union and cannot make it read-only",
                            ro.display()
                        ),
                    });
                }
                (writable_roots.as_slice(), *network)
            }
        };

        let err = |e: landlock::RulesetError| SandboxError::RulesetBuildFailed(e.to_string());
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(handled_fs())
            .map_err(err)?;
        if self.abi >= IOCTL_ABI {
            // Device ioctls (TIOCSTI and friends) on files opened inside the
            // sandbox are denied unless granted below (review M1).
            ruleset = ruleset.handle_access(AccessFs::IoctlDev).map_err(err)?;
        }
        if !network && self.abi >= NETWORK_ABI {
            ruleset = ruleset
                .handle_access(AccessNet::from_all(ABI::V4))
                .map_err(err)?;
        }
        if self.abi >= SCOPE_ABI {
            // Signals and abstract Unix sockets cannot reach processes outside
            // the sandbox (review M2).
            ruleset = ruleset
                .scope(Scope::AbstractUnixSocket | Scope::Signal)
                .map_err(err)?;
        }
        let mut created = ruleset.create().map_err(err)?;

        // Reads: everything except the deny set.
        let io = |path: &Path, e: std::io::Error| {
            SandboxError::RulesetBuildFailed(format!("cannot open {}: {e}", path.display()))
        };
        let root_read = AccessFs::ReadDir | AccessFs::Execute;
        created = created
            .add_rule(PathBeneath::new(
                open_nofollow(Path::new("/"), true).map_err(|e| io(Path::new("/"), e))?,
                root_read,
            ))
            .map_err(err)?;
        for path in split_read_grants(&self.deny_read, &mut list_dir) {
            // An entry that vanished since the listing needs no grant.
            let Ok(fd) = open_nofollow(&path, false) else {
                continue;
            };
            created = created
                .add_rule(PathBeneath::new(fd, AccessFs::ReadFile))
                .map_err(err)?;
        }

        // Writes: the writable set only, through the descriptors held since
        // the session started.
        for root in writable_roots {
            let Some(held) = self.roots.iter().find(|held| held.path == *root) else {
                return Err(SandboxError::RulesetBuildFailed(format!(
                    "writable root {} was not opened when the session started",
                    root.display()
                )));
            };
            let fd = held.fd.try_clone().map_err(|e| io(root, e))?;
            let access = if held.directory {
                handled_fs()
            } else if self.abi >= IOCTL_ABI {
                AccessFs::from_file(ABI::V3) | AccessFs::IoctlDev
            } else {
                AccessFs::from_file(ABI::V3)
            };
            created = created
                .add_rule(PathBeneath::new(fd, access))
                .map_err(err)?;
        }
        Ok(created)
    }
}

/// The kernel's Landlock ABI, or why there is none.
fn kernel_abi() -> Result<u32, String> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: landlock_create_ruleset(NULL, 0, VERSION) only returns a number.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if v >= 0 {
        return Ok(u32::try_from(v).unwrap_or(u32::MAX));
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOSYS) => Err("Landlock is not built into this kernel".into()),
        Some(libc::EOPNOTSUPP) => {
            Err("Landlock is disabled at boot (it is missing from the `lsm=` list)".into())
        }
        other => Err(format!(
            "the Landlock probe failed (errno {}; a seccomp filter may block it)",
            other.unwrap_or(0)
        )),
    }
}

/// Directory entries of `dir`, without symlinks.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
        .map(|e| e.path())
        .collect()
}

/// The deny split: the paths that receive `READ_FILE`. With no denied path
/// that is `/`; otherwise, for every directory on the way from `/` to a denied
/// path, each entry that is neither denied nor itself on such a way.
/// `deny` holds absolute canonical paths; `list` returns full entry paths.
pub(crate) fn split_read_grants(
    deny: &[PathBuf],
    list: &mut dyn FnMut(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    if deny.is_empty() {
        return vec![PathBuf::from("/")];
    }
    let mut grants = Vec::new();
    let mut pending = vec![PathBuf::from("/")];
    while let Some(dir) = pending.pop() {
        for entry in list(&dir) {
            if deny.contains(&entry) {
                continue;
            }
            if deny.iter().any(|d| d.starts_with(&entry)) {
                pending.push(entry);
            } else {
                grants.push(entry);
            }
        }
    }
    grants
}

#[async_trait]
impl SandboxManager for LandlockSandbox {
    fn kind(&self) -> SandboxAdapterKind {
        SandboxAdapterKind::Landlock
    }

    async fn apply(&self, cmd: &mut Command, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        // Built in the parent: the child only restricts itself.
        let mut ruleset = Some(self.build_ruleset(policy)?);
        let inject = self.inject_not_enforced;
        // SAFETY: runs after fork, before exec. No allocation: `restrict_self`
        // issues prctl/landlock syscalls on an owned fd, and every error is a
        // raw errno.
        unsafe {
            cmd.pre_exec(move || {
                let Some(ruleset) = ruleset.take() else {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                };
                let status = match ruleset.restrict_self() {
                    Ok(status) => status.ruleset,
                    Err(_) => return Err(std::io::Error::from_raw_os_error(libc::EPERM)),
                };
                let observed = if inject {
                    RulesetStatus::NotEnforced
                } else {
                    status
                };
                if observed == RulesetStatus::FullyEnforced {
                    Ok(())
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EPERM))
                }
            });
        }
        Ok(())
    }
}

/// Whether the real-kernel tests can run: `true` when this host has Landlock
/// ABI >= `min_abi`. Otherwise a loud named skip, or a panic when
/// `RUSTAIN_REQUIRE_LANDLOCK=1` (hosted CI), so CI cannot pass by skipping.
#[doc(hidden)]
pub fn require_landlock(test: &str, min_abi: u32) -> bool {
    let required = crate::infrastructure::utils::env_var_trimmed("RUSTAIN_REQUIRE_LANDLOCK")
        .is_some_and(|v| v == "1");
    require_landlock_with(test, min_abi, LandlockSandbox::probe(), required)
}

/// [`require_landlock`] with the probe result and the CI setting injected.
#[doc(hidden)]
pub fn require_landlock_with(
    test: &str,
    min_abi: u32,
    probe: Result<u32, String>,
    required: bool,
) -> bool {
    let missing = match probe {
        Ok(abi) if abi >= min_abi => {
            eprintln!("{test}: Landlock ABI {abi} detected");
            return true;
        }
        Ok(abi) => format!("Landlock ABI {abi} is below {min_abi}"),
        Err(reason) => reason,
    };
    assert!(
        !required,
        "{test}: RUSTAIN_REQUIRE_LANDLOCK=1 but {missing}"
    );
    eprintln!("SKIPPED {test}: {missing}");
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fake_tree(entries: &[(&str, &[&str])]) -> BTreeMap<PathBuf, Vec<PathBuf>> {
        entries
            .iter()
            .map(|(dir, names)| {
                (
                    PathBuf::from(dir),
                    names.iter().map(|n| Path::new(dir).join(n)).collect(),
                )
            })
            .collect()
    }

    fn split(deny: &[&str], tree: &BTreeMap<PathBuf, Vec<PathBuf>>) -> Vec<PathBuf> {
        let deny: Vec<PathBuf> = deny.iter().map(PathBuf::from).collect();
        let mut grants =
            split_read_grants(&deny, &mut |dir| tree.get(dir).cloned().unwrap_or_default());
        grants.sort();
        grants
    }

    #[test]
    fn split_without_a_deny_set_grants_the_root() {
        assert_eq!(split(&[], &BTreeMap::new()), vec![PathBuf::from("/")]);
    }

    #[test]
    fn split_grants_every_sibling_along_the_way_but_not_the_denied_path() {
        let tree = fake_tree(&[
            ("/", &["etc", "home", "tmp"]),
            ("/home", &["u", "v"]),
            ("/home/u", &[".rustain", ".ssh", "notes.txt"]),
            ("/home/u/.rustain", &["auth.json", "usage"]),
        ]);
        let grants = split(&["/home/u/.rustain/auth.json"], &tree);
        let expected: Vec<PathBuf> = [
            "/etc",
            "/home/u/.rustain/usage",
            "/home/u/.ssh",
            "/home/u/notes.txt",
            "/home/v",
            "/tmp",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(grants, expected);
    }

    #[test]
    fn split_handles_two_denied_paths_sharing_ancestors() {
        let tree = fake_tree(&[
            ("/", &["home", "opt"]),
            ("/home", &["u"]),
            ("/home/u", &["a", "b", "c"]),
        ]);
        let grants = split(&["/home/u/a", "/home/u/b"], &tree);
        assert_eq!(
            grants,
            vec![PathBuf::from("/home/u/c"), PathBuf::from("/opt")]
        );
    }

    #[test]
    fn k12_require_landlock_panics_under_the_ci_setting_on_abi_2() {
        assert!(!require_landlock_with("k12", 3, Ok(2), false));
        assert!(require_landlock_with("k12", 3, Ok(3), true));
        let caught = std::panic::catch_unwind(|| require_landlock_with("k12", 3, Ok(2), true));
        assert!(
            caught.is_err(),
            "ABI 2 under RUSTAIN_REQUIRE_LANDLOCK=1 must fail"
        );
        let caught = std::panic::catch_unwind(|| {
            require_landlock_with("k12", 3, Err("not built".into()), true)
        });
        assert!(caught.is_err());
    }

    #[test]
    fn landlock_probe_reports_the_host_abi() {
        match LandlockSandbox::probe() {
            Ok(abi) => eprintln!("Landlock probe: ABI {abi}"),
            Err(reason) => eprintln!("Landlock probe: unavailable ({reason})"),
        }
        let _ = require_landlock("landlock_probe_reports_the_host_abi", MIN_ABI);
    }

    #[tokio::test]
    async fn permissive_and_read_only_under_writable_root_are_refused() {
        let sb = LandlockSandbox::new(MIN_ABI, Vec::new(), &[]).unwrap();
        assert!(matches!(
            sb.build_ruleset(&SandboxPolicy::Permissive),
            Err(SandboxError::Unsupported { .. })
        ));
        let ws = PathBuf::from("/tmp/ws");
        assert!(matches!(
            sb.build_ruleset(&SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![ws.clone()],
                read_only_paths: vec![ws.join(".git")],
                network: true,
            }),
            Err(SandboxError::Unsupported { .. })
        ));
    }

    /// Spawn `sh -c script` under `policy` and return (exit success, stderr).
    async fn run(sb: &LandlockSandbox, policy: &SandboxPolicy, script: &str) -> (bool, String) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        sb.apply(&mut cmd, policy).await.expect("apply");
        let out = cmd.output().await.expect("spawn under landlock");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[tokio::test]
    async fn read_only_policy_blocks_writes_and_keeps_reads() {
        if !require_landlock("read_only_policy_blocks_writes_and_keeps_reads", MIN_ABI) {
            return;
        }
        let abi = LandlockSandbox::probe().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let sb = LandlockSandbox::new(abi, Vec::new(), &[]).unwrap();
        let policy = SandboxPolicy::ReadOnly { network: true };
        let (ok, _) = run(&sb, &policy, &format!("cat {}", file.display())).await;
        assert!(ok, "reads stay allowed");
        let (ok, _) = run(&sb, &policy, &format!("echo y > {}", file.display())).await;
        assert!(!ok, "writes are denied");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
    }
}
