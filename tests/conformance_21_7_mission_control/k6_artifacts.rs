//! K6 / AC8 / A6 / A15 (21-7 slice 7-B): the Artifacts tab and the artifact window. The tab is the
//! durable artifact fold as a list (rows and lineage from the production fold, never a second
//! renderer); `Enter` on a row and `/artifact show <id>` open the same `Surface::ArtifactWindow`
//! (facts, then the stored body Rendered by default and Source on `v`); everything painted is
//! sanitized, `c` copies the ORIGINAL bytes and is refused above `COPY_MAX_BYTES` before any
//! string is built. The fold is read through the production `WorkspaceJournalReader`, the
//! commands go through the production parser and the production bridge callees, keys travel
//! through the local host's decoder, and every assertion reads painted cells, the one frame on
//! the stack, the notice store or the clipboard closure. No sleeps.
use super::support::*;
use async_trait::async_trait;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::artifact_window::{ArtifactBody, ArtifactWindowState, Live};
use rustain::adapters::tui::clipboard::ClipboardResult;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::markdown_body::BodyMode;
use rustain::adapters::tui::presentation::{self, Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId, ContentHash,
    EvidenceArtifact, HostBinding, PermissionMode, ProvenanceTag, ReviewStatus, ReviewVerdict,
    RoomEvent,
};
use rustain::domain::ports::{PatchReviewError, PatchReviewRecorder};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::infrastructure::runtime::artifact_bridge;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const TAB_TITLE: &str = "Mission Control · Artifacts";

fn hash(seed: char) -> ContentHash {
    ContentHash::parse_hex(&std::iter::repeat_n(seed, 64).collect::<String>()).expect("hex")
}

fn aid(seed: char) -> ArtifactId {
    ArtifactId::from(hash(seed))
}

fn full_id(seed: char) -> String {
    aid(seed).as_str().to_owned()
}

fn artifact(seed: char, kind: ArtifactKind, producer: &str, host: &str) -> ArtifactRef {
    EvidenceArtifact {
        id: aid(seed),
        kind,
        producer: AgentId::parse(producer).expect("agent id"),
        content_hash: hash('0'),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: vec![],
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new(host, "ws"),
    }
}

/// A recorder whose only job is the stored body (the window reads it through this port).
struct Bodies(HashMap<ArtifactId, Vec<u8>>);

#[async_trait]
impl PatchReviewRecorder for Bodies {
    async fn record_verdict(
        &self,
        _artifact: ArtifactRef,
        _reviewer: AgentId,
        _verdict: ReviewVerdict,
    ) -> Result<ArtifactRef, PatchReviewError> {
        Err(PatchReviewError::Refused("not under test".to_owned()))
    }

    async fn body(&self, id: &ArtifactId) -> Result<Vec<u8>, PatchReviewError> {
        self.0
            .get(id)
            .cloned()
            .ok_or_else(|| PatchReviewError::Refused("no stored body".to_owned()))
    }

    fn effective_policy(&self) -> MergeBackPolicy {
        MergeBackPolicy::default()
    }
}

/// Evidence `a`, a patch `b` that depends on it, an input request `c`; bodies for `a` and `b`
/// as given (`c` has none stored).
async fn fixture(width: u16, height: u16, a: &[u8], b: &[u8]) -> Rig {
    let ws = Workspace::new().await;
    let host = rustain::infrastructure::subagent::current_host_id(ws.path());
    let mut evidence = artifact('a', ArtifactKind::Evidence, "spoke-1", &host);
    evidence.review = None;
    let mut patch = artifact('b', ArtifactKind::Patch, "spoke-2", &host);
    patch.depends_on = vec![evidence.id.clone()];
    let mut ticket = artifact('c', ArtifactKind::InputRequest, "spoke-3", &host);
    ticket.review = None;
    for artifact in [&evidence, &patch, &ticket] {
        ws.node
            .append_room(RoomEvent::ArtifactCreated {
                artifact: artifact.clone(),
            })
            .await
            .expect("append the artifact");
    }
    let mut rig = Rig::plain(ws, width, height).await;
    let mut bodies = HashMap::new();
    bodies.insert(aid('a'), a.to_vec());
    bodies.insert(aid('b'), b.to_vec());
    rig.app.patch_review = Some(Arc::new(Bodies(bodies)));
    attach_journal(&mut rig);
    rig
}

/// `/artifacts` through the production parser and the production dispatch callee, then a draw.
async fn open_tab(rig: &mut Rig) {
    let mut scratch = TuiState::new(130, 30);
    scratch.input_buffer = "/artifacts".to_owned();
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut scratch) else {
        panic!("/artifacts must use the production command parser");
    };
    assert_eq!(name, "artifacts");
    artifact_bridge::artifacts_command(
        &mut rig.f.h.state,
        CONVERSATION,
        args.as_deref(),
        &rig.app,
        PermissionMode::Yolo,
    )
    .await;
    rig.paint();
}

/// `/artifact <arguments>` through the production parser and callee, then a draw.
async fn slash_artifact(rig: &mut Rig, line: &str) {
    let mut scratch = TuiState::new(130, 30);
    scratch.input_buffer = line.to_owned();
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut scratch) else {
        panic!("{line} must use the production command parser");
    };
    assert_eq!(name, "artifact");
    artifact_bridge::artifact_command(
        &mut rig.f.h.state,
        CONVERSATION,
        args.as_deref(),
        &rig.app,
        PermissionMode::Yolo,
    )
    .await;
    rig.paint();
}

/// Move the list selection (through the real `j` key) until the detail preview names the
/// artifact: the selected row may be painted in its narrow safety-first form (decision and
/// hazard first, identifier last), so the identity is read from the preview's `id` fact.
fn select_row(rig: &mut Rig, prefix: &str) {
    let fact = format!("id        {prefix}");
    for _ in 0..6 {
        if body_lines(rig).iter().any(|row| row.contains(&fact)) {
            return;
        }
        rig.key(Key::Char('j'));
        rig.paint();
    }
    panic!("no detail preview names {prefix}: {:?}", body_lines(rig));
}

/// The cells of the frame rectangle, row by row.
fn frame_text(rig: &Rig) -> Vec<String> {
    let rect = frame_rect(rig);
    let buffer = rig.f.buffer();
    (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn copies(rig: &mut Rig) -> Arc<Mutex<Vec<String>>> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let seen = sink.clone();
    rig.f.h.state.clipboard_copier = Some(Arc::new(move |text| {
        seen.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    }));
    sink
}

fn last_notice(rig: &Rig) -> String {
    rig.f
        .h
        .state
        .notifications
        .records()
        .last()
        .expect("a notice was raised")
        .message
        .clone()
}

fn window(rig: &Rig) -> &ArtifactWindowState {
    rig.f
        .h
        .state
        .artifact_window
        .as_ref()
        .expect("the artifact window is open")
}

fn hazardous(c: char) -> bool {
    c == '\u{1b}'
        || c.is_control()
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

#[tokio::test]
async fn the_artifacts_tab_lists_every_row_with_its_lineage() {
    let mut rig = fixture(260, 30, b"evidence body", b"patch body").await;
    open_tab(&mut rig).await;
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(top_title(&rig).as_deref(), Some(TAB_TITLE));
    assert_eq!(rig.f.h.state.mission_control.tab, McTab::Artifacts);
    let painted = body_lines(&rig).join("\n");
    for expected in [
        "evidence aaaaaa",
        "patch bbbbbb",
        "input-request cccccc",
        "as of",
    ] {
        assert!(painted.contains(expected), "{expected}: {painted}");
    }
    assert!(
        !painted.to_ascii_lowercase().contains(" live"),
        "the fold is never claimed live: {painted}"
    );
    assert!(
        rail_line(&rig).contains("Esc") && rail_line(&rig).contains("Back"),
        "{}",
        rail_line(&rig)
    );
    // The selected row's detail (wide: next to the list) carries the lineage, one level.
    select_row(&mut rig, "bbbbbb");
    assert!(
        rail_line(&rig).contains("Open"),
        "a selected row offers Enter: {}",
        rail_line(&rig)
    );
    let detail = body_lines(&rig).join("\n");
    assert!(
        detail.contains("depends on aaaaaa"),
        "the lineage line renders: {detail}"
    );
}

#[tokio::test]
async fn enter_and_artifact_show_open_the_same_artifact_window() {
    // Through the hub: select a row, `Enter`, the effect the event loop runs for the action.
    let mut hub = fixture(260, 40, b"evidence body", b"patch body\nsecond line").await;
    open_tab(&mut hub).await;
    select_row(&mut hub, "bbbbbb");
    let action = hub.key(Key::Enter);
    assert_eq!(action, InputAction::OpenArtifactWindow(full_id('b')));
    artifact_bridge::open_window(
        &hub.app,
        &mut hub.f.h.state,
        CONVERSATION,
        &full_id('b'),
        PermissionMode::Yolo,
    )
    .await;
    hub.paint();
    // Through the typed verb on a fresh session.
    let mut typed = fixture(260, 40, b"evidence body", b"patch body\nsecond line").await;
    slash_artifact(&mut typed, "/artifact show bbbbbb").await;
    for rig in [&hub, &typed] {
        assert_eq!(top_surface(rig), Some(Surface::ArtifactWindow));
        assert_eq!(top_title(rig).as_deref(), Some("Artifact · patch bbbbbb"));
        assert_eq!(window(rig).id, full_id('b'));
        assert!(presentation::consistent(&rig.f.h.state));
    }
    // The title row differs by door (stacked over the hub it carries the breadcrumb); every
    // row below it is the same facts and body.
    assert_eq!(
        frame_text(&hub)[1..],
        frame_text(&typed)[1..],
        "the same facts and body are painted by either door"
    );
    let painted = frame_text(&typed).join("\n");
    for expected in [
        "patch bbbbbb",
        "producer  spoke-2",
        "patch body",
        "second line",
    ] {
        assert!(painted.contains(expected), "{expected}: {painted}");
    }
    // The window backs to what is beneath it: the hub on its tab.
    hub.key(Key::Esc);
    assert_eq!(top_surface(&hub), Some(Surface::MissionControl));
    assert_eq!(top_title(&hub).as_deref(), Some(TAB_TITLE));
    // A second `show` retargets the one window.
    slash_artifact(&mut typed, "/artifact show aaaaaa").await;
    assert_eq!(window(&typed).id, full_id('a'));
    assert_eq!(
        top_title(&typed).as_deref(),
        Some("Artifact · evidence aaaaaa")
    );
    assert_eq!(
        typed
            .f
            .h
            .state
            .modal_stack
            .iter()
            .filter(|frame| frame.surface == Surface::ArtifactWindow)
            .count(),
        1
    );
}

#[tokio::test]
async fn v_toggles_rendered_and_source_and_a_link_shows_its_url() {
    let body = b"# Heading\n\nSee [the docs](https://example.com/docs) now.\n";
    let mut rig = fixture(130, 40, b"", body).await;
    slash_artifact(&mut rig, "/artifact show bbbbbb").await;
    assert_eq!(
        window(&rig).mode,
        BodyMode::Rendered,
        "Rendered is the default"
    );
    let rendered = frame_text(&rig).join("\n");
    assert!(
        rendered.contains("the docs (https://example.com/docs)"),
        "a link paints as `label (url)`: {rendered}"
    );
    assert!(!rendered.contains("[the docs]"), "{rendered}");
    assert!(rail_line(&rig).contains("Source"), "{}", rail_line(&rig));
    rig.key(Key::Char('v'));
    rig.paint();
    assert_eq!(window(&rig).mode, BodyMode::Source);
    let source = frame_text(&rig).join("\n");
    assert!(source.contains("# Heading"), "{source}");
    assert!(
        source.contains("[the docs](https://example.com/docs)"),
        "{source}"
    );
    assert!(rail_line(&rig).contains("Rendered"));
    rig.key(Key::Char('v'));
    rig.paint();
    assert_eq!(window(&rig).mode, BodyMode::Rendered);
}

#[tokio::test]
async fn escape_and_bidi_bytes_never_reach_a_cell_and_copy_returns_the_original_bytes() {
    let original = "plain \u{1b}[31mred\u{1b}[0m \u{202e}rtl\u{2066}iso\u{7} end\n";
    let mut rig = fixture(130, 40, b"", original.as_bytes()).await;
    slash_artifact(&mut rig, "/artifact show bbbbbb").await;
    let sink = copies(&mut rig);
    for mode_key in [None, Some(Key::Char('v'))] {
        if let Some(key) = mode_key {
            rig.key(key);
            rig.paint();
        }
        let cells: Vec<char> = rig
            .f
            .buffer()
            .content
            .iter()
            .flat_map(|cell| cell.symbol().chars())
            .collect();
        assert!(
            !cells.iter().any(|c| hazardous(*c)),
            "{:?}: no escape, control or bidi override is ever a painted cell",
            window(&rig).mode
        );
        assert!(
            frame_text(&rig).join("\n").contains("red"),
            "the sanitized text is still shown: {:?}",
            frame_text(&rig)
        );
    }
    assert!(sink.lock().unwrap().is_empty(), "nothing copied yet");
    rig.key(Key::Char('c'));
    assert_eq!(
        &*sink.lock().unwrap(),
        &[original.to_owned()],
        "copy returns the stored bytes, not the display"
    );
    assert_eq!(last_notice(&rig), "Copied to clipboard");
}

fn big_window(len: usize) -> ArtifactWindowState {
    ArtifactWindowState::new(
        full_id('b'),
        "patch bbbbbb\n  id        bbbb".to_owned(),
        ArtifactBody::Source(vec![b'a'; len]),
    )
}

// Mutation M4's killer: the bound is judged on the byte length BEFORE any copy string is
// built. A body one byte over the bound is refused with a notice, the clipboard is never
// reached and no string is built; a body exactly at the bound is copied (and built once).
#[tokio::test]
async fn artifact_copy_above_the_bound_builds_nothing() {
    let mut rig = fixture(100, 30, b"", b"").await;
    let sink = copies(&mut rig);
    reducer::open_artifact_window(
        &mut rig.f.h.state,
        big_window(COPY_MAX_BYTES + 1),
        "Artifact · patch bbbbbb".to_owned(),
    );
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::ArtifactWindow));
    assert!(window(&rig).display_truncated());
    assert!(
        window(&rig).live().copy,
        "the key is live (its hint painted): only the bound can refuse it"
    );
    assert!(
        frame_text(&rig).join("\n").contains("showing the first"),
        "{:?}",
        frame_text(&rig)
    );
    rig.key(Key::Char('c'));
    let notice = last_notice(&rig);
    assert!(
        notice.contains("Copy failed") && notice.contains("8 MiB"),
        "the refusal is a notice: {notice}"
    );
    assert!(
        sink.lock().unwrap().is_empty(),
        "the clipboard is never called"
    );
    assert_eq!(window(&rig).copy_builds(), 0, "no copy string was built");

    // Positive control: exactly at the bound the copy is built once and goes out whole. (The
    // hint is set by hand: painting an 8 MiB body is not the property under test.)
    reducer::open_artifact_window(
        &mut rig.f.h.state,
        big_window(COPY_MAX_BYTES),
        "Artifact · patch bbbbbb".to_owned(),
    );
    assert!(!window(&rig).display_truncated());
    window(&rig).set_live(Live {
        search: false,
        copy: true,
        view: false,
    });
    rig.key(Key::Char('c'));
    assert_eq!(window(&rig).copy_builds(), 1);
    {
        let copied = sink.lock().unwrap();
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].len(), COPY_MAX_BYTES);
    }
    assert_eq!(last_notice(&rig), "Copied to clipboard");
}

#[tokio::test]
async fn copy_is_inert_until_its_hint_is_painted() {
    let mut rig = fixture(130, 40, b"", b"patch body").await;
    let sink = copies(&mut rig);
    slash_artifact(&mut rig, "/artifact show bbbbbb").await;
    // A different window replaces it before any draw: the key's hint was never painted.
    reducer::open_artifact_window(
        &mut rig.f.h.state,
        big_window(8),
        "Artifact · patch bbbbbb".to_owned(),
    );
    assert!(!window(&rig).live().copy);
    rig.key(Key::Char('c'));
    assert!(sink.lock().unwrap().is_empty());
    rig.paint();
    rig.key(Key::Char('c'));
    assert_eq!(sink.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unknown_id_is_refused_as_a_warning_and_opens_nothing() {
    let mut rig = fixture(130, 40, b"", b"patch body").await;
    slash_artifact(&mut rig, "/artifact show ffffff").await;
    let warnings = rig.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(!warnings[0].1.is_empty());
    // 7B-01: the runtime delivers a notice only to the active or a background conversation;
    // one addressed to `Some("")` is dropped. The Warning names the asking conversation.
    assert_eq!(warnings[0].0.as_deref(), Some(CONVERSATION), "{warnings:?}");
    assert!(rig.f.h.state.modal_stack.is_empty(), "no frame opened");
    assert!(rig.f.h.state.artifact_window.is_none());
    // Positive control: a known id opens.
    slash_artifact(&mut rig, "/artifact show bbbbbb").await;
    assert_eq!(top_surface(&rig), Some(Surface::ArtifactWindow));
    assert!(rig.warnings().is_empty());
}

#[tokio::test]
async fn an_unreadable_body_is_said_in_the_window_and_never_faked() {
    // `c` has no stored body in the fixture's port.
    let mut rig = fixture(130, 40, b"", b"patch body").await;
    slash_artifact(&mut rig, "/artifact show cccccc").await;
    assert_eq!(top_surface(&rig), Some(Surface::ArtifactWindow));
    let painted = frame_text(&rig).join("\n");
    assert!(painted.contains("body unavailable"), "{painted}");
    // No recorder composed at all: the same honest line, no body invented.
    rig.app.patch_review = None;
    slash_artifact(&mut rig, "/artifact show bbbbbb").await;
    let painted = frame_text(&rig).join("\n");
    assert!(painted.contains("body unavailable"), "{painted}");
    assert!(!painted.contains("patch body"), "{painted}");
}

fn open_body(rig: &mut Rig, body: Vec<u8>) {
    reducer::open_artifact_window(
        &mut rig.f.h.state,
        ArtifactWindowState::new(
            full_id('b'),
            "patch bbbbbb\n  id        bbbb".to_owned(),
            ArtifactBody::Source(body),
        ),
        "Artifact · patch bbbbbb".to_owned(),
    );
    rig.paint();
}

// 7B-02: the markdown parser decodes numeric character references AFTER sanitization, so a
// reference to a bidi format character must be defused to a visible escape first (21-8 8B-01).
#[tokio::test]
async fn numeric_references_to_bidi_are_defused_in_the_rendered_window() {
    let mut rig = fixture(130, 40, b"", b"").await;
    open_body(
        &mut rig,
        b"a&#x202e;b&#8238;c&#X2066;d &amp; &#x41;".to_vec(),
    );
    let painted = frame_text(&rig).join("\n");
    assert!(
        !painted
            .chars()
            .any(|c| ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)),
        "{painted:?}"
    );
    assert!(painted.contains("\\u{202e}"), "{painted}");
    // Positive control: an ordinary reference still decodes.
    assert!(painted.contains("& A"), "{painted}");
}

// 7B-03: a wide code block scrolls sideways on its own through the actual `l` / `h` / `$` keys
// (MW-5/MW-6) and the rail paints `Columns` only where a wide block exists.
#[tokio::test]
async fn a_wide_code_block_scrolls_sideways_through_real_keys_at_a_narrow_frame() {
    let mut rig = fixture(60, 24, b"", b"").await;
    let long = format!("```\n{}END\n```\n", "0123456789".repeat(12));
    open_body(&mut rig, long.into_bytes());
    let before = frame_text(&rig).join("\n");
    assert!(
        before.contains("Columns"),
        "the rail offers the columns keys:\n{before}"
    );
    assert!(
        !before.contains("END"),
        "the block end is clipped:\n{before}"
    );
    for _ in 0..3 {
        rig.key(Key::Char('$'));
        rig.paint();
    }
    let after = frame_text(&rig).join("\n");
    assert!(
        after.contains("END"),
        "`$` reaches the last column:\n{after}"
    );
    rig.key(Key::Char('0'));
    rig.paint();
    assert!(!frame_text(&rig).join("\n").contains("END"));
    // Positive control: a body with no wide block offers no columns hint.
    open_body(&mut rig, b"short".to_vec());
    assert!(!frame_text(&rig).join("\n").contains("Columns"));
}

// 7B-04: a body above the copy bound is still inspectable: its first part is shown as source
// under a notice naming what is not shown; search and `v` stay; Copy keeps its refusal.
#[tokio::test]
async fn a_body_above_the_copy_bound_is_shown_truncated_with_a_notice_never_replaced() {
    let mut rig = fixture(130, 40, b"", b"").await;
    let sink = copies(&mut rig);
    let mut body = b"first line of a huge body\n".to_vec();
    body.resize(COPY_MAX_BYTES + 10, b'z');
    open_body(&mut rig, body);
    let painted = frame_text(&rig).join("\n");
    assert!(painted.contains("first line of a huge body"), "{painted}");
    assert!(painted.contains("are not shown"), "{painted}");
    assert!(window(&rig).display_truncated());
    assert!(window(&rig).live().search, "search stays available");
    rig.key(Key::Char('c'));
    assert!(sink.lock().unwrap().is_empty());
    assert_eq!(window(&rig).copy_builds(), 0);
}

// 7B-08: a body that is not UTF-8 is never copied as a lossy "original".
#[tokio::test]
async fn a_non_utf8_body_is_refused_for_copy_with_nothing_on_the_clipboard() {
    let mut rig = fixture(130, 40, b"", b"").await;
    let sink = copies(&mut rig);
    open_body(&mut rig, vec![0xff, b'a', b'\n']);
    rig.key(Key::Char('c'));
    assert!(sink.lock().unwrap().is_empty());
    assert_eq!(window(&rig).copy_builds(), 0);
    assert!(
        last_notice(&rig).contains("not valid UTF-8"),
        "{}",
        last_notice(&rig)
    );
}
