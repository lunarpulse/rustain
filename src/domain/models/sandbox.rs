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
        /// Paths the deny set names that tool commands can still read: denied
        /// paths inside a writable root, and writable roots inside a denied
        /// directory (Landlock grants are a union).
        unprotected: Vec<PathBuf>,
        /// Configured or default writable roots that do not exist (skipped).
        missing_roots: Vec<PathBuf>,
        /// Signals and abstract Unix sockets are scoped to the sandbox
        /// (Landlock ABI 6+).
        ipc_scoped: bool,
        /// Writable roots that are `/`, the home directory or an ancestor of
        /// it: no write bound worth the name, so YOLO is refused.
        broad_roots: Vec<PathBuf>,
        /// rustain's own credential store is readable by tool commands.
        credentials_unprotected: bool,
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

    /// The status part of the notice (ADR-23-1-01 §7); the session adds the
    /// configuration facts (see [`SandboxSession::notice`]).
    pub fn notice(&self) -> SandboxNotice {
        match self {
            SandboxStatus::Enforced {
                abi,
                network_enforced,
                unprotected,
                ipc_scoped,
                broad_roots,
                credentials_unprotected,
                ..
            } => {
                let mut text = format!(
                    "Sandbox on (Landlock ABI {abi}): tool commands can write only inside the \
                     workspace, temp and cache directories."
                );
                // Enforced is info level (ADR-23-1-01 §7) unless the bound has
                // a hole the user must know about.
                let mut level = NoticeLevel::Info;
                if !broad_roots.is_empty() {
                    level = NoticeLevel::Advisory;
                    text.push_str(&format!(
                        " The writable root {} covers your home directory, so writes are not \
                         bounded and YOLO is disabled.",
                        join_paths(broad_roots)
                    ));
                }
                if *credentials_unprotected {
                    level = NoticeLevel::Advisory;
                    text.push_str(
                        " rustain's credential store lies inside a writable root and is \
                         readable by tool commands.",
                    );
                }
                if !unprotected.is_empty() {
                    text.push_str(&format!(
                        " Readable despite [sandbox].deny_read: {}.",
                        join_paths(unprotected)
                    ));
                }
                if *abi < 4 {
                    if !network_enforced {
                        level = NoticeLevel::Advisory;
                    }
                    text.push(' ');
                    text.push_str(&network_unenforced_text(*abi));
                }
                if *ipc_scoped {
                    text.push_str(
                        " Signals and abstract Unix sockets cannot reach processes outside it;",
                    );
                } else {
                    text.push_str(&format!(
                        " Landlock ABI {abi} cannot scope signals or abstract Unix sockets;"
                    ));
                }
                text.push_str(
                    " services reachable over Unix socket files (D-Bus, tmux, Docker, X11) act \
                     outside the sandbox.",
                );
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
        "Landlock ABI {abi} cannot restrict the network: tool commands keep network access, \
         in Plan mode too."
    )
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one "YOLO permitted" decision for `/mode yolo` and `ask --yolo`. YOLO
/// is refused when `auto` found no usable Landlock, and when a writable root
/// covers the home directory: in YOLO the sandbox is the only boundary left.
/// `Err` carries the notice to show.
pub fn yolo_permitted(session: &SandboxSession) -> Result<(), SandboxNotice> {
    match &session.status {
        SandboxStatus::Unavailable { .. } => Err(session.notice()),
        SandboxStatus::Enforced { broad_roots, .. } if !broad_roots.is_empty() => {
            Err(session.notice())
        }
        SandboxStatus::Enforced { .. }
        | SandboxStatus::Off
        | SandboxStatus::NotSupported { .. } => Ok(()),
    }
}

/// Where the effective `[sandbox]` settings came from (Story 23.1 review H1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxProvenance {
    /// The layer that set `[sandbox].adapter` (a file path, "environment",
    /// "command line" or "built-in default").
    pub adapter_source: String,
    /// Weakening settings a workspace layer tried and that were ignored, one
    /// sentence each, naming the file and key.
    pub ignored: Vec<String>,
}

/// The sandbox facts a composed session carries: the bound, its status and
/// where the configuration came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSession {
    pub scope: SandboxScope,
    pub status: SandboxStatus,
    /// `[sandbox].adapter` as resolved.
    pub adapter: String,
    /// `[sandbox].writable_roots` as configured (expanded, before canonical
    /// resolution).
    pub configured_roots: Vec<PathBuf>,
    pub provenance: SandboxProvenance,
}

impl SandboxSession {
    /// A session whose sandbox is configured off (contexts that spawn nothing,
    /// and tests that do not exercise the sandbox).
    pub fn off() -> Self {
        Self {
            scope: SandboxScope::off(),
            status: SandboxStatus::Off,
            adapter: "noop".to_string(),
            configured_roots: Vec::new(),
            provenance: SandboxProvenance::default(),
        }
    }

    /// The one notice renderer (ADR-23-1-01 §7): the status, then the extra
    /// writable roots, the layer that chose the adapter, and every ignored
    /// workspace attempt to weaken the sandbox (warning level).
    pub fn notice(&self) -> SandboxNotice {
        let mut notice = self.status.notice();
        if !self.configured_roots.is_empty() {
            notice.text.push_str(&format!(
                " Extra writable roots ([sandbox].writable_roots): {}.",
                join_paths(&self.configured_roots)
            ));
        }
        if !self.provenance.adapter_source.is_empty() {
            notice.text.push_str(&format!(
                " Adapter \"{}\" set by {}.",
                self.adapter, self.provenance.adapter_source
            ));
        }
        for ignored in &self.provenance.ignored {
            notice.level = NoticeLevel::Advisory;
            notice.text.push(' ');
            notice.text.push_str(ignored);
        }
        notice
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
        let enforced = enforced(6).notice();
        assert_eq!(enforced.level, NoticeLevel::Info);
        assert_eq!(
            enforced.text,
            "Sandbox on (Landlock ABI 6): tool commands can write only inside the workspace, \
             temp and cache directories. Signals and abstract Unix sockets cannot reach \
             processes outside it; services reachable over Unix socket files (D-Bus, tmux, \
             Docker, X11) act outside the sandbox."
        );
    }

    fn enforced(abi: u32) -> SandboxStatus {
        SandboxStatus::Enforced {
            abi,
            network_enforced: true,
            unprotected: vec![],
            missing_roots: vec![],
            ipc_scoped: abi >= 6,
            broad_roots: vec![],
            credentials_unprotected: false,
        }
    }

    fn session(status: SandboxStatus) -> SandboxSession {
        SandboxSession {
            status,
            ..SandboxSession::off()
        }
    }

    #[test]
    fn enforced_notice_names_network_and_unprotected_gaps() {
        let mut gaps = enforced(3);
        if let SandboxStatus::Enforced {
            network_enforced,
            unprotected,
            ..
        } = &mut gaps
        {
            *network_enforced = false;
            *unprotected = vec![PathBuf::from("/w/secret")];
        }
        let notice = gaps.notice();
        assert_eq!(notice.level, NoticeLevel::Advisory);
        assert!(
            notice
                .text
                .contains("Readable despite [sandbox].deny_read: /w/secret."),
            "{}",
            notice.text
        );
        assert!(
            notice.text.contains(
                "Landlock ABI 3 cannot restrict the network: tool commands keep network access, \
                 in Plan mode too."
            ),
            "{}",
            notice.text
        );
        assert!(
            notice
                .text
                .contains("Landlock ABI 3 cannot scope signals or abstract Unix sockets;"),
            "{}",
            notice.text
        );
        // L3: on ABI 3 the Plan-mode network claim is never made, even with
        // `network = true` (info level then).
        let plan = enforced(3).notice();
        assert_eq!(plan.level, NoticeLevel::Info);
        assert!(plan.text.contains("in Plan mode too"), "{}", plan.text);
        // Unprotected paths alone stay info level and are listed.
        let mut listed = enforced(6);
        if let SandboxStatus::Enforced { unprotected, .. } = &mut listed {
            *unprotected = vec![PathBuf::from("/w/secret")];
        }
        let listed = listed.notice();
        assert_eq!(listed.level, NoticeLevel::Info);
        assert!(listed.text.contains("/w/secret"), "{}", listed.text);
    }

    #[test]
    fn broad_roots_and_exposed_credentials_warn() {
        let mut broad = enforced(6);
        if let SandboxStatus::Enforced { broad_roots, .. } = &mut broad {
            *broad_roots = vec![PathBuf::from("/home/u")];
        }
        let notice = broad.notice();
        assert_eq!(notice.level, NoticeLevel::Advisory);
        assert!(
            notice.text.contains(
                "The writable root /home/u covers your home directory, so writes are not \
                 bounded and YOLO is disabled."
            ),
            "{}",
            notice.text
        );
        let mut exposed = enforced(6);
        if let SandboxStatus::Enforced {
            credentials_unprotected,
            ..
        } = &mut exposed
        {
            *credentials_unprotected = true;
        }
        assert_eq!(exposed.notice().level, NoticeLevel::Advisory);
    }

    #[test]
    fn yolo_is_refused_when_unavailable_or_unbounded() {
        let unavailable = session(SandboxStatus::Unavailable { reason: "x".into() });
        assert_eq!(yolo_permitted(&unavailable), Err(unavailable.notice()));
        let mut broad = enforced(6);
        if let SandboxStatus::Enforced { broad_roots, .. } = &mut broad {
            *broad_roots = vec![PathBuf::from("/")];
        }
        let broad = session(broad);
        assert_eq!(yolo_permitted(&broad), Err(broad.notice()));
        for status in [
            SandboxStatus::Off,
            SandboxStatus::NotSupported { os: "macos".into() },
            enforced(4),
        ] {
            assert_eq!(
                yolo_permitted(&session(status.clone())),
                Ok(()),
                "{status:?}"
            );
        }
    }

    #[test]
    fn session_notice_lists_roots_adapter_source_and_ignored_weakening() {
        let mut s = session(enforced(6));
        s.adapter = "auto".into();
        s.configured_roots = vec![PathBuf::from("/opt/tool")];
        s.provenance = SandboxProvenance {
            adapter_source: "built-in default".into(),
            ignored: vec!["Ignored `[sandbox].adapter` in /w/.rustain/config.toml.".into()],
        };
        let notice = s.notice();
        assert_eq!(notice.level, NoticeLevel::Advisory);
        assert!(
            notice
                .text
                .contains(" Extra writable roots ([sandbox].writable_roots): /opt/tool."),
            "{}",
            notice.text
        );
        assert!(
            notice
                .text
                .contains(" Adapter \"auto\" set by built-in default."),
            "{}",
            notice.text
        );
        assert!(
            notice
                .text
                .ends_with("Ignored `[sandbox].adapter` in /w/.rustain/config.toml."),
            "{}",
            notice.text
        );
    }
}
