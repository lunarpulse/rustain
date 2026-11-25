//! Story 23.1 — the one sandbox resolver (ADR-23-1-01 §4, §6).
//!
//! [`resolve_sandbox`] is the only place that binds a sandbox in production:
//! the TUI, `ask`, ACP and daemon roots call it with the probed host
//! ([`SandboxProbe::detect`]); tests inject the probe.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::adapters::sandbox::NoOpSandbox;
use crate::domain::models::sandbox::{SandboxPolicy, SandboxScope, SandboxSession, SandboxStatus};
use crate::domain::models::{AppConfig, PermissionMode};
use crate::domain::ports::SandboxManager;
use crate::infrastructure::utils::env_var_trimmed;

/// Host locations that feed the writable set and the deny set.
#[derive(Clone, Debug, Default)]
pub struct HostPaths {
    pub home: Option<PathBuf>,
    /// `/tmp`, `$TMPDIR`, `/dev/shm`.
    pub temp_dirs: Vec<PathBuf>,
    /// `/dev/null`, `/dev/zero`, … (file-scoped rules).
    pub device_files: Vec<PathBuf>,
    /// Toolchain caches (ADR-23-1-01 owner decision 2): `$XDG_CACHE_HOME` or
    /// `~/.cache`, cargo `registry`, `git`, `.package-cache`, and `~/.npm`.
    pub cache_dirs: Vec<PathBuf>,
    /// rustain's credential store: `data_dir()/auth.json`.
    pub credential_files: Vec<PathBuf>,
}

impl HostPaths {
    /// The running host's paths.
    pub fn detect() -> Self {
        let home = dirs::home_dir();
        let mut temp_dirs = vec![PathBuf::from("/tmp")];
        if let Some(tmp) = env_var_trimmed("TMPDIR") {
            temp_dirs.push(PathBuf::from(tmp));
        }
        temp_dirs.push(PathBuf::from("/dev/shm"));
        let device_files = [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let mut cache_dirs = Vec::new();
        if let Some(xdg) = env_var_trimmed("XDG_CACHE_HOME") {
            cache_dirs.push(PathBuf::from(xdg));
        } else if let Some(home) = &home {
            cache_dirs.push(home.join(".cache"));
        }
        let cargo_home = env_var_trimmed("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".cargo")));
        if let Some(cargo) = cargo_home {
            for sub in ["registry", "git", ".package-cache"] {
                cache_dirs.push(cargo.join(sub));
            }
        }
        if let Some(home) = &home {
            cache_dirs.push(home.join(".npm"));
        }
        let credential_files = crate::infrastructure::paths::data_dir()
            .map(|dir| vec![dir.join("auth.json")])
            .unwrap_or_default();
        Self {
            home,
            temp_dirs,
            device_files,
            cache_dirs,
            credential_files,
        }
    }
}

/// What the Landlock probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandlockProbe {
    Available { abi: u32 },
    Unavailable { reason: String },
    NotSupported { os: String },
}

impl LandlockProbe {
    /// Probe the running kernel and build.
    pub fn detect() -> Self {
        #[cfg(all(target_os = "linux", feature = "sandbox"))]
        {
            match crate::adapters::sandbox::LandlockSandbox::probe() {
                Ok(abi) => LandlockProbe::Available { abi },
                Err(reason) => LandlockProbe::Unavailable { reason },
            }
        }
        #[cfg(all(target_os = "linux", not(feature = "sandbox")))]
        {
            LandlockProbe::Unavailable {
                reason: NOT_BUILT.to_string(),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            LandlockProbe::NotSupported {
                os: std::env::consts::OS.to_string(),
            }
        }
    }
}

const NOT_BUILT: &str = "this build does not include the `sandbox` feature";

/// The probe seam of [`resolve_sandbox`] (pattern: `ChildHandle::open_with`).
#[derive(Clone, Debug)]
pub struct SandboxProbe {
    pub landlock: LandlockProbe,
    pub host: HostPaths,
    /// Test seam: the Landlock `pre_exec` check sees a not-enforced status.
    #[doc(hidden)]
    pub inject_not_enforced: bool,
}

impl SandboxProbe {
    /// The running host.
    pub fn detect() -> Self {
        Self {
            landlock: LandlockProbe::detect(),
            host: HostPaths::detect(),
            inject_not_enforced: false,
        }
    }
}

/// A bound sandbox: the manager, the policy for the requested mode, and the
/// session facts (scope and status).
#[derive(Clone)]
pub struct ResolvedSandbox {
    pub manager: Arc<dyn SandboxManager>,
    pub policy: SandboxPolicy,
    pub session: SandboxSession,
}

impl std::fmt::Debug for ResolvedSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSandbox")
            .field("manager", &self.manager.kind())
            .field("policy", &self.policy)
            .field("session", &self.session)
            .finish()
    }
}

impl ResolvedSandbox {
    /// The sandbox configured off: NoOp, `Permissive`.
    pub fn off() -> Self {
        Self {
            manager: Arc::new(NoOpSandbox),
            policy: SandboxPolicy::Permissive,
            session: SandboxSession::off(),
        }
    }

    /// A fresh manager slot holding this manager.
    pub fn slot(&self) -> Arc<arc_swap::ArcSwap<Arc<dyn SandboxManager>>> {
        Arc::new(arc_swap::ArcSwap::from_pointee(Arc::clone(&self.manager)))
    }

    /// A fresh shared policy cell holding this policy.
    pub fn policy_cell(&self) -> Arc<tokio::sync::RwLock<SandboxPolicy>> {
        Arc::new(tokio::sync::RwLock::new(self.policy.clone()))
    }
}

/// A `[sandbox]` setting no session can start with.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxResolveError {
    #[error(
        "[sandbox].adapter = \"landlock\" requires Landlock, but {reason}. Use a Linux kernel \
         with Landlock ABI 3 or later (6.2+), or set [sandbox].adapter = \"auto\" or \"noop\"."
    )]
    LandlockRequired { reason: String },
    #[error("{0}")]
    InvalidAdapter(String),
}

/// Resolve `[sandbox]` for `workspace` and `mode` against the probed host
/// (ADR-23-1-01 §6). `"noop"` → off; `"auto"` → Landlock when available,
/// else unconfined with status `Unavailable`/`NotSupported`; `"landlock"` →
/// Landlock or [`SandboxResolveError::LandlockRequired`].
pub fn resolve_sandbox(
    app_config: &AppConfig,
    workspace: &Path,
    mode: PermissionMode,
    probe: &SandboxProbe,
) -> Result<ResolvedSandbox, SandboxResolveError> {
    let config = &app_config.sandbox;
    let required = match config.adapter.as_str() {
        "noop" => return Ok(ResolvedSandbox::off()),
        "auto" => false,
        "landlock" => true,
        other => {
            return Err(SandboxResolveError::InvalidAdapter(
                crate::infrastructure::startup::validate_sandbox_adapter(other)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| format!("unknown sandbox adapter `{other}`")),
            ));
        }
    };
    let host = &probe.host;
    let (writable_roots, missing_roots) = writable_set(config, workspace, host);
    let scope = SandboxScope {
        writable_roots,
        network: config.network,
        off: false,
    };

    let unavailable =
        |reason: String| -> Result<(Arc<dyn SandboxManager>, SandboxStatus), SandboxResolveError> {
            if required {
                Err(SandboxResolveError::LandlockRequired { reason })
            } else {
                Ok((
                    Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>,
                    SandboxStatus::Unavailable { reason },
                ))
            }
        };
    let (manager, status) = match &probe.landlock {
        LandlockProbe::Available { abi } => {
            let (protected, unprotected) = deny_set(config, workspace, host, &scope.writable_roots);
            match landlock_manager(*abi, protected, probe.inject_not_enforced) {
                Some(manager) => (
                    manager,
                    SandboxStatus::Enforced {
                        abi: *abi,
                        network_enforced: config.network || *abi >= 4,
                        unprotected,
                        missing_roots,
                    },
                ),
                None => unavailable(NOT_BUILT.to_string())?,
            }
        }
        LandlockProbe::Unavailable { reason } => unavailable(reason.clone())?,
        LandlockProbe::NotSupported { os } => {
            if required {
                return Err(SandboxResolveError::LandlockRequired {
                    reason: format!("Landlock does not exist on {os}"),
                });
            }
            (
                Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>,
                SandboxStatus::NotSupported { os: os.clone() },
            )
        }
    };
    let policy = SandboxPolicy::from_mode(mode, &scope);
    Ok(ResolvedSandbox {
        manager,
        policy,
        session: SandboxSession { scope, status },
    })
}

#[cfg(all(target_os = "linux", feature = "sandbox"))]
fn landlock_manager(
    abi: u32,
    deny_read: Vec<PathBuf>,
    inject_not_enforced: bool,
) -> Option<Arc<dyn SandboxManager>> {
    Some(Arc::new(
        crate::adapters::sandbox::LandlockSandbox::new(abi, deny_read)
            .with_injected_not_enforced(inject_not_enforced),
    ))
}

#[cfg(not(all(target_os = "linux", feature = "sandbox")))]
fn landlock_manager(
    _abi: u32,
    _deny_read: Vec<PathBuf>,
    _inject_not_enforced: bool,
) -> Option<Arc<dyn SandboxManager>> {
    None
}

/// `~/x` against home, a relative path against the workspace.
fn expand(path: &Path, workspace: &Path, home: Option<&Path>) -> PathBuf {
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), home) {
        return home.join(rest);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// `W` (ADR-23-1-01 §4): canonical existing roots, and the missing ones.
fn writable_set(
    config: &crate::domain::models::SandboxConfig,
    workspace: &Path,
    host: &HostPaths,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let candidates = std::iter::once(workspace.to_path_buf())
        .chain(host.temp_dirs.iter().cloned())
        .chain(host.device_files.iter().cloned())
        .chain(host.cache_dirs.iter().cloned())
        .chain(
            config
                .writable_roots
                .iter()
                .map(|p| expand(p, workspace, host.home.as_deref())),
        );
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut missing = Vec::new();
    for candidate in candidates {
        match std::fs::canonicalize(&candidate) {
            Ok(root) => {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            Err(_) => missing.push(candidate),
        }
    }
    (roots, missing)
}

/// The deny set, canonical: (protected, unprotected). A denied path under a
/// writable root cannot be carved out (Landlock grants are a union).
fn deny_set(
    config: &crate::domain::models::SandboxConfig,
    workspace: &Path,
    host: &HostPaths,
    writable_roots: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut protected = Vec::new();
    let mut unprotected = Vec::new();
    let paths = host.credential_files.iter().cloned().chain(
        config
            .deny_read
            .iter()
            .map(|p| expand(p, workspace, host.home.as_deref())),
    );
    for path in paths {
        let path = canonical_or_lexical(&path);
        let bucket = if writable_roots.iter().any(|root| path.starts_with(root)) {
            &mut unprotected
        } else {
            &mut protected
        };
        if !bucket.contains(&path) {
            bucket.push(path);
        }
    }
    (protected, unprotected)
}

/// Canonical when it exists; else the canonical parent joined with the name.
fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// The TUI `SetPermissionMode` arm's sandbox half (ADR-23-1-01 §6): YOLO is
/// refused while the sandbox is unavailable — the mode stays and the notice is
/// shown. Otherwise the mode is set and the Bash policy re-derived. Returns
/// whether the mode changed.
pub(crate) async fn apply_permission_mode(
    mode: PermissionMode,
    security: &dyn crate::domain::ports::SecurityPort,
    sandbox: &SandboxSession,
    sandbox_policy: &tokio::sync::RwLock<SandboxPolicy>,
    event_bus: &crate::infrastructure::runtime::event_bus::EventBus,
) -> bool {
    if mode == PermissionMode::Yolo
        && let Err(notice) = crate::domain::models::sandbox::yolo_permitted(&sandbox.status)
    {
        let _ = event_bus.emit_domain(crate::domain::events::AppEvent::SystemNotice {
            conversation_id: None,
            level: notice.level,
            message: notice.text,
        });
        return false;
    }
    security.set_mode(mode);
    let new_policy = SandboxPolicy::from_mode(mode, &sandbox.scope);
    tracing::info!(
        target: "rustain::sandbox",
        mode = ?mode,
        policy = ?new_policy,
        "Sandbox policy updated on permission mode change"
    );
    *sandbox_policy.write().await = new_policy;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(root: &Path) -> HostPaths {
        let home = root.join("home");
        let data = home.join(".rustain");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(home.join(".cache")).unwrap();
        HostPaths {
            home: Some(home.clone()),
            temp_dirs: vec![root.join("tmp")],
            device_files: vec![PathBuf::from("/dev/null")],
            cache_dirs: vec![home.join(".cache"), home.join(".npm")],
            credential_files: vec![data.join("auth.json")],
        }
    }

    fn config(adapter: &str) -> AppConfig {
        let mut config = AppConfig::default();
        config.sandbox.adapter = adapter.to_string();
        config
    }

    fn probe(root: &Path, landlock: LandlockProbe) -> SandboxProbe {
        SandboxProbe {
            landlock,
            host: host(root),
            inject_not_enforced: false,
        }
    }

    #[test]
    fn noop_is_off_and_permissive() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve_sandbox(
            &config("noop"),
            dir.path(),
            PermissionMode::Yolo,
            &probe(dir.path(), LandlockProbe::Available { abi: 4 }),
        )
        .unwrap();
        assert_eq!(r.policy, SandboxPolicy::Permissive);
        assert_eq!(r.session, SandboxSession::off());
        assert_eq!(
            r.manager.kind(),
            crate::adapters::sandbox::SandboxAdapterKind::NoOp
        );
    }

    #[test]
    fn writable_set_holds_workspace_temp_devices_caches_and_configured_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        std::fs::create_dir_all(dir.path().join("extra")).unwrap();
        let mut cfg = config("auto");
        cfg.sandbox.writable_roots = vec![PathBuf::from("../extra"), PathBuf::from("~/missing")];
        let p = probe(
            dir.path(),
            LandlockProbe::Unavailable { reason: "x".into() },
        );
        let (roots, missing) = writable_set(&cfg.sandbox, &ws, &p.host);
        let canon = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        assert_eq!(
            roots,
            vec![
                canon(ws.clone()),
                canon(dir.path().join("tmp")),
                PathBuf::from("/dev/null"),
                canon(dir.path().join("home/.cache")),
                canon(dir.path().join("extra")),
            ]
        );
        assert_eq!(
            missing,
            vec![
                dir.path().join("home/.npm"),
                dir.path().join("home/missing")
            ]
        );
    }

    #[test]
    fn auto_without_landlock_is_unavailable_and_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve_sandbox(
            &config("auto"),
            dir.path(),
            PermissionMode::Yolo,
            &probe(
                dir.path(),
                LandlockProbe::Unavailable {
                    reason: "Landlock is not built into this kernel".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(
            r.session.status,
            SandboxStatus::Unavailable {
                reason: "Landlock is not built into this kernel".into()
            }
        );
        assert!(matches!(r.policy, SandboxPolicy::WorkspaceWrite { .. }));
        assert_eq!(
            r.manager.kind(),
            crate::adapters::sandbox::SandboxAdapterKind::NoOp
        );
    }

    #[test]
    fn required_landlock_without_landlock_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        for landlock in [
            LandlockProbe::Unavailable {
                reason: "Landlock is disabled at boot".into(),
            },
            LandlockProbe::NotSupported { os: "macos".into() },
        ] {
            let err = resolve_sandbox(
                &config("landlock"),
                dir.path(),
                PermissionMode::Normal,
                &probe(dir.path(), landlock),
            )
            .unwrap_err();
            assert!(
                matches!(err, SandboxResolveError::LandlockRequired { .. }),
                "{err}"
            );
            assert!(
                err.to_string()
                    .starts_with("[sandbox].adapter = \"landlock\" requires Landlock, but ")
            );
        }
    }

    #[test]
    fn auto_on_another_os_is_not_supported() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve_sandbox(
            &config("auto"),
            dir.path(),
            PermissionMode::Yolo,
            &probe(
                dir.path(),
                LandlockProbe::NotSupported { os: "macos".into() },
            ),
        )
        .unwrap();
        assert_eq!(
            r.session.status,
            SandboxStatus::NotSupported { os: "macos".into() }
        );
    }

    #[test]
    fn unknown_adapter_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_sandbox(
            &config("selinux"),
            dir.path(),
            PermissionMode::Normal,
            &probe(dir.path(), LandlockProbe::Available { abi: 4 }),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown sandbox adapter `selinux`")
        );
    }

    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    #[test]
    fn enforced_status_splits_the_deny_set_by_writable_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut cfg = config("auto");
        cfg.sandbox.deny_read = vec![PathBuf::from("secret.txt"), PathBuf::from("~/.ssh")];
        cfg.sandbox.network = false;
        let p = probe(dir.path(), LandlockProbe::Available { abi: 3 });
        let r = resolve_sandbox(&cfg, &ws, PermissionMode::Normal, &p).unwrap();
        match r.session.status {
            SandboxStatus::Enforced {
                abi,
                network_enforced,
                unprotected,
                ..
            } => {
                assert_eq!(abi, 3);
                assert!(!network_enforced, "network=false on ABI 3 is not enforced");
                assert_eq!(
                    unprotected,
                    vec![std::fs::canonicalize(&ws).unwrap().join("secret.txt")]
                );
            }
            other => panic!("expected Enforced, got {other:?}"),
        }
        let (protected, _) = deny_set(&cfg.sandbox, &ws, &p.host, &r.session.scope.writable_roots);
        let home = std::fs::canonicalize(dir.path().join("home")).unwrap();
        assert_eq!(
            protected,
            vec![home.join(".rustain/auth.json"), home.join(".ssh")]
        );
    }
}
