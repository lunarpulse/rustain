//! Story C-12 (AC1, AC2) unit witnesses of the channel step. Feature
//! availability is injected, so every menu variant and both saved formats run
//! in every build; the real binary is driven by `tests/conformance_c12_init_channel.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::destination::{Destination, Refusal, inspect, splice_channels};
use super::*;
use crate::adapters::profile_resolver::toml_resolver::{
    FileSystemProfileSource, TomlProfileResolver,
};
use crate::domain::models::{PortDimension, ResolvedProfile};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::profile_loader::ProfileLoader;

const BOTH: ChannelFeatures = ChannelFeatures {
    telegram: true,
    slack: true,
};
const NEITHER: ChannelFeatures = ChannelFeatures {
    telegram: false,
    slack: false,
};
const TELEGRAM_ONLY: ChannelFeatures = ChannelFeatures {
    telegram: true,
    slack: false,
};
const SLACK_ONLY: ChannelFeatures = ChannelFeatures {
    telegram: false,
    slack: true,
};

/// An inert sentinel standing in for a credential; never a real token.
const SENTINEL: &str = "C12-INERT-SENTINEL-NOT-A-TOKEN";

const WORK: &str = "# Dana's work assistant\nname = \"work\"\ndescription = \"Work\"\n\
                    extends = \"base\"\n\n[persona]\nadapter = \"minimal\"\n\n\
                    [overrides]\nmodel = \"work-model\"\n";

struct World {
    _tmp: tempfile::TempDir,
    profiles: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        Self {
            _tmp: tmp,
            profiles,
        }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.profiles.join(format!("{name}.toml"));
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Every file under the world with its bytes (links as their target text).
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
                if kind.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    out.insert(path, target.to_string_lossy().into_owned().into_bytes());
                } else if kind.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(self._tmp.path(), &mut out);
        out
    }
}

struct Run {
    outcome: ChannelOutcome,
    text: String,
    read: u64,
}

fn drive(target: &Target, features: ChannelFeatures, answers: &str) -> Run {
    let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
    let mut output = Vec::new();
    let outcome = run_channel_step(target, features, &mut input, &mut output).unwrap();
    Run {
        outcome,
        text: String::from_utf8(output).unwrap(),
        read: input.position(),
    }
}

fn load(profiles: &Path, name: &str) -> ResolvedProfile {
    ProfileLoader::new(
        &AdapterCatalog,
        &FileSystemProfileSource::new(profiles.to_path_buf()),
    )
    .load_ignoring_channel_feature(name)
    .unwrap()
}

fn channel(profile: &ResolvedProfile) -> (String, Option<toml::Value>) {
    let reference = &profile.selection.dimensions[&PortDimension::Channels];
    (reference.adapter.clone(), reference._config.clone())
}

fn assert_fits(text: &str, skip: &Path) {
    let skip = skip.display().to_string();
    // The scripted input has no echoed Enter, so consecutive prompts share a line;
    // on a terminal each answer ends its prompt's line.
    let text = text
        .replace("[y/N] ", "[y/N] \n")
        .replace("]: ", "]: \n")
        .replace("go back): ", "go back): \n");
    for line in text.lines().filter(|line| !line.contains(&skip)) {
        assert!(line.chars().count() <= 80, "over 80 columns: {line:?}");
    }
}

#[test]
fn menu_numbering_is_fixed_and_unavailable_rows_name_the_feature() {
    let currents = [
        CurrentChannel::Terminal,
        CurrentChannel::Known(ChannelChoice::Telegram),
        CurrentChannel::Known(ChannelChoice::Slack),
    ];
    for features in [BOTH, TELEGRAM_ONLY, SLACK_ONLY, NEITHER] {
        for current in &currents {
            let lines = menu_lines(current, features);
            assert_eq!(lines.len(), 3);
            assert!(lines[0].starts_with("  1. Telegram — "), "{lines:?}");
            assert!(lines[1].starts_with("  2. Slack — "), "{lines:?}");
            let last = if *current == CurrentChannel::Terminal {
                "  3. Skip [Enter]"
            } else {
                "  3. Keep existing [Enter]"
            };
            assert_eq!(lines[2], last);
            assert_eq!(
                lines[0]
                    == "  1. Telegram — unavailable in this build (requires the telegram feature)",
                !features.telegram
            );
            assert_eq!(
                lines[1] == "  2. Slack — unavailable in this build (requires the slack feature)",
                !features.slack
            );
            for line in &lines {
                assert!(line.chars().count() <= 80, "{line:?}");
            }
        }
    }
    let telegram = CurrentChannel::Known(ChannelChoice::Telegram);
    assert_eq!(
        menu_lines(&telegram, BOTH),
        [
            "  1. Telegram — edit allowed DM chat IDs",
            "  2. Slack — replace Telegram with Slack app DMs",
            "  3. Keep existing [Enter]",
        ]
    );
}

#[test]
fn feature_sets_shape_the_step_without_writing() {
    let world = World::new();
    world.write("work", WORK);
    let target = inspect(&world.profiles, "work");
    let before = world.snapshot();

    // Neither feature: a short notice replaces the menu; nothing is read.
    let none = drive(&target, NEITHER, "2\nU012ABCDEF\ny\n");
    assert_eq!(none.read, 0);
    assert!(
        none.text
            .contains("Telegram and Slack are unavailable in this build")
    );
    assert!(!none.text.contains("Selection [3]"));
    assert_eq!(none.outcome, ChannelOutcome::Skipped);

    // One feature: the other is listed, explained on selection, and returns
    // to the menu without entering ID setup.
    for (features, missing, feature) in
        [(TELEGRAM_ONLY, "2", "slack"), (SLACK_ONLY, "1", "telegram")]
    {
        let run = drive(&target, features, &format!("{missing}\n\n"));
        let guidance =
            format!("{feature} feature (cargo build --features {feature}). Nothing changed.");
        assert!(run.text.contains(&guidance), "{}", run.text);
        assert_eq!(
            run.text.matches("Selection [3]: ").count(),
            2,
            "{}",
            run.text
        );
        assert!(!run.text.contains("(comma-separated"), "{}", run.text);
        assert_eq!(run.outcome, ChannelOutcome::Skipped);
    }
    assert_eq!(world.snapshot(), before);
}

#[test]
fn slack_ids_are_corrected_by_position_without_echo_then_saved_to_the_user_profile() {
    let world = World::new();
    let path = world.write("work", WORK);
    let before = load(&world.profiles, "work");
    let target = inspect(&world.profiles, "work");
    assert!(matches!(target.destination, Destination::User(_)));

    let run = drive(
        &target,
        BOTH,
        "2\nC012ABCDEF\nU012ABCDEF, W0345GH ,U012ABCDEF\ny\n",
    );
    let ordered = [
        "Messaging channel (optional) — profile: work",
        "Current channel: none",
        "  2. Slack — allowed app DM user IDs",
        "Slack user IDs (comma-separated; Enter to go back): ",
        "Entry 1 must be a Slack user ID, not a channel ID.",
        "  Adapter: Slack (app DMs over Socket Mode)",
        "  Profile: work",
        "  Allowed Slack users: 2",
        "Save, replacing this profile's channel settings? [y/N] ",
    ];
    let mut at = 0;
    for expected in ordered {
        let found = run.text[at..].find(expected);
        assert!(found.is_some(), "{expected:?} not after {at}: {}", run.text);
        at += found.unwrap() + expected.len();
    }
    for id in ["C012ABCDEF", "U012ABCDEF", "W0345GH"] {
        assert!(!run.text.contains(id), "an ID was echoed: {}", run.text);
    }
    assert_eq!(
        run.outcome,
        ChannelOutcome::Saved {
            choice: ChannelChoice::Slack,
            profile: "work".into(),
            path: path.clone(),
        }
    );
    assert_fits(&run.text, &path);

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with(WORK), "other bytes kept: {saved}");
    let after = load(&world.profiles, "work");
    let (adapter, config) = channel(&after);
    assert_eq!(adapter, "slack");
    assert_eq!(
        config.unwrap().as_table().unwrap().clone(),
        toml::toml! { allowed_user_ids = ["U012ABCDEF", "W0345GH"] }
    );
    for (port, reference) in &before.selection.dimensions {
        if *port != PortDimension::Channels {
            let now = &after.selection.dimensions[port];
            assert_eq!(
                (&now.adapter, &now._config),
                (&reference.adapter, &reference._config)
            );
        }
    }
    assert!(!saved.contains(SENTINEL));
}

#[test]
fn same_adapter_keeps_safe_siblings_and_a_switch_carries_nothing() {
    let world = World::new();
    let original = "name = \"work\"\nextends = \"base\"\n\n# messaging\n[channels]\n\
                    adapter = \"telegram\"\n\n[channels.config]\nallowed_chat_ids = [1, 2]\n\
                    poll_timeout = 30\n\n[scheduler] # kept\nadapter = \"none\"\n";
    let path = world.write("work", original);
    let target = inspect(&world.profiles, "work");
    assert_eq!(
        target.current,
        CurrentChannel::Known(ChannelChoice::Telegram)
    );

    let run = drive(&target, BOTH, "1\n42, 7 ,42\ny\n");
    assert!(
        run.text
            .contains("  1. Telegram — edit allowed DM chat IDs")
    );
    assert!(run.text.contains("  Allowed Telegram chats: 2"));
    assert!(matches!(run.outcome, ChannelOutcome::Saved { .. }));
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with("name = \"work\"\nextends = \"base\"\n\n# messaging\n"));
    assert!(
        saved.ends_with("[scheduler] # kept\nadapter = \"none\"\n"),
        "{saved}"
    );
    let (adapter, config) = channel(&load(&world.profiles, "work"));
    assert_eq!(adapter, "telegram");
    assert_eq!(
        config.unwrap().as_table().unwrap().clone(),
        toml::toml! { allowed_chat_ids = [42, 7]
        poll_timeout = 30 }
    );

    // Switching adapters replaces the whole table: no old field or ID.
    let target = inspect(&world.profiles, "work");
    let run = drive(&target, BOTH, "2\nU012ABCDEF\ny\n");
    assert!(
        run.text
            .contains("  2. Slack — replace Telegram with Slack app DMs")
    );
    let (adapter, config) = channel(&load(&world.profiles, "work"));
    assert_eq!(adapter, "slack");
    assert_eq!(
        config.unwrap().as_table().unwrap().clone(),
        toml::toml! { allowed_user_ids = ["U012ABCDEF"] }
    );
}

#[test]
fn an_inline_credential_is_named_never_shown_and_its_file_never_rewritten() {
    let world = World::new();
    let content = format!(
        "name = \"work\"\nextends = \"base\"\n[channels]\nadapter = \"telegram\"\n\
         [channels.config]\nbot_token = \"{SENTINEL}\"\nallowed_chat_ids = [1]\n"
    );
    let path = world.write("work", &content);
    let target = inspect(&world.profiles, "work");
    assert!(matches!(
        &target.destination,
        Destination::Refused(Refusal::InlineCredential { key, .. }) if key == "bot_token"
    ));
    let before = world.snapshot();
    let run = drive(&target, BOTH, "1\n42\ny\n");
    assert_eq!(run.read, 0, "no menu: {}", run.text);
    assert!(
        run.text
            .contains("Put the token in TELEGRAM_BOT_TOKEN in the")
    );
    assert!(run.text.contains("delete the bot_token line by hand"));
    assert!(!run.text.contains(SENTINEL));
    assert_eq!(run.outcome, ChannelOutcome::Kept("Telegram".into()));
    assert_fits(&run.text, &path);
    assert_eq!(world.snapshot(), before);
}

#[test]
fn an_inherited_credential_blocks_only_the_same_adapter() {
    let world = World::new();
    let parent = format!(
        "name = \"team\"\nextends = \"base\"\n[channels]\nadapter = \"telegram\"\n\
         [channels.config]\nbot_token = \"{SENTINEL}\"\nallowed_chat_ids = [5]\n"
    );
    let parent_path = world.write("team", &parent);
    let child = "name = \"work\"\nextends = \"team\"\n";
    let child_path = world.write("work", child);
    let target = inspect(&world.profiles, "work");
    assert!(matches!(target.destination, Destination::User(_)));

    let run = drive(&target, BOTH, "1\n2\nU012ABCDEF\ny\n");
    assert!(
        run.text
            .contains("This profile inherits Telegram settings that hold a token (bot_token).")
    );
    assert!(!run.text.contains(SENTINEL));
    assert!(matches!(run.outcome, ChannelOutcome::Saved { .. }));
    assert_eq!(
        std::fs::read_to_string(&parent_path).unwrap(),
        parent,
        "parent untouched"
    );
    let saved = std::fs::read_to_string(&child_path).unwrap();
    assert!(
        saved.starts_with(child) && !saved.contains(SENTINEL),
        "{saved}"
    );
    assert_eq!(channel(&load(&world.profiles, "work")).0, "slack");
    assert_eq!(channel(&load(&world.profiles, "team")).0, "telegram");
}

#[test]
fn skip_back_invalid_decline_and_end_of_input_write_nothing() {
    let world = World::new();
    world.write("work", WORK);
    let target = inspect(&world.profiles, "work");
    let before = world.snapshot();
    for answers in [
        "",
        "\n",
        "3\n",
        "7\n\n",
        "2\n\n\n",
        "2\nC012ABCDEF\n\n\n",
        "2\nU012ABCDEF\n",
        "2\nU012ABCDEF\n\n",
        "2\nU012ABCDEF\nn\n",
        "1\n-100123\n42\nno\n",
        "2\n",
    ] {
        let run = drive(&target, BOTH, answers);
        assert_eq!(
            run.outcome,
            ChannelOutcome::Skipped,
            "{answers:?}: {}",
            run.text
        );
        assert_eq!(world.snapshot(), before, "{answers:?}");
    }
}

/// Mutant control: the derivative keeps the preset's *effective* baseline
/// (preview fallbacks applied; never raw `cron` or `extends` + `preview`) and
/// changes only the channel; the preset itself still resolves as before.
#[test]
fn embedded_presets_derive_their_effective_baseline_with_only_the_channel_replaced() {
    for preset in ["coding", "personal-assistant"] {
        let world = World::new();
        let original = TomlProfileResolver::new(preset, world.profiles.clone())
            .unwrap()
            .resolve_active()
            .unwrap();
        let target = inspect(&world.profiles, preset);
        let Destination::Derived(derived) = &target.destination else {
            panic!("{preset}: {:?}", target.destination);
        };
        let name = format!("{preset}-channel");
        assert_eq!(derived.name, name);

        let run = drive(&target, BOTH, "1\ny\n42\ny\n");
        let ordered = [
            format!("'{preset}' is a built-in profile; init does not change it."),
            format!("  {name}"),
            format!("Use new profile '{name}' for this channel? [y/N] "),
            "Telegram chat IDs (comma-separated; Enter to go back): ".to_string(),
            format!("  Profile: {name} (new)"),
            "  Allowed Telegram chats: 1".to_string(),
            format!("Create '{name}' with these channel settings? [y/N] "),
        ];
        let mut at = 0;
        for expected in &ordered {
            let found = run.text[at..].find(expected.as_str());
            assert!(found.is_some(), "{expected:?}: {}", run.text);
            at += found.unwrap() + expected.len();
        }
        let path = world.profiles.join(format!("{name}.toml"));
        assert_eq!(
            run.outcome,
            ChannelOutcome::Saved {
                choice: ChannelChoice::Telegram,
                profile: name.clone(),
                path: path.clone(),
            }
        );
        assert_fits(&run.text, &path);

        let text = std::fs::read_to_string(&path).unwrap();
        let definition: toml::Table = toml::from_str(&text).unwrap();
        assert!(definition.get("extends").is_none(), "{text}");
        assert!(definition.get("preview").is_none(), "{text}");
        assert!(!text.contains("cron"), "{text}");
        let derived = load(&world.profiles, &name);
        assert!(!derived.preview);
        for (port, reference) in &original.selection.dimensions {
            if *port == PortDimension::Channels {
                continue;
            }
            let got = &derived.selection.dimensions[port];
            assert_eq!(
                (&got.adapter, &got._config),
                (&reference.adapter, &reference._config),
                "{preset} {port:?}"
            );
        }
        assert_eq!(
            serde_json::to_value(&derived.overrides).unwrap(),
            serde_json::to_value(&original.overrides).unwrap()
        );
        let (adapter, config) = channel(&derived);
        assert_eq!(adapter, "telegram");
        assert_eq!(
            config.unwrap().as_table().unwrap().clone(),
            toml::toml! { allowed_chat_ids = [42] }
        );
        if preset == "personal-assistant" {
            assert_eq!(
                derived.selection.dimensions[&PortDimension::Scheduler].adapter,
                "none"
            );
        }

        // The preset is neither shadowed nor changed.
        assert!(!world.profiles.join(format!("{preset}.toml")).exists());
        let again = TomlProfileResolver::new(preset, world.profiles.clone())
            .unwrap()
            .resolve_active()
            .unwrap();
        assert_eq!(again.preview, original.preview);
        for (port, reference) in &original.selection.dimensions {
            assert_eq!(again.selection.dimensions[port].adapter, reference.adapter);
        }

        // A second run finds the name taken and refuses before any prompt.
        let target = inspect(&world.profiles, preset);
        assert!(matches!(
            &target.destination,
            Destination::Refused(Refusal::NameTaken { name: taken }) if *taken == name
        ));
        let before = world.snapshot();
        let rerun = drive(&target, BOTH, "1\ny\n7\ny\n");
        assert_eq!(rerun.read, 0);
        assert!(
            rerun
                .text
                .contains(&format!("rustain --profile {name} init"))
        );
        assert_eq!(world.snapshot(), before);
    }
}

#[test]
fn declining_the_new_profile_creates_nothing() {
    let world = World::new();
    let target = inspect(&world.profiles, "coding");
    let before = world.snapshot();
    for answers in [
        "2\n\n",
        "2\nn\n",
        "2\ny\n\n\n",
        "2\ny\nU012ABCDEF\n\n",
        "2\ny\nU012ABCDEF\n",
    ] {
        let run = drive(&target, BOTH, answers);
        assert_eq!(
            run.outcome,
            ChannelOutcome::Skipped,
            "{answers:?}: {}",
            run.text
        );
        assert_eq!(world.snapshot(), before, "{answers:?}");
    }
}

/// Mutant control: no fallback, bypassed, community, linked or preview file
/// ever becomes the destination, and the refusal comes before any prompt.
#[cfg(unix)]
#[test]
fn unresolved_and_unsafe_targets_are_refused_before_any_prompt() {
    let world = World::new();
    let outside = world._tmp.path().join("outside.toml");
    std::fs::write(&outside, WORK.replace("\"work\"", "\"linked\"")).unwrap();
    std::os::unix::fs::symlink(&outside, world.profiles.join("linked.toml")).unwrap();
    std::fs::create_dir_all(world.profiles.join("community")).unwrap();
    std::fs::write(
        world.profiles.join("community").join("club.toml"),
        WORK.replace("\"work\"", "\"club\""),
    )
    .unwrap();
    world.write("draft", &format!("preview = true\n{WORK}"));
    // An oversize file shadowing a built-in is skipped by the resolver: it
    // must not turn into the built-in's derivative.
    world.write("coding", &format!("{WORK}#{}\n", "x".repeat(1024 * 1024)));
    let read_only = world.write("locked", WORK);
    let mut permissions = std::fs::metadata(&read_only).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&read_only, permissions).unwrap();

    let cases: [(&str, fn(&Refusal) -> bool); 7] = [
        ("ghost", |r| *r == Refusal::NotFound),
        ("../work", |r| *r == Refusal::NotFound),
        ("linked", |r| matches!(r, Refusal::Unsafe { .. })),
        ("club", |r| matches!(r, Refusal::Community { .. })),
        ("draft", |r| matches!(r, Refusal::Preview { .. })),
        ("coding", |r| matches!(r, Refusal::Bypassed { .. })),
        ("locked", |r| matches!(r, Refusal::ReadOnly { .. })),
    ];
    let before = world.snapshot();
    for (name, expected) in cases {
        let target = inspect(&world.profiles, name);
        let Destination::Refused(refusal) = &target.destination else {
            panic!("{name}: {:?}", target.destination);
        };
        assert!(expected(refusal), "{name}: {refusal:?}");
        let run = drive(&target, BOTH, "2\ny\nU012ABCDEF\ny\n");
        assert_eq!(run.read, 0, "{name}: {}", run.text);
        assert!(!run.text.contains("Selection [3]"), "{name}");
        assert_eq!(run.outcome, ChannelOutcome::Skipped, "{name}");
    }
    assert!(!world.profiles.join("coding-channel.toml").exists());
    assert!(!world.profiles.join("ghost-channel.toml").exists());
    assert_eq!(world.snapshot(), before);

    // A linked profiles directory is never written through.
    let real = world._tmp.path().join("real-profiles");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("work.toml"), WORK).unwrap();
    let linked_dir = world._tmp.path().join("linked-profiles");
    std::os::unix::fs::symlink(&real, &linked_dir).unwrap();
    for name in ["work", "coding"] {
        let target = inspect(&linked_dir, name);
        assert!(
            matches!(&target.destination, Destination::Refused(Refusal::Unsafe { path }) if *path == linked_dir),
            "{name}: {:?}",
            target.destination
        );
    }
}

#[cfg(unix)]
#[test]
fn an_existing_entry_for_the_derived_name_is_never_replaced() {
    for place in ["user-link", "community"] {
        let world = World::new();
        let occupied = match place {
            "user-link" => {
                let path = world.profiles.join("coding-channel.toml");
                std::os::unix::fs::symlink(world._tmp.path().join("nowhere"), &path).unwrap();
                path
            }
            _ => {
                let dir = world.profiles.join("community");
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join("coding-channel.toml");
                std::fs::write(&path, "name = \"coding-channel\"\n").unwrap();
                path
            }
        };
        let target = inspect(&world.profiles, "coding");
        assert!(
            matches!(
                &target.destination,
                Destination::Refused(Refusal::NameTaken { .. })
            ),
            "{place}"
        );
        let before = world.snapshot();
        let run = drive(&target, BOTH, "2\ny\nU012ABCDEF\ny\n");
        assert_eq!(run.read, 0);
        assert_eq!(world.snapshot(), before, "{place}: {occupied:?}");
    }
}

/// Failure control: a file that changed after init read it is not
/// overwritten, the step reports `save failed`, and no temporary is left.
#[test]
fn a_profile_changed_meanwhile_fails_the_save_and_keeps_its_bytes() {
    let world = World::new();
    let path = world.write("work", WORK);
    let target = inspect(&world.profiles, "work");
    let concurrent = format!("{WORK}# edited by hand meanwhile\n");
    std::fs::write(&path, &concurrent).unwrap();
    let before = world.snapshot();
    let run = drive(&target, BOTH, "2\nU012ABCDEF\ny\n");
    let ChannelOutcome::Failed { reason } = &run.outcome else {
        panic!("{:?}", run.outcome);
    };
    assert!(
        reason.contains("changed while init was running"),
        "{reason}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), concurrent);
    assert_eq!(world.snapshot(), before, "no temporary file is left");
    let summary = run.outcome.summary_lines().join("\n");
    assert!(summary.contains("Channel change not saved (save failed)."));
    assert!(summary.contains("existing channel settings are unchanged"));
}

#[test]
fn splice_replaces_only_the_channel_table() {
    let original = "name = \"w\"\n# lead\n[persona]\nadapter = \"minimal\"\n\n[channels]\n\
                    adapter = \"telegram\" # old\n\n[channels.config]\nallowed_chat_ids = [1]\n\n\
                    [ overrides ]\nmodel = \"m\" # keep\n";
    let mut channels = toml::Table::new();
    channels.insert("adapter".into(), toml::Value::String("slack".into()));
    channels.insert(
        "config".into(),
        toml::Value::Table(toml::toml! { allowed_user_ids = ["U1"] }),
    );
    let spliced = splice_channels(original, &channels).unwrap();
    assert!(
        spliced
            .starts_with("name = \"w\"\n# lead\n[persona]\nadapter = \"minimal\"\n\n[channels]\n")
    );
    assert!(
        spliced.ends_with("\n[ overrides ]\nmodel = \"m\" # keep\n"),
        "{spliced}"
    );
    assert!(
        !spliced.contains("telegram") && !spliced.contains("# old"),
        "{spliced}"
    );

    // Layouts the line splice cannot edit safely are refused, not guessed.
    for unsafe_layout in [
        "name = \"w\"\nchannels.adapter = \"telegram\"\n",
        "name = \"w\"\nchannels = { adapter = \"telegram\" }\n",
        "name = \"w\"\nnote = \"\"\"\n[channels]\n\"\"\"\n",
    ] {
        assert!(
            matches!(
                splice_channels(unsafe_layout, &channels),
                Err(destination::SaveError::Unverified)
            ),
            "{unsafe_layout}"
        );
    }
}

#[test]
fn summary_states_name_env_variables_and_the_actual_profile() {
    let saved = |choice| {
        ChannelOutcome::Saved {
            choice,
            profile: "work-assistant".into(),
            path: PathBuf::from("/home/dana/.config/rustain/profiles/work-assistant.toml"),
        }
        .summary_lines()
    };
    let slack = saved(ChannelChoice::Slack);
    assert_eq!(
        slack,
        [
            "  Slack settings saved (not connected).",
            "  Profile: work-assistant",
            "  File: /home/dana/.config/rustain/profiles/work-assistant.toml",
            "  Before starting the daemon, set SLACK_APP_TOKEN and SLACK_BOT_TOKEN in",
            "  its environment, then run:",
            "    rustain --profile work-assistant daemon start",
            "  A supervised daemon needs these variables in its own environment.",
            "  A daemon that is already running keeps its old settings until you",
            "  restart it with this profile.",
        ]
    );
    let telegram = saved(ChannelChoice::Telegram);
    assert_eq!(
        telegram[3],
        "  Before starting the daemon, set TELEGRAM_BOT_TOKEN in"
    );
    assert_eq!(
        ChannelOutcome::Skipped.summary_lines(),
        ["  Channel setup skipped."]
    );
    assert_eq!(
        ChannelOutcome::Kept("Telegram".into()).summary_lines(),
        ["  Existing Telegram configuration kept."]
    );
    for line in slack.iter().chain(&telegram) {
        assert!(line.chars().count() <= 80, "{line:?}");
    }
}

#[test]
fn corrective_messages_name_the_position_only() {
    let cases = [
        (
            ChannelChoice::Slack,
            "U1, C012ABCDEF",
            "Entry 2 must be a Slack user ID, not a channel ID.",
        ),
        (
            ChannelChoice::Slack,
            "U1,,U2",
            "Entry 2 is empty; remove the extra comma.",
        ),
        (
            ChannelChoice::Slack,
            "*",
            "Entry 1 is a wildcard; list each ID instead.",
        ),
        (
            ChannelChoice::Slack,
            "dana",
            "Entry 1 must be a Slack user ID (U or W, then capitals and digits).",
        ),
        (
            ChannelChoice::Telegram,
            "5, -100123",
            "Entry 2 must be a positive chat ID; group IDs are not accepted.",
        ),
        (
            ChannelChoice::Telegram,
            "@dana",
            "Entry 1 must be a numeric Telegram chat ID.",
        ),
    ];
    for (choice, input, message) in cases {
        let error = Allowlist::parse(choice, input).unwrap_err();
        assert_eq!(entry_error(choice, error), message);
    }
    assert_eq!(
        Allowlist::parse(ChannelChoice::Slack, "  \n").unwrap(),
        None
    );
}
