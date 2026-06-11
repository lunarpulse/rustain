//! Service-file rendering (Story 12.1b Task 2, AC-12-1b-1/2/9) — **pure** template
//! substitution: parameters in → rendered unit/plist `String` out, no filesystem.
//! The install-location resolution lives in `infrastructure::paths` (single source
//! of truth shared by `install` and `uninstall`); this module only renders.
//!
//! The two templates are `include_str!`-embedded from `dist/` so the generator and
//! the checked-in reference can never drift (AC-12-1b-9 review-checklist intent).

#![cfg(unix)]

/// systemd unit template (checked-in reference, embedded so it can't drift).
const SYSTEMD_TEMPLATE: &str = include_str!("../../../dist/rustain.service.template");
/// launchd plist template (checked-in reference, embedded so it can't drift).
const LAUNCHD_TEMPLATE: &str = include_str!("../../../dist/com.rustain.daemon.plist.template");

/// Everything the renderers substitute. Resolved by the `install` arm from
/// `current_exe()`, the workspace, the active profile, the invoking user, and any
/// env overrides present in the generating environment (AC-12-1b-1/3).
pub struct ServiceParams {
    /// Absolute path to the rustain executable (`std::env::current_exe()`).
    pub exe: String,
    /// Active profile name forwarded to the supervised daemon.
    pub profile: String,
    /// Workspace directory (becomes `WorkingDirectory`).
    pub workspace: String,
    /// Invoking user — written as `User=` only in `--system` systemd scope.
    pub user: String,
    /// `--system` scope (systemd writes `User=`); default is `--user`.
    pub system: bool,
    /// Daemon log path (launchd `StandardOutPath`/`StandardErrorPath`).
    pub log_path: String,
    /// launchd `Label` / unit identity (`com.rustain.<hash>`).
    pub label: String,
    /// `RUSTAIN_DATA_DIR` to pass through, only when set in the generating env.
    pub data_dir: Option<String>,
    /// `RUSTAIN_CONFIG_DIR` to pass through, only when set in the generating env.
    pub config_dir: Option<String>,
}

impl ServiceParams {
    /// Env pass-through pairs present in the generating environment, in stable order
    /// (AC-12-1b-1: pass through ONLY when set, so test/CI overrides survive while
    /// default installs rely on `$HOME`).
    fn env_pairs(&self) -> Vec<(&'static str, &str)> {
        let mut pairs = Vec::new();
        if let Some(v) = self.data_dir.as_deref() {
            pairs.push(("RUSTAIN_DATA_DIR", v));
        }
        if let Some(v) = self.config_dir.as_deref() {
            pairs.push(("RUSTAIN_CONFIG_DIR", v));
        }
        pairs
    }
}

/// Single-pass `{placeholder}` substitution (Story 12-1d AC-12-1d-5). Scans `template`
/// once left-to-right, replacing each `{key}` with its mapped value; an UNKNOWN
/// `{...}` is emitted literally. Because the scan never re-examines emitted output, a
/// substituted VALUE that itself contains `{env_block}`/`{user_directive}` (e.g. a
/// workspace path or profile name with braces) is NOT re-expanded — this closes the
/// sequential-`.replace()` template-injection defer from the 12.1b review.
fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(close_rel) = after.find('}') {
            let key = &after[1..close_rel];
            match vars.iter().find(|(k, _)| *k == key) {
                Some((_, v)) => out.push_str(v),
                // Unknown placeholder: emit literally (keeps `{...}` that isn't ours).
                None => out.push_str(&after[..=close_rel]),
            }
            rest = &after[close_rel + 1..];
        } else {
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// XML-escape text destined for a plist `<string>` element (Story 12-1d AC-12-1d-5):
/// `&` first, then `<`/`>`. Closes the launchd XML-injection defer — a workspace /
/// profile / env value containing `<>&` would otherwise produce a malformed plist.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the systemd unit (AC-12-1b-1). `--system` adds `User=`; env overrides
/// become `Environment=` lines. `ExecStart` uses `daemon start --foreground`
/// (AC-12-1b-9 — baked into the template). Substitution is single-pass (AC-12-1d-5).
pub fn render_systemd_unit(p: &ServiceParams) -> String {
    let user_directive = if p.system {
        format!("User={}\n", p.user)
    } else {
        String::new()
    };
    let env_block: String = p
        .env_pairs()
        .iter()
        .map(|(k, v)| format!("Environment={k}={v}\n"))
        .collect();

    render_template(
        SYSTEMD_TEMPLATE,
        &[
            ("exe", &p.exe),
            ("profile", &p.profile),
            ("workspace", &p.workspace),
            ("user_directive", &user_directive),
            ("env_block", &env_block),
        ],
    )
}

/// Render the launchd plist (AC-12-1b-2). `KeepAlive { SuccessfulExit = false }` and
/// the `--foreground` ProgramArguments are baked into the template; env overrides
/// become an `EnvironmentVariables` dict. All substituted values are XML-escaped and
/// substitution is single-pass (AC-12-1d-5).
pub fn render_launchd_plist(p: &ServiceParams) -> String {
    let env_block = {
        let pairs = p.env_pairs();
        if pairs.is_empty() {
            String::new()
        } else {
            let mut s = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
            for (k, v) in pairs {
                s.push_str(&format!(
                    "        <key>{}</key>\n        <string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v),
                ));
            }
            s.push_str("    </dict>\n");
            s
        }
    };

    render_template(
        LAUNCHD_TEMPLATE,
        &[
            ("label", &xml_escape(&p.label)),
            ("exe", &xml_escape(&p.exe)),
            ("profile", &xml_escape(&p.profile)),
            ("workspace", &xml_escape(&p.workspace)),
            ("log_path", &xml_escape(&p.log_path)),
            // env_block is already XML-escaped per-value above; it is plist markup, not
            // a user value, so it must NOT be escaped again.
            ("env_block", &env_block),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ServiceParams {
        ServiceParams {
            exe: "/usr/local/bin/rustain".into(),
            profile: "coding".into(),
            workspace: "/home/me/proj".into(),
            user: "me".into(),
            system: false,
            log_path: "/home/me/proj/.rustain/daemon.log".into(),
            label: "com.rustain.abc123".into(),
            data_dir: None,
            config_dir: None,
        }
    }

    // ── Layer 1 — template-content assertions (AC-12-1b-7) ────────────────────

    #[test]
    fn systemd_unit_uses_foreground_and_restart_policy() {
        let unit = render_systemd_unit(&params());
        // THE load-bearing token (AC-12-1b-9): foreground, never a bare `daemon start`.
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/rustain --profile coding daemon start --foreground"
        ));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec="));
        assert!(unit.contains("StartLimitIntervalSec="));
        assert!(unit.contains("StartLimitBurst="));
        assert!(unit.contains("Type=simple"));
        assert!(unit.contains("WorkingDirectory=/home/me/proj"));
        // AC-12-1b-9: no bare `daemon start` that lacks `--foreground`.
        assert!(!unit.contains("daemon start\n"));
        // user scope → no User= line.
        assert!(!unit.contains("User="));
        // no env pass-through unless set.
        assert!(!unit.contains("Environment="));
    }

    #[test]
    fn systemd_system_scope_writes_user_and_env() {
        let mut p = params();
        p.system = true;
        p.data_dir = Some("/tmp/data".into());
        p.config_dir = Some("/tmp/cfg".into());
        let unit = render_systemd_unit(&p);
        assert!(unit.contains("User=me"));
        assert!(unit.contains("Environment=RUSTAIN_DATA_DIR=/tmp/data"));
        assert!(unit.contains("Environment=RUSTAIN_CONFIG_DIR=/tmp/cfg"));
    }

    #[test]
    fn launchd_plist_keepalive_successfulexit_false_and_foreground() {
        let plist = render_launchd_plist(&params());
        assert!(plist.contains("<key>KeepAlive</key>"));
        // Assert the *false* (relaunch only on unclean exit) — AC-12-1b-2/7.
        assert!(plist.contains("<key>SuccessfulExit</key>"));
        assert!(plist.contains("<false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<true/>"));
        // foreground in ProgramArguments (AC-12-1b-9).
        assert!(plist.contains("<string>--foreground</string>"));
        assert!(plist.contains("<string>com.rustain.abc123</string>"));
        assert!(plist.contains("/home/me/proj/.rustain/daemon.log"));
        // no env dict unless set.
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn launchd_env_block_rendered_when_set() {
        let mut p = params();
        p.data_dir = Some("/tmp/data".into());
        let plist = render_launchd_plist(&p);
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>RUSTAIN_DATA_DIR</key>"));
        assert!(plist.contains("<string>/tmp/data</string>"));
    }

    // ── AC-12-1d-5: template-injection + XML-escape hardening (pulled forward) ────

    #[test]
    fn render_template_does_not_re_expand_substituted_values() {
        // A value containing another placeholder must be emitted verbatim, NOT
        // re-scanned — the sequential-`.replace()` injection the 12.1b review flagged.
        let out = render_template(
            "A={one} B={two}",
            &[("one", "{two}-literal"), ("two", "SECOND")],
        );
        assert_eq!(out, "A={two}-literal B=SECOND");
    }

    #[test]
    fn render_template_leaves_unknown_placeholders_literal() {
        assert_eq!(
            render_template("x={known} y={unknown}", &[("known", "K")]),
            "x=K y={unknown}"
        );
    }

    #[test]
    fn systemd_workspace_with_placeholder_text_is_not_expanded() {
        // Pathological workspace path containing `{env_block}` must NOT inject env
        // lines, and (user scope) no Environment= should appear at all.
        let mut p = params();
        p.workspace = "/home/me/{env_block}/proj".into();
        let unit = render_systemd_unit(&p);
        assert!(unit.contains("WorkingDirectory=/home/me/{env_block}/proj"));
        assert!(!unit.contains("Environment="));
    }

    #[test]
    fn launchd_xml_escapes_special_chars_in_values() {
        // `<>&` in a substituted value would otherwise produce a malformed plist.
        let mut p = params();
        p.profile = "a&b<c>d".into();
        p.workspace = "/tmp/x & y".into();
        let plist = render_launchd_plist(&p);
        assert!(plist.contains("<string>a&amp;b&lt;c&gt;d</string>"));
        assert!(plist.contains("<string>/tmp/x &amp; y</string>"));
        // raw unescaped forms must be gone from the substituted values.
        assert!(!plist.contains("a&b<c>d"));
    }

    #[test]
    fn launchd_env_values_are_xml_escaped() {
        let mut p = params();
        p.data_dir = Some("/data/a&b".into());
        let plist = render_launchd_plist(&p);
        assert!(plist.contains("<string>/data/a&amp;b</string>"));
    }
}
