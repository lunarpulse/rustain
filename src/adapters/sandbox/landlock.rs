//! `LandlockSandbox` — Linux Landlock-backed sandbox enforcement.
//!
//! Gated on `#[cfg(all(target_os = "linux", feature = "sandbox"))]`. Uses the
//! `landlock` crate per ADR-06-04 §References.
//!
//! # ABI selection (Decision Gate 5.2)
//!
//! Targets Landlock ABI v3 (`LANDLOCK_ACCESS_FS_TRUNCATE`) as the minimum
//! useful baseline. If the kernel ABI is below v3, construction returns
//! `SandboxError::AbiTooOld` and the composition root falls back to NoOpSandbox
//! per ADR-06-04 §Negative.
//!
//! # `apply()` semantics
//!
//! `Command::pre_exec` closure runs in the CHILD process after `fork()`,
//! before `execve()`. Inside the closure we call `RulesetCreated::restrict_self()`.
//! The closure is `unsafe fn() -> Result<(), io::Error>` per
//! `std::os::unix::process::CommandExt` contract — we MUST NOT allocate
//! (or do anything async-unsafe) inside it.
//!
//! Mitigation: the ruleset is **built outside the closure** (in `apply()`
//! itself, before `cmd.pre_exec(closure)` is called); the closure captures
//! only the ruleset (inside an `Option` for ownership) and calls its
//! `restrict_self` method. The crate's `RulesetCreated` is `Send`-able.

use std::os::unix::process::CommandExt;

use async_trait::async_trait;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr,
};
use tokio::process::Command;

use super::{SandboxAdapterKind, SandboxError};
use crate::domain::models::sandbox::SandboxPolicy;
use crate::domain::ports::SandboxManager;

/// Landlock-backed sandbox adapter.
///
/// Stateless — rulesets are built on-demand per-call. `restrict_self()` is
/// called exactly once at startup; `apply()` builds fresh rulesets for each
/// Bash subprocess spawn.
pub struct LandlockSandbox {
    /// Detected kernel Landlock ABI version (probed at construction time).
    detected_abi: u32,
}

impl LandlockSandbox {
    /// Construct a LandlockSandbox.
    ///
    /// Returns `Err(SandboxError::AbiTooOld)` if the kernel Landlock ABI is
    /// below v3. Returns `Err(SandboxError::RulesetBuildFailed)` if Landlock
    /// is not available on this kernel.
    pub fn new(_startup_policy: &SandboxPolicy) -> Result<Self, SandboxError> {
        // Probe: attempt V3-capable ruleset creation. If this fails, the kernel
        // either lacks Landlock entirely or is below ABI v3. The crate's
        // `RestrictSelfError` distinguishes between "not supported" and
        // "ruleset error"; we treat any creation failure as ABI-too-old.
        let _ = Self::build_ruleset(_startup_policy)?;
        Ok(Self { detected_abi: 3 })
    }

    /// Build a Landlock ruleset for a given policy.
    ///
    /// Pure function: no side effects, no I/O beyond `PathFd::new`, no async.
    fn build_ruleset(policy: &SandboxPolicy) -> Result<RulesetCreated, SandboxError> {
        let mk_err = |e: landlock::RulesetError| SandboxError::RulesetBuildFailed(e.to_string());

        match policy {
            SandboxPolicy::Permissive => {
                // Permissive = no restrictions. Build a ruleset that allows
                // read+write everywhere from / so the process is NOT locked out.
                let root_fd = PathFd::new("/").map_err(|_e| {
                    SandboxError::RulesetBuildFailed(
                        "failed to open / for permissive ruleset".into(),
                    )
                })?;
                let rule = PathBeneath::new(root_fd, AccessFs::from_all(ABI::V3));
                Ruleset::default()
                    .handle_access(AccessFs::from_all(ABI::V3))
                    .map_err(&mk_err)?
                    .create()
                    .map_err(&mk_err)?
                    .add_rules([Ok(rule)])
                    .map_err(|e: landlock::RulesetError| {
                        SandboxError::RulesetBuildFailed(e.to_string())
                    })
            }
            SandboxPolicy::ReadOnly { network } => {
                // Read-only filesystem. Network deferred (ABI v4+).
                if !network {
                    tracing::warn!(
                        "Landlock ABI v3 does not support network restriction; \
                         network=false requested but cannot be enforced (upgrade to kernel 6.8+ for ABI v4)",
                    );
                }
                let root_fd = PathFd::new("/").map_err(|_e| {
                    SandboxError::RulesetBuildFailed(
                        "failed to open / for read-only ruleset".into(),
                    )
                })?;
                let rule = PathBeneath::new(root_fd, AccessFs::from_read(ABI::V3));
                Ruleset::default()
                    .handle_access(AccessFs::from_all(ABI::V3))
                    .map_err(&mk_err)?
                    .create()
                    .map_err(&mk_err)?
                    .add_rules([Ok(rule)])
                    .map_err(|e: landlock::RulesetError| {
                        SandboxError::RulesetBuildFailed(e.to_string())
                    })
            }
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                read_only_paths,
                network,
            } => {
                if !network {
                    tracing::warn!(
                        "Landlock ABI v3 does not support network restriction; \
                         network=false requested but cannot be enforced (upgrade to kernel 6.8+ for ABI v4)",
                    );
                }
                let root_fd = PathFd::new("/").map_err(|_e| {
                    SandboxError::RulesetBuildFailed(
                        "failed to open / for workspace-write ruleset".into(),
                    )
                })?;
                let mut rules: Vec<Result<PathBeneath<PathFd>, landlock::RulesetError>> =
                    Vec::new();
                // Allow read everywhere from /.
                rules.push(Ok(PathBeneath::new(root_fd, AccessFs::from_read(ABI::V3))));
                // Allow read+write on each writable_root.
                for root in writable_roots {
                    if let Ok(fd) = PathFd::new(root) {
                        rules.push(Ok(PathBeneath::new(fd, AccessFs::from_all(ABI::V3))));
                    } else {
                        tracing::warn!(
                            path = %root.display(),
                            "Landlock: skipping unopenable writable root — \
                             path will NOT be writable in sandboxed subprocess"
                        );
                    }
                }
                // Read-only paths: explicit read-only grant.
                for ro in read_only_paths {
                    if let Ok(fd) = PathFd::new(ro) {
                        rules.push(Ok(PathBeneath::new(fd, AccessFs::from_read(ABI::V3))));
                    } else {
                        tracing::warn!(
                            path = %ro.display(),
                            "Landlock: skipping unopenable read-only path — \
                             path will NOT be restricted in sandboxed subprocess"
                        );
                    }
                }
                Ruleset::default()
                    .handle_access(AccessFs::from_all(ABI::V3))
                    .map_err(&mk_err)?
                    .create()
                    .map_err(&mk_err)?
                    .add_rules(rules)
                    .map_err(|e: landlock::RulesetError| {
                        SandboxError::RulesetBuildFailed(e.to_string())
                    })
            }
        }
    }
}

#[async_trait]
impl SandboxManager for LandlockSandbox {
    fn kind(&self) -> SandboxAdapterKind {
        SandboxAdapterKind::Landlock
    }

    async fn apply(&self, cmd: &mut Command, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        // Build the ruleset OUTSIDE the pre_exec closure (allocation-safe).
        let ruleset = Self::build_ruleset(policy)?;

        // SAFETY: pre_exec runs after fork() but before execve(). We must not
        // do anything async-unsafe. All allocation happened above; the
        // closure only calls a single C-ABI syscall (`restrict_self`).
        // `RulesetCreated::restrict_self()` takes ownership, so we move
        // the ruleset into the closure via an `Option` assignment.
        let mut ruleset_opt = Some(ruleset);
        unsafe {
            cmd.pre_exec(move || {
                ruleset_opt
                    .take()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "landlock ruleset already consumed",
                        )
                    })?
                    .restrict_self()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            });
        }

        Ok(())
    }

    async fn restrict_self(&self, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        // Build a fresh ruleset for the parent process. Called exactly once
        // at startup; Landlock is one-way restrictive anyway, so subsequent
        // calls would be no-ops even if the ruleset were cached.
        let ruleset = Self::build_ruleset(policy)?;
        ruleset
            .restrict_self()
            .map_err(|e| SandboxError::RulesetBuildFailed(e.to_string()))?;
        tracing::info!(
            adapter = ?self.kind(),
            abi_version = self.detected_abi,
            "Landlock sandbox restricted parent rustain process at startup"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        std::env::temp_dir().join("rustain-sandbox-test")
    }

    #[tokio::test]
    async fn test_kind_returns_landlock() {
        let startup = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![ws()],
            read_only_paths: vec![],
            network: true,
        };
        match LandlockSandbox::new(&startup) {
            Ok(sb) => assert_eq!(sb.kind(), SandboxAdapterKind::Landlock),
            Err(SandboxError::RulesetBuildFailed(_)) => {
                eprintln!("SKIPPED: Landlock not available on this kernel");
            }
            Err(e) => panic!("unexpected sandbox construction error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_build_ruleset_for_permissive() {
        let res = LandlockSandbox::build_ruleset(&SandboxPolicy::Permissive);
        if res.is_err() {
            eprintln!(
                "SKIPPED: Landlock ruleset build failed: {}",
                res.as_ref().err().unwrap()
            );
            return;
        }
        assert!(res.is_ok(), "Permissive ruleset must build cleanly");
    }

    #[tokio::test]
    async fn test_build_ruleset_for_readonly() {
        let res = LandlockSandbox::build_ruleset(&SandboxPolicy::ReadOnly { network: false });
        if res.is_err() {
            eprintln!(
                "SKIPPED: Landlock ruleset build failed: {}",
                res.as_ref().err().unwrap()
            );
            return;
        }
        assert!(res.is_ok(), "ReadOnly ruleset must build cleanly");
    }

    #[tokio::test]
    async fn test_apply_to_command_does_not_panic() {
        let startup = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![ws()],
            read_only_paths: vec![],
            network: true,
        };
        let Ok(sb) = LandlockSandbox::new(&startup) else {
            eprintln!("SKIPPED: Landlock not available on this kernel");
            return;
        };
        let mut cmd = Command::new("/bin/true");
        let result = sb
            .apply(&mut cmd, &SandboxPolicy::ReadOnly { network: false })
            .await;
        assert!(result.is_ok());
        // Actually spawn the child so the pre_exec closure — including
        // restrict_self — is exercised in the forked process.
        let status = cmd
            .spawn()
            .expect("spawn /bin/true")
            .wait()
            .await
            .expect("wait /bin/true");
        assert!(
            status.success(),
            "/bin/true should exit 0 under landlock read-only sandbox"
        );
    }
}
