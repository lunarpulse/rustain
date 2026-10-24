//! Story 18.9c: offline consent through the real CLI, never the runtime bypass.

use std::path::{Path, PathBuf};
use std::process::Output;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustain::adapters::policy::{JournalConsentProjection, resolve_workspace_policy};
use rustain::adapters::tui::handlers::team_command::render_team_status;
use rustain::domain::models::{JournalRecord, PeerId, PrincipalKind, RoomEvent};
use rustain::domain::ports::RoomJournalReader as _;
use rustain::infrastructure::subagent::node_journal::{NodeJournal, WorkspaceJournalReader};

struct Fixture {
    workspace: tempfile::TempDir,
    data: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            workspace: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        }
    }

    fn ws(&self) -> &Path {
        self.workspace.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        assert_cmd::Command::cargo_bin("rustain")
            .unwrap()
            .args(["--profile", "coding"])
            .args(args)
            .current_dir(self.ws())
            .env("RUSTAIN_DATA_DIR", self.data.path())
            .env("RUSTAIN_CONFIG_DIR", self.config.path())
            .env("HOME", self.config.path())
            .output()
            .expect("run real rustain binary")
    }

    fn roster(&self) {
        std::fs::create_dir_all(self.ws().join(".rustain")).unwrap();
        std::fs::write(
            self.ws().join(".rustain/a2a.json"),
            serde_json::json!({"agents": {
                "alice": {"url": "https://alice.example/a2a", "pinnedKey": {
                    "alg": "EdDSA", "x": URL_SAFE_NO_PAD.encode([7u8; 32])
                }},
                "bob": {"url": "https://bob.example/a2a"}
            }})
            .to_string(),
        )
        .unwrap();
    }

    async fn grant(&self, sender: &PeerId) -> PathBuf {
        let journal = NodeJournal::open_workspace(self.ws()).await.unwrap();
        journal
            .append_room(RoomEvent::ConsentGranted {
                sender: Some(sender.clone()),
                granted_at: 10,
                principal: Some(PrincipalKind::Pinned),
            })
            .await
            .unwrap();
        journal.path().to_path_buf()
    }
}

fn stdout_ok(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn refused(output: Output, prefix: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(prefix), "{stderr}");
}

/// Include directory names and file bytes, so a read/no-op cannot hide writes.
fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, dir: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let is_dir = path.is_dir();
            entries.push((
                path.strip_prefix(root).unwrap().to_path_buf(),
                (!is_dir).then(|| std::fs::read(&path).unwrap()),
            ));
            if is_dir {
                visit(root, &path, entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[tokio::test]
async fn status_prints_the_tui_renderer_byte_for_byte() {
    let fixture = Fixture::new();
    fixture.roster();
    fixture
        .grant(&PeerId::from_public_key(&[7u8; 32]).unwrap())
        .await;
    std::fs::write(
        fixture.ws().join(".rustain/a2a-interaction.toml"),
        "[interaction.defaults]\nresponse_mode = \"notify-and-draft\"\nnotification = \"queue\"\n",
    )
    .unwrap();
    let peers = rustain::adapters::a2a::config::parse_workspace_a2a_config(
        &fixture.ws().join(".rustain/a2a.json"),
    )
    .unwrap();
    let projection = JournalConsentProjection::load_workspace(fixture.ws())
        .await
        .unwrap();
    let (policy, _) = resolve_workspace_policy(fixture.ws(), &peers, &projection).unwrap();
    let before = snapshot(fixture.ws());
    assert_eq!(
        stdout_ok(fixture.run(&["team", "status"])),
        format!("{}\n", render_team_status(&policy, &projection, &peers))
    );
    assert_eq!(snapshot(fixture.ws()), before);

    let empty = Fixture::new();
    let before = snapshot(empty.ws());
    let projection = JournalConsentProjection::load_workspace(empty.ws())
        .await
        .unwrap();
    let (policy, _) = resolve_workspace_policy(empty.ws(), &[], &projection).unwrap();
    let expected = format!("{}\n", render_team_status(&policy, &projection, &[]));
    assert!(expected.ends_with("Peers: no known peers.\n"));
    assert_eq!(stdout_ok(empty.run(&["team", "status"])), expected);
    assert_eq!(snapshot(empty.ws()), before);
}

#[tokio::test]
async fn untrust_revokes_once_then_is_a_no_op() {
    let fixture = Fixture::new();
    fixture.roster();
    let alice = PeerId::from_public_key(&[7u8; 32]).unwrap();
    let path = fixture.grant(&alice).await;
    assert_eq!(
        stdout_ok(fixture.run(&["team", "untrust", "alice"])),
        format!("Revoked trust for alice ({alice}). Future tasks require consent.\n")
    );
    let entries = WorkspaceJournalReader::open_workspace(fixture.ws())
        .load_entries()
        .await
        .unwrap();
    assert_eq!(entries.len(), 2, "exactly one revoke is appended");
    assert!(matches!(
        &entries.last().unwrap().record,
        JournalRecord::Room(RoomEvent::ConsentRevoked { sender: Some(peer), revoked_at })
            if peer == &alice && *revoked_at >= 10
    ));
    assert!(
        stdout_ok(fixture.run(&["team", "status"]))
            .contains(&format!("- alice ({alice}): revoked"))
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        stdout_ok(fixture.run(&["team", "untrust", "alice"])),
        format!("No active grant for alice ({alice}); nothing changed.\n")
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let empty = Fixture::new();
    let never = PeerId::from_public_key(&[8u8; 32]).unwrap();
    let before = snapshot(empty.ws());
    assert_eq!(
        stdout_ok(empty.run(&["team", "untrust", never.as_str()])),
        format!("No active grant for {never} ({never}); nothing changed.\n")
    );
    assert_eq!(snapshot(empty.ws()), before);
}

#[tokio::test]
async fn refusals_are_named_and_write_nothing() {
    let fixture = Fixture::new();
    fixture.roster();
    let before = snapshot(fixture.ws());
    for target in ["bob", "nobody"] {
        refused(
            fixture.run(&["team", "untrust", target]),
            &format!(
                "rustain: team untrust failed: '{target}' has no usable pinned identity. Standing consent must key on a transport-authenticated PeerId, never a rename-unstable alias. Pin the peer (pinned_key) or supply a full PeerId."
            ),
        );
        assert_eq!(snapshot(fixture.ws()), before);
        assert!(!fixture.ws().join(".rustain/rooms").exists());
    }
    std::fs::write(
        fixture.ws().join(".rustain/a2a-interaction.toml"),
        "[invalid",
    )
    .unwrap();
    let before = snapshot(fixture.ws());
    refused(
        fixture.run(&["team", "status"]),
        "rustain: team status failed:",
    );
    assert_eq!(snapshot(fixture.ws()), before);
    for (args, expected) in [
        (vec!["team", "trust"], "unrecognized subcommand"),
        (vec!["team", "send", "a", "b"], "unrecognized subcommand"),
        (
            vec!["team", "untrust"],
            "required arguments were not provided",
        ),
    ] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    let help = stdout_ok(fixture.run(&["team", "--help"]));
    for verb in ["log", "status", "untrust"] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(&format!("{verb} "))),
            "{help}"
        );
    }

    // A directory at the journal path deterministically fails reads, even as root.
    let unreadable = Fixture::new();
    let rooms = unreadable.ws().join(".rustain/rooms");
    std::fs::create_dir_all(rooms.join(format!(
        "room-{}.jsonl",
        rustain::infrastructure::paths::workspace_hash(unreadable.ws())
    )))
    .unwrap();
    let before = snapshot(unreadable.ws());
    refused(
        unreadable.run(&["team", "status"]),
        "rustain: team status failed:",
    );
    let alice = PeerId::from_public_key(&[7u8; 32]).unwrap();
    refused(
        unreadable.run(&["team", "untrust", alice.as_str()]),
        "rustain: team untrust failed:",
    );
    assert_eq!(snapshot(unreadable.ws()), before);

    // The fold succeeds, but a directory at the writer's lock path refuses append.
    let unwritable = Fixture::new();
    unwritable.roster();
    let journal = unwritable.grant(&alice).await;
    let lock = journal.with_extension("lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    let before = snapshot(unwritable.ws());
    refused(
        unwritable.run(&["team", "untrust", "alice"]),
        "rustain: team untrust failed:",
    );
    assert_eq!(snapshot(unwritable.ws()), before);
}

#[tokio::test]
async fn a_profile_only_peer_resolves_through_the_active_profile() {
    let fixture = Fixture::new();
    let profiles = fixture.config.path().join("profiles");
    std::fs::create_dir(&profiles).unwrap();
    std::fs::write(
        profiles.join("coding.toml"),
        format!(
            r#"
name = "coding"
description = "CLI consent profile-only roster fixture"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
[tools.config.a2a.prof-peer]
url = "https://profile.example/a2a"
pinned_key = {{ alg = "EdDSA", x = "{}" }}
"#,
            URL_SAFE_NO_PAD.encode([9u8; 32])
        ),
    )
    .unwrap();
    let peer = PeerId::from_public_key(&[9u8; 32]).unwrap();
    fixture.grant(&peer).await;
    assert!(!fixture.ws().join(".rustain/a2a.json").exists());
    assert!(
        stdout_ok(fixture.run(&["team", "status"]))
            .contains(&format!("- prof-peer ({peer}): trusted (journaled)"))
    );
    assert_eq!(
        stdout_ok(fixture.run(&["team", "untrust", "prof-peer"])),
        format!("Revoked trust for prof-peer ({peer}). Future tasks require consent.\n")
    );
}
