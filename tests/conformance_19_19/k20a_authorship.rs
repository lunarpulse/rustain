use super::*;
use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::{state::TabRenderState, theme::Theme, widgets::chat_pane};
use rustain::domain::models::{Authorship, ChatMessage, SendAuthorization, StreamingState};
use std::collections::{BTreeMap, HashMap};

const START: &str = "K20A_APPROVED_START";
const END: &str = "K20A_APPROVED_END";

fn paint(conversation: &Conversation) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(160, 140)).unwrap();
    let mut tab = TabRenderState::default();
    terminal
        .draw(|frame| {
            chat_pane::render_attached(
                frame,
                frame.area(),
                conversation,
                &StreamingState::default(),
                0,
                false,
                &Theme::dark(),
                &mut tab,
                &HashMap::new(),
                &BTreeMap::new(),
                None,
                &HashMap::new(),
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(160)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn facts(
    row: &ChatMessage,
    authorship: Authorship,
    authorization: SendAuthorization,
    content: &str,
) {
    assert_eq!(row.authorship, authorship, "{} authorship", row.id);
    assert_eq!(
        row.send_authorization, authorization,
        "{} send authorization",
        row.id
    );
    assert_eq!(row.content, content, "{} exact body", row.id);
}

fn painted_span(view: &Conversation, row: &ChatMessage, automatic: bool) {
    let lines = paint(view);
    let start_token = row.content.lines().next().unwrap();
    let end_token = row.content.lines().last().unwrap();
    let start = lines
        .iter()
        .position(|line| line.contains(start_token))
        .expect("row start sentinel painted");
    let end = lines
        .iter()
        .position(|line| line.contains(end_token))
        .expect("row end sentinel painted; inspected span not clipped");
    assert!(end > start, "nonempty complete sentinel span");
    // The role line is the nearest nonempty prefix before the body; no neighbour's
    // marker can satisfy (or fail) this row's assertion.
    let role = (0..start)
        .rev()
        .find(|&i| lines[i].trim_end().ends_with("  [agent-composed]"))
        .expect("row role line ends with two-space agent-composed tag");
    let previous_end = view
        .messages
        .iter()
        .filter(|other| other.id != row.id)
        .filter_map(|other| other.content.lines().last())
        .filter(|s| !s.is_empty())
        .filter_map(|s| lines.iter().rposition(|line| line.contains(s)))
        .filter(|&i| i < start)
        .max();
    assert!(
        previous_end.is_none_or(|previous| role > previous),
        "row role must follow previous neighbour boundary"
    );
    let next_body = view
        .messages
        .iter()
        .filter(|other| other.id != row.id)
        .filter_map(|other| other.content.lines().next())
        .filter(|s| !s.is_empty())
        .filter_map(|s| lines.iter().position(|line| line.contains(s)))
        .filter(|&i| i > start)
        .min();
    assert!(
        next_body.is_none_or(|next| end < next),
        "row end must precede neighbour body boundary"
    );
    let span = &lines[role..=end];
    assert!(
        span.iter().any(|line| line.contains(end_token)),
        "complete inspected span includes end sentinel"
    );
    assert_eq!(
        span.iter()
            .any(|line| line.contains("┆ [auto-sent]  [✗] Retract")),
        automatic,
        "automatic marker in complete row span: {span:#?}"
    );
    if !automatic {
        assert!(
            span.iter()
                .all(|line| !line.contains("[auto-sent]") && !line.contains("[✗] Retract")),
            "human-decided row must never have automatic chrome: {span:#?}"
        );
    }
}

struct DraftFixture {
    host: Host,
    service: SocketService,
    writer: tokio::net::UnixStream,
    reader: tokio::net::UnixStream,
    writer_view: Conversation,
    reader_view: Conversation,
    rows: Vec<ChatMessage>,
}

async fn approved() -> DraftFixture {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.runtime_factory = Some(Arc::new(|workspace, storage, _| {
        build_runtime(
            Arc::new(ScriptedProvider {
                text: format!("{START}\napproved draft body\n{END}"),
                requests: Arc::new(parking_lot::Mutex::new(Vec::new())),
                gate: None,
            }),
            storage,
            workspace,
        )
    }));
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, snapshot) = service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let mut writer_view = seed_conversation(&snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    let mut sender = new_sender(vec![host.peer("drafting")]).await;
    let mut state = TuiState::new(130, 30);
    let mut rows = Vec::new();
    for (index, action, content) in [
        (
            0,
            PeerDraftAction::Approve,
            format!("{START}\napproved draft body\n{END}"),
        ),
        (
            1,
            PeerDraftAction::Edit {
                content: "K20A_EDITED_START\noperator edited bytes intact\nK20A_EDITED_END".into(),
            },
            "K20A_EDITED_START\noperator edited bytes intact\nK20A_EDITED_END".into(),
        ),
    ] {
        sender
            .type_command(
                &mut state,
                &format!("/team send --type consultation drafting k20a approval question {index}"),
            )
            .await;
        settle_send(&mut sender, &mut state).await;
        let roots = received(&room_events(&host.journal).await);
        let task = roots[index].1.clone();
        let node = waiting_node(&host, &task).await;
        let (operations, _) = history_fence(&mut writer).await;
        apply_operations(&mut writer_view, &operations);
        write_frame(
            &mut writer,
            &ClientFrame::ResolvePeerDraft {
                node: node.clone(),
                action,
            },
        )
        .await
        .unwrap();
        let (_, _, outcome) = recipient_action_result(&mut writer, Some(&mut writer_view)).await;
        assert!(
            matches!(outcome, RecipientActionOutcome::Applied { reply: Some(_) }),
            "correlated draft action result required: {outcome:?}"
        );
        let (operations, history) = history_fence(&mut writer).await;
        apply_operations(&mut writer_view, &operations);
        let row = history
            .iter()
            .find(|row| row.content == content)
            .expect("resolved row persisted after successful history fence")
            .clone();
        let events = room_events(&host.journal).await;
        assert!(events.iter().any(|(_, event)| matches!(event, RoomEvent::RecipientItemReplied { via_node: Some(n), authorship: Authorship::AgentComposed, send_authorization: SendAuthorization::HumanDecided, content: body, .. } if n.as_str() == node && body == &content)), "RecipientItemReplied AgentComposed/HumanDecided missing after action history fence");
        assert!(events.iter().any(|(_, event)| matches!(event, RoomEvent::PeerDraftResolved { node: n, agent_composed: true, sent: true, .. } if n.as_str() == node)), "PeerDraftResolved sent agent draft missing after action history fence");
        facts(
            &row,
            Authorship::AgentComposed,
            SendAuthorization::HumanDecided,
            &content,
        );
        let terminal = poll_state(&host.endpoint, &task, "completed").await;
        assert_eq!(terminal["result"]["status"]["state"], "completed");
        assert!(room_events(&host.journal).await.iter().any(|(_, event)| matches!(event, RoomEvent::PeerDisclosure { node: n, authorship: Authorship::AgentComposed, send_authorization: SendAuthorization::HumanDecided, .. } if n.as_str() == node)), "PeerDisclosure must carry staged AgentComposed/HumanDecided after task served");
        let (operations, reader_history) = history_fence(&mut reader).await;
        apply_operations(&mut reader_view, &operations);
        for candidate in [
            writer_view.messages.iter().find(|r| r.id == row.id),
            reader_view.messages.iter().find(|r| r.id == row.id),
            reader_history.iter().find(|r| r.id == row.id),
        ] {
            facts(
                candidate.expect("both continuous clients and fenced history contain resolved row"),
                Authorship::AgentComposed,
                SendAuthorization::HumanDecided,
                &content,
            );
        }
        rows.push(row);
    }
    DraftFixture {
        host,
        service,
        writer,
        reader,
        writer_view,
        reader_view,
        rows,
    }
}

#[tokio::test]
async fn k01_approved_and_edited_drafts_are_agent_composed_never_auto_sent() {
    let fixture = approved().await;
    let (_fresh, _, snapshot) = fixture
        .service
        .attach(ConnectionTier::TrustedLocal, true)
        .await;
    let reload = fixture
        .host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .unwrap()
        .unwrap();
    for row in &fixture.rows {
        for view in [
            &fixture.writer_view,
            &fixture.reader_view,
            &seed_conversation(&snapshot),
            &reload,
        ] {
            facts(
                view.messages
                    .iter()
                    .find(|r| r.id == row.id)
                    .expect("live/reconnect/reload resolved row"),
                Authorship::AgentComposed,
                SendAuthorization::HumanDecided,
                &row.content,
            );
        }
        for view in [
            &fixture.writer_view,
            &fixture.reader_view,
            &seed_conversation(&snapshot),
        ] {
            painted_span(view, row, false);
        }
    }
    fixture.host.cancel.cancel();
    fixture.host.http.await.unwrap().unwrap();
}

#[tokio::test]
async fn k01_auto_response_is_agent_composed_and_automatic() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, snapshot) = service.attach(ConnectionTier::TrustedLocal, false).await;
    let mut sender = new_sender(vec![host.peer("auto")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation auto k20a automatic control",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    assert_eq!(
        poll_state(&host.endpoint, &roots[0].1, "completed").await["result"]["status"]["state"],
        "completed"
    );
    let (operations, history) = history_fence(&mut writer).await;
    let mut view = seed_conversation(&snapshot);
    apply_operations(&mut view, &operations);
    let row = history
        .iter()
        .find(|r| r.send_authorization == SendAuthorization::Automatic)
        .expect("A2A auto control records Automatic");
    facts(
        row,
        Authorship::AgentComposed,
        SendAuthorization::Automatic,
        &row.content,
    );
    let lines = paint(&view);
    assert!(
        lines.iter().any(|l| l.ends_with("  [agent-composed]")),
        "auto role tag"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("┆ [auto-sent]  [✗] Retract")),
        "auto marker positive control"
    );
    let events = room_events(&host.journal).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, e)| matches!(e, RoomEvent::RecipientItemReplied { .. }))
            .count(),
        0,
        "auto template never manufactures reply turns"
    );
    assert!(
        events.iter().any(|(_, e)| matches!(
            e,
            RoomEvent::PeerDisclosure {
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::Automatic,
                ..
            }
        )),
        "served auto disclosure facts"
    );
    host.cancel.cancel();
    host.http.await.unwrap().unwrap();
}

#[tokio::test]
async fn k02_auto_response_retract_refuses_an_approved_draft() {
    let mut fixture = approved().await;
    let id = fixture.rows[0].id.clone();
    let before = room_events(&fixture.host.journal)
        .await
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::AutoResponseRetracted { .. }))
        .count();
    write_frame(
        &mut fixture.writer,
        &ClientFrame::RetractAutoResponse {
            message_id: id.clone(),
            target_seq: None,
        },
    )
    .await
    .unwrap();
    write_frame(
        &mut fixture.writer,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 200,
        },
    )
    .await
    .unwrap();
    let mut refusal = None;
    loop {
        match socket_frame(&mut fixture.writer).await {
            DaemonFrame::Error(error) => refusal = Some(error),
            DaemonFrame::PeerRowOperation(operation) => {
                apply_peer_row_operation(&mut fixture.writer_view, Arc::unwrap_or_clone(operation));
            }
            DaemonFrame::History { .. } => break,
            _ => {}
        }
    }
    assert!(
        matches!(&refusal, Some(ProtocolError::Internal(message)) if message == "retraction refused: NotAutomatic"),
        "exact NotAutomatic refusal required: {refusal:?}"
    );
    for (stream, view) in [
        (&mut fixture.writer, &mut fixture.writer_view),
        (&mut fixture.reader, &mut fixture.reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        apply_operations(view, &operations);
        assert!(
            history
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .retracted_at_ms
                .is_none(),
            "approved persisted row unchanged"
        );
        assert!(
            view.messages
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .retracted_at_ms
                .is_none(),
            "both clients unchanged"
        );
    }
    let after = room_events(&fixture.host.journal)
        .await
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::AutoResponseRetracted { .. }))
        .count();
    assert_eq!(before, after, "refusal appends no retract event");
    fixture.host.cancel.cancel();
    fixture.host.http.await.unwrap().unwrap();
}

#[tokio::test]
async fn k09_orphan_settlement_and_tool_evidence_record_their_facts() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let service = SocketService::start(&mut host).await;
    let mut sender = new_sender(vec![host.peer("orphan")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation orphan k20a stale pending",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let node = waiting_node(&host, &roots[0].1).await;
    drop(service);
    let mut host = host.restart().await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, snapshot) = service.attach(ConnectionTier::TrustedLocal, false).await;
    let control = snapshot
        .peer_row_controls
        .values()
        .find(|c| c.node_id == node)
        .expect("stale pending row survives restart");
    assert!(!control.node_resolvable, "orphan route required");
    let item_id = control
        .item_address
        .as_ref()
        .unwrap()
        .item()
        .as_str()
        .to_owned();
    let content = "K20A_ORPHAN_START\nhuman direct answer retained\nK20A_ORPHAN_END";
    write_frame(
        &mut writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id,
            text: content.into(),
            verdict: None,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(
            recipient_action_result(&mut writer, None).await.2,
            RecipientActionOutcome::Applied { reply: Some(_) }
        ),
        "orphan direct reply applied"
    );
    let (_, history) = history_fence(&mut writer).await;
    let row = history
        .iter()
        .find(|r| r.content == content)
        .expect("ReplaceContent preserves orphan response");
    facts(
        row,
        Authorship::HumanWritten,
        SendAuthorization::HumanDecided,
        content,
    );
    let reload = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .unwrap()
        .unwrap();
    facts(
        reload.messages.iter().find(|r| r.id == row.id).unwrap(),
        Authorship::HumanWritten,
        SendAuthorization::HumanDecided,
        content,
    );
    let rendered = paint(&Conversation {
        messages: vec![row.clone()],
        ..Default::default()
    })
    .join("\n");
    assert!(
        !rendered.contains("[agent-composed]")
            && !rendered.contains("[auto-sent]")
            && !rendered.contains("[✗] Retract"),
        "orphan human answer is untagged"
    );
    host.cancel.cancel();
    host.http.await.unwrap().unwrap();

    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.runtime_factory = Some(Arc::new(|workspace, storage, _| {
        std::fs::write(
            workspace.join("k20a-evidence.txt"),
            "K20A real Read evidence",
        )
        .unwrap();
        k10_tools::actual_tool_runtime(
            workspace,
            storage,
            k10_tools::evidence_provider(workspace.join("k20a-evidence.txt").display().to_string()),
        )
    }));
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, _) = service.attach(ConnectionTier::TrustedLocal, false).await;
    let mut sender = new_sender(vec![host.peer("tools")]).await;
    sender
        .type_command(
            &mut state,
            "/team send --type consultation tools k20a tool evidence",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    waiting_node(&host, &roots[0].1).await;
    let (_, history) = history_fence(&mut writer).await;
    let row = history
        .iter()
        .find(|row| row.tool_calls.iter().any(|tool| tool.id == "managed-read"))
        .expect("production finish_peer_buffer persisted peer tool evidence");
    assert!(
        !row.id.starts_with("peer-response-"),
        "tool evidence has generated identity"
    );
    let reload = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .unwrap()
        .unwrap();
    let row = reload
        .messages
        .iter()
        .find(|r| r.id == row.id)
        .expect("tool evidence reload");
    facts(
        row,
        Authorship::AgentComposed,
        SendAuthorization::Unknown,
        &row.content,
    );
    let painted = paint(&Conversation {
        messages: vec![row.clone()],
        ..Default::default()
    })
    .join("\n");
    assert!(
        painted.contains("  [agent-composed]"),
        "tool evidence role tag"
    );
    assert!(
        !painted.contains("[auto-sent]") && !painted.contains("[✗] Retract"),
        "tool evidence has no automatic chrome"
    );
    host.cancel.cancel();
    host.http.await.unwrap().unwrap();
}

#[path = "k20a_privacy.rs"]
mod privacy;
