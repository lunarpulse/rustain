//! Story 19.29 AC2–AC4 — redirect targets under a Bash command pattern.
//!
//! # Front door
//!
//! The executing keystones drive `ToolScheduler::schedule_with_provenance_and_restriction`
//! with the REAL `SecurityAdapter` (workspace = `<tmp>/ws`) and the REAL
//! `ToolSetAdapter`, which runs `bash -c <command>` with cwd = the workspace.
//! The witness for "never reached the shell" is the target file's absence: bash
//! opens a redirect target before it resolves the command, so the file exists
//! after the run whether or not `kubectl` is installed.
//!
//! Every outside path is a SIBLING tempdir of the workspace (`<tmp>/out`), never
//! `$HOME`, `/etc` or a fixed `/tmp` path. `~` / `$HOME` attacks (AC3) run at
//! the permission-chain level only: a regression there must never write the
//! developer's real `~/.bashrc`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::adapters::noop::NoOpToolSet;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{
    AgentToolRestriction, PermissionMode, ProvenanceTag, SkillToolRestriction, ToolCall, ToolPolicy,
};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::permission_chain::{self, PermissionDecision};
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

const WORKSPACE_PARENT_ERROR: &str =
    "Workspace violation: Parent directory resolves outside workspace";
const EXPANSION_REASON_TAIL: &str =
    "uses shell expansion ($, ~, *, ?, [ or {), so it cannot be checked before the command runs";

/// The production scheduler over the real security and Bash adapters.
struct Door {
    scheduler: Arc<ToolScheduler>,
    approvals: Arc<ApprovalRuntime>,
    /// `<base>/ws` — the workspace and the shell's cwd.
    workspace: PathBuf,
    /// `<base>/out` — a sibling directory, OUTSIDE the workspace.
    outside: PathBuf,
    /// `<base>` — `../x` from the workspace lands here, still inside the tempdir.
    base: PathBuf,
    _tmp: tempfile::TempDir,
}

fn door(mode: PermissionMode) -> Door {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    let outside = base.join("out");
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let security = Arc::new(SecurityAdapter::new(workspace.clone()));
    security.set_mode(mode);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(base.join("sessions")),
    );
    let tools: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        workspace.clone(),
        storage,
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    ));
    let approvals = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approvals.clone(), 16);
    Door {
        scheduler,
        approvals,
        workspace,
        outside,
        base,
        _tmp: tmp,
    }
}

fn skill_restriction(items: &[&str]) -> SkillToolRestriction {
    SkillToolRestriction {
        skill_names: vec!["kube".to_string()],
        items: items.iter().map(|item| (*item).to_string()).collect(),
        inherited: false,
    }
}

fn agent_restriction(items: &[&str]) -> AgentToolRestriction {
    let declared: BTreeSet<String> = items.iter().map(|item| (*item).to_string()).collect();
    AgentToolRestriction {
        agent_name: "ops".to_string(),
        policy: ToolPolicy::Allowlist {
            tools: declared.clone(),
        },
        declared_items: declared,
    }
}

/// Bounded: a call the chain admits in Normal mode parks on an approval card
/// forever, so a timeout IS the failure "the command reached the approval
/// stage" — never a hung test binary.
async fn run_bash(
    door: &Door,
    command: &str,
    skill: Option<&SkillToolRestriction>,
    agent: Option<&AgentToolRestriction>,
) -> ToolCall {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_bash_unbounded(door, command, skill, agent),
    )
    .await
    .unwrap_or_else(|_| panic!("command={command:?}: the call was admitted to the approval stage"))
}

async fn run_bash_unbounded(
    door: &Door,
    command: &str,
    skill: Option<&SkillToolRestriction>,
    agent: Option<&AgentToolRestriction>,
) -> ToolCall {
    door.scheduler
        .clone()
        .schedule_with_provenance_and_restriction(
            ApprovalSource::ForegroundTurn {
                conversation_id: "conv-19-29".to_string(),
            },
            vec![ToolCallRequest {
                id: "bash-1".to_string(),
                tool_name: "Bash".to_string(),
                input: serde_json::json!({ "command": command }),
            }],
            CancellationToken::new(),
            skill,
            agent,
            ProvenanceTag::UserOriginated,
            None,
        )
        .await
        .remove(0)
}

fn error_text(call: ToolCall) -> String {
    match call {
        ToolCall::Error { error, .. } => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn skill_refusal(reason: &str) -> String {
    format!(
        "Tool 'Bash' redirect refused by skill 'kube': {reason}. Redirect to a literal path inside the workspace, or drop the redirect. To run it unrestricted, the operator can deactivate skill 'kube'."
    )
}

// ── AC2 — attacks never reach the shell, in every mode ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ac2_redirects_to_outside_paths_are_refused_before_execution_in_every_mode() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let door = door(mode);
        std::os::unix::fs::symlink(&door.outside, door.workspace.join("link")).unwrap();
        let o = door.outside.display().to_string();
        let w = |name: &str| door.outside.join(name);

        // (command, witness file that must not exist afterwards)
        let cases: Vec<(String, PathBuf)> = vec![
            (format!("kubectl get pods > {o}/passwd"), w("passwd")),
            (format!("kubectl config view --raw > {o}/x"), w("x")),
            (format!("kubectl version >{o}/a1"), w("a1")),
            (format!("kubectl version 2>{o}/a2"), w("a2")),
            (format!("kubectl version 2>>{o}/a3"), w("a3")),
            (format!("kubectl version &>{o}/a4"), w("a4")),
            (format!("kubectl version &>>{o}/a5"), w("a5")),
            (format!("kubectl version >&{o}/a6"), w("a6")),
            (format!("kubectl version >|{o}/a7"), w("a7")),
            (format!("kubectl version <>{o}/a8"), w("a8")),
            (format!("kubectl version > \"{o}/a9\""), w("a9")),
            (format!("kubectl version > '{o}/a10'"), w("a10")),
            // Escaped leading `/`, empty quotes and a quoted `/` all name the
            // same absolute path to bash.
            (format!("kubectl version > \\{o}/a11"), w("a11")),
            (format!("kubectl version > ''{o}/a12"), w("a12")),
            (format!("kubectl version > '/'{}/a13", &o[1..]), w("a13")),
            // A line continuation deletes itself: `>\⏎/x` is `>/x`.
            (format!("kubectl version >\\\n{o}/a14"), w("a14")),
            // A redirect in the SECOND segment.
            (
                format!("kubectl get pods && kubectl version > {o}/a15"),
                w("a15"),
            ),
            // A workspace symlink pointing outside.
            ("kubectl version > link/a16".to_string(), w("a16")),
            // Traversal: `<base>/ws/../x` is `<base>/x`, inside the tempdir.
            (
                "kubectl version > ../traversal".to_string(),
                door.base.join("traversal"),
            ),
        ];

        let skill = skill_restriction(&["Bash(kubectl:*)", "Bash(echo:*)"]);
        let mut approval_events = door.approvals.subscribe();
        for (command, witness) in &cases {
            let text = error_text(run_bash(&door, command, Some(&skill), None).await);
            assert!(
                text.starts_with("Tool 'Bash' redirect refused by skill 'kube': "),
                "mode={mode:?} command={command:?}: {text}"
            );
            assert!(
                !witness.exists(),
                "mode={mode:?} command={command:?}: the shell must never have run; {} exists",
                witness.display()
            );
        }
        assert!(
            approval_events.try_recv().is_err(),
            "mode={mode:?}: a refused redirect must never raise an approval card"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_the_refusal_text_names_the_redirect_the_error_and_the_next_step() {
    let door = door(PermissionMode::Yolo);
    let o = door.outside.display().to_string();
    let skill = skill_restriction(&["Bash(kubectl:*)"]);
    let text = error_text(
        run_bash(
            &door,
            &format!("kubectl get pods > {o}/passwd"),
            Some(&skill),
            None,
        )
        .await,
    );
    assert_eq!(
        text,
        skill_refusal(&format!(
            "'> {o}/passwd' writes to a path the workspace check refuses ({WORKSPACE_PARENT_ERROR})"
        ))
    );
    assert!(!door.outside.join("passwd").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_admitted_redirects_inside_the_workspace_run_in_yolo() {
    let door = door(PermissionMode::Yolo);
    let skill = skill_restriction(&["Bash(echo:*)", "Bash(kubectl:*)"]);

    for command in [
        "echo ok > out.txt",
        "echo ok > sub/nested.txt",
        "echo ok 2>/dev/null",
        "echo ok 2>&1",
    ] {
        let call = run_bash(&door, command, Some(&skill), None).await;
        assert!(
            matches!(call, ToolCall::Success { .. }),
            "command={command:?}: {call:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(door.workspace.join("out.txt")).unwrap(),
        "ok\n"
    );
    assert_eq!(
        std::fs::read_to_string(door.workspace.join("sub/nested.txt")).unwrap(),
        "ok\n"
    );
}

// ── AC2/AC3 — chain level, no execution ────────────────────────────────────

async fn chain(security: &SecurityAdapter, items: &[&str], command: &str) -> PermissionDecision {
    let restriction = skill_restriction(items);
    permission_chain::check_with_source_and_provenance_and_restriction(
        security,
        "Bash",
        &serde_json::json!({ "command": command }),
        Some(&restriction),
        None,
        None,
        &NoOpToolSet,
        None,
        ProvenanceTag::UserOriginated,
        None,
    )
    .await
}

fn chain_security(mode: PermissionMode) -> (SecurityAdapter, tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(tmp.path()).unwrap();
    let security = SecurityAdapter::new(workspace.clone());
    security.set_mode(mode);
    (security, tmp, workspace)
}

fn denial(decision: PermissionDecision) -> String {
    match decision {
        PermissionDecision::Deny(reason) => reason,
        other => panic!("expected a hard deny, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_a_read_redirect_to_a_blocked_path_is_refused_with_reads_from() {
    let (security, _tmp, _ws) = chain_security(PermissionMode::Yolo);
    let text = denial(
        chain(
            &security,
            &["Bash(kubectl:*)"],
            "kubectl apply -f - < /etc/passwd",
        )
        .await,
    );
    assert_eq!(
        text,
        skill_refusal(
            "'< /etc/passwd' reads from a path the workspace check refuses (Workspace violation: Access to /etc/ is not allowed)"
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_dev_null_is_exact_never_a_dev_prefix() {
    let (security, _tmp, _ws) = chain_security(PermissionMode::Yolo);
    let items = ["Bash(kubectl:*)"];
    for command in [
        "kubectl get pods 2>/dev/null",
        "kubectl get pods &>/dev/null",
    ] {
        assert_eq!(
            chain(&security, &items, command).await,
            PermissionDecision::Allow,
            "command={command:?}"
        );
    }
    let text = denial(chain(&security, &items, "kubectl version > /dev/shm/x").await);
    assert!(
        text.contains("writes to a path the workspace check refuses (Workspace violation: Access to /dev/ is not allowed)"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_targets_that_cannot_be_checked_are_refused_never_guessed() {
    let (security, _tmp, _ws) = chain_security(PermissionMode::Yolo);
    let kubectl = ["Bash(kubectl:*)"];
    let expansion_rows = [
        ("kubectl version > ~/.bashrc", "> ~/.bashrc"),
        ("kubectl version >> $HOME/.bashrc", ">> $HOME/.bashrc"),
        ("kubectl version > \"$HOME\"/.bashrc", "> \"$HOME\"/.bashrc"),
        ("kubectl version > /tmp/*", "> /tmp/*"),
        ("kubectl version > /tmp/{a,b}", "> /tmp/{a,b}"),
    ];
    for (command, redir) in expansion_rows {
        assert_eq!(
            denial(chain(&security, &kubectl, command).await),
            skill_refusal(&format!("the target of '{redir}' {EXPANSION_REASON_TAIL}")),
            "command={command:?}"
        );
    }

    let cd_reason = |redir: &str| {
        skill_refusal(&format!(
            "the relative target of '{redir}' cannot be checked because the command changes directory"
        ))
    };
    assert_eq!(
        denial(
            chain(
                &security,
                &["Bash(cd:*)", "Bash(kubectl:*)"],
                "cd /tmp && kubectl version > x"
            )
            .await
        ),
        cd_reason("> x")
    );
    assert_eq!(
        denial(
            chain(
                &security,
                &["Bash(builtin:*)", "Bash(kubectl:*)"],
                "builtin cd /tmp && kubectl version > x"
            )
            .await
        ),
        cd_reason("> x")
    );

    assert_eq!(
        denial(chain(&security, &kubectl, "kubectl version >").await),
        skill_refusal("'>' has no target")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_the_same_commands_with_a_literal_workspace_target_are_admitted() {
    let (security, _tmp, workspace) = chain_security(PermissionMode::Yolo);
    let kubectl = ["Bash(kubectl:*)"];
    for command in [
        "kubectl version > out.txt",
        "kubectl version >> out.txt",
        "kubectl version 2> err.txt",
    ] {
        assert_eq!(
            chain(&security, &kubectl, command).await,
            PermissionDecision::Allow,
            "command={command:?}"
        );
    }
    // After a `cd`, only an ABSOLUTE in-workspace target is admitted.
    let absolute = format!("cd sub && kubectl version > {}/x", workspace.display());
    assert_eq!(
        chain(&security, &["Bash(cd:*)", "Bash(kubectl:*)"], &absolute).await,
        PermissionDecision::Allow
    );
}

// ── AC4 — nothing changes where no command pattern restricts Bash ──────────

#[tokio::test(flavor = "multi_thread")]
async fn ac4_unrestricted_bash_still_reaches_the_shell_with_any_redirect() {
    enum Axis {
        None,
        Skill(SkillToolRestriction),
        Agent(AgentToolRestriction),
    }
    let axes = vec![
        ("no restriction", Axis::None),
        ("skill [Bash]", Axis::Skill(skill_restriction(&["Bash"]))),
        (
            "agent [Bash(*)]",
            Axis::Agent(agent_restriction(&["Bash(*)"])),
        ),
        (
            "agent [Bash(*), Bash(kubectl:*)]",
            Axis::Agent(agent_restriction(&["Bash(*)", "Bash(kubectl:*)"])),
        ),
    ];
    let door = door(PermissionMode::Yolo);
    for (index, (label, axis)) in axes.into_iter().enumerate() {
        let witness = door.outside.join(format!("unrestricted-{index}"));
        let command = format!("echo x > {}", witness.display());
        let (skill, agent) = match &axis {
            Axis::None => (None, None),
            Axis::Skill(restriction) => (Some(restriction), None),
            Axis::Agent(restriction) => (None, Some(restriction)),
        };
        let call = run_bash(&door, &command, skill, agent).await;
        assert!(
            matches!(call, ToolCall::Success { .. }),
            "{label}: {call:?}"
        );
        assert!(
            witness.exists(),
            "{label}: unrestricted Bash is exactly as before — the redirect ran"
        );
    }
}

// ── Patch round 1 (R19-29-01/02/04) — lexical bypasses of the one scanner ───
//
// Payloads 1 and 2 are the review's. `run_bash` is bounded, so a Normal-mode
// call the chain admits fails the test instead of hanging it.

#[tokio::test(flavor = "multi_thread")]
async fn r19_29_01_02_04_lexical_bypasses_never_reach_the_shell_in_every_mode() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let door = door(mode);
        let o = door.outside.display().to_string();
        let inside = door.workspace.join("inside");

        // (command, witness that must not exist afterwards)
        let cases: Vec<(String, PathBuf)> = vec![
            // R19-29-01: quotes inside comments hid the real redirect.
            (
                format!("echo ok # '\necho escaped > {o}/19-29-comment # '"),
                door.outside.join("19-29-comment"),
            ),
            // R19-29-02: `$'\''` is one literal apostrophe to Bash.
            (
                format!("echo $'\\'' > {o}/19-29-ansi # '"),
                door.outside.join("19-29-ansi"),
            ),
            // A comment is not a comment inside `${…}`.
            (
                format!("echo ${{x:- # }}; echo > {o}/19-29-brace"),
                door.outside.join("19-29-brace"),
            ),
            // ANSI-C quoting anywhere in the command.
            (
                format!("echo $'a' > {o}/19-29-ansi2"),
                door.outside.join("19-29-ansi2"),
            ),
            // `$[…]` / `((…))` arithmetic nest their own quoting.
            (
                format!("echo $[1+1] > {o}/19-29-arith"),
                door.outside.join("19-29-arith"),
            ),
            // R19-29-04: a continuation between the fd and the operator.
            ("echo 2\\\n>inside".to_string(), inside.clone()),
        ];

        let skill = skill_restriction(&["Bash(echo:*)"]);
        let mut approval_events = door.approvals.subscribe();
        for (command, witness) in &cases {
            let text = error_text(run_bash(&door, command, Some(&skill), None).await);
            assert!(
                text.starts_with("Tool 'Bash' command not allowed by skill 'kube'")
                    || text.starts_with("Tool 'Bash' redirect refused by skill 'kube': "),
                "mode={mode:?} command={command:?}: {text}"
            );
            assert!(
                !witness.exists(),
                "mode={mode:?} command={command:?}: the shell must never have run; {} exists",
                witness.display()
            );
        }
        assert!(
            approval_events.try_recv().is_err(),
            "mode={mode:?}: a refused command must never raise an approval card"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn r19_29_01_02_the_same_payloads_are_refused_on_the_agent_axis() {
    let door = door(PermissionMode::Yolo);
    let o = door.outside.display().to_string();
    let agent = agent_restriction(&["Bash(echo:*)"]);
    for (command, witness) in [
        (
            format!("echo ok # '\necho escaped > {o}/agent-comment # '"),
            door.outside.join("agent-comment"),
        ),
        (
            format!("echo $'\\'' > {o}/agent-ansi # '"),
            door.outside.join("agent-ansi"),
        ),
    ] {
        let text = error_text(run_bash(&door, &command, None, Some(&agent)).await);
        assert!(
            text.starts_with("Tool 'Bash' command not allowed. Allowed: [Bash(echo:*)]")
                && text.ends_with(" by agent 'ops'"),
            "command={command:?}: {text}"
        );
        assert!(!witness.exists(), "command={command:?}: {witness:?} exists");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn r19_29_01_02_in_workspace_controls_with_quotes_and_hashes_still_execute() {
    let door = door(PermissionMode::Yolo);
    let skill = skill_restriction(&["Bash(echo:*)"]);
    // Quoted `#`, a mid-word `#`, `$#`, a double-quoted `$"…"` string, a
    // single-quoted `$'` and an escaped `\$'…'` are all modelled exactly.
    for (command, file, content) in [
        (
            "echo '# not a comment' > c1.txt",
            "c1.txt",
            "# not a comment\n",
        ),
        ("echo a#b > c2.txt", "c2.txt", "a#b\n"),
        ("echo \"it's\" > c3.txt", "c3.txt", "it's\n"),
        ("echo '$' > c4.txt", "c4.txt", "$\n"),
        ("echo \\$'x' > c5.txt", "c5.txt", "$x\n"),
        ("echo $\"hi\" > c6.txt", "c6.txt", "hi\n"),
    ] {
        let call = run_bash(&door, command, Some(&skill), None).await;
        assert!(
            matches!(call, ToolCall::Success { .. }),
            "command={command:?}: {call:?}"
        );
        assert_eq!(
            std::fs::read_to_string(door.workspace.join(file)).unwrap(),
            content,
            "command={command:?}"
        );
    }
}

// ── Patch round 2 (G1 continued openers, G2 extglob, G3 quoted continuation) ─

#[tokio::test(flavor = "multi_thread")]
async fn round2_continued_openers_extglob_and_quoted_continuation_never_reach_the_shell() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let door = door(mode);
        let o = door.outside.display().to_string();

        // (command, witness that must not exist afterwards)
        let cases: Vec<(String, PathBuf)> = vec![
            // G1: Bash deletes `\⏎` before it sees `$'`, so `$\⏎'\''` is one
            // literal apostrophe and the real redirect follows.
            (
                format!("echo $\\\n'\\'' > {o}/r2-leak # '"),
                door.outside.join("r2-leak"),
            ),
            (
                format!("echo $\\\n[1] > {o}/r2-arith"),
                door.outside.join("r2-arith"),
            ),
            // G3: `"c\⏎d"` is `cd`, so the relative `leak` lands in `{o}`.
            (
                format!("builtin \"c\\\nd\" {o} && echo x > r2-cwd-leak"),
                door.outside.join("r2-cwd-leak"),
            ),
        ];

        let skill = skill_restriction(&["Bash(echo:*)", "Bash(builtin:*)"]);
        let mut approval_events = door.approvals.subscribe();
        for (command, witness) in &cases {
            let text = error_text(run_bash(&door, command, Some(&skill), None).await);
            assert!(
                text.starts_with("Tool 'Bash' command not allowed by skill 'kube'")
                    || text.starts_with("Tool 'Bash' redirect refused by skill 'kube': "),
                "mode={mode:?} command={command:?}: {text}"
            );
            assert!(
                !witness.exists(),
                "mode={mode:?} command={command:?}: the shell must never have run; {} exists",
                witness.display()
            );
        }
        assert!(
            approval_events.try_recv().is_err(),
            "mode={mode:?}: a refused command must never raise an approval card"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn round2_in_workspace_controls_still_execute() {
    let door = door(PermissionMode::Yolo);
    let skill = skill_restriction(&["Bash(echo:*)", "Bash(builtin:*)"]);
    for (command, file) in [
        ("echo x > plain.txt", "plain.txt"),
        ("builtin echo x > builtin.txt", "builtin.txt"),
        ("echo $\\\nHOME > cont.txt", "cont.txt"),
    ] {
        let call = run_bash(&door, command, Some(&skill), None).await;
        assert!(
            matches!(call, ToolCall::Success { .. }),
            "command={command:?}: {call:?}"
        );
        assert!(door.workspace.join(file).exists(), "command={command:?}");
    }
}

// ── Patch round 3 (R19-29-07): the G2 witness, for real ─────────────────────
//
// `shopt -s extglob` on an EARLIER line (admitted for the test) makes Bash
// expand `@(secret)` / `+(secret)` / `!(sub)` to the workspace leaf `secret`,
// a symlink to an existing OUTSIDE file with sentinel content. The round-1
// scanner cut the target at `(` and checked the absent leaf `@`, so the shell
// truncated the outside file. The assertion is the sentinel's content.

const SENTINEL: &str = "SENTINEL-DO-NOT-OVERWRITE\n";

#[tokio::test(flavor = "multi_thread")]
async fn r19_29_07_extglob_targets_never_overwrite_an_outside_file_through_a_workspace_symlink() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let door = door(mode);
        let secret = door.outside.join("secret");
        std::os::unix::fs::symlink(&secret, door.workspace.join("secret")).unwrap();
        let skill = skill_restriction(&["Bash(shopt:*)", "Bash(echo:*)"]);
        let mut approval_events = door.approvals.subscribe();

        for target in ["@(secret)", "+(secret)", "!(sub)", "@(secret|nothing)"] {
            std::fs::write(&secret, SENTINEL).unwrap();
            let command = format!("shopt -s extglob\necho clobbered > {target}");
            let text = error_text(run_bash(&door, &command, Some(&skill), None).await);
            assert!(
                text.starts_with("Tool 'Bash' command not allowed by skill 'kube'")
                    || text.starts_with("Tool 'Bash' redirect refused by skill 'kube': "),
                "mode={mode:?} target={target}: {text}"
            );
            assert_eq!(
                std::fs::read_to_string(&secret).unwrap(),
                SENTINEL,
                "mode={mode:?} target={target}: the outside file was overwritten"
            );
        }
        assert!(
            approval_events.try_recv().is_err(),
            "mode={mode:?}: a refused command must never raise an approval card"
        );
    }
}

/// Paired control: with extglob enabled the same shape against a LITERAL
/// in-workspace target still runs.
#[tokio::test(flavor = "multi_thread")]
async fn r19_29_07_control_extglob_enabled_literal_in_workspace_target_still_runs() {
    let door = door(PermissionMode::Yolo);
    let skill = skill_restriction(&["Bash(shopt:*)", "Bash(echo:*)"]);
    let call = run_bash(
        &door,
        "shopt -s extglob\necho ok > literal.txt",
        Some(&skill),
        None,
    )
    .await;
    assert!(matches!(call, ToolCall::Success { .. }), "{call:?}");
    assert_eq!(
        std::fs::read_to_string(door.workspace.join("literal.txt")).unwrap(),
        "ok\n"
    );
}
