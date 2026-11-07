//! K7 (AC12, AC13): where the launchpad's facts come from and when they are
//! recomputed. `git_head::read_branch` reads real scratch repositories;
//! `launchpad_facts::refresh` is driven with a counting branch reader (the
//! injected seam) to prove an unchanged draw reads nothing.
use std::cell::Cell;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use rustain::adapters::tui::{launchpad::AuthState, launchpad::LaunchpadEnv, state::TuiState};
use rustain::domain::clock::MockClock;
use rustain::domain::services::session_index::SessionIndex;
use rustain::infrastructure::git_head::{Head, read_branch};
use rustain::infrastructure::runtime::launchpad_facts::{self, attached_facts};

use super::k6_launchpad_view::{NOW_MS, empty_conversation, git_workspace, summary};

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn read_branch_reads_every_head_shape() {
    // A branch.
    let dir = git_workspace("ref: refs/heads/feature/x\n");
    assert_eq!(
        read_branch(dir.path()),
        Some(Head::Branch("feature/x".into()))
    );
    assert_eq!(read_branch(dir.path()).unwrap().label(), "feature/x");
    // A detached SHA-1 and SHA-256 head show the first seven digits.
    let sha1 = "0123456789abcdef0123456789ABCDEF01234567";
    let dir = git_workspace(&format!("{sha1}\n"));
    assert_eq!(
        read_branch(dir.path()),
        Some(Head::Detached("0123456".into()))
    );
    assert_eq!(read_branch(dir.path()).unwrap().label(), "detached 0123456");
    let sha256 = "a".repeat(64);
    let dir = git_workspace(&sha256);
    assert_eq!(
        read_branch(dir.path()),
        Some(Head::Detached("aaaaaaa".into()))
    );
}

#[test]
fn read_branch_walks_up_to_the_first_dot_git() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let nested = dir.path().join("crates/app/src");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(read_branch(&nested), Some(Head::Branch("main".into())));
    // The nearest `.git` wins over an outer one.
    write(
        dir.path(),
        "crates/app/.git/HEAD",
        "ref: refs/heads/inner\n",
    );
    assert_eq!(read_branch(&nested), Some(Head::Branch("inner".into())));
}

#[test]
fn read_branch_follows_a_worktree_git_file_once() {
    let dir = tempfile::tempdir().unwrap();
    // A relative `gitdir:` resolves against the directory holding the `.git` file.
    write(
        dir.path(),
        "main/.git/worktrees/wt/HEAD",
        "ref: refs/heads/wt-branch\n",
    );
    write(dir.path(), "wt/.git", "gitdir: ../main/.git/worktrees/wt\n");
    assert_eq!(
        read_branch(&dir.path().join("wt")),
        Some(Head::Branch("wt-branch".into()))
    );
    // An absolute `gitdir:` works too.
    let absolute = dir.path().join("main/.git/worktrees/wt");
    write(
        dir.path(),
        "abs/.git",
        &format!("gitdir: {}\n", absolute.display()),
    );
    assert_eq!(
        read_branch(&dir.path().join("abs")),
        Some(Head::Branch("wt-branch".into()))
    );
    // It is followed once: a pointer to a pointer is not a repository.
    write(
        dir.path(),
        "hop/.git",
        &format!("gitdir: {}/hop2\n", dir.path().display()),
    );
    write(dir.path(), "hop2", "gitdir: elsewhere\n");
    assert_eq!(read_branch(&dir.path().join("hop")), None);
}

#[test]
fn read_branch_refuses_what_it_cannot_place() {
    // No repository anywhere above.
    let bare = tempfile::tempdir().unwrap();
    assert_eq!(read_branch(bare.path()), None);
    // Garbage HEAD, an empty branch name, a short hex string, a non-hex 40-char string.
    for head in [
        "this is not a head\n",
        "ref: refs/heads/\n",
        "ref: refs/tags/v1\n",
        "abc123\n",
        "gggggggggggggggggggggggggggggggggggggggg",
        "",
    ] {
        let dir = git_workspace(head);
        assert_eq!(read_branch(dir.path()), None, "{head:?}");
    }
    // A HEAD larger than 4 KiB is not read at all.
    let dir = git_workspace(&format!("ref: refs/heads/{}\n", "x".repeat(5_000)));
    assert_eq!(read_branch(dir.path()), None);
    // Exactly at the bound it is still read.
    let name = "y".repeat(4_096 - "ref: refs/heads/".len());
    let dir = git_workspace(&format!("ref: refs/heads/{name}"));
    assert_eq!(read_branch(dir.path()), Some(Head::Branch(name)));
    // A missing HEAD and a `.git` that points nowhere.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    assert_eq!(read_branch(dir.path()), None);
    write(dir.path(), "dangling/.git", "gitdir: nowhere\n");
    assert_eq!(read_branch(&dir.path().join("dangling")), None);
}

#[test]
fn read_branch_strips_control_characters_from_the_name() {
    let dir = git_workspace("ref: refs/heads/a\u{1b}[31mb\u{202e}c\n");
    let head = read_branch(dir.path()).unwrap();
    assert_eq!(head.label(), "abc");
}

fn local_state(workspace: &Path, locked: Arc<AtomicBool>) -> TuiState {
    let mut state = TuiState::new(100, 30);
    state.terminal_width = 100;
    state.launchpad_env = Some(LaunchpadEnv::new(workspace.to_path_buf(), move |id| {
        id == "openai" && locked.load(Ordering::SeqCst)
    }));
    state
}

fn facts(state: &TuiState) -> &rustain::adapters::tui::launchpad::LaunchpadFacts {
    state.tab_render_states[&state.active_tab_id]
        .launchpad
        .as_ref()
        .expect("launchpad facts")
}

#[test]
fn recent_excludes_active_and_empty_sessions() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    let index = SessionIndex::build(vec![
        summary("active", "The one on screen", 10, 5),
        summary("empty", "Nothing said", 20, 0),
        summary("a", "Newest listed", 100, 2),
        summary("b", "Fix\nthe\u{1b}[31m bug", 4_000, 3),
        summary("c", "   ", 90_000, 1),
        summary("d", "Fourth, cut off", 200_000, 9),
        summary("e", "Fifth", 300_000, 9),
    ]);
    let clock = MockClock::at_wall_ms(NOW_MS);
    launchpad_facts::refresh(
        &mut state,
        &empty_conversation("active"),
        "m",
        "anthropic",
        &index,
        &clock,
    );
    let recent = &facts(&state).recent;
    let shown: Vec<(&str, &str)> = recent
        .iter()
        .map(|session| (session.title.as_str(), session.age.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("Newest listed", "1m ago"),
            // Sanitized to one line, no controls.
            ("Fix the bug", "1h ago"),
            // An empty title is `Untitled`; ages come from the injected clock.
            ("Untitled", "Nov 13"),
        ]
    );
}

#[test]
fn a_draw_with_an_unchanged_key_reads_nothing() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let locked = Arc::new(AtomicBool::new(false));
    let mut state = local_state(dir.path(), locked.clone());
    let index = SessionIndex::new();
    let clock = MockClock::at_wall_ms(NOW_MS);
    let reads = Cell::new(0usize);
    let mut count = |_: &Path| {
        reads.set(reads.get() + 1);
        Some(Head::Branch("main".into()))
    };
    let mut refresh = |state: &mut TuiState, id: &str, model: &str, provider: &str| {
        launchpad_facts::refresh_with(
            state,
            &empty_conversation(id),
            model,
            provider,
            &index,
            &clock,
            &mut count,
        )
    };
    assert!(
        refresh(&mut state, "c1", "m1", "anthropic"),
        "first draw builds"
    );
    assert_eq!(reads.get(), 1);
    for _ in 0..5 {
        assert!(
            !refresh(&mut state, "c1", "m1", "anthropic"),
            "unchanged draw"
        );
    }
    assert_eq!(reads.get(), 1, "five unchanged draws read no git");
    // Each member of the key triggers exactly one refresh.
    assert!(refresh(&mut state, "c1", "m2", "anthropic"), "model switch");
    assert_eq!(reads.get(), 2);
    assert!(refresh(&mut state, "c1", "m2", "openai"), "provider switch");
    assert_eq!(reads.get(), 3);
    assert_eq!(facts(&state).auth, AuthState::SignedIn);
    locked.store(true, Ordering::SeqCst);
    assert!(
        refresh(&mut state, "c1", "m2", "openai"),
        "an unlock/lock flips the key"
    );
    assert_eq!(reads.get(), 4);
    assert_eq!(facts(&state).auth, AuthState::NotSignedIn);
    assert!(
        refresh(&mut state, "c2", "m2", "openai"),
        "/new is a new conversation id"
    );
    assert_eq!(reads.get(), 5);
    assert!(!refresh(&mut state, "c2", "m2", "openai"));
    assert_eq!(reads.get(), 5);
}

#[test]
fn a_login_that_unlocks_the_provider_flips_the_auth_word() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let locked = Arc::new(AtomicBool::new(true));
    let mut state = local_state(dir.path(), locked.clone());
    let clock = MockClock::at_wall_ms(NOW_MS);
    let index = SessionIndex::new();
    let conversation = empty_conversation("c1");
    launchpad_facts::refresh(&mut state, &conversation, "gpt-5", "openai", &index, &clock);
    assert_eq!(facts(&state).auth, AuthState::NotSignedIn);
    locked.store(false, Ordering::SeqCst);
    launchpad_facts::refresh(&mut state, &conversation, "gpt-5", "openai", &index, &clock);
    assert_eq!(facts(&state).auth, AuthState::SignedIn);
}

#[test]
fn new_after_a_turn_lists_the_previous_session_under_recent() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    let clock = MockClock::at_wall_ms(NOW_MS);
    // Before the first turn the stored session list holds nothing of this run.
    let mut index = SessionIndex::new();
    launchpad_facts::refresh(
        &mut state,
        &empty_conversation("c1"),
        "m",
        "anthropic",
        &index,
        &clock,
    );
    assert!(facts(&state).recent.is_empty());
    // The turn is saved (the index learns it), then `/new` opens a fresh conversation.
    index.touch("c1", Some("Explain the build".into()), Some(4));
    launchpad_facts::refresh(
        &mut state,
        &empty_conversation("c2"),
        "m",
        "anthropic",
        &index,
        &clock,
    );
    let recent = &facts(&state).recent;
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].title, "Explain the build");
}

#[test]
fn a_conversation_with_messages_paints_no_launchpad_but_keeps_the_root() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    let clock = MockClock::at_wall_ms(NOW_MS);
    let mut conversation = empty_conversation("c1");
    conversation.messages.push(crate::render_helpers::make_msg(
        "m1",
        rustain::domain::models::MessageRole::User,
    ));
    let refreshed = launchpad_facts::refresh_with(
        &mut state,
        &conversation,
        "m",
        "anthropic",
        &SessionIndex::new(),
        &clock,
        &mut |_| panic!("a conversation with messages never reads git"),
    );
    assert!(!refreshed);
    let tab = &state.tab_render_states[&state.active_tab_id];
    assert!(tab.launchpad.is_none());
    // The tool lines still get their workspace root, set once.
    assert_eq!(tab.workspace_root.as_deref(), Some(dir.path()));
}

#[test]
fn a_state_without_an_environment_refreshes_nothing() {
    let mut state = TuiState::new(80, 24);
    let refreshed = launchpad_facts::refresh(
        &mut state,
        &empty_conversation("c1"),
        "m",
        "anthropic",
        &SessionIndex::new(),
        &MockClock::at_wall_ms(NOW_MS),
    );
    assert!(!refreshed);
    assert!(state.tab_render_states.is_empty());
}

#[test]
fn the_selected_model_is_the_displayed_model() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    state.selected_model = Some("picked-model".into());
    launchpad_facts::refresh(
        &mut state,
        &empty_conversation("c1"),
        "configured-model",
        "anthropic",
        &SessionIndex::new(),
        &MockClock::at_wall_ms(NOW_MS),
    );
    assert_eq!(facts(&state).model, "picked-model");
}

#[test]
fn attached_facts_name_the_workspace_and_branch_once() {
    let dir = git_workspace("ref: refs/heads/attached-branch\n");
    let facts = attached_facts(dir.path(), 90);
    assert_eq!(facts.branch.as_deref(), Some("attached-branch"));
    assert_eq!(facts.terminal_width, 90);
    assert!(facts.recent.is_empty());
    assert_eq!(facts.workspace, dir.path().to_string_lossy());
    let no_repo = tempfile::tempdir().unwrap();
    assert_eq!(attached_facts(no_repo.path(), 90).branch, None);
}

// ── patch round (review 9-B P5, P6, P7, P8) ────────────────────────────────

#[cfg(unix)]
#[test]
fn a_fifo_or_device_in_place_of_git_never_blocks_and_reads_as_no_repo() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mkfifo = |path: &Path| {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    };
    // `.git` itself is a FIFO.
    let dir = tempfile::tempdir().unwrap();
    mkfifo(&dir.path().join(".git"));
    assert_eq!(read_branch(dir.path()), None);
    // `.git/HEAD` is a FIFO.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    mkfifo(&dir.path().join(".git/HEAD"));
    assert_eq!(read_branch(dir.path()), None);
    // `.git/HEAD` is a symlink to a character device.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::os::unix::fs::symlink("/dev/null", dir.path().join(".git/HEAD")).unwrap();
    assert_eq!(read_branch(dir.path()), None);
    // Control: a symlink to a regular file is still followed.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    write(dir.path(), "real-head", "ref: refs/heads/linked\n");
    std::os::unix::fs::symlink(dir.path().join("real-head"), dir.path().join(".git/HEAD")).unwrap();
    assert_eq!(read_branch(dir.path()), Some(Head::Branch("linked".into())));
}

#[test]
fn recent_follows_the_session_index_on_every_empty_draw_without_reading_git() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    let clock = MockClock::at_wall_ms(NOW_MS);
    let reads = Cell::new(0usize);
    let mut count = |_: &Path| {
        reads.set(reads.get() + 1);
        Some(Head::Branch("main".into()))
    };
    let conversation = empty_conversation("c2");
    let mut refresh = |state: &mut TuiState, index: &SessionIndex| {
        launchpad_facts::refresh_with(
            state,
            &conversation,
            "m",
            "anthropic",
            index,
            &clock,
            &mut count,
        )
    };
    // `/new` before the title arrives: the stored session is still untitled.
    assert!(refresh(
        &mut state,
        &SessionIndex::build(vec![summary("c1", "New Conversation", 5, 2)])
    ));
    assert_eq!(facts(&state).recent[0].title, "New Conversation");
    // The title arrives, a second session is saved, the first ages: same key,
    // new list, no git read, and the call reports no rebuild.
    let index = SessionIndex::build(vec![
        summary("c1", "Explain the build", 3_700, 2),
        summary("c3", "Another tab", 20, 4),
    ]);
    assert!(!refresh(&mut state, &index));
    let shown: Vec<_> = facts(&state)
        .recent
        .iter()
        .map(|s| (s.title.as_str(), s.age.as_str()))
        .collect();
    assert_eq!(
        shown,
        [("Another tab", "just now"), ("Explain the build", "1h ago")]
    );
    // A deleted session leaves the list.
    assert!(!refresh(&mut state, &SessionIndex::new()));
    assert!(facts(&state).recent.is_empty());
    assert_eq!(reads.get(), 1, "only the first draw read git");
}

#[test]
fn install_and_refresh_follow_the_real_registry_lock_state() {
    use rustain::adapters::provider::ProviderRegistry;
    use rustain::domain::models::provider::LockedProvider;
    let dir = git_workspace("ref: refs/heads/main\n");
    let registry = Arc::new(ProviderRegistry::new());
    registry.register_locked(LockedProvider {
        provider_id: "openai".into(),
        display_name: "OpenAI".into(),
        sign_in_hint: String::new(),
        models: Vec::new(),
    });
    let mut state = TuiState::new(100, 30);
    launchpad_facts::install(&mut state, dir.path(), &registry);
    let clock = MockClock::at_wall_ms(NOW_MS);
    let conversation = empty_conversation("c1");
    let index = SessionIndex::new();
    launchpad_facts::refresh(&mut state, &conversation, "gpt-5", "openai", &index, &clock);
    assert_eq!(facts(&state).auth, AuthState::NotSignedIn);
    // A login unlocks the provider: the next draw flips the word.
    registry.unlock("openai");
    assert!(launchpad_facts::refresh(
        &mut state,
        &conversation,
        "gpt-5",
        "openai",
        &index,
        &clock
    ));
    assert_eq!(facts(&state).auth, AuthState::SignedIn);
    // Another provider's lock never affects this one.
    registry.register_locked(LockedProvider {
        provider_id: "anthropic".into(),
        display_name: "Anthropic".into(),
        sign_in_hint: String::new(),
        models: Vec::new(),
    });
    assert!(!launchpad_facts::refresh(
        &mut state,
        &conversation,
        "gpt-5",
        "openai",
        &index,
        &clock
    ));
    assert_eq!(facts(&state).auth, AuthState::SignedIn);
}

#[test]
fn model_and_provider_strings_are_one_clean_line() {
    let dir = git_workspace("ref: refs/heads/main\n");
    let mut state = local_state(dir.path(), Arc::default());
    launchpad_facts::refresh(
        &mut state,
        &empty_conversation("c1"),
        "m\u{1b}[31m\u{202e}x\ny",
        "gw\u{202e}\u{7}z",
        &SessionIndex::new(),
        &MockClock::at_wall_ms(NOW_MS),
    );
    let f = facts(&state);
    assert_eq!(f.model, "mx y");
    assert_eq!(f.provider, "gwz");
}
