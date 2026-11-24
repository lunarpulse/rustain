//! Story C-12 (AC1, AC2): the Messaging channel step of `rustain init`, driven
//! through the real debug binary on a real 80x24 PTY — `startup::run` →
//! `Command::Init` → `init::run_init` → `run_init_with_paths` — with an
//! isolated HOME, config directory and workspace. Every profile the step saves
//! is resolved again with the real `TomlProfileResolver`, the resolver daemon
//! startup uses, so the assertions are on what the daemon would consume.
//!
//! The process environment carries an inert sentinel in every channel
//! credential variable: init must never read, write or show it. Feature-gated
//! cases follow the build (`cfg!(feature = …)`), so the file passes in the
//! default build and with `--features telegram,cron,slack`; the unit tests in
//! `src/adapters/cli/init_channel/tests.rs` cover every feature set by
//! injection.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::domain::models::{PortDimension, ResolvedProfile};
use rustain::domain::ports::ProfileResolver;

const SLACK: bool = cfg!(feature = "slack");
const TELEGRAM: bool = cfg!(feature = "telegram");

/// An inert value in every channel credential variable; never a real token.
const SENTINEL: &str = "C12-INERT-SENTINEL-NOT-A-TOKEN";

const DEADLINE: Duration = Duration::from_secs(60);

const SETUP_POLICY: &str =
    "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"queue\"\n";

/// An isolated HOME with a config directory and a workspace.
struct World {
    tmp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    profiles: PathBuf,
    workspace: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let config = home.join("config");
        let workspace = tmp.path().join("workspace");
        for dir in [&config, &workspace, &home.join("data"), &home.join("logs")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self {
            profiles: config.join("profiles"),
            tmp,
            home,
            config,
            workspace,
        }
    }

    /// An existing setup: answering "n" to the overwrite question then leaves
    /// config, permissions and policy alone, so only the channel step can write.
    fn seeded(active_profile: Option<&str>) -> Self {
        let world = Self::new();
        let config = active_profile.map_or_else(
            || "# operator config\n".to_string(),
            |name| format!("# operator config\nactive_profile = \"{name}\"\n"),
        );
        std::fs::write(world.config.join("config.toml"), config).unwrap();
        let claude = world.workspace.join(".claude");
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            "{\"permissions\":{\"allow\":[]}}",
        )
        .unwrap();
        let policy = world.workspace.join(".rustain");
        std::fs::create_dir_all(&policy).unwrap();
        std::fs::write(policy.join("a2a-interaction.toml"), SETUP_POLICY).unwrap();
        world
    }

    fn profile(&self, name: &str, content: &str) -> PathBuf {
        std::fs::create_dir_all(&self.profiles).unwrap();
        let path = self.profiles.join(format!("{name}.toml"));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn env(&self, command: &mut Command) {
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("xdg-config"))
            .env("XDG_DATA_HOME", self.home.join("xdg-data"))
            .env("RUSTAIN_CONFIG_DIR", &self.config)
            .env("RUSTAIN_DATA_DIR", self.home.join("data"))
            .env(
                "RUSTAIN_LOG_PATH",
                self.home.join("logs").join("rustain.log"),
            )
            .env("TERM", "xterm-256color")
            .env("TELEGRAM_BOT_TOKEN", SENTINEL)
            .env("SLACK_APP_TOKEN", SENTINEL)
            .env("SLACK_BOT_TOKEN", SENTINEL);
    }

    /// The binary without a terminal (stdin closed).
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rustain"));
        self.env(&mut command);
        command.args(args).stdin(Stdio::null());
        command
    }

    /// `rustain <args>` with stdin, stdout and stderr on one 80x24 PTY.
    fn pty(&self, args: &[&str]) -> Pty {
        let (master, slave) = open_pty();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rustain"));
        self.env(&mut command);
        command
            .args(args)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let child = command.spawn().unwrap();
        // The command holds the parent's copies of the slave: drop them, or
        // the master never sees the child's exit.
        drop(command);
        Pty::start(child, master)
    }

    /// Every file under the config directory (profiles included) with its
    /// bytes; a link records its target.
    fn config_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        snapshot(&self.config, &mut out);
        out
    }

    fn profile_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        snapshot(&self.profiles, &mut out);
        out
    }

    fn stored_active_profile(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.config.join("config.toml")).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        table
            .get("active_profile")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    }

    /// The active profile real startup selects with no `--profile` and no
    /// `RUSTAIN_PROFILE`: the stored selection, as `profile list` marks it.
    fn listed_active_profile(&self) -> String {
        let output = self
            .command(&["profile", "list", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        rows.iter()
            .filter(|row| row["active"] == true)
            .filter_map(|row| row["name"].as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// No file anywhere in the world holds the credential sentinel.
    fn assert_no_sentinel(&self) {
        let mut files = BTreeMap::new();
        snapshot(self.tmp.path(), &mut files);
        for (path, bytes) in files {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(SENTINEL),
                "credential sentinel written to {}",
                path.display()
            );
        }
    }
}

fn snapshot(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
        if kind.is_symlink() {
            let target = std::fs::read_link(&path).unwrap();
            out.insert(path, target.into_os_string().into_encoded_bytes());
        } else if kind.is_dir() {
            snapshot(&path, out);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            out.insert(path, bytes);
        }
    }
}

fn open_pty() -> (std::fs::File, std::fs::File) {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills two newly owned descriptors.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors are fresh from openpty and owned here.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

/// A child on a PTY: ordered expectations over the received bytes.
struct Pty {
    child: std::process::Child,
    writer: std::fs::File,
    seen: Arc<Mutex<Vec<u8>>>,
    at: usize,
    done: std::sync::mpsc::Receiver<()>,
}

impl Pty {
    fn start(child: std::process::Child, master: std::fs::File) -> Self {
        let writer = master.try_clone().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done) = std::sync::mpsc::channel();
        let sink = seen.clone();
        let mut reader = master;
        std::thread::spawn(move || {
            let mut block = [0u8; 4096];
            // Linux reports EIO rather than EOF once the slave closes.
            while let Ok(n @ 1..) = reader.read(&mut block) {
                sink.lock().unwrap().extend_from_slice(&block[..n]);
            }
            let _ = done_tx.send(());
        });
        Self {
            child,
            writer,
            seen,
            at: 0,
            done,
        }
    }

    fn transcript(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    /// Wait for `text` after everything matched so far.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            {
                let seen = self.seen.lock().unwrap();
                if let Some(found) = seen[self.at..]
                    .windows(text.len())
                    .position(|window| window == text.as_bytes())
                {
                    self.at += found + text.len();
                    return;
                }
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("{text:?} not seen; transcript:\n{}", self.transcript());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn expect_all(&mut self, texts: &[&str]) {
        for text in texts {
            self.expect(text);
        }
    }

    /// Type one line and press Enter.
    fn send(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\r").unwrap();
        self.writer.flush().unwrap();
    }

    /// End of input (Ctrl+D at the start of a line).
    fn end_of_input(&mut self) {
        self.writer.write_all(b"\x04").unwrap();
        self.writer.flush().unwrap();
    }

    /// Decline the provider sign-in and require a clean exit.
    fn decline_sign_in(mut self) -> String {
        self.expect("Sign in to a model provider now? [y/n] ");
        self.send("n");
        if self.done.recv_timeout(DEADLINE).is_err() {
            let _ = self.child.kill();
            panic!("init did not exit; transcript:\n{}", self.transcript());
        }
        let status = self.child.wait().unwrap();
        let text = self.transcript();
        assert!(status.success(), "{status:?}: {text}");
        assert!(
            !text.contains(SENTINEL),
            "credential sentinel shown: {text}"
        );
        text
    }
}

/// The setup questions before the channel step. `Some(answer)`: an existing
/// setup asks to overwrite first; the policy questions follow a fresh setup or
/// an accepted overwrite.
fn answer_setup(pty: &mut Pty, overwrite: Option<bool>) {
    if let Some(overwrite) = overwrite {
        pty.expect("Configuration already exists. Overwrite? [y/n] ");
        pty.send(if overwrite { "y" } else { "n" });
        if !overwrite {
            return;
        }
    }
    for _ in 0..2 {
        pty.expect("Selection [1]: ");
        pty.send("");
    }
}

/// The step's lines (from its header to the sign-in offer) fit 80 columns,
/// except lines carrying a path of this world or echoed ID input.
fn assert_step_fits(transcript: &str, world: &World) {
    let root = world.tmp.path().display().to_string();
    let start = transcript.find("Messaging channel (optional)").unwrap();
    let end = transcript.find("Sign in to a model provider").unwrap();
    for line in transcript[start..end].split('\n') {
        let line = line.trim_end_matches('\r');
        if line.contains(&root) || line.contains("(comma-separated;") {
            continue;
        }
        assert!(line.chars().count() <= 80, "over 80 columns: {line:?}");
    }
}

fn resolve(world: &World, name: &str) -> ResolvedProfile {
    TomlProfileResolver::new(name, world.profiles.clone())
        .unwrap_or_else(|error| panic!("{name}: {error}"))
        .resolve_active()
        .unwrap()
}

fn channel_config(profile: &ResolvedProfile) -> (String, Option<toml::Table>) {
    let reference = &profile.selection.dimensions[&PortDimension::Channels];
    (
        reference.adapter.clone(),
        reference
            ._config
            .as_ref()
            .and_then(toml::Value::as_table)
            .cloned(),
    )
}

fn assert_same_except_channel(want: &ResolvedProfile, got: &ResolvedProfile, what: &str) {
    assert_eq!(
        want.selection.dimensions.len(),
        got.selection.dimensions.len(),
        "{what}"
    );
    for (port, reference) in &want.selection.dimensions {
        if *port == PortDimension::Channels {
            continue;
        }
        let other = &got.selection.dimensions[port];
        assert_eq!(
            (&other.adapter, &other._config),
            (&reference.adapter, &reference._config),
            "{what} {port:?}"
        );
    }
    assert_eq!(
        serde_json::to_value(&got.overrides).unwrap(),
        serde_json::to_value(&want.overrides).unwrap(),
        "{what} overrides"
    );
}

#[cfg(feature = "slack")]
const WORK_ASSISTANT: &str = "# Dana's assistant\nname = \"work-assistant\"\n\
                              description = \"Dana's work assistant\"\nextends = \"coding\"\n\n\
                              [overrides]\ndefault_plan_mode = true\n";

const TRAVEL: &str = "name = \"travel\"\nextends = \"base\"\n";

/// Dana's path: Slack, a pasted conversation ID corrected by position, a
/// deduplicated list, count and destination, explicit confirmation, the
/// summary — then a rerun where Enter keeps it.
#[cfg(feature = "slack")]
#[test]
fn dana_saves_slack_to_her_user_profile_and_a_rerun_keeps_it() {
    let world = World::seeded(None);
    let path = world.profile("work-assistant", WORK_ASSISTANT);
    let before = resolve(&world, "work-assistant");

    let mut pty = world.pty(&["--profile", "work-assistant", "init"]);
    answer_setup(&mut pty, Some(false));
    pty.expect_all(&[
        "Messaging channel (optional) — profile: work-assistant",
        "Current channel: none",
        "  1. Telegram — ",
        "  2. Slack — allowed app DM user IDs",
        "  3. Skip [Enter]",
        "Selection [3]: ",
    ]);
    pty.send("2");
    pty.expect("Slack user IDs (comma-separated; Enter to go back): ");
    pty.send("C012ABCDEF");
    pty.expect("Entry 1 must be a Slack user ID, not a channel ID.");
    pty.expect("Slack user IDs (comma-separated; Enter to go back): ");
    pty.send("U012ABCDEF, W0345GH ,U012ABCDEF");
    pty.expect_all(&[
        "Review:",
        "  Adapter: Slack (app DMs over Socket Mode)",
        "  Profile: work-assistant",
        "  File: ",
        "  Allowed Slack users: 2",
        "Save, replacing this profile's channel settings? [y/N] ",
    ]);
    pty.send("y");
    pty.expect_all(&[
        "rustain init completed!",
        "Messaging channel:",
        "  Slack settings saved (not connected).",
        "  Profile: work-assistant",
        "  File: ",
        "  Before starting the daemon, set SLACK_APP_TOKEN and SLACK_BOT_TOKEN in",
        "    rustain --profile work-assistant daemon start",
        "  A supervised daemon needs these variables in its own environment.",
        "  restart it with this profile.",
    ]);
    let transcript = pty.decline_sign_in();
    assert_step_fits(&transcript, &world);
    // The corrective line names the position only; the pasted value appears
    // only as the terminal's echo of the operator's own input.
    let corrective: Vec<&str> = transcript
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| line.contains("Entry 1"))
        .collect();
    assert_eq!(
        corrective,
        ["Entry 1 must be a Slack user ID, not a channel ID."]
    );

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with(WORK_ASSISTANT), "{saved}");
    let after = resolve(&world, "work-assistant");
    assert_eq!(
        channel_config(&after),
        (
            "slack".to_string(),
            Some(toml::toml! { allowed_user_ids = ["U012ABCDEF", "W0345GH"] })
        )
    );
    assert_same_except_channel(&before, &after, "work-assistant");

    // Rerun: Enter keeps the saved channel byte for byte.
    let saved_state = world.config_snapshot();
    let mut pty = world.pty(&["--profile", "work-assistant", "init"]);
    answer_setup(&mut pty, Some(false));
    pty.expect_all(&[
        "Current channel: Slack",
        "  1. Telegram — ",
        "  2. Slack — edit allowed app DM user IDs",
        "  3. Keep existing [Enter]",
        "Selection [3]: ",
    ]);
    pty.send("");
    pty.expect("  Existing Slack configuration kept.");
    pty.decline_sign_in();
    assert_eq!(world.config_snapshot(), saved_state);
    world.assert_no_sentinel();
}

/// A child inheriting Telegram from a user parent: the child gets its own
/// channel table (safe sibling kept, allowlist replaced), the parent never
/// changes, and an accepted global overwrite keeps the stored selection.
#[cfg(feature = "telegram")]
#[test]
fn telegram_edits_the_child_never_the_parent_and_keeps_the_stored_selection() {
    let world = World::seeded(Some("travel"));
    world.profile("travel", TRAVEL);
    let parent = "name = \"team\"\nextends = \"base\"\n\n[channels]\nadapter = \"telegram\"\n\n\
                  [channels.config]\nallowed_chat_ids = [5]\npoll_timeout = 30\n";
    let parent_path = world.profile("team", parent);
    let child = "name = \"work\"\nextends = \"team\"\n\n[persona]\nadapter = \"minimal\"\n";
    let child_path = world.profile("work", child);

    let mut pty = world.pty(&["--profile", "work", "init"]);
    answer_setup(&mut pty, Some(true));
    pty.expect_all(&[
        "Messaging channel (optional) — profile: work",
        "Current channel: Telegram",
        "  1. Telegram — edit allowed DM chat IDs",
        "  3. Keep existing [Enter]",
        "Selection [3]: ",
    ]);
    pty.send("1");
    pty.expect("Telegram chat IDs (comma-separated; Enter to go back): ");
    pty.send("-100123");
    pty.expect("Entry 1 must be a positive chat ID; group IDs are not accepted.");
    pty.expect("Telegram chat IDs (comma-separated; Enter to go back): ");
    pty.send("42, 7");
    pty.expect_all(&[
        "  Adapter: Telegram (bot DMs)",
        "  Profile: work",
        "  Allowed Telegram chats: 2",
        "Save, replacing this profile's channel settings? [y/N] ",
    ]);
    pty.send("yes");
    pty.expect_all(&[
        "  Telegram settings saved (not connected).",
        "  Before starting the daemon, set TELEGRAM_BOT_TOKEN in",
        "    rustain --profile work daemon start",
    ]);
    let transcript = pty.decline_sign_in();
    assert_step_fits(&transcript, &world);

    assert_eq!(std::fs::read_to_string(&parent_path).unwrap(), parent);
    assert!(
        std::fs::read_to_string(&child_path)
            .unwrap()
            .starts_with(child)
    );
    assert_eq!(
        channel_config(&resolve(&world, "work")),
        (
            "telegram".to_string(),
            Some(toml::toml! { allowed_chat_ids = [42, 7]
            poll_timeout = 30 })
        )
    );
    assert_eq!(
        channel_config(&resolve(&world, "team")).1,
        Some(toml::toml! { allowed_chat_ids = [5]
        poll_timeout = 30 })
    );
    let config = std::fs::read_to_string(world.config.join("config.toml")).unwrap();
    assert!(
        config.contains("# Generated by rustain init"),
        "overwrite accepted"
    );
    assert_eq!(world.stored_active_profile().as_deref(), Some("travel"));
    assert_eq!(world.listed_active_profile(), "travel");
    world.assert_no_sentinel();
}

/// Owner decision 2026-11-19: an embedded preset gets a separately named new
/// profile after its own confirmation; the preset, the stored selection and
/// every non-channel setting stay as they resolve today.
#[cfg(feature = "slack")]
#[test]
fn embedded_coding_gets_a_named_derivative_and_nothing_else_moves() {
    let world = World::seeded(Some("travel"));
    world.profile("travel", TRAVEL);
    let coding = resolve(&world, "coding");

    let mut pty = world.pty(&["--profile", "coding", "init"]);
    answer_setup(&mut pty, Some(true));
    pty.expect_all(&[
        "Messaging channel (optional) — profile: coding",
        "Current channel: none",
        "Selection [3]: ",
    ]);
    pty.send("2");
    pty.expect_all(&[
        "'coding' is a built-in profile; init does not change it.",
        "  coding-channel",
        "Use new profile 'coding-channel' for this channel? [y/N] ",
    ]);
    pty.send("y");
    pty.expect("Slack user IDs (comma-separated; Enter to go back): ");
    pty.send("U012ABCDEF");
    pty.expect_all(&[
        "  Profile: coding-channel (new)",
        "  Allowed Slack users: 1",
        "Create 'coding-channel' with these channel settings? [y/N] ",
    ]);
    pty.send("y");
    pty.expect_all(&[
        "  Slack settings saved (not connected).",
        "  Profile: coding-channel",
        "    rustain --profile coding-channel daemon start",
    ]);
    let transcript = pty.decline_sign_in();
    assert_step_fits(&transcript, &world);

    assert!(
        !world.profiles.join("coding.toml").exists(),
        "never shadowed"
    );
    let derived = resolve(&world, "coding-channel");
    assert!(!derived.preview);
    assert_same_except_channel(&coding, &derived, "coding-channel");
    assert_eq!(
        channel_config(&derived),
        (
            "slack".to_string(),
            Some(toml::toml! { allowed_user_ids = ["U012ABCDEF"] })
        )
    );
    let again = resolve(&world, "coding");
    assert_same_except_channel(&coding, &again, "coding");
    assert_eq!(channel_config(&again).0, channel_config(&coding).0);
    assert_eq!(world.stored_active_profile().as_deref(), Some("travel"));
    assert_eq!(world.listed_active_profile(), "travel");
    world.assert_no_sentinel();
}

/// The personal-assistant derivative keeps the preset's *effective* baseline:
/// scheduler `none` (never its dormant raw `cron`), no `extends`/`preview`.
#[cfg(feature = "telegram")]
#[test]
fn personal_assistant_derivative_keeps_scheduler_none() {
    let world = World::new();
    let preset = resolve(&world, "personal-assistant");
    assert!(preset.preview);

    let mut pty = world.pty(&["--profile", "personal-assistant", "init"]);
    answer_setup(&mut pty, None);
    pty.expect("Selection [3]: ");
    pty.send("1");
    pty.expect("Use new profile 'personal-assistant-channel' for this channel? [y/N] ");
    pty.send("y");
    pty.expect("Telegram chat IDs (comma-separated; Enter to go back): ");
    pty.send("42");
    pty.expect("Create 'personal-assistant-channel' with these channel settings? [y/N] ");
    pty.send("y");
    pty.expect("    rustain --profile personal-assistant-channel daemon start");
    let transcript = pty.decline_sign_in();
    assert_step_fits(&transcript, &world);

    let path = world.profiles.join("personal-assistant-channel.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    assert!(
        table.get("extends").is_none() && table.get("preview").is_none(),
        "{text}"
    );
    assert!(!text.contains("cron"), "{text}");
    let derived = resolve(&world, "personal-assistant-channel");
    assert!(!derived.preview);
    assert_same_except_channel(&preset, &derived, "personal-assistant-channel");
    assert_eq!(
        derived.selection.dimensions[&PortDimension::Scheduler].adapter,
        "none"
    );
    assert_eq!(
        channel_config(&derived),
        (
            "telegram".to_string(),
            Some(toml::toml! { allowed_chat_ids = [42] })
        )
    );
    let again = resolve(&world, "personal-assistant");
    assert!(again.preview);
    assert_eq!(channel_config(&again).0, "terminal");
    assert_eq!(
        again.selection.dimensions[&PortDimension::Scheduler].adapter,
        "none"
    );
    world.assert_no_sentinel();
}

enum Step<'a> {
    Expect(&'a str),
    Send(&'a str),
    EndOfInput,
}

/// Skip, back, invalid input, decline, default No and end of input — and the
/// global overwrite's "y" alone — leave every profile byte unchanged. The
/// saves above make these zero-write results non-vacuous.
#[test]
fn no_confirmed_save_means_no_profile_byte_changes() {
    let world = World::seeded(Some("travel"));
    world.profile("travel", TRAVEL);
    world.profile("work", "name = \"work\"\nextends = \"base\"\n");
    let work_before = resolve(&world, "work");
    let Some((number, valid, invalid)) = (if SLACK {
        Some(("2", "U012ABCDEF", "C012ABCDEF"))
    } else if TELEGRAM {
        Some(("1", "42", "-5"))
    } else {
        None
    }) else {
        // Neither channel feature: a short notice replaces the menu.
        let before = world.config_snapshot();
        let mut pty = world.pty(&["--profile", "work", "init"]);
        answer_setup(&mut pty, Some(false));
        pty.expect_all(&[
            "Current channel: none",
            "Telegram and Slack are unavailable in this build (they need the",
            "  Channel setup skipped.",
        ]);
        let transcript = pty.decline_sign_in();
        assert!(!transcript.contains("Selection [3]"), "{transcript}");
        assert_eq!(world.config_snapshot(), before);
        return;
    };
    let ids = "(comma-separated; Enter to go back): ";
    let confirm = "[y/N] ";
    let cases: Vec<(&str, Vec<Step>)> = vec![
        ("enter", vec![Step::Send("")]),
        ("end at menu", vec![Step::EndOfInput]),
        (
            "back",
            vec![
                Step::Send(number),
                Step::Expect(ids),
                Step::Send(""),
                Step::Expect("Selection [3]: "),
                Step::Send(""),
            ],
        ),
        (
            "invalid then back",
            vec![
                Step::Send(number),
                Step::Expect(ids),
                Step::Send(invalid),
                Step::Expect("Entry 1 must be"),
                Step::Expect(ids),
                Step::Send(""),
                Step::Expect("Selection [3]: "),
                Step::Send(""),
            ],
        ),
        (
            "end at ids",
            vec![Step::Send(number), Step::Expect(ids), Step::EndOfInput],
        ),
        (
            "decline",
            vec![
                Step::Send(number),
                Step::Expect(ids),
                Step::Send(valid),
                Step::Expect(confirm),
                Step::Send("n"),
            ],
        ),
        (
            "default no",
            vec![
                Step::Send(number),
                Step::Expect(ids),
                Step::Send(valid),
                Step::Expect(confirm),
                Step::Send(""),
            ],
        ),
        (
            "end at confirmation",
            vec![
                Step::Send(number),
                Step::Expect(ids),
                Step::Send(valid),
                Step::Expect(confirm),
                Step::EndOfInput,
            ],
        ),
    ];
    let before = world.config_snapshot();
    for (name, steps) in cases {
        let mut pty = world.pty(&["--profile", "work", "init"]);
        answer_setup(&mut pty, Some(false));
        pty.expect("Selection [3]: ");
        for step in steps {
            match step {
                Step::Expect(text) => pty.expect(text),
                Step::Send(line) => pty.send(line),
                Step::EndOfInput => pty.end_of_input(),
            }
        }
        pty.expect("  Channel setup skipped.");
        pty.decline_sign_in();
        assert_eq!(world.config_snapshot(), before, "{name}");
    }

    // The global overwrite's "y" is not channel consent.
    let profiles = world.profile_snapshot();
    let mut pty = world.pty(&["--profile", "work", "init"]);
    answer_setup(&mut pty, Some(true));
    pty.expect("Selection [3]: ");
    pty.send(number);
    pty.expect(ids);
    pty.send(valid);
    pty.expect(confirm);
    pty.send("");
    pty.expect("  Channel setup skipped.");
    let transcript = pty.decline_sign_in();
    assert_step_fits(&transcript, &world);
    assert_eq!(world.profile_snapshot(), profiles);
    assert_eq!(world.stored_active_profile().as_deref(), Some("travel"));
    assert_eq!(world.listed_active_profile(), "travel");
    let work_after = resolve(&world, "work");
    assert_same_except_channel(&work_before, &work_after, "work");
    assert_eq!(channel_config(&work_after), channel_config(&work_before));
    world.assert_no_sentinel();
}

/// Mutant controls: an unresolved name never falls back to `coding` as the
/// destination; a bypassed file shadowing a built-in is not treated as the
/// built-in; links, community files, user previews and occupied derivative
/// names are refused; an inline credential is named, never shown or copied.
/// Each refusal comes before any prompt and changes no byte.
#[test]
fn refused_destinations_are_explained_before_any_prompt() {
    let world = World::seeded(None);
    let outside = world.home.join("outside.toml");
    std::fs::write(&outside, "name = \"linked\"\nextends = \"base\"\n").unwrap();
    std::fs::create_dir_all(world.profiles.join("community")).unwrap();
    std::os::unix::fs::symlink(&outside, world.profiles.join("linked.toml")).unwrap();
    std::fs::write(
        world.profiles.join("community").join("club.toml"),
        "name = \"club\"\nextends = \"base\"\n",
    )
    .unwrap();
    world.profile(
        "draft",
        "name = \"draft\"\nextends = \"base\"\npreview = true\n",
    );
    world.profile(
        "legacy",
        &format!(
            "name = \"legacy\"\nextends = \"base\"\n[channels]\nadapter = \"telegram\"\n\
             [channels.config]\nbot_token = \"{SENTINEL}\"\nallowed_chat_ids = [1]\n"
        ),
    );
    world.profile(
        "base-channel",
        "name = \"base-channel\"\nextends = \"base\"\n",
    );
    world.profile(
        "coding",
        &format!(
            "name = \"coding\"\nextends = \"base\"\n#{}\n",
            "x".repeat(1 << 20)
        ),
    );

    let cases = [
        (
            "ghost",
            "Profile 'ghost' was not found; no channel was set up.",
        ),
        (
            "coding",
            "This profile file is skipped when profiles load, so init will",
        ),
        (
            "linked",
            "Init does not write through a link or to a non-regular file:",
        ),
        (
            "club",
            "Profile 'club' is a community profile; init does not edit it:",
        ),
        (
            "draft",
            "Profile 'draft' is a preview profile; init does not change it:",
        ),
        (
            "legacy",
            "delete the bot_token line by hand, then run init again.",
        ),
        (
            "base",
            "Profile 'base-channel' already exists, so init will not create it.",
        ),
    ];
    let before = world.config_snapshot();
    let outside_before = std::fs::read(&outside).unwrap();
    for (name, refusal) in cases {
        let mut pty = world.pty(&["--profile", name, "init"]);
        answer_setup(&mut pty, Some(false));
        pty.expect(&format!("Messaging channel (optional) — profile: {name}"));
        pty.expect(refusal);
        pty.expect("rustain init completed!");
        let transcript = pty.decline_sign_in();
        let step = &transcript[transcript.find("Messaging channel (optional)").unwrap()..];
        assert!(!step.contains("Selection [3]"), "{name}: {transcript}");
        if name == "legacy" {
            assert!(step.contains("Put the token in TELEGRAM_BOT_TOKEN in the"));
            assert!(step.contains("  Existing Telegram configuration kept."));
        }
        assert_eq!(world.config_snapshot(), before, "{name}");
    }
    assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
    for never in ["ghost.toml", "ghost-channel.toml", "coding-channel.toml"] {
        assert!(!world.profiles.join(never).exists(), "{never}");
    }
    // The only sentinel on disk is the operator's own legacy file.
    let legacy = world.profiles.join("legacy.toml");
    let mut files = BTreeMap::new();
    snapshot(world.tmp.path(), &mut files);
    for (path, bytes) in files {
        if path != legacy {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(SENTINEL),
                "{path:?}"
            );
        }
    }
}

/// AC1 ordering: a profile selecting a channel whose feature is missing still
/// fails closed through the normal front doors, while `init` reaches its
/// channel step and keeps it; an unrelated validation error still stops init.
#[test]
fn a_missing_channel_feature_blocks_startup_but_not_init_keep() {
    let world = World::seeded(None);
    let chat = world.profile(
        "chat",
        "name = \"chat\"\nextends = \"base\"\n[channels]\nadapter = \"slack\"\n\
         [channels.config]\nallowed_user_ids = [\"U012ABCDEF\"]\n",
    );
    let list = world
        .command(&["--profile", "chat", "profile", "list", "--json"])
        .output()
        .unwrap();
    if SLACK {
        assert!(list.status.success(), "{list:?}");
    } else {
        assert_eq!(list.status.code(), Some(2), "{list:?}");
        assert!(String::from_utf8_lossy(&list.stderr).contains("Profile load failed"));
        let daemon = world
            .command(&["--profile", "chat", "daemon", "start"])
            .output()
            .unwrap();
        assert_eq!(daemon.status.code(), Some(2), "{daemon:?}");
        assert!(String::from_utf8_lossy(&daemon.stderr).contains("Profile load failed"));
    }

    let before = world.config_snapshot();
    let mut pty = world.pty(&["--profile", "chat", "init"]);
    answer_setup(&mut pty, Some(false));
    pty.expect("Messaging channel (optional) — profile: chat");
    if SLACK {
        pty.expect("Current channel: Slack\r\n");
    } else {
        pty.expect("Current channel: Slack (unavailable in this build)");
    }
    if SLACK || TELEGRAM {
        pty.expect("Selection [3]: ");
        pty.send("");
    }
    pty.expect("  Existing Slack configuration kept.");
    pty.decline_sign_in();
    assert_eq!(world.config_snapshot(), before);
    assert!(chat.exists());

    world.profile(
        "broken",
        "name = \"broken\"\nextends = \"base\"\n[channels]\nadapter = \"slack\"\n\
         [scheduler]\nadapter = \"cronn\"\n",
    );
    let init = world
        .command(&["--profile", "broken", "init"])
        .output()
        .unwrap();
    assert_eq!(init.status.code(), Some(2), "{init:?}");
    assert!(String::from_utf8_lossy(&init.stderr).contains("Profile load failed"));
}

/// AC1: stdin on a terminal with stdout redirected skips only the channel
/// step: no channel prompt, no read, and init still exits on its own.
#[test]
fn redirected_stdout_skips_only_the_channel_step() {
    let world = World::seeded(None);
    world.profile("work", "name = \"work\"\nextends = \"base\"\n");
    let before = world.config_snapshot();
    let (master, slave) = open_pty();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustain"));
    world.env(&mut command);
    command
        .args(["--profile", "work", "init"])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    let mut child = command.spawn().unwrap();
    drop(command);
    let mut writer = master.try_clone().unwrap();
    // Only the overwrite answer: a channel prompt would wait for more.
    writer.write_all(b"n\r").unwrap();
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("init waited for input with stdout redirected");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    drop(master);
    assert!(status.success(), "{status:?}: {stdout}");
    assert!(stdout.contains("Messaging channel setup skipped (output is not a terminal)."));
    assert!(!stdout.contains("Selection [3]"), "{stdout}");
    assert!(stdout.contains("  Channel setup skipped."), "{stdout}");
    assert!(stdout.contains("Skipping provider sign-in (not an interactive terminal)."));
    assert_eq!(world.config_snapshot(), before);
}
