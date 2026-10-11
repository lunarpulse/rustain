use super::*;

#[tokio::test]
async fn k10_default_operator_request_excludes_unselected_received_display() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let mut sender = new_sender(vec![host.peer("recipient")]).await;
    let mut state = TuiState::new(130, 30);
    let needle = "peer-lineage-before-selection-19-19-needle";
    sender
        .type_command(
            &mut state,
            &format!("/team send --type consultation recipient {needle}"),
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let (_, history) = history_fence(&mut writer).await;
        if history
            .iter()
            .any(|message| message.content.contains(needle))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "real HTTP arrival must remain in the raw daemon history"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        host.requests.lock().is_empty(),
        "wait arrival is not a model turn"
    );
    write_frame(
        &mut writer,
        &ClientFrame::UserMessage {
            text: "ordinary local question with no adopted item".to_owned(),
            images: Vec::new(),
        },
    )
    .await
    .unwrap();
    let request = loop {
        if let Some(request) = host.requests.lock().first().cloned() {
            break request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual attached user turn did not reach the provider"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let actual = serde_json::to_string(&request).unwrap();
    assert!(actual.contains("ordinary local question with no adopted item"));
    assert!(
        !actual.contains(needle),
        "unselected peer-display lineage leaked through the real daemon request: {actual}"
    );
    let (_, history) = history_fence(&mut writer).await;
    assert!(
        history
            .iter()
            .any(|message| message.content.contains(needle)),
        "audience filtering must not erase the visible/raw transcript"
    );
}

#[derive(Clone, Copy)]
enum DisplayCase {
    Awaiting,
    Draft,
    Auto,
    Rejected,
}

fn request_contains_text(value: &serde_json::Value, text: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(text),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| request_contains_text(value, text)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| request_contains_text(value, text)),
        _ => false,
    }
}

async fn display_row_stays_out_of_operator_request(case: DisplayCase) {
    let mode = match case {
        DisplayCase::Awaiting => ResponseMode::NotifyAndWait,
        DisplayCase::Auto => ResponseMode::NotifyAndAuto,
        DisplayCase::Draft | DisplayCase::Rejected => ResponseMode::NotifyAndDraft,
    };
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, mode);
    options.draft_mentions_workspace = true;
    let mut host = Host::serve_with(options).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, snapshot) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let mut view = seed_conversation(&snapshot);
    let mut sender = new_sender(vec![host.peer("recipient")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation recipient source producer isolation",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let expected_stage = match case {
        DisplayCase::Awaiting => PeerRowStage::AwaitingOperator,
        DisplayCase::Auto => PeerRowStage::Settled,
        DisplayCase::Draft | DisplayCase::Rejected => PeerRowStage::Ready,
    };
    let deadline = tokio::time::Instant::now() + SETTLE;
    let (row_id, node_id) = loop {
        let (operations, _) = history_fence(&mut writer).await;
        apply_operations(&mut view, &operations);
        if let Some((id, control)) = view
            .peer_row_controls
            .iter()
            .find(|(_, control)| control.stage == expected_stage)
        {
            break (id.clone(), control.node_id.clone());
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual producer did not publish its expected stage"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if matches!(case, DisplayCase::Rejected) {
        write_frame(
            &mut writer,
            &ClientFrame::ResolvePeerDraft {
                node: node_id,
                action: PeerDraftAction::Reject,
            },
        )
        .await
        .unwrap();
        let (_, _, outcome) = recipient_action_result(&mut writer, Some(&mut view)).await;
        assert!(matches!(
            outcome,
            RecipientActionOutcome::Applied { reply: None }
        ));
        assert_eq!(view.peer_row_controls[&row_id].stage, PeerRowStage::Settled);
    }
    let peer_body = view
        .messages
        .iter()
        .find(|row| row.id == row_id)
        .unwrap()
        .content
        .clone();
    let before = host.requests.lock().len();
    let local_question = "ordinary operator question after a display-only response";
    attach_user_turn(&mut writer, local_question).await;
    let request = loop {
        if let Some(request) = host.requests.lock().get(before).cloned() {
            break serde_json::to_value(request).unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operator turn did not reach real provider"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(request_contains_text(&request, local_question));
    assert!(
        !request_contains_text(&request, &peer_body),
        "peer response display entered the full operator request: {request}"
    );
    let (_, history) = history_fence(&mut writer).await;
    assert!(
        history
            .iter()
            .any(|row| row.id == row_id && row.content == peer_body),
        "audience selection erased or rewrote the raw producer evidence"
    );
}

#[tokio::test]
async fn k10_awaiting_placeholder_remains_display_only() {
    display_row_stays_out_of_operator_request(DisplayCase::Awaiting).await;
}

#[tokio::test]
async fn k10_ready_draft_remains_display_only() {
    display_row_stays_out_of_operator_request(DisplayCase::Draft).await;
}

#[tokio::test]
async fn k10_auto_response_remains_display_only() {
    display_row_stays_out_of_operator_request(DisplayCase::Auto).await;
}

#[tokio::test]
async fn k10_rejected_draft_remains_display_only() {
    display_row_stays_out_of_operator_request(DisplayCase::Rejected).await;
}
