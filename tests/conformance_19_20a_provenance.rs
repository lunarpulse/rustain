//! Story 19.20a K03/K05: durable carrier compatibility and production rendering.
//! Act production is covered by K01/K07; synthetic User/System cases below are
//! renderer-robustness evidence, not claims about production answering acts.
#![cfg(unix)]

#[path = "common/render_helpers.rs"]
mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use rustain::adapters::daemon::protocol::ToolInputWithheld;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::{state::TabRenderState, theme::Theme, widgets::chat_pane};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    Authorship, ChannelKind, ChatMessage, ContentBlockType, Conversation, InvocationStatus,
    MessageRole, SendAuthorization, StopReason, StreamingState, ViewState,
};
use rustain::domain::ports::StoragePort;
use serde_json::{Value, json};

const RETRACTED_AT: i64 = 1_700_000_123_000;

#[tokio::test]
async fn journal_rows_keep_unknown_defaults_and_survive_future_tokens() {
    use rustain::domain::models::{
        AgentId, DispatchAct, ItemAddress, ItemId, JournalRecord, PeerId, ReplyKind, ReplyTurnId,
        RoomEvent, TurnAddress,
    };
    use rustain::domain::ports::RoomJournalReader;
    use rustain::domain::services::transparency::fold_transparency;
    use rustain::infrastructure::subagent::node_journal::{NodeJournal, WorkspaceJournalReader};

    let peer = PeerId::from_public_key(&[61; 32]).unwrap();
    let address = ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay("ri_question"));
    for carrier in 0..3 {
        for shape in [
            "missing",
            "future-authorship",
            "future-authorization",
            "known",
            "composed-only",
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let journal = NodeJournal::open_workspace(workspace.path()).await.unwrap();
            let known = matches!(shape, "known" | "composed-only");
            let authorship = if known {
                Authorship::AgentComposed
            } else {
                Authorship::Unknown
            };
            let send_authorization = if shape == "known" && carrier != 1 {
                SendAuthorization::Automatic
            } else if known {
                SendAuthorization::HumanDecided
            } else {
                SendAuthorization::Unknown
            };
            let event = match carrier {
                0 => RoomEvent::RemoteEnvelopeDispatched {
                    peer: peer.clone(),
                    task: Some("provenance-task".into()),
                    bytes: 4,
                    act: DispatchAct::Task,
                    content_hash: None,
                    endpoint: None,
                    action: None,
                    authorship,
                    send_authorization,
                },
                1 => RoomEvent::RecipientItemReplied {
                    address: TurnAddress::answering(
                        &address,
                        ReplyTurnId::from_replay("rr_answer"),
                    ),
                    answers: address.item().clone(),
                    content: "body".into(),
                    kind: ReplyKind::Answer,
                    via_node: None,
                    authorship,
                    send_authorization,
                },
                _ => RoomEvent::PeerDisclosure {
                    peer: Some(peer.clone()),
                    node: AgentId::root(),
                    task: Some("provenance-task".into()),
                    disclosed_bytes: 4,
                    authorship,
                    send_authorization,
                },
            };
            journal.append_room(event.clone()).await.unwrap();
            journal.append_room(event).await.unwrap();
            let raw = tokio::fs::read_to_string(journal.path()).await.unwrap();
            let mut lines: Vec<Value> = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            if !known {
                let payload = &lines[0]["record"]["payload"];
                assert_eq!(
                    payload.get("authorship").is_some(),
                    carrier == 1,
                    "M19: reply always writes authorship; other carriers omit Unknown"
                );
                assert!(
                    payload.get("send_authorization").is_none(),
                    "Unknown authorization must be omitted"
                );
                // Legacy missing fields are exercised even for replies, whose writer always emits authorship.
                lines[0]["record"]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("authorship");
                if shape == "future-authorship" {
                    lines[0]["record"]["payload"]["authorship"] = json!("future-author");
                } else if shape == "future-authorization" {
                    lines[0]["record"]["payload"]["send_authorization"] =
                        json!("future-authorizer");
                }
                let bytes = lines
                    .iter()
                    .map(|line| format!("{line}\n"))
                    .collect::<String>();
                tokio::fs::write(journal.path(), bytes).await.unwrap();
            }
            let reopened = NodeJournal::open_workspace(workspace.path()).await.unwrap();
            let direct = reopened
                .load()
                .await
                .expect("M14/M15: future token on NON-LAST row must not be CorruptRecord");
            let reader = WorkspaceJournalReader::open_workspace(workspace.path());
            let projected = reader
                .load_entries()
                .await
                .expect("future journal remains readable through production reader");
            for entries in [&direct, &projected] {
                assert_eq!(
                    entries.len(),
                    2,
                    "both non-last future row and valid following row survive"
                );
                for entry in entries.iter() {
                    let (a, s) = match &entry.record {
                        JournalRecord::Room(
                            RoomEvent::RemoteEnvelopeDispatched {
                                authorship,
                                send_authorization,
                                ..
                            }
                            | RoomEvent::RecipientItemReplied {
                                authorship,
                                send_authorization,
                                ..
                            }
                            | RoomEvent::PeerDisclosure {
                                authorship,
                                send_authorization,
                                ..
                            },
                        ) => (*authorship, *send_authorization),
                        other => panic!("unexpected replayed record: {other:?}"),
                    };
                    assert_eq!(
                        a, authorship,
                        "M18: journal missing/future authorship defaults to Unknown"
                    );
                    assert_eq!(
                        s, send_authorization,
                        "journal authorization round-trips or defaults to Unknown"
                    );
                }
                let rows = fold_transparency(entries.iter());
                assert_eq!(rows.len(), 2);
                for row in rows {
                    assert_eq!(
                        row.summary.contains("[agent-composed]"),
                        known,
                        "M35/M43: truthfully render authorship: {}",
                        row.summary
                    );
                    assert_eq!(
                        row.summary.contains("[auto-sent]"),
                        send_authorization == SendAuthorization::Automatic,
                        "M42: only Automatic renders auto-sent: {}",
                        row.summary
                    );
                    if send_authorization == SendAuthorization::Automatic {
                        assert!(row.summary.ends_with(" [agent-composed] [auto-sent]"));
                    } else if known {
                        assert!(row.summary.ends_with(" [agent-composed]"));
                    }
                }
            }
        }
    }
}
const BODY: &str = "PROVENANCE_BODY_END";

struct Painted {
    rows: Vec<String>,
    result: chat_pane::RenderResult,
    trailing_blank_rows: usize,
}

// These are the two production front doors used by event_loop and attach_loop;
// deliberately do not use chat_pane::render (the test-only wrapper).
#[allow(clippy::too_many_arguments)]
fn paint(
    conversation: &Conversation,
    view: &ViewState,
    width: u16,
    height: u16,
    attached: bool,
    scroll: usize,
    cache: &mut TabRenderState,
    withheld: &HashMap<String, ToolInputWithheld>,
    bookmarks: &[usize],
) -> Painted {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = Theme::dark();
    let streaming = StreamingState::default();
    let clock = MockClock::at_wall_ms(1_700_000_005_000);
    let mut result = None;
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            result = Some(if attached {
                chat_pane::render_attached(
                    frame,
                    area,
                    conversation,
                    &streaming,
                    scroll,
                    false,
                    &theme,
                    cache,
                    &HashMap::new(),
                    &BTreeMap::new(),
                    None,
                    withheld,
                )
            } else {
                chat_pane::render_with_search(
                    frame,
                    area,
                    conversation,
                    None,
                    &streaming,
                    view,
                    &clock,
                    scroll,
                    false,
                    &theme,
                    cache,
                    &HashMap::new(),
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    bookmarks,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            });
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    Painted {
        rows,
        result: result.unwrap(),
        // All plain local fixtures end in one Markdown paragraph. Its layout
        // contract emits one trailing blank row (markdown/layout.rs); unlike
        // turn prose, that blank has no painted rail glyph. PeerText is plain
        // wrapped text, not Markdown, and has no such trailing row.
        trailing_blank_rows: usize::from(conversation.messages.last().is_some_and(|message| {
            !message.content_blocks.contains(&ContentBlockType::PeerText)
                && !(message.role == MessageRole::Assistant
                    && conversation
                        .turns
                        .iter()
                        .any(|turn| turn.id.0 == message.id))
        })),
    }
}

fn full_paint(conversation: &Conversation, attached: bool) -> Painted {
    paint(
        conversation,
        &ViewState::default(),
        80,
        100,
        attached,
        0,
        &mut TabRenderState::default(),
        &HashMap::new(),
        &[],
    )
}

fn assert_height(painted: &Painted) {
    // Count physical rows, including internal blank spacing. Account for the
    // known final Markdown paragraph blank, not unpainted viewport padding;
    // turn rails and collapsed summaries remain measured by their last glyph.
    let painted_height = painted
        .rows
        .iter()
        .rposition(|row| !row.is_empty())
        .expect("positive control: the fixture must actually be painted")
        + 1
        + painted.trailing_blank_rows;
    assert_eq!(
        painted.result.total_content_height, painted_height,
        "M10–M13/M34/M58: painted physical rows must equal computed height: {:?}",
        painted.rows
    );
}

fn assert_chrome(painted: &Painted, message: &ChatMessage, turn_backed: bool) {
    let tags: Vec<_> = painted
        .rows
        .iter()
        .filter(|row| row.contains("[agent-composed]"))
        .collect();
    assert_eq!(
        tags.len(),
        usize::from(message.authorship == Authorship::AgentComposed),
        "M01: authorship independently controls exactly one neutral tag"
    );
    if let Some(tag) = tags.first() {
        if turn_backed {
            assert_eq!(
                tag.as_str(),
                "[agent-composed]",
                "turn tag is a separate neutral prefix line"
            );
        } else {
            let role = match message.role {
                MessageRole::Assistant => "Assistant:",
                MessageRole::User => "You:",
                MessageRole::System => "System:",
            };
            assert!(tag.ends_with(&format!("{role}  [agent-composed]")), "{tag}");
        }
    }
    let automatic = message.send_authorization == SendAuthorization::Automatic;
    let marker = match (automatic, message.retracted_at_ms) {
        (true, None) => Some("┆ [auto-sent]  [✗] Retract"),
        (true, Some(_)) => Some("┆ [auto-sent]  [retracted 22:15]  [✗] Retract (used)"),
        (false, Some(_)) => Some("┆ [retracted 22:15]  [✗] Retract (used)"),
        (false, None) => None,
    };
    let marker_rows: Vec<_> = painted
        .rows
        .iter()
        .filter(|row| row.contains("[✗] Retract"))
        .collect();
    assert_eq!(
        marker_rows.len(),
        usize::from(marker.is_some()),
        "M01/M34: authorization or recorded retraction, never authorship, controls the marker"
    );
    if let Some(expected) = marker {
        assert_eq!(marker_rows[0].as_str(), expected, "exact A5 marker copy");
    }
    assert_eq!(
        painted.rows.iter().any(|row| row.contains("[auto-sent]")),
        automatic,
        "future/legacy authorization must not fabricate automation"
    );
}

fn session_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.meta.json"))
}

fn read_session(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

async fn both_loaders(root: &Path, id: &str) -> [Conversation; 2] {
    // Fresh instances exercise startup and daemon restore separately.
    let startup = FileSystemStorage::new(root.to_path_buf())
        .load_conversation_with_exit(id)
        .await
        .expect("K03 startup loader must accept future provenance tokens")
        .expect("startup loader retains session")
        .0;
    let daemon = FileSystemStorage::new(root.to_path_buf())
        .load_conversation(id)
        .await
        .expect("K03 daemon loader must accept future provenance tokens")
        .expect("daemon loader retains session");
    [startup, daemon]
}

#[tokio::test]
async fn session_files_keep_carrier_defaults_and_survive_future_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let storage = FileSystemStorage::new(root.to_path_buf());
    // One on-disk row is rewritten per case; no direct serde-deserialization
    // substitute for either production loader (K03, M14–M17/M20/M21/M36).
    let cases = [
        (
            Some("agent-composed"),
            Some("automatic"),
            Authorship::AgentComposed,
            SendAuthorization::Automatic,
        ),
        (
            Some("agent-composed"),
            Some("human-decided"),
            Authorship::AgentComposed,
            SendAuthorization::HumanDecided,
        ),
        (
            Some("human-written"),
            Some("human-decided"),
            Authorship::HumanWritten,
            SendAuthorization::HumanDecided,
        ),
        (
            Some("unknown"),
            Some("unknown"),
            Authorship::Unknown,
            SendAuthorization::Unknown,
        ),
        (
            Some("future-authorship"),
            Some("automatic"),
            Authorship::Unknown,
            SendAuthorization::Automatic,
        ),
        (
            Some("agent-composed"),
            Some("future-authorization"),
            Authorship::AgentComposed,
            SendAuthorization::Unknown,
        ),
        (
            None,
            None,
            Authorship::HumanWritten,
            SendAuthorization::Unknown,
        ),
    ];
    for (index, (auth_token, send_token, authorship, authorization)) in
        cases.into_iter().enumerate()
    {
        let id = format!("k03-{index}");
        let mut message = common::make_msg("carrier", MessageRole::Assistant);
        message.content = BODY.into();
        message.authorship = authorship;
        message.send_authorization = authorization;
        let mut conversation = common::make_conversation(vec![message], vec![]);
        conversation.id = id.clone();
        storage.save_conversation(&conversation).await.unwrap();
        let path = session_path(root, &id);
        let mut disk = read_session(&path);
        // Positive serialization controls precede the future-token rewrite,
        // so omission of a known nondefault value cannot be hidden by it.
        if authorship == Authorship::AgentComposed {
            assert_eq!(
                disk["messages"][0]["authorship"], "agent-composed",
                "K03 known authorship round-trips"
            );
        }
        if authorship == Authorship::HumanWritten {
            assert!(
                disk["messages"][0].get("authorship").is_none(),
                "M17/M16: legacy authorship bytes unchanged"
            );
        }
        match authorization {
            SendAuthorization::Automatic => {
                assert_eq!(
                    disk["messages"][0]["sendAuthorization"], "automatic",
                    "K03 known Automatic authorization round-trips"
                );
            }
            SendAuthorization::HumanDecided => {
                assert_eq!(
                    disk["messages"][0]["sendAuthorization"], "human-decided",
                    "K03 known HumanDecided authorization round-trips"
                );
            }
            SendAuthorization::Unknown => {
                assert!(
                    disk["messages"][0].get("sendAuthorization").is_none(),
                    "M20: ChatMessage omits Unknown send authorization"
                );
            }
        }
        let row = disk["messages"][0].as_object_mut().unwrap();
        row.remove("authorship");
        row.remove("sendAuthorization");
        if let Some(token) = auth_token {
            row.insert("authorship".into(), json!(token));
        }
        if let Some(token) = send_token {
            row.insert("sendAuthorization".into(), json!(token));
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();
        for loaded in both_loaders(root, &id).await {
            let row = &loaded.messages[0];
            assert_eq!(
                row.authorship, authorship,
                "M16: missing authorship is HumanWritten; future authorship is Unknown: {auth_token:?}"
            );
            assert_eq!(
                row.send_authorization, authorization,
                "authorization axis: {send_token:?}"
            );
            for attached in [false, true] {
                let painted = full_paint(&loaded, attached);
                assert_height(&painted);
                assert_chrome(&painted, row, true);
                assert!(painted.rows.iter().any(|line| line.contains(BODY)));
            }
            if auth_token.is_none() {
                FileSystemStorage::new(root.to_path_buf())
                    .save_conversation(&loaded)
                    .await
                    .unwrap();
                let saved = read_session(&path);
                assert!(
                    saved["messages"][0].get("authorship").is_none(),
                    "M17/M16: legacy authorship bytes unchanged"
                );
                assert!(
                    saved["messages"][0].get("sendAuthorization").is_none(),
                    "M20: legacy re-save omits Unknown send authorization"
                );
            }
        }
    }

    // Default construction is independently checked through real persistence.
    let mut default_conversation = common::make_conversation(vec![ChatMessage::default()], vec![]);
    default_conversation.id = "k03-default".into();
    storage
        .save_conversation(&default_conversation)
        .await
        .unwrap();
    let default_disk = read_session(&session_path(root, "k03-default"));
    assert!(
        default_disk["messages"][0].get("authorship").is_none(),
        "M21: default row saves neither key (authorship)"
    );
    assert!(
        default_disk["messages"][0]
            .get("sendAuthorization")
            .is_none(),
        "M21/M20: default row saves neither key (send authorization)"
    );

    // AC2(b2): messages-only legacy files migrate to turns in BOTH loaders.
    // rebuild_messages_mirror is the stated nearest public production seam;
    // reducer's test-compatibility path is intentionally not involved.
    let mut automatic = common::make_msg("mirror-auto", MessageRole::Assistant);
    automatic.content = "MIRROR_AUTOMATIC".into();
    automatic.authorship = Authorship::AgentComposed;
    automatic.send_authorization = SendAuthorization::Automatic;
    let mut legacy = common::make_msg("mirror-legacy", MessageRole::Assistant);
    legacy.content = "MIRROR_LEGACY".into();
    legacy.authorship = Authorship::AgentComposed;
    legacy.retracted_at_ms = Some(RETRACTED_AT);
    let mut conversation =
        common::make_conversation(vec![automatic.clone(), legacy.clone()], vec![]);
    conversation.id = "k03-mirror".into();
    storage.save_conversation(&conversation).await.unwrap();
    let mirror_path = session_path(root, "k03-mirror");
    let mut disk = read_session(&mirror_path);
    assert!(disk["turns"].as_array().is_none_or(Vec::is_empty));
    disk["messages"][1]
        .as_object_mut()
        .unwrap()
        .remove("sendAuthorization");
    std::fs::write(&mirror_path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();
    for mut loaded in both_loaders(root, "k03-mirror").await {
        assert_eq!(
            loaded.turns.len(),
            2,
            "both Assistant rows must migrate into turns"
        );
        loaded.rebuild_messages_mirror();
        for expected in [&automatic, &legacy] {
            let row = loaded
                .messages
                .iter()
                .find(|row| row.id == expected.id)
                .unwrap();
            assert_eq!(
                row.authorship, expected.authorship,
                "M50: mirror keeps authorship"
            );
            assert_eq!(
                row.send_authorization, expected.send_authorization,
                "M50: mirror keeps authorization"
            );
            assert_eq!(
                row.retracted_at_ms, expected.retracted_at_ms,
                "M50: mirror keeps exact retraction timestamp"
            );
        }
        for attached in [false, true] {
            let painted = full_paint(&loaded, attached);
            assert_height(&painted);
            let legacy_start = painted.result.message_boundaries[1];
            let first = Painted {
                rows: painted.rows[..legacy_start].to_vec(),
                result: painted.result,
                trailing_blank_rows: 0,
            };
            assert_chrome(&first, &automatic, true);
            let second = Painted {
                rows: painted.rows[legacy_start..].to_vec(),
                result: first.result,
                trailing_blank_rows: 0,
            };
            assert_chrome(&second, &legacy, true);
        }
    }
    // Also exercise the legacy plain-row branch: its role tag and consumed
    // retract line must remain visible without claiming an automatic send.
    let plain_legacy = common::make_conversation(vec![legacy.clone()], vec![]);
    for attached in [false, true] {
        let painted = full_paint(&plain_legacy, attached);
        assert_height(&painted);
        assert_chrome(&painted, &legacy, false);
    }
}

// K04 journal keystone is intentionally owned separately; keep this target's
// shared production-render helpers available to that addition.

#[test]
fn two_axis_render_height_and_cache_matrix() {
    for width in [60, 80, 130] {
        for authorship in [
            Authorship::HumanWritten,
            Authorship::AgentComposed,
            Authorship::Unknown,
        ] {
            for authorization in [
                SendAuthorization::HumanDecided,
                SendAuthorization::Automatic,
                SendAuthorization::Unknown,
            ] {
                for retracted in [false, true] {
                    for peer in [false, true] {
                        // User/System combinations are renderer robustness only.
                        for role in [
                            MessageRole::Assistant,
                            MessageRole::User,
                            MessageRole::System,
                        ] {
                            let mut message = common::make_msg("matrix", role);
                            message.content = BODY.into();
                            message.authorship = authorship;
                            message.send_authorization = authorization;
                            message.retracted_at_ms = retracted.then_some(RETRACTED_AT);
                            if peer {
                                message.content_blocks.push(ContentBlockType::PeerText);
                            }
                            let conversation =
                                common::make_conversation(vec![message.clone()], vec![]);
                            for attached in [false, true] {
                                let painted = paint(
                                    &conversation,
                                    &ViewState::default(),
                                    width,
                                    100,
                                    attached,
                                    0,
                                    &mut TabRenderState::default(),
                                    &HashMap::new(),
                                    &[],
                                );
                                assert_height(&painted);
                                assert_chrome(&painted, &message, false);
                                assert!(painted.rows.iter().any(|row| row.contains(BODY)));
                            }
                        }
                        // Turn-backed coverage is Assistant-only. Use actual parts,
                        // and change collapse only through the production ViewState.
                        let turn = common::make_turn(
                            "matrix-turn",
                            vec![common::prose(BODY)],
                            Some(StopReason::EndTurn),
                        );
                        let mut message = common::make_msg("matrix-turn", MessageRole::Assistant);
                        message.authorship = authorship;
                        message.send_authorization = authorization;
                        message.retracted_at_ms = retracted.then_some(RETRACTED_AT);
                        if peer {
                            message.content_blocks.push(ContentBlockType::PeerText);
                        }
                        let conversation =
                            common::make_conversation(vec![message.clone()], vec![turn.clone()]);
                        for collapsed in [false, true] {
                            let mut view = ViewState::default();
                            view.collapsed.insert(turn.id.clone(), collapsed);
                            let painted = paint(
                                &conversation,
                                &view,
                                width,
                                100,
                                false,
                                0,
                                &mut TabRenderState::default(),
                                &HashMap::new(),
                                &[],
                            );
                            assert_height(&painted);
                            assert_chrome(&painted, &message, true);
                            let prefix = usize::from(authorship == Authorship::AgentComposed)
                                + usize::from(
                                    authorization == SendAuthorization::Automatic || retracted,
                                );
                            if !collapsed {
                                assert_eq!(
                                    painted.rows.iter().position(|row| row.contains(BODY)),
                                    Some(prefix)
                                );
                                assert!(painted.result.block_boundaries.contains(&prefix));
                            } else {
                                assert!(painted.rows[prefix].contains('▸'));
                                if authorship == Authorship::AgentComposed {
                                    assert_eq!(painted.rows[0], "[agent-composed]");
                                }
                                if authorization == SendAuthorization::Automatic || retracted {
                                    let marker_row =
                                        usize::from(authorship == Authorship::AgentComposed);
                                    assert!(painted.rows[marker_row].contains("[✗] Retract"));
                                }
                                assert_eq!(
                                    painted.result.block_boundaries,
                                    vec![0],
                                    "collapsed rows retain only their message boundary, with no internal block jumps"
                                );
                            }
                        }
                        let painted = paint(
                            &conversation,
                            &ViewState::default(),
                            width,
                            100,
                            true,
                            0,
                            &mut TabRenderState::default(),
                            &HashMap::new(),
                            &[],
                        );
                        assert_height(&painted);
                        assert_chrome(&painted, &message, true);
                    }
                }
            }
        }
        assert_all_role_prefixes_fit(width);
        assert_turn_boundaries_and_focus(width);
        assert_authorization_only_cache_flip(width);
    }
}

fn assert_all_role_prefixes_fit(width: u16) {
    let mut message = common::make_msg("prefixes", MessageRole::Assistant);
    message.content = BODY.into();
    message.origin = ChannelKind::Telegram;
    message.synthetic = true;
    message.authorship = Authorship::AgentComposed;
    let mut conversation = common::make_conversation(vec![message], vec![]);
    conversation.fork_source = Some(
        serde_json::from_value(json!({
            "conversationId": "source", "messageIndex": 0, "checkpointId": 0
        }))
        .unwrap(),
    );
    let painted = paint(
        &conversation,
        &ViewState::default(),
        width,
        100,
        false,
        0,
        &mut TabRenderState::default(),
        &HashMap::new(),
        &[0],
    );
    assert_height(&painted);
    assert!(painted.rows[0].contains("[telegram]"));
    assert!(painted.rows[0].contains('🔀'));
    assert!(painted.rows[0].contains(Theme::dark().bookmark_glyph.trim()));
    assert!(painted.rows[0].contains('⤷'));
    assert!(painted.rows[0].ends_with("Assistant:  [agent-composed]"));
    assert!(
        painted.rows[1].contains(BODY),
        "all role prefixes occupy exactly one row at floor width"
    );
}

fn assert_authorization_only_cache_flip(width: u16) {
    for role in [MessageRole::User, MessageRole::System] {
        let mut message = common::make_msg("same-cache-key", role);
        message.content = BODY.into();
        let mut conversation = common::make_conversation(vec![message], vec![]);
        // One cache, same row id/body/width: two helper calls that recreate the
        // cache would not kill M13. Only authorization changes between renders.
        let mut cache = TabRenderState::default();
        let before = paint(
            &conversation,
            &ViewState::default(),
            width,
            100,
            false,
            0,
            &mut cache,
            &HashMap::new(),
            &[],
        );
        assert_height(&before);
        assert_chrome(&before, &conversation.messages[0], false);
        conversation.messages[0].send_authorization = SendAuthorization::Automatic;
        let after = paint(
            &conversation,
            &ViewState::default(),
            width,
            100,
            false,
            0,
            &mut cache,
            &HashMap::new(),
            &[],
        );
        assert_height(&after);
        assert_chrome(&after, &conversation.messages[0], false);
        assert_eq!(
            after.result.total_content_height,
            before.result.total_content_height + 1,
            "M13: authorization-only change invalidates the same row's cached height"
        );
        assert_eq!(
            after.rows.iter().position(|row| row.contains(BODY)),
            before
                .rows
                .iter()
                .position(|row| row.contains(BODY))
                .map(|line| line + 1)
        );
    }
}

fn assert_turn_boundaries_and_focus(width: u16) {
    let mut turn = common::make_turn(
        "offset-turn",
        vec![
            common::prose("BLOCK_BEFORE"),
            common::tool("Read", InvocationStatus::Success),
            common::prose("BLOCK_BETWEEN"),
            common::tool("Bash", InvocationStatus::Success),
            common::prose("BLOCK_AFTER"),
        ],
        Some(StopReason::EndTurn),
    );
    // common::make_turn preserves supplied part ids. Give this multi-tool
    // fixture distinct real ids so focus evidence cannot alias both tools.
    for (index, part) in turn.parts.iter_mut().enumerate() {
        match part {
            rustain::domain::models::turn::TurnPart::Prose { id, .. }
            | rustain::domain::models::turn::TurnPart::ToolInvocation { id, .. } => {
                *id = rustain::domain::models::turn::PartId(index as u64);
            }
            _ => unreachable!("this fixture contains only prose and tools"),
        }
    }
    let tool_ids: Vec<_> = turn
        .parts
        .iter()
        .filter_map(|part| {
            if let rustain::domain::models::turn::TurnPart::ToolInvocation { id, tool, .. } = part {
                Some((
                    rustain::domain::models::turn::tool_call_id_for(&turn.id, *id),
                    tool.as_str(),
                ))
            } else {
                None
            }
        })
        .collect();
    for (authorship, authorization) in [
        (Authorship::HumanWritten, SendAuthorization::Unknown),
        (Authorship::AgentComposed, SendAuthorization::HumanDecided),
        (Authorship::AgentComposed, SendAuthorization::Automatic),
    ] {
        let mut message = common::make_msg("offset-turn", MessageRole::Assistant);
        message.authorship = authorship;
        message.send_authorization = authorization;
        // A preceding message forces turn-relative cached offsets to be
        // translated to conversation coordinates exactly once.
        let mut preceding = common::make_msg("preceding", MessageRole::User);
        preceding.content = "PRECEDING_BODY".into();
        let conversation =
            common::make_conversation(vec![preceding, message.clone()], vec![turn.clone()]);
        for attached in [false, true] {
            // F10: nonempty map exercises the uncached attached layout path;
            // the empty map is its positive cached-path control.
            for withheld_nonempty in [false, true] {
                if withheld_nonempty && (!attached || authorization != SendAuthorization::Automatic)
                {
                    continue;
                }
                let mut withheld = HashMap::new();
                if withheld_nonempty {
                    withheld.insert(tool_ids[0].0.clone(), ToolInputWithheld::PeerTier);
                }
                let mut cache = TabRenderState::default();
                let full = paint(
                    &conversation,
                    &ViewState::default(),
                    width,
                    100,
                    attached,
                    0,
                    &mut cache,
                    &withheld,
                    &[],
                );
                assert_height(&full);
                assert_chrome(&full, &message, true);
                let turn_start = full.result.message_boundaries[1];
                let prefix = usize::from(authorship == Authorship::AgentComposed)
                    + usize::from(authorization == SendAuthorization::Automatic)
                    + usize::from(attached);
                assert_eq!(
                    full.rows
                        .iter()
                        .position(|row| row.contains("BLOCK_BEFORE")),
                    Some(turn_start + prefix)
                );
                for label in ["BLOCK_BEFORE", "BLOCK_BETWEEN", "BLOCK_AFTER"] {
                    let painted_start = full
                        .rows
                        .iter()
                        .position(|row| row.contains(label))
                        .unwrap();
                    assert!(
                        full.result.block_boundaries.contains(&painted_start),
                        "M34/M58: painted prose block {label} at {painted_start} must match public boundaries {:?}",
                        full.result.block_boundaries
                    );
                }
                for (id, name) in &tool_ids {
                    let start = full
                        .rows
                        .iter()
                        .position(|row| row.contains(name))
                        .expect("positive control: every tool header is painted");
                    assert!(
                        full.result.block_boundaries.contains(&start),
                        "painted tool {name} starts at public block boundary {start}"
                    );
                    // Clip both before and after the start, keeping the SAME
                    // TabRenderState: caches remain turn-relative on reuse.
                    for viewport in [1_u16, 3] {
                        let scroll = full
                            .result
                            .total_content_height
                            .saturating_sub(viewport as usize)
                            .saturating_sub(start);
                        let clipped = paint(
                            &conversation,
                            &ViewState::default(),
                            width,
                            viewport,
                            attached,
                            scroll,
                            &mut cache,
                            &withheld,
                            &[],
                        );
                        assert!(
                            clipped.rows[0].contains(name),
                            "scroll tool {name} to viewport top: {:?}",
                            clipped.rows
                        );
                        assert_eq!(
                            clipped.result.focused_tool_id.as_deref(),
                            Some(id.as_str()),
                            "M34/M58: chrome shifts focused tool offsets along with painted starts"
                        );
                        assert_eq!(
                            clipped.result.block_boundaries, full.result.block_boundaries,
                            "clipping must not change conversation-level block coordinates"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn migrated_sessions_preserve_decorations_search_and_selected_tool_copy() {
    use rustain::adapters::tui::state::TuiState;
    use rustain::domain::models::conversation::ForkSource;
    use rustain::domain::models::{CheckpointId, ToolCallInfo, ToolResultInfo};
    use rustain::domain::services::search::find_matches;

    let temp = tempfile::tempdir().unwrap();
    let mut message = common::make_msg("legacy-decorated", MessageRole::Assistant);
    message.content = "PARTIAL answer".into();
    message.origin = ChannelKind::Telegram;
    message.synthetic = true;
    message.stop_reason = Some(StopReason::Cancelled);
    message.tool_calls.push(ToolCallInfo {
        id: "provider-original-call".into(),
        name: "Bash".into(),
        input: json!({"command": "printf selected"}),
        result: Some(ToolResultInfo {
            content: "SELECTED_TOOL_OUTPUT".into(),
            is_error: false,
            diff: Default::default(),
        }),
        started_at_ms: Some(1_000),
        completed_at_ms: Some(2_000),
        status: None,
    });
    let mut conversation = common::make_conversation(vec![message], vec![]);
    conversation.id = "legacy-decorations".into();
    conversation.fork_source = Some(ForkSource {
        conversation_id: "parent".into(),
        message_index: 0,
        checkpoint_id: CheckpointId(0),
    });
    FileSystemStorage::new(temp.path().to_path_buf())
        .save_conversation(&conversation)
        .await
        .unwrap();
    for mut loaded in both_loaders(temp.path(), &conversation.id).await {
        for rebuild in [false, true] {
            if rebuild {
                loaded.rebuild_messages_mirror();
            }
            for collapsed in [false, true] {
                let mut view = ViewState::default();
                view.collapsed.insert(loaded.turns[0].id.clone(), collapsed);
                let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
                let mut theme = Theme::dark();
                theme.search_highlight_focused =
                    ratatui::style::Style::default().bg(ratatui::style::Color::Magenta);
                let matches = find_matches(&loaded, "PARTIAL");
                let mut cache = TabRenderState::default();
                let mut rendered = None;
                terminal
                    .draw(|frame| {
                        rendered = Some(chat_pane::render_with_search(
                            frame,
                            Rect::new(0, 0, 60, 40),
                            &loaded,
                            None,
                            &StreamingState::default(),
                            &view,
                            &MockClock::at_wall_ms(3_000),
                            0,
                            false,
                            &theme,
                            &mut cache,
                            &HashMap::new(),
                            &BTreeMap::new(),
                            Some("PARTIAL"),
                            matches.first(),
                            &matches,
                            &[0],
                            None,
                            None,
                            None,
                            None,
                            None,
                        ));
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let screen = (0..40)
                    .map(|y| {
                        (0..60)
                            .map(|x| buffer.cell((x, y)).unwrap().symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                for marker in [
                    "[telegram]",
                    "[interrupted]",
                    "🔀",
                    "⤷",
                    theme.bookmark_glyph.trim(),
                ] {
                    assert!(
                        screen.contains(marker),
                        "R2 migrated decoration {marker} missing: {screen}"
                    );
                }
                assert_eq!(
                    screen.contains('▸'),
                    collapsed,
                    "R2 a completed migrated turn must honor fold requests"
                );
                assert!(
                    screen.contains("PARTIAL answer"),
                    "R2 migrated prose remains visible"
                );
                for (y, row) in screen.lines().enumerate() {
                    if let Some(byte) = row.find("PARTIAL") {
                        let x = row[..byte].chars().count();
                        assert_eq!(
                            buffer.cell((x as u16, y as u16)).unwrap().bg
                                == ratatui::style::Color::Magenta,
                            row.contains("PARTIAL answer"),
                            "R2 only the matched prose, not reasoning, receives focused highlighting: {row}"
                        );
                    }
                }
                if !collapsed {
                    let mut state = TuiState::new(60, 40);
                    state.select_tool_explicitly(rendered.unwrap().focused_tool_id.unwrap());
                    assert_eq!(
                        state.resolve_copy_content(&loaded),
                        "SELECTED_TOOL_OUTPUT",
                        "R3 selecting a migrated rendered tool copies its result before and after rebuild"
                    );
                }
            }
        }
    }
}

#[test]
fn provenance_prefixes_preserve_fold_navigation_geometry() {
    use rustain::domain::models::view_state::{AnchorMode, ViewEvent};
    let mut messages = Vec::new();
    let mut turns = Vec::new();
    for (id, body) in [
        ("above", "above\n\none\n\ntwo\n\nthree\n\nfour\n\nfive"),
        ("focus", "FOCUSED\n\nsix\n\nseven\n\neight\n\nnine"),
    ] {
        let mut message = common::make_msg(id, MessageRole::Assistant);
        message.content = body.into();
        message.authorship = Authorship::AgentComposed;
        message.send_authorization = SendAuthorization::Automatic;
        message.origin = ChannelKind::Telegram;
        let turn = common::make_turn(id, vec![common::prose(body)], Some(StopReason::EndTurn));
        messages.push(message);
        turns.push(turn);
    }
    let conversation = common::make_conversation(messages, turns);
    let mut view = ViewState {
        mode: AnchorMode::Reading,
        focused_turn: Some(conversation.turns[1].id.clone()),
        ..ViewState::default()
    };
    let mut cache = TabRenderState::default();
    let full = paint(
        &conversation,
        &view,
        60,
        100,
        false,
        0,
        &mut cache,
        &HashMap::new(),
        &[0],
    );
    let before = chat_pane::build_layout_metrics(
        &conversation,
        &view,
        &mut cache,
        &Theme::dark(),
        60,
        8,
        &MockClock::at_wall_ms(3_000),
        &HashMap::new(),
        &[0],
    );
    assert_eq!(
        before.total_content_height, full.result.total_content_height,
        "R4 navigation must count every painted provenance and metadata prefix"
    );
    assert_eq!(
        before.focused_turn_top,
        Some(full.result.message_boundaries[1]),
        "R4 focused turn coordinates must include preceding provenance prefixes"
    );
    let screen_row = 2;
    let previous_max = full.result.total_content_height - 8;
    view.scroll_offset = previous_max - (full.result.message_boundaries[1] - screen_row);
    chat_pane::set_turn_collapsed(&mut view, &mut cache, &conversation.turns[0].id, true);
    let folded = paint(
        &conversation,
        &view,
        60,
        100,
        false,
        0,
        &mut cache,
        &HashMap::new(),
        &[0],
    );
    let after = chat_pane::build_layout_metrics(
        &conversation,
        &view,
        &mut cache,
        &Theme::dark(),
        60,
        8,
        &MockClock::at_wall_ms(3_000),
        &HashMap::new(),
        &[0],
    );
    assert_eq!(
        after.total_content_height, folded.result.total_content_height,
        "R4 collapsed navigation uses the same prefix geometry"
    );
    let offset = view.reconcile(
        Some(ViewEvent::FoldToggle {
            turn_id: conversation.turns[0].id.clone(),
            prev_focused_turn_top: Some(full.result.message_boundaries[1]),
            prev_max_offset: previous_max,
        }),
        &after,
    );
    let visible_top = folded
        .result
        .total_content_height
        .saturating_sub(8)
        .saturating_sub(offset);
    assert_eq!(
        folded.result.message_boundaries[1] - visible_top,
        screen_row,
        "R4 folding above a marked turn preserves its actual screen row"
    );
}
