use super::*;

async fn item_received(host: &Host, body: &str) -> (String, String) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(item) = room_events(&host.journal)
            .await
            .into_iter()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived {
                    address,
                    task,
                    content,
                    ..
                } if content == body => Some((address.item().as_str().to_owned(), task)),
                _ => None,
            })
        {
            return item;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual HTTP ingress did not record {body:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn own_send(sender: &mut Sender, state: &mut TuiState, body: &str) {
    sender
        .type_command(state, &format!("/team send --type consultation b {body}"))
        .await;
    settle_send(sender, state).await;
}

async fn thread(sender: &mut Sender, state: &mut TuiState, peer: &str, item: &str) -> String {
    sender
        .type_command(state, &format!("/team thread {peer} {item}"))
        .await;
    let event = tokio::time::timeout(SETTLE, sender.domain_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let AppEvent::TeamThreadReady {
        conversation_id,
        message,
    } = event
    else {
        panic!("thread front door must publish its settled result: {event:?}");
    };
    let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
    rustain::infrastructure::runtime::transparency_bridge::team_thread_ready(
        "conv",
        state,
        &mut tabs,
        &conversation_id,
        message,
    );
    state.feedback_blocks[rustain::adapters::tui::handlers::team_command::TEAM_THREAD_BLOCK_ID]
        .message
        .clone()
}

fn assert_label(block: &str, item: &str, label: &str, body: &str) {
    let actual = block.lines().find_map(|line| {
        line.strip_prefix("  ")
            .and_then(|row| row.strip_suffix(item))
            .map(str::trim)
    });
    assert_eq!(
        actual,
        Some(label),
        "actual served item must retain ownership label {label}: {block}"
    );
    assert!(
        block.contains(body),
        "actual body must remain visible: {block}"
    );
}

#[tokio::test]
async fn k08_send_after_seed_and_cold_replay_both_retain_hash_checked_you() {
    let host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("b")]).await;
    let mut state = TuiState::new(130, 30);
    own_send(&mut sender, &mut state, "before ledger seed").await;
    let (first, _) = item_received(&host, "before ledger seed").await;
    assert_label(
        &thread(&mut sender, &mut state, "b", &first).await,
        &first,
        "you",
        "before ledger seed",
    );
    own_send(&mut sender, &mut state, "after ledger seed").await;
    let (second, _) = item_received(&host, "after ledger seed").await;
    assert_label(
        &thread(&mut sender, &mut state, "b", &second).await,
        &second,
        "you",
        "after ledger seed",
    );
    let mut sender = sender_over(vec![host.peer("b")], sender.journal).await;
    assert_label(
        &thread(&mut sender, &mut state, "b", &second).await,
        &second,
        "you",
        "after ledger seed",
    );
    assert!(host.requests.lock().is_empty());
}

#[tokio::test]
async fn k08_historical_same_alias_without_endpoint_still_requires_task_and_hash() {
    let host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("b")]).await;
    let mut state = TuiState::new(130, 30);
    own_send(&mut sender, &mut state, "legacy correlation bytes").await;
    let (item, _) = item_received(&host, "legacy correlation bytes").await;
    let (_, original) = room_events(&sender.journal)
        .await
        .into_iter()
        .find(|(_, event)| {
            matches!(
                event,
                RoomEvent::RemoteEnvelopeDispatched {
                    task: Some(_),
                    content_hash: Some(_),
                    ..
                }
            )
        })
        .unwrap();
    // Historical sender input only: the item and body were really received over
    // HTTP. No expected item/reply/card row is inserted in the recipient journal.
    for case in ["legacy", "changed hash", "missing hash", "missing task"] {
        let mut row = original.clone();
        let RoomEvent::RemoteEnvelopeDispatched {
            endpoint,
            content_hash,
            task,
            ..
        } = &mut row
        else {
            unreachable!();
        };
        *endpoint = None;
        match case {
            "changed hash" => {
                let changed = rustain::domain::models::ContentHash::from_bytes([0; 32]);
                assert_ne!(*content_hash, Some(changed));
                *content_hash = Some(changed);
            }
            "missing hash" => *content_hash = None,
            "missing task" => *task = None,
            _ => {}
        }
        let history = journal().await;
        history.room.record_event(row).await.unwrap();
        let mut historical_sender = sender_over(vec![host.peer("b")], history).await;
        let block = thread(&mut historical_sender, &mut state, "b", &item).await;
        assert_label(
            &block,
            &item,
            if case == "legacy" {
                "you"
            } else {
                "another sender"
            },
            "legacy correlation bytes",
        );
    }
    assert!(host.requests.lock().is_empty());
}

#[tokio::test]
async fn k08_renamed_unrelated_origin_cannot_forge_own_task_and_content() {
    let original = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let unrelated = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![original.peer("b"), unrelated.peer("b2")]).await;
    let mut state = TuiState::new(130, 30);
    own_send(
        &mut sender,
        &mut state,
        "same task and body is not authorship",
    )
    .await;
    let (own_item, task) = item_received(&original, "same task and body is not authorship").await;
    assert_label(
        &thread(&mut sender, &mut state, "b", &own_item).await,
        &own_item,
        "you",
        "same task and body is not authorship",
    );
    // An unrelated real host serves exactly the owner's task and content hash.
    // Neither its renamed alias nor its different origin correlates to the send.
    let response = rpc(&unrelated.endpoint, serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "message/send",
        "params": { "message": { "kind": "message", "messageId": task,
            "role": "user", "parts": [{"kind": "text", "text": "same task and body is not authorship"}],
            "metadata": {"x-rustain-message-type": "consultation"} } }
    })).await;
    assert!(
        response.get("error").is_none(),
        "real foreign host must accept ingress: {response}"
    );
    let (foreign_item, foreign_task) =
        item_received(&unrelated, "same task and body is not authorship").await;
    assert_eq!(foreign_task, task);
    assert_label(
        &thread(&mut sender, &mut state, "b2", &foreign_item).await,
        &foreign_item,
        "another sender",
        "same task and body is not authorship",
    );
    let mut sender = sender_over(vec![unrelated.peer("b2")], sender.journal).await;
    assert_label(
        &thread(&mut sender, &mut state, "b2", &foreign_item).await,
        &foreign_item,
        "another sender",
        "same task and body is not authorship",
    );
    assert!(original.requests.lock().is_empty());
    assert!(unrelated.requests.lock().is_empty());
}

#[tokio::test]
async fn k08_renamed_alias_same_origin_keeps_own_label() {
    let host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("b")]).await;
    let mut state = TuiState::new(130, 30);
    own_send(&mut sender, &mut state, "rename retains this sent body").await;
    let (item, _) = item_received(&host, "rename retains this sent body").await;
    let mut renamed = sender_over(vec![host.peer("b2")], sender.journal).await;
    assert_label(
        &thread(&mut renamed, &mut state, "b2", &item).await,
        &item,
        "you",
        "rename retains this sent body",
    );
    assert!(host.requests.lock().is_empty());
}

#[tokio::test]
async fn k08_same_alias_move_and_pin_add_keep_own_label() {
    let original = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let moved = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![original.peer("b")]).await;
    let mut state = TuiState::new(130, 30);
    let body = "same alias moved host own-dispatch control";
    own_send(&mut sender, &mut state, body).await;
    let (_, task) = item_received(&original, body).await;

    let response = rpc(
        &moved.endpoint,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": { "message": { "kind": "message", "messageId": task,
                "role": "user", "parts": [{"kind": "text", "text": body}],
                "metadata": {"x-rustain-message-type": "consultation"} } }
        }),
    )
    .await;
    assert!(
        response.get("error").is_none(),
        "moved real host accepts the matching item: {response}"
    );
    let (moved_item, moved_task) = item_received(&moved, body).await;
    assert_eq!(moved_task, task);

    let mut moved_sender = sender_over(vec![moved.peer("b")], sender.journal).await;
    assert_label(
        &thread(&mut moved_sender, &mut state, "b", &moved_item).await,
        &moved_item,
        "you",
        body,
    );

    let mut pin_sender = {
        use base64::Engine;
        let pin = rustain::domain::models::PinnedKey::new(
            rustain::domain::models::PinnedKeyAlgorithm::EdDsa,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                IdentityKeyStore::new(moved.keys.path())
                    .load_or_generate()
                    .expect("moved host identity")
                    .identity()
                    .public_key
                    .as_slice(),
            ),
            None,
        );
        let Sender { journal, .. } = moved_sender;
        sender_over(vec![moved.peer("b").with_pinned_key(Some(pin))], journal).await
    };
    assert_label(
        &thread(&mut pin_sender, &mut state, "b", &moved_item).await,
        &moved_item,
        "you",
        body,
    );
    assert!(original.requests.lock().is_empty());
    assert!(moved.requests.lock().is_empty());
    original.cancel.cancel();
    moved.cancel.cancel();
    original
        .http
        .await
        .expect("original HTTP task")
        .expect("original HTTP shutdown");
    moved
        .http
        .await
        .expect("moved HTTP task")
        .expect("moved HTTP shutdown");
}
