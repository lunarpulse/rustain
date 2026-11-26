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
        // No `/dev/tty`: commands run in their own session without a
        // controlling terminal (review M1).
        let device_files = [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
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
        // The store and the temporary file it is written through before the
        // rename (review L2).
        let credential_files = crate::infrastructure::paths::data_dir()
            .map(|dir| vec![dir.join("auth.json"), dir.join("auth.json.tmp")])
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
    let host = &probe.host;
    let configured_roots: Vec<PathBuf> = config
        .writable_roots
        .iter()
        .map(|p| expand(p, workspace, host.home.as_deref()))
        .collect();
    let session = |scope: SandboxScope, status: SandboxStatus| SandboxSession {
        scope,
        status,
        adapter: config.adapter.clone(),
        configured_roots: configured_roots.clone(),
        provenance: config.provenance.clone(),
    };
    let required = match config.adapter.as_str() {
        "noop" => {
            return Ok(ResolvedSandbox {
                session: session(SandboxScope::off(), SandboxStatus::Off),
                ..ResolvedSandbox::off()
            });
        }
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
            let (protected, unprotected, credentials_unprotected) =
                deny_set(config, workspace, host, &scope.writable_roots);
            match landlock_manager(
                *abi,
                protected,
                &scope.writable_roots,
                probe.inject_not_enforced,
            ) {
                Some(Ok(manager)) => (
                    manager,
                    SandboxStatus::Enforced {
                        abi: *abi,
                        network_enforced: config.network || *abi >= 4,
                        unprotected,
                        missing_roots,
                        ipc_scoped: *abi >= 6,
                        broad_roots: broad_roots(&scope.writable_roots, host.home.as_deref()),
                        credentials_unprotected,
                    },
                ),
                Some(Err(reason)) => unavailable(reason)?,
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
        session: session(scope, status),
    })
}

/// The Landlock manager, holding a descriptor of every writable root opened
/// now (`O_PATH|O_NOFOLLOW`): later spawns reuse these inodes, so a confined
/// command that swaps a root for a symlink widens nothing (review H2).
#[cfg(all(target_os = "linux", feature = "sandbox"))]
fn landlock_manager(
    abi: u32,
    deny_read: Vec<PathBuf>,
    writable_roots: &[PathBuf],
    inject_not_enforced: bool,
) -> Option<Result<Arc<dyn SandboxManager>, String>> {
    Some(
        crate::adapters::sandbox::LandlockSandbox::new(abi, deny_read, writable_roots)
            .map(|sb| {
                Arc::new(sb.with_injected_not_enforced(inject_not_enforced))
                    as Arc<dyn SandboxManager>
            })
            .map_err(|e| e.to_string()),
    )
}

#[cfg(not(all(target_os = "linux", feature = "sandbox")))]
fn landlock_manager(
    _abi: u32,
    _deny_read: Vec<PathBuf>,
    _writable_roots: &[PathBuf],
    _inject_not_enforced: bool,
) -> Option<Result<Arc<dyn SandboxManager>, String>> {
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

/// The deny set, canonical: (protected, unprotected, credentials exposed). A
/// denied path under a writable root cannot be carved out (Landlock grants are
/// a union), and a writable root inside a denied directory stays readable;
/// both are reported as unprotected while the rest of the directory stays
/// denied.
fn deny_set(
    config: &crate::domain::models::SandboxConfig,
    workspace: &Path,
    host: &HostPaths,
    writable_roots: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
    let mut protected: Vec<PathBuf> = Vec::new();
    let mut unprotected: Vec<PathBuf> = Vec::new();
    let mut credentials_exposed = false;
    let credentials = host.credential_files.iter().map(|p| (p.clone(), true));
    let configured = config
        .deny_read
        .iter()
        .map(|p| (expand(p, workspace, host.home.as_deref()), false));
    for (path, credential) in credentials.chain(configured) {
        let path = canonical_or_lexical(&path);
        if writable_roots.iter().any(|root| path.starts_with(root)) {
            credentials_exposed |= credential;
            if !unprotected.contains(&path) {
                unprotected.push(path);
            }
            continue;
        }
        for root in writable_roots {
            if root.starts_with(&path) && !unprotected.contains(root) {
                unprotected.push(root.clone());
            }
        }
        if !protected.contains(&path) {
            protected.push(path);
        }
    }
    (protected, unprotected, credentials_exposed)
}

/// Writable roots that bound nothing: `/`, the home directory or an ancestor
/// of it (Story 23.1 review M3).
fn broad_roots(writable_roots: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let home = home.map(canonical_or_lexical);
    writable_roots
        .iter()
        .filter(|root| {
            root.as_path() == Path::new("/") || home.as_ref().is_some_and(|h| h.starts_with(root))
        })
        .cloned()
        .collect()
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

/// `rustain doctor`'s sandbox row and the sandbox its MCP probe spawns under,
/// from the same resolver the sessions use. When `[sandbox]` refuses the
/// session (a required Landlock is missing) the probe gets `Err` and starts
/// no server (Story 23.1 review CR231-01).
pub fn doctor_sandbox_check(
    app_config: &AppConfig,
    workspace: &Path,
    probe: &SandboxProbe,
) -> crate::adapters::cli::doctor::SandboxCheck {
    let resolved = resolve_sandbox(app_config, workspace, PermissionMode::Normal, probe);
    crate::adapters::cli::doctor::SandboxCheck {
        adapter: app_config.sandbox.adapter.clone(),
        mcp: match &resolved {
            Ok(r) => Ok(Some((Arc::clone(&r.manager), r.session.scope.ceiling()))),
            Err(e) => Err(e.to_string()),
        },
        outcome: resolved.map(|r| r.session).map_err(|e| e.to_string()),
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
        && let Err(notice) = crate::domain::models::sandbox::yolo_permitted(sandbox)
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
        let (protected, _, exposed) =
            deny_set(&cfg.sandbox, &ws, &p.host, &r.session.scope.writable_roots);
        let home = std::fs::canonicalize(dir.path().join("home")).unwrap();
        assert_eq!(
            protected,
            vec![home.join(".rustain/auth.json"), home.join(".ssh")]
        );
        assert!(!exposed, "the credential store lies outside W");
    }

    /// CR231-06 / L1: a denied directory that contains a writable root stays
    /// denied, and the writable subtree inside it is reported as readable.
    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    #[test]
    fn a_denied_directory_containing_a_writable_root_reports_the_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let ws = home.join("project");
        std::fs::create_dir_all(&ws).unwrap();
        let mut cfg = config("auto");
        cfg.sandbox.deny_read = vec![PathBuf::from("~")];
        let p = probe(dir.path(), LandlockProbe::Available { abi: 6 });
        let r = resolve_sandbox(&cfg, &ws, PermissionMode::Normal, &p).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let (protected, unprotected, _) =
            deny_set(&cfg.sandbox, &ws, &p.host, &r.session.scope.writable_roots);
        assert!(protected.contains(&canon(&home)), "{protected:?}");
        match r.session.status {
            SandboxStatus::Enforced { unprotected, .. } => {
                assert!(unprotected.contains(&canon(&ws)), "{unprotected:?}");
                assert!(
                    unprotected.contains(&canon(&home.join(".cache"))),
                    "{unprotected:?}"
                );
            }
            other => panic!("expected Enforced, got {other:?}"),
        }
        assert!(unprotected.contains(&canon(&ws)));
    }

    /// M3: a workspace that is the home directory bounds nothing: warning and
    /// YOLO refused; the credential store inside it is reported readable.
    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    #[test]
    fn a_home_or_root_workspace_is_broad_and_refuses_yolo() {
        let dir = tempfile::tempdir().unwrap();
        let p = probe(dir.path(), LandlockProbe::Available { abi: 6 });
        let home = dir.path().join("home");
        for workspace in [home.clone(), dir.path().to_path_buf()] {
            let r =
                resolve_sandbox(&config("auto"), &workspace, PermissionMode::Normal, &p).unwrap();
            match &r.session.status {
                SandboxStatus::Enforced {
                    broad_roots,
                    credentials_unprotected,
                    ..
                } => {
                    assert_eq!(
                        broad_roots,
                        &vec![std::fs::canonicalize(&workspace).unwrap()]
                    );
                    assert!(*credentials_unprotected);
                }
                other => panic!("expected Enforced, got {other:?}"),
            }
            assert!(crate::domain::models::yolo_permitted(&r.session).is_err());
            assert_eq!(
                r.session.notice().level,
                crate::domain::models::NoticeLevel::Advisory
            );
        }
        // A workspace beside home is bounded.
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let r = resolve_sandbox(&config("auto"), &ws, PermissionMode::Normal, &p).unwrap();
        assert!(crate::domain::models::yolo_permitted(&r.session).is_ok());
    }

    fn unavailable_probe(root: &Path) -> SandboxProbe {
        probe(
            root,
            LandlockProbe::Unavailable {
                reason: "Landlock is not built into this kernel".into(),
            },
        )
    }

    /// K7 (AC7), `ask`: the YOLO composition refuses an unavailable sandbox
    /// with the notice; with the sandbox configured off, YOLO composes.
    #[test]
    fn k7_ask_yolo_composition_is_refused_while_the_sandbox_is_unavailable() {
        use crate::domain::errors::AdapterCompositionError;
        use crate::infrastructure::composition::build_cli_core_with_sandbox;
        let dir = tempfile::tempdir().unwrap();
        let p = unavailable_probe(dir.path());
        let persona = || Arc::new(crate::adapters::noop::NoOpPersona);
        let refused = build_cli_core_with_sandbox(&config("auto"), dir.path(), true, persona(), &p);
        match refused {
            Err(AdapterCompositionError::Sandbox(text)) => assert_eq!(
                text,
                SandboxStatus::Unavailable {
                    reason: "Landlock is not built into this kernel".into()
                }
                .notice()
                .text
            ),
            Err(other) => panic!("expected the sandbox refusal, got {other}"),
            Ok(_) => panic!("YOLO must be refused while the sandbox is unavailable"),
        }
        // Normal mode runs unconfined (fail-open exception), YOLO with noop composes.
        assert!(
            build_cli_core_with_sandbox(&config("auto"), dir.path(), false, persona(), &p).is_ok()
        );
        let (_, session) =
            build_cli_core_with_sandbox(&config("noop"), dir.path(), true, persona(), &p)
                .expect("noop YOLO composes");
        assert_eq!(session.status, SandboxStatus::Off);
    }

    /// K7 (AC7), TUI: the `SetPermissionMode` arm's handler keeps the mode and
    /// emits the notice; with the sandbox off, YOLO is applied.
    #[tokio::test]
    async fn k7_tui_mode_arm_keeps_the_mode_and_shows_the_notice() {
        use crate::domain::ports::SecurityPort;
        let dir = tempfile::tempdir().unwrap();
        let p = unavailable_probe(dir.path());
        let resolved =
            resolve_sandbox(&config("auto"), dir.path(), PermissionMode::Normal, &p).unwrap();
        let security = crate::adapters::security_adapter::SecurityAdapter::new(dir.path().into());
        let (bus, mut rx) = crate::infrastructure::runtime::event_bus::EventBus::new(8);
        let cell = resolved.policy_cell();
        let applied = apply_permission_mode(
            PermissionMode::Yolo,
            &security,
            &resolved.session,
            &cell,
            &bus,
        )
        .await;
        assert!(!applied);
        assert_eq!(security.current_mode(), PermissionMode::Normal);
        assert_eq!(*cell.read().await, resolved.policy);
        match rx.try_recv() {
            Ok(crate::domain::events::AppEvent::SystemNotice { message, .. }) => {
                assert_eq!(message, resolved.session.notice().text)
            }
            other => panic!("expected the sandbox notice, got {other:?}"),
        }

        let off = resolve_sandbox(&config("noop"), dir.path(), PermissionMode::Normal, &p).unwrap();
        let applied =
            apply_permission_mode(PermissionMode::Yolo, &security, &off.session, &cell, &bus).await;
        assert!(applied);
        assert_eq!(security.current_mode(), PermissionMode::Yolo);
        assert!(rx.try_recv().is_err(), "no notice when YOLO is allowed");
    }

    /// K9 (AC6): tools composed from `daemon_compose_context` confine Bash.
    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    #[tokio::test]
    async fn k9_daemon_tools_confine_bash() {
        use crate::adapters::sandbox::landlock::{MIN_ABI, require_landlock};
        if !require_landlock("k9_daemon_tools_confine_bash", MIN_ABI) {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // W = the workspace and /dev/null only, so `outside` is not in it.
        let p = SandboxProbe {
            landlock: LandlockProbe::detect(),
            host: HostPaths {
                home: None,
                temp_dirs: vec![],
                device_files: vec![PathBuf::from("/dev/null")],
                cache_dirs: vec![],
                credential_files: vec![],
            },
            inject_not_enforced: false,
        };
        let resolved =
            resolve_sandbox(&config("auto"), ws.path(), PermissionMode::Normal, &p).unwrap();
        let out = std::fs::canonicalize(outside.path()).unwrap();
        assert!(
            !resolved
                .session
                .scope
                .writable_roots
                .iter()
                .any(|r| out.starts_with(r)),
            "precondition: the outside dir is not under W"
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = crate::infrastructure::composition::daemon_compose_context(
            ws.path(),
            ws.path(),
            Arc::new(crate::adapters::noop::NoOpStorage),
            tx,
            "passthrough".into(),
            None,
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Vec::new(),
            true,
            &resolved,
        );
        let tools =
            crate::infrastructure::composition::build_tools("builtin-only", None, &ctx).unwrap();
        let bash = |command: String| {
            let tools = Arc::clone(&tools);
            async move {
                match tools
                    .execute(
                        "Bash",
                        serde_json::json!({ "command": command }),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                {
                    Ok(r) => !r.is_error,
                    Err(_) => false,
                }
            }
        };
        assert!(!bash(format!("echo x > '{}/f'", out.display())).await);
        assert!(!out.join("f").exists());
        assert!(bash(format!("echo x > '{}/f'", ws.path().display())).await);
        assert!(ws.path().join("f").exists());
    }
}
