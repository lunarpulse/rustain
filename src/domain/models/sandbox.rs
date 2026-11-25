//! Sandbox policy, scope and status (ADR-06-04, amended by ADR-23-1-01).
//!
//! Pure domain values: the writable set and the probe result are computed by
//! the composition root (`infrastructure::sandbox::resolve_sandbox`) and
//! carried here so every surface derives policies, notices and the YOLO gate
//! from the same facts.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{NoticeLevel, PermissionMode};

/// Sandbox policy for one spawned process tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxPolicy {
    /// No restrictions. Produced only when the sandbox is configured off
    /// (`[sandbox].adapter = "noop"`).
    Permissive,
    /// Read-only filesystem, optionally with network blocked.
    ReadOnly { network: bool },
    /// Writable within `writable_roots`. `read_only_paths` is kept for the
    /// ADR-10-3 narrowing checks; the derivation never fills it, and the
    /// Landlock adapter refuses a read-only path under a writable root
    /// (Landlock grants are a union over the path).
    WorkspaceWrite {
        writable_roots: Vec<PathBuf>,
        read_only_paths: Vec<PathBuf>,
        network: bool,
    },
}

/// The session's sandbox bound: the writable set `W` and the network setting,
/// or `off` when the user turned the sandbox off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxScope {
    /// `W`: workspace, temp, device files, toolchain caches and
    /// `[sandbox].writable_roots` (canonical, existing paths only).
    pub writable_roots: Vec<PathBuf>,
    /// `[sandbox].network`.
    pub network: bool,
    /// `[sandbox].adapter = "noop"`: every policy is `Permissive`.
    pub off: bool,
}

impl SandboxScope {
    /// The scope of a sandbox that is configured off.
    pub fn off() -> Self {
        Self {
            writable_roots: Vec::new(),
            network: true,
            off: true,
        }
    }

    /// The ceiling policy MCP stdio servers run under: `W` with network.
    pub fn ceiling(&self) -> SandboxPolicy {
        if self.off {
            return SandboxPolicy::Permissive;
        }
        SandboxPolicy::WorkspaceWrite {
            writable_roots: self.writable_roots.clone(),
            read_only_paths: Vec::new(),
            network: true,
        }
    }
}

impl SandboxPolicy {
    /// Derive the Bash-tool policy from the permission mode (ADR-23-1-01 §4).
    /// YOLO removes prompts, not the bound: it gets the same policy as Normal.
    pub fn from_mode(mode: PermissionMode, scope: &SandboxScope) -> Self {
        if scope.off {
            return SandboxPolicy::Permissive;
        }
        match mode {
            PermissionMode::Plan => SandboxPolicy::ReadOnly { network: false },
            PermissionMode::Normal | PermissionMode::AutoEdit | PermissionMode::Yolo => {
                SandboxPolicy::WorkspaceWrite {
                    writable_roots: scope.writable_roots.clone(),
                    read_only_paths: Vec::new(),
                    network: scope.network,
                }
            }
        }
    }
}

/// What the sandbox does in this session (ADR-23-1-01 §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Landlock confines every tool command.
    Enforced {
        /// Kernel Landlock ABI.
        abi: u32,
        /// `false` only when `network = false` was requested on ABI < 4.
        network_enforced: bool,
        /// Deny-set paths under a writable root: Landlock cannot exclude them.
        unprotected: Vec<PathBuf>,
        /// Configured or default writable roots that do not exist (skipped).
        missing_roots: Vec<PathBuf>,
    },
    /// `auto` found no usable Landlock: commands run unconfined, YOLO refused.
    Unavailable { reason: String },
    /// `[sandbox].adapter = "noop"`.
    Off,
    /// No sandbox exists for this OS yet.
    NotSupported { os: String },
}

/// One rendered status line, the same words on every surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxNotice {
    pub level: NoticeLevel,
    pub text: String,
}

impl SandboxStatus {
    /// Whether commands are confined by the kernel.
    pub fn is_enforced(&self) -> bool {
        matches!(self, SandboxStatus::Enforced { .. })
    }

    /// The one notice renderer (ADR-23-1-01 §7).
    pub fn notice(&self) -> SandboxNotice {
        match self {
            SandboxStatus::Enforced {
                abi,
                network_enforced,
                unprotected,
                ..
            } => {
                let mut text = format!(
                    "Sandbox on (Landlock ABI {abi}): tool commands can write only inside the \
                     workspace, temp and cache directories."
                );
                let mut level = NoticeLevel::Info;
                if !unprotected.is_empty() {
                    level = NoticeLevel::Advisory;
                    text.push_str(&format!(
                        " Not protected from reads (inside a writable root): {}.",
                        join_paths(unprotected)
                    ));
                }
                if !network_enforced {
                    level = NoticeLevel::Advisory;
                    text.push(' ');
                    text.push_str(&network_unenforced_text(*abi));
                }
                SandboxNotice { level, text }
            }
            SandboxStatus::Unavailable { reason } => SandboxNotice {
                level: NoticeLevel::Advisory,
                text: format!(
                    "Sandbox unavailable: {reason}. Tool commands run without OS confinement \
                     and YOLO is disabled. Use a Linux kernel with Landlock ABI 3 or later \
                     (6.2+), or set [sandbox].adapter = \"noop\" to accept unconfined YOLO."
                ),
            },
            SandboxStatus::Off => SandboxNotice {
                level: NoticeLevel::Info,
                text: "Sandbox off ([sandbox].adapter = \"noop\"): tool commands run without \
                       OS confinement."
                    .to_string(),
            },
            SandboxStatus::NotSupported { os } => SandboxNotice {
                level: NoticeLevel::Info,
                text: format!(
                    "Sandbox not available on {os}: tool commands run without OS confinement."
                ),
            },
        }
    }
}

fn network_unenforced_text(abi: u32) -> String {
    format!(
        "Sandbox network restriction unavailable (Landlock ABI {abi}): tool commands keep \
         network access."
    )
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one "YOLO permitted" decision for `/mode yolo` and `ask --yolo`.
/// YOLO is refused only when `auto` found no usable Landlock: in YOLO the
/// sandbox is the only boundary left. `Err` carries the notice to show.
pub fn yolo_permitted(status: &SandboxStatus) -> Result<(), SandboxNotice> {
    match status {
        SandboxStatus::Unavailable { .. } => Err(status.notice()),
        SandboxStatus::Enforced { .. }
        | SandboxStatus::Off
        | SandboxStatus::NotSupported { .. } => Ok(()),
    }
}

/// The sandbox facts a composed session carries: the bound and its status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSession {
    pub scope: SandboxScope,
    pub status: SandboxStatus,
}

impl SandboxSession {
    /// A session whose sandbox is configured off (contexts that spawn nothing,
    /// and tests that do not exercise the sandbox).
    pub fn off() -> Self {
        Self {
            scope: SandboxScope::off(),
            status: SandboxStatus::Off,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> SandboxScope {
        SandboxScope {
            writable_roots: vec![PathBuf::from("/tmp/ws"), PathBuf::from("/tmp")],
            network: true,
            off: false,
        }
    }

    #[test]
    fn from_mode_plan_is_readonly_no_network() {
        assert_eq!(
            SandboxPolicy::from_mode(PermissionMode::Plan, &scope()),
            SandboxPolicy::ReadOnly { network: false }
        );
    }

    #[test]
    fn from_mode_normal_autoedit_and_yolo_are_workspace_write_without_read_only_paths() {
        for mode in [
            PermissionMode::Normal,
            PermissionMode::AutoEdit,
            PermissionMode::Yolo,
        ] {
            assert_eq!(
                SandboxPolicy::from_mode(mode, &scope()),
                SandboxPolicy::WorkspaceWrite {
                    writable_roots: scope().writable_roots,
                    read_only_paths: vec![],
                    network: true,
                },
                "{mode:?}"
            );
        }
    }

    #[test]
    fn from_mode_carries_the_network_setting() {
        let mut s = scope();
        s.network = false;
        match SandboxPolicy::from_mode(PermissionMode::Yolo, &s) {
            SandboxPolicy::WorkspaceWrite { network, .. } => assert!(!network),
            other => panic!("expected WorkspaceWrite, got {other:?}"),
        }
    }

    #[test]
    fn permissive_only_when_the_sandbox_is_off() {
        for mode in [
            PermissionMode::Plan,
            PermissionMode::Normal,
            PermissionMode::AutoEdit,
            PermissionMode::Yolo,
        ] {
            assert_eq!(
                SandboxPolicy::from_mode(mode, &SandboxScope::off()),
                SandboxPolicy::Permissive
            );
            assert_ne!(
                SandboxPolicy::from_mode(mode, &scope()),
                SandboxPolicy::Permissive
            );
        }
    }

    #[test]
    fn ceiling_is_workspace_write_with_network() {
        let mut s = scope();
        s.network = false;
        assert_eq!(
            s.ceiling(),
            SandboxPolicy::WorkspaceWrite {
                writable_roots: s.writable_roots.clone(),
                read_only_paths: vec![],
                network: true,
            }
        );
        assert_eq!(SandboxScope::off().ceiling(), SandboxPolicy::Permissive);
    }

    #[test]
    fn notice_texts_are_the_adr_texts() {
        assert_eq!(
            SandboxStatus::Unavailable {
                reason: "Landlock is not built into this kernel".into()
            }
            .notice(),
            SandboxNotice {
                level: NoticeLevel::Advisory,
                text: "Sandbox unavailable: Landlock is not built into this kernel. Tool commands \
                       run without OS confinement and YOLO is disabled. Use a Linux kernel with \
                       Landlock ABI 3 or later (6.2+), or set [sandbox].adapter = \"noop\" to \
                       accept unconfined YOLO."
                    .into(),
            }
        );
        assert_eq!(
            SandboxStatus::Off.notice(),
            SandboxNotice {
                level: NoticeLevel::Info,
                text: "Sandbox off ([sandbox].adapter = \"noop\"): tool commands run without OS \
                       confinement."
                    .into(),
            }
        );
        assert_eq!(
            SandboxStatus::NotSupported { os: "macos".into() }
                .notice()
                .text,
            "Sandbox not available on macos: tool commands run without OS confinement."
        );
        let enforced = SandboxStatus::Enforced {
            abi: 6,
            network_enforced: true,
            unprotected: vec![],
            missing_roots: vec![],
        }
        .notice();
        assert_eq!(enforced.level, NoticeLevel::Info);
        assert_eq!(
            enforced.text,
            "Sandbox on (Landlock ABI 6): tool commands can write only inside the workspace, \
             temp and cache directories."
        );
    }

    #[test]
    fn enforced_notice_names_network_and_unprotected_gaps() {
        let notice = SandboxStatus::Enforced {
            abi: 3,
            network_enforced: false,
            unprotected: vec![PathBuf::from("/w/secret")],
            missing_roots: vec![],
        }
        .notice();
        assert_eq!(notice.level, NoticeLevel::Advisory);
        assert!(
            notice
                .text
                .contains("Not protected from reads (inside a writable root): /w/secret."),
            "{}",
            notice.text
        );
        assert!(
            notice.text.ends_with(
                "Sandbox network restriction unavailable (Landlock ABI 3): tool commands keep \
                 network access."
            ),
            "{}",
            notice.text
        );
    }

    #[test]
    fn yolo_is_refused_only_when_the_sandbox_is_unavailable() {
        let unavailable = SandboxStatus::Unavailable { reason: "x".into() };
        assert_eq!(yolo_permitted(&unavailable), Err(unavailable.notice()));
        for status in [
            SandboxStatus::Off,
            SandboxStatus::NotSupported { os: "macos".into() },
            SandboxStatus::Enforced {
                abi: 4,
                network_enforced: true,
                unprotected: vec![],
                missing_roots: vec![],
            },
        ] {
            assert_eq!(yolo_permitted(&status), Ok(()), "{status:?}");
        }
    }
}
