//! Story 18.3b — front-door keystones for the policy startup validator (AC5) and
//! the `rustain doctor` explainer (AC6), plus the AC4 structural ratchet and the
//! AC7 domain boundary.
//!
//! Everything here enters through a **real production entry point**: a spawned
//! `rustain daemon start --foreground` process for AC5, and the real `rustain
//! doctor --json` binary for AC6. Neither calls `resolve_effective_policy` or the
//! validator directly — that is the explicitly forbidden bypass, and it would prove
//! the fold works while leaving the wiring unexercised (the 17.3 RC-B failure).

use std::path::Path;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;

const INDIVIDUAL: &str = "a2a-interaction.toml";
const TEAM: &str = "team-policy.toml";

fn write_policy(workspace: &Path, name: &str, body: &str) {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).expect("create .rustain");
    std::fs::write(dir.join(name), body).expect("write policy file");
}

/// A pair that conflicts on BOTH binding quantities: the team raises urgency and
/// caps automation.
fn write_conflicting_pair(workspace: &Path) {
    write_policy(
        workspace,
        INDIVIDUAL,
        "[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\nnotification = \"digest\"\nstatus_detail_minimum = \"story-only\"\n",
    );
    write_policy(
        workspace,
        TEAM,
        "[team.defaults]\nresponse_mode = \"notify-and-draft\"\nnotification = \"immediate\"\n",
    );
}
fn doctor_json_output(
    workspace: &Path,
    data_dir: &Path,
    config_dir: &Path,
) -> std::process::Output {
    std::process::Command::new(cargo_bin("rustain"))
        .arg("doctor")
        .arg("--json")
        .current_dir(workspace)
        .env("RUSTAIN_DATA_DIR", data_dir)
        .env("RUSTAIN_CONFIG_DIR", config_dir)
        .output()
        .expect("run `rustain doctor --json`")
}

// ──────────────────────────────────────────────────────────────────
// AC5 — NFR66 through the real daemon, which has no TTY
// ──────────────────────────────────────────────────────────────────
/// Poll the daemon's log until it contains every required row, or time out.
///
/// `logging.rs` rotates by size with a date suffix, so the file on disk is
/// `rustain.log.<date>`, not `rustain.log` — every matching file is concatenated
/// so the test cannot miss a rotation boundary.
///
/// Returns the log contents so a failure can print what the daemon actually said.
fn wait_for_log(data_dir: &Path, needles: &[&str], budget: Duration) -> Result<String, String> {
    let deadline = Instant::now() + budget;
    let mut last = String::new();
    while Instant::now() < deadline {
        let text = read_daemon_log(data_dir);
        if needles.iter().all(|needle| text.contains(needle)) {
            return Ok(text);
        }
        if !text.is_empty() {
            last = text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last)
}

/// Concatenate every `rustain.log*` file in `data_dir`.
fn read_daemon_log(data_dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return String::new();
    };
    let mut names: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rustain.log"))
        })
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n")
}
/// AC5 keystone — the daemon resolves and reports policy at startup, through its
/// LOG.
///
/// **Front door:** the real `rustain daemon start --foreground` process. The child
/// gets `stdout`/`stderr` as null pipes and no controlling terminal, so this is
/// exactly the no-TTY case.
///
/// **Mutant this must turn RED:** make startup validation a no-op when no TTY is
/// detected. The daemon is the primary case for this validation, not the excluded
/// one, so a TTY gate silences the report precisely where it matters.
///
/// **Forbidden bypass, deliberately not used:** calling `validate_startup_policies`
/// or `resolve_effective_policy` directly and claiming startup coverage.
#[test]
#[cfg(unix)]
fn ac5_daemon_startup_reports_policy_conflicts_through_the_log() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    write_conflicting_pair(workspace.path());

    let mut child = std::process::Command::new(cargo_bin("rustain"))
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .env("RUSTAIN_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn `rustain daemon start --foreground`");

    let outcome = wait_for_log(
        data_dir.path(),
        &[
            "interaction policy resolved",
            "notification urgency",
            "raised from",
            "digest",
            "immediate",
            "response automation",
            "lowered",
            "notify-and-auto",
            "notify-and-draft",
            "resolution",
        ],
        Duration::from_secs(45),
    );
    let _ = child.kill();
    let _ = child.wait();

    let log = outcome.unwrap_or_else(|log| {
        panic!(
            "the daemon never reported the resolved interaction policy.\n\
             A no-TTY no-op would look exactly like this.\nLog was:\n{log}"
        )
    });

    // The derivation, not just the answer — both changed quantities, each with the
    // (individual, team) pair that produced it.
    for needle in [
        "notification urgency",
        "raised from",
        "digest",
        "immediate",
        "response automation",
        "lowered",
        "notify-and-auto",
        "notify-and-draft",
    ] {
        assert!(
            log.contains(needle),
            "startup log omitted `{needle}`:\n{log}"
        );
    }
    // NFR66's resolution guidance reached the log too.
    assert!(
        log.contains("resolution"),
        "startup log carried no resolution guidance:\n{log}"
    );
}

/// AC5 positive control — a clean pair produces NO conflict report, but startup
/// still says it validated.
///
/// A validator that always warns is as useless as one that never does; a validator
/// that says nothing at all is indistinguishable from one that did not run.
#[test]
#[cfg(unix)]
fn ac5_positive_control_a_clean_pair_reports_zero_conflicts() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    // Individual is already stricter than the team on both binding quantities.
    write_policy(
        workspace.path(),
        INDIVIDUAL,
        "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"immediate\"\n",
    );
    write_policy(
        workspace.path(),
        TEAM,
        "[team.defaults]\nresponse_mode = \"notify-and-auto\"\nnotification = \"queue\"\n",
    );

    let mut child = std::process::Command::new(cargo_bin("rustain"))
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .env("RUSTAIN_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let outcome = wait_for_log(
        data_dir.path(),
        &["interaction policy resolved", "conflicts=0"],
        Duration::from_secs(45),
    );
    let _ = child.kill();
    let _ = child.wait();

    let log = outcome.unwrap_or_else(|log| panic!("no policy report at startup:\n{log}"));
    assert!(
        log.contains("conflicts=0"),
        "a clean pair must report zero conflicts:\n{log}"
    );
    assert!(
        !log.contains("policy conflict"),
        "a clean pair must not emit a conflict line:\n{log}"
    );
}

/// AC2 + AC5 — a malformed policy file is **fatal** at startup and never falls
/// through to a permissive default.
#[test]
#[cfg(unix)]
fn ac5_a_malformed_policy_file_stops_the_daemon() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    write_policy(workspace.path(), TEAM, "[team.defaults\n");

    // BOUNDED, deliberately: `.output()` here would block forever the moment this
    // assertion stops holding, because a daemon that accepts the malformed file
    // goes on to run its lifecycle loop. A test that hangs reports nothing; this
    // one fails.
    let mut child = std::process::Command::new(cargo_bin("rustain"))
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .env("RUSTAIN_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("poll daemon") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }

    let status = status.expect(
        "the daemon kept running with a malformed policy file — fail-closed was not honoured",
    );
    assert!(
        !status.success(),
        "a malformed policy file must stop the daemon, not be tolerated"
    );
    // The diagnostic lands in the daemon's LOG, not on stdout/stderr — which is
    // the whole reason AC5 forbids reusing `doctor`'s `println!` printer. Asserting
    // it here proves the operator can actually find out why startup refused.
    let log = read_daemon_log(data_dir.path());
    assert!(
        log.contains(TEAM),
        "the failure must name the offending file in the daemon log:\n{log}"
    );
    assert!(
        log.contains("fail-closed"),
        "the log must explain that nothing was applied:\n{log}"
    );
    // Fail-closed means exactly this: the permissive mode was never reached.
    assert!(
        !log.contains("interaction policy resolved"),
        "a malformed file must not produce a resolved policy:\n{log}"
    );
}

// ──────────────────────────────────────────────────────────────────
// AC6 — the explainer through the real `rustain doctor` binary
// ──────────────────────────────────────────────────────────────────

/// AC6 keystone at integration level — the real `rustain doctor --json` binary.
///
/// Enters through `main` → `run_doctor` → `build_check_list` → `HealthCheck::run` →
/// `doctor/json.rs`, which is the whole point: it proves the machine-readable
/// surface carries the same fields as the human one, and that the check is actually
/// wired rather than merely present.
#[test]
fn ac6_real_doctor_binary_reports_the_policy_derivation_in_json() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    write_conflicting_pair(workspace.path());

    let output = doctor_json_output(workspace.path(), data_dir.path(), config_dir.path());
    let baseline_workspace = tempfile::tempdir().expect("baseline workspace");
    let baseline_data = tempfile::tempdir().expect("baseline data");
    let baseline_config = tempfile::tempdir().expect("baseline config");
    let baseline = doctor_json_output(
        baseline_workspace.path(),
        baseline_data.path(),
        baseline_config.path(),
    );
    assert_eq!(
        output.status.code(),
        baseline.status.code(),
        "policy warnings changed doctor process status: warning={} baseline={}",
        output.status,
        baseline.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json is not JSON: {e}\n{stdout}"));

    let policy_check = report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == "Interaction policy")
        .unwrap_or_else(|| panic!("no interaction-policy check in doctor --json:\n{stdout}"));

    // A policy warning must not change the doctor process status (CheckTier::Info).
    assert_eq!(policy_check["status"], "warning", "{policy_check}");

    let detail = &policy_check["detail"];
    assert!(
        !detail.is_null(),
        "the machine-readable view carries no policy detail — a surface that only \
         exists in the human view is half a surface:\n{policy_check}"
    );

    // Per dimension: effective value, the pair that produced it, and provenance.
    assert_eq!(detail["notification_urgency"]["effective"], "immediate");
    assert_eq!(detail["notification_urgency"]["individual"], "digest");
    assert_eq!(detail["notification_urgency"]["team"], "immediate");
    assert_eq!(
        detail["notification_urgency"]["source"],
        "team-floor-raised"
    );
    assert_eq!(detail["notification_urgency"]["source_file"], TEAM);

    assert_eq!(
        detail["response_automation"]["effective"],
        "notify-and-draft"
    );
    assert_eq!(
        detail["response_automation"]["individual"],
        "notify-and-auto"
    );
    assert_eq!(detail["response_automation"]["team"], "notify-and-draft");
    assert_eq!(detail["response_automation"]["source"], "team-capped");

    // Sharing breadth is never merged.
    assert_eq!(detail["sharing_breadth"]["merge"], "not-merged");
    assert_eq!(detail["sharing_breadth"]["enforced"], false);
    assert_eq!(detail["sharing_breadth"]["source_file"], INDIVIDUAL);
    assert_eq!(detail["consent"], serde_json::json!([]));
    assert_eq!(detail["journal_projection_empty"], true);

    assert_eq!(detail["digest_interval_minutes"], 15);
    assert_eq!(detail["conflicts"], 2);

    // The human message carries the derivation too, with the three distinct
    // sentences.
    let message = policy_check["message"].as_str().expect("message");
    assert!(message.contains("raised from"), "{message}");
    assert!(message.contains("lowered"), "{message}");
    assert!(
        message.contains("Not enforced") || message.contains("no team norm"),
        "{message}"
    );
    // And the resolution guidance NFR66 demands.
    assert!(
        policy_check["fix"].is_string(),
        "NFR66 guidance must reach `fix`:\n{policy_check}"
    );
}

/// AC6 — a malformed file is the only policy-check `Fail`. The policy check is
/// `CheckTier::Info`, so it must not change the process exit status produced by
/// unrelated core checks.
#[test]
fn ac6_real_doctor_binary_fails_the_check_but_not_the_run_on_a_malformed_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    write_policy(workspace.path(), INDIVIDUAL, "not = = toml");

    let output = doctor_json_output(workspace.path(), data_dir.path(), config_dir.path());
    let baseline_workspace = tempfile::tempdir().expect("baseline workspace");
    let baseline_data = tempfile::tempdir().expect("baseline data");
    let baseline_config = tempfile::tempdir().expect("baseline config");
    let baseline = doctor_json_output(
        baseline_workspace.path(),
        baseline_data.path(),
        baseline_config.path(),
    );
    assert_eq!(
        output.status.code(),
        baseline.status.code(),
        "an informational policy-check failure changed doctor process status: failure={} baseline={}",
        output.status,
        baseline.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"));
    let policy_check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["name"] == "Interaction policy")
        .expect("policy check present");

    assert_eq!(policy_check["status"], "fail", "{policy_check}");
    assert!(
        policy_check["fix"].is_string(),
        "`fix` is REQUIRED for Fail (doctor/mod.rs:44-55): {policy_check}"
    );
    assert!(
        policy_check["message"]
            .as_str()
            .unwrap_or_default()
            .contains(INDIVIDUAL),
        "the failure must name the file: {policy_check}"
    );
}

/// AC6 — an unpinned per-sender target is REPORTED, never refused.
///
/// The Epic-18 scope fence: admission (who may reach this host at all) is FR157 /
/// Story 18.4. If identity resolution started refusing unpinned peers, 18-3b would
/// have silently become an admission gate two stories early.
#[test]
fn ac6_an_unpinned_per_sender_target_is_reported_not_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    let data_dir = tempfile::tempdir().expect("data dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    write_policy(
        workspace.path(),
        INDIVIDUAL,
        "[interaction.overrides.\"drive-by\"]\nresponse_mode = \"notify-and-auto\"\n",
    );
    // A peer with NO pinned key → TrustTier::Unverified.
    std::fs::write(
        workspace.path().join(".rustain").join("a2a.json"),
        r#"{"agents":{"drive-by":{"url":"https://peer.example/a2a"}}}"#,
    )
    .expect("write a2a.json");

    let output = std::process::Command::new(cargo_bin("rustain"))
        .arg("doctor")
        .arg("--json")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .env("RUSTAIN_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"));
    let policy_check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["name"] == "Interaction policy")
        .expect("policy check present");

    // Reported as a warning, and NOT a failure — reporting, never refusing.
    assert_eq!(policy_check["status"], "warning", "{policy_check}");
    let message = policy_check["message"].as_str().expect("message");
    assert!(
        message.contains("UNPINNED") && message.contains("drive-by"),
        "an unpinned per-sender target must be named:\n{message}"
    );

    let senders = policy_check["detail"]["sender_overrides"]
        .as_array()
        .expect("sender_overrides array");
    assert_eq!(senders.len(), 1);
    assert!(
        senders[0]["identity"].get("Unpinned").is_some(),
        "identity must record that the binding is a pseudonym, not a pin: {}",
        senders[0]
    );
}

// ──────────────────────────────────────────────────────────────────
// AC4 — structural ratchet: the delivery path consumes effective policy
// ──────────────────────────────────────────────────────────────────

/// Story 18.3c keeps this structural guard deliberately narrow: `decide` owns
/// relationship consent, while response automation travels through its separate
/// policy answer. The end-to-end sender-mode keystone is
/// `conformance_18_3c_response_modes::ac1_pinned_sender_mode_routes_through_bus_and_unpinned_fails_closed`.
#[test]
fn ac4_ratchet_effective_policy_reaches_delivery_without_overloading_relationship() {
    let source = std::fs::read_to_string("src/domain/ports/agent_message_bus.rs")
        .expect("read agent_message_bus.rs");

    assert!(
        source.contains("fn response_policy(&self, header: &MessageHeader)")
            && source.contains("verified_peer_id"),
        "the delivery policy must derive response automation from the verified sender header"
    );
    assert!(
        source.contains("EffectivePolicy")
            && source.contains("sender_policy_for")
            && source.contains("response_policy"),
        "the delivery policy must consult EffectivePolicy through sender_policy_for"
    );
    assert!(
        source.contains("fn decide(") && source.contains("recipient_ownership: OwnershipKind"),
        "DeliveryPolicy::decide's relationship-disposition signature changed"
    );
    assert!(
        source.contains("relationship_disposition(recipient_ownership)"),
        "response mode must not overload the relationship consent disposition"
    );
}

#[test]
fn ac4_ratchet_daemon_composition_installs_effective_delivery_policy() {
    let source = std::fs::read_to_string("src/adapters/daemon/mod.rs").expect("read daemon/mod.rs");
    let production = source
        .split("async fn run_daemon_foreground(")
        .nth(1)
        .expect("production daemon composition root exists");
    assert!(
        production.contains("EffectiveDeliveryPolicy::new")
            && production.contains("peer_bus_slot_with_policy"),
        "the daemon composition root must install the resolved policy into its one peer bus"
    );
    assert!(
        !production
            .contains("let peer_bus = crate::adapters::daemon::server::default_peer_bus_slot"),
        "production must not silently retain the relationship-only default bus"
    );
}

#[test]
fn ac4_ratchet_startup_names_the_semantic_message_type_deferral() {
    let source = std::fs::read_to_string("src/adapters/daemon/policy_startup.rs")
        .expect("read policy_startup.rs");
    assert!(
        source.contains("MESSAGE_TYPE_DEFERRAL_NOTICE")
            && source.contains("per-sender response mode is enforced")
            && source.contains("semantic message type is not carried"),
        "startup must state that per-sender policy is live while message-type selection remains deferred"
    );
}

// ──────────────────────────────────────────────────────────────────
// AC7 — domain boundary for the merge core
// ──────────────────────────────────────────────────────────────────

/// AC7 — the merge core is effect-free: no async runtime, no I/O, no lock.
///
/// `domain/` is not lexically scanned by the `std::sync` lock ratchet
/// (`tests/conformance.rs:494` covers `src/infrastructure` and `src/adapters`
/// only), so a lock that migrated into this file would escape every existing gate.
/// This test is that gate.
#[test]
fn ac7_team_policy_core_holds_no_io_async_or_lock() {
    let source = std::fs::read_to_string("src/domain/services/team_policy.rs")
        .expect("read domain/services/team_policy.rs");

    // Only the import lines matter — prose in doc comments legitimately mentions
    // `tokio` and locks when explaining why they are absent.
    let imports: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("use "))
        .collect();

    for forbidden in [
        "tokio",
        "ratatui",
        "reqwest",
        "clap",
        "crossterm",
        "arc_swap",
        "figment",
    ] {
        assert!(
            !imports.iter().any(|line| line.contains(forbidden)),
            "domain/services/team_policy.rs imports `{forbidden}`; the merge core must stay \
             effect-free (architecture.md:789)"
        );
    }

    for forbidden in ["Mutex", "RwLock", "std::fs", "std::io"] {
        assert!(
            !imports.iter().any(|line| line.contains(forbidden)),
            "domain/services/team_policy.rs reaches for `{forbidden}`; folding the decision \
             into a shell deletes the test seam (architecture.md:1775)"
        );
    }

    // The core is sync by construction: an `async fn` here would mean the fold
    // moved into an async shell.
    let non_test = source
        .split("#[cfg(test)]")
        .next()
        .expect("source before the test module");
    assert!(
        !non_test.contains("async fn"),
        "the merge core must not be async — the decision belongs outside the shell"
    );
    assert!(
        !non_test.contains(".await"),
        "the merge core must not await — it performs no effects"
    );
}
