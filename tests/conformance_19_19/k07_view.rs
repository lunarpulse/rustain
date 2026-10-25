use super::*;

async fn open_view(workspace: &Path, item_id: &str) -> TuiState {
    let (app, _events) = app_state(workspace);
    let mut state = TuiState::new(130, 30);
    state.input_buffer = format!("/team view {item_id}");
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("/team view must use the production command parser");
    };
    assert_eq!(name, "team");
    team_command(&mut state, "recipient-view", args.as_deref(), &app).await;
    state
}

async fn open_view_recording(workspace: &Path, item_id: &str) -> (TuiState, RequestTrace) {
    let (app, _events, requests) = app_state_recording(workspace);
    let mut state = TuiState::new(130, 30);
    state.input_buffer = format!("/team view {item_id}");
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("/team view must use the production command parser");
    };
    assert_eq!(name, "team");
    team_command(&mut state, "recipient-view", args.as_deref(), &app).await;
    (state, requests)
}

async fn received_id(journal: &Journal, body: &str) -> String {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(id) = room_events(journal)
            .await
            .iter()
            .rev()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived {
                    address, content, ..
                } if content == body => Some(address.item().as_str().to_owned()),
                _ => None,
            })
        {
            return id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual HTTP ingress must durably receive the exact body before exercising View"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn http_ingress(
    host: &Host,
    client: &reqwest::Client,
    key: &str,
    message_id: &str,
    body: &str,
    semantic_type: Option<&str>,
) {
    let mut metadata = serde_json::Map::new();
    if let Some(semantic_type) = semantic_type {
        metadata.insert(
            "x-rustain-message-type".to_owned(),
            serde_json::Value::String(semantic_type.to_owned()),
        );
    }
    let response = client
        .post(&host.endpoint)
        .header("x-api-key", key)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {
                "message": {
                    "kind": "message",
                    "messageId": message_id,
                    "role": "user",
                    "parts": [{"kind": "text", "text": body}],
                    "metadata": metadata
                }
            }
        }))
        .send()
        .await
        .expect("actual authenticated HTTP request");
    assert!(
        response.status().is_success(),
        "authenticated HTTP status: {}",
        response.status()
    );
    let answer: serde_json::Value = response.json().await.expect("HTTP JSON-RPC response");
    assert!(
        answer.get("result").is_some(),
        "accepted production HTTP ingress: {answer}"
    );
}
async fn create_linked_thread(
    rounds: usize,
    padding: usize,
) -> (Host, String, String, String, Vec<String>) {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let mut sender = new_sender(vec![host.peer("recipient")]).await;
    let mut state = TuiState::new(130, 30);

    let root_body = format!("root-question {}", "r".repeat(padding));
    sender
        .type_command(
            &mut state,
            &format!("/team send --type consultation recipient {root_body}"),
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let root_id = received_id(&host.journal, &root_body).await;
    let root_row = received(&room_events(&host.journal).await)
        .into_iter()
        .find(|(id, _, _)| id == &root_id)
        .expect("root HTTP ingress event");
    let mut current_id = root_id.clone();
    let mut current_task = root_row.1;
    let mut opened_id = String::new();
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;

    for index in 0..rounds {
        if index == 0 {
            waiting_node(&host, &current_task).await;
        }
        let answer = format!("answer-{index} {}", "a".repeat(padding));
        write_frame(
            &mut writer,
            &ClientFrame::ReplyToRecipientItem {
                item_id: current_id.clone(),
                text: answer,
                verdict: None,
            },
        )
        .await
        .expect("send actual recipient reply over socket");
        let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
        assert_eq!(action, RecipientAction::Reply);
        assert_eq!(
            target,
            RecipientActionTarget::Item {
                item_id: current_id
            }
        );
        let reply = match outcome {
            RecipientActionOutcome::Applied {
                reply: Some(receipt),
            } => receipt,
            other => panic!("socket reply must return its durable receipt: {other:?}"),
        };

        let followup_body = format!("followup-{index} {}", "f".repeat(padding));
        sender
            .type_command(
                &mut state,
                &format!("/team followup recipient {} {followup_body}", reply.turn_id),
            )
            .await;
        settle_send(&mut sender, &mut state).await;
        received_id(&host.journal, &followup_body).await;
        let rows = room_events(&host.journal).await;
        let followup_address = rows.iter().find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                address,
                content,
                in_reply_to: Some(parent),
                ..
            } if content == &followup_body => {
                Some((address.item().as_str().to_owned(), parent.clone()))
            }
            _ => None,
        });
        assert_eq!(
            followup_address.as_ref().map(|(_, parent)| parent.as_str()),
            Some(reply.turn_id.as_str()),
            "actual follow-up HTTP ingress links to the socket-produced reply turn"
        );
        let linked_id = followup_address.expect("linked received item").0;
        current_id = linked_id;
        current_task = received(&rows)
            .into_iter()
            .find(|(id, _, _)| id == &current_id)
            .expect("follow-up is a real HTTP received item")
            .1;
        let status = poll_state(&host.endpoint, &current_task, "completed").await;
        assert_eq!(status["result"]["status"]["state"], "completed");
        if index == 0 {
            opened_id.clone_from(&current_id);
        }
    }
    drop(writer);
    drop(socket);
    let events = room_events(&host.journal).await;
    let turn_ids = events
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. } => {
                Some(address.item().as_str().to_owned())
            }
            RoomEvent::RecipientItemReplied { address, .. } => {
                Some(address.turn().as_str().to_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let newest_id = turn_ids.last().cloned().expect("produced thread turns");
    (host, root_id, opened_id, newest_id, turn_ids)
}
#[tokio::test]
async fn k07_view_discloses_unrecognized_type_from_http_ingress() {
    let key = "k07-view-unknown-wire-type";
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.security.auth = Some(rustain::adapters::a2a::auth::A2aServerAuth::ApiKey(
        rustain::adapters::a2a::auth::ApiKeySet::unbound(vec![key.into()]),
    ));
    let host = Host::serve_with(options).await;
    let body = "body claims architecture_update but has a future semantic type";
    http_ingress(
        &host,
        &reqwest::Client::new(),
        key,
        "k07-unknown-type-task",
        body,
        Some("future_type"),
    )
    .await;
    let id = received_id(&host.journal, body).await;
    let state = open_view(&host.journal.path, &id).await;
    let surface = state
        .recipient_surface
        .expect("unknown semantic type does not hide received item");
    assert_eq!(
        surface.message_type,
        rustain::domain::models::SemanticMessageType::Unknown,
        "K07: body text cannot forge the persisted semantic type"
    );
    assert!(surface.content.contains(body));
}
#[tokio::test]
async fn k07_view_pins_opened_older_item_and_bounds_newest_fifty_turns() {
    let (host, root_id, opened_id, newest_id, turn_ids) = create_linked_thread(26, 16).await;
    assert!(
        turn_ids.len() > 50,
        "the real follow-up/auto-reply chain exceeds 50 turns"
    );

    let state = open_view(&host.journal.path, &opened_id).await;
    let surface = state.recipient_surface.expect("older linked item opens");
    assert_eq!(surface.root_id.as_deref(), Some(root_id.as_str()));
    assert_ne!(root_id, opened_id);
    assert!(surface.omitted_before > 0);
    assert!(
        surface.content.contains(&opened_id),
        "K07: explicitly opened older item is retained in the bounded page"
    );
    assert!(surface.content.contains(&newest_id));
    assert_eq!(
        surface.content.matches("followup-0 ").count(),
        1,
        "pinned opened body occurs once"
    );
    assert!(surface.content.contains("followup-25 "));
    assert!(
        surface.content.find("followup-24 ").unwrap()
            < surface.content.find("followup-25 ").unwrap(),
        "retained newest turns stay chronological"
    );
    let visible_turns = turn_ids
        .iter()
        .filter(|turn_id| {
            surface.content.lines().any(|line| {
                line.strip_prefix("  ")
                    .and_then(|row| row.strip_prefix(turn_id.as_str()))
                    .is_some_and(|rest| rest.starts_with(" · "))
            })
        })
        .count();
    assert_eq!(
        visible_turns, 50,
        "root/opened are deduplicated within the newest-turn bound"
    );
}

#[tokio::test]
async fn k07_view_bounds_newest_page_to_896_kibibytes() {
    let (host, root_id, opened_id, newest_id, turn_ids) = create_linked_thread(16, 32 * 1024).await;
    assert!(
        turn_ids.len() < 50,
        "this thread stays below the 50-turn limit"
    );

    let state = open_view(&host.journal.path, &opened_id).await;
    let surface = state.recipient_surface.expect("linked local thread opens");
    assert_eq!(surface.root_id.as_deref(), Some(root_id.as_str()));
    assert!(
        surface.omitted_before > 0,
        "older content is omitted by response-byte bounds"
    );
    assert!(
        surface.content.len() <= 896 * 1024,
        "K07: received page stays within the 896 KiB response bound"
    );
    assert!(surface.content.contains(&opened_id));
    assert!(surface.content.contains(&newest_id));
}

#[tokio::test]
async fn k07_view_reads_http_received_type_and_raw_body_after_restart() {
    let recipient = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let body =
        "needle-free provider text λ\nfn before() {  retain_spacing(); }\nerror:   raw bytes";
    let mut sender = new_sender(vec![recipient.peer("recipient")]).await;
    let mut send_state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut send_state,
            &format!("/team send --type consultation recipient {body}"),
        )
        .await;
    settle_send(&mut sender, &mut send_state).await;

    let id = received_id(&recipient.journal, body).await;
    let (item, view_requests) = open_view_recording(&recipient.journal.path, &id).await;
    let surface = item
        .recipient_surface
        .expect("local View opens recipient surface");
    assert_eq!(
        surface.message_type,
        rustain::domain::models::SemanticMessageType::Consultation
    );
    assert_eq!(surface.item_state, Some("received"));
    assert!(surface.content.contains(&id));
    assert!(surface.content.contains(&body.len().to_string()));
    assert_eq!(
        view_requests.lock().len(),
        0,
        "local View must not generate a model request through its AppState provider"
    );
    assert!(surface.content.contains("needle-free provider text λ"));
    assert!(
        surface
            .content
            .contains("fn before() {  retain_spacing(); }")
    );
    assert!(surface.content.contains("error:   raw bytes"));
    assert_eq!(
        surface.content.matches("fn before()").count(),
        1,
        "opened body is rendered once"
    );
    assert_eq!(surface.content.matches("raw bytes").count(), 1);
    assert!(
        surface
            .root_id
            .as_deref()
            .is_some_and(|root| root == id.as_str())
    );

    let (missing, refused_view_requests) =
        open_view_recording(&recipient.journal.path, "not-a-real-item").await;
    assert!(
        missing.recipient_surface.is_none(),
        "unknown ids do not open a fabricated item"
    );
    assert!(
        missing
            .active_feedback_id
            .as_ref()
            .is_some_and(|id| missing.feedback_blocks.contains_key(id))
    );
    assert_eq!(
        refused_view_requests.lock().len(),
        0,
        "a refused View must not generate a model request through its AppState provider"
    );
    let legacy_body = "legacy untyped message uses safe unknown type";
    sender
        .type_command(
            &mut send_state,
            &format!("/team send recipient {legacy_body}"),
        )
        .await;
    settle_send(&mut sender, &mut send_state).await;
    let legacy_id = received_id(&recipient.journal, legacy_body).await;
    let legacy = open_view(&recipient.journal.path, &legacy_id).await;
    let legacy = legacy
        .recipient_surface
        .expect("untyped received item remains readable");
    assert_eq!(
        legacy.message_type,
        rustain::domain::models::SemanticMessageType::Unknown
    );

    let restarted = recipient.restart().await;
    let replayed = open_view(&restarted.journal.path, &id).await;
    let surface = replayed
        .recipient_surface
        .expect("restarted production workspace replays received item");
    assert_eq!(
        surface.message_type,
        rustain::domain::models::SemanticMessageType::Consultation,
        "K07: persisted semantic type survives a recipient restart"
    );
    assert!(surface.content.contains("needle-free provider text λ"));
    assert!(
        surface
            .content
            .contains("fn before() {  retain_spacing(); }")
    );
    assert_eq!(surface.content.matches("fn before()").count(), 1);
}

#[tokio::test]
async fn k07_view_includes_body_at_the_64_kibibyte_read_ceiling() {
    let recipient = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let body = "x".repeat(64 * 1024);
    let mut sender = new_sender(vec![recipient.peer("recipient")]).await;
    let mut send_state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut send_state,
            &format!("/team send --type consultation recipient {body}"),
        )
        .await;
    settle_send(&mut sender, &mut send_state).await;

    let id = received_id(&recipient.journal, &body).await;
    let state = open_view(&recipient.journal.path, &id).await;
    let surface = state.recipient_surface.expect("received item opens");
    assert!(surface.content.contains("65536"));
    assert!(
        surface.content.contains(&body),
        "the exact ceiling-sized body remains available, not merely a prefix"
    );
}

#[tokio::test]
async fn k07_view_withholds_historical_received_body_over_64_kibibytes() {
    let key = "k07-view-over-limit-body-secret";
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.security.auth = Some(rustain::adapters::a2a::auth::A2aServerAuth::ApiKey(
        rustain::adapters::a2a::auth::ApiKeySet::unbound(vec![key.into()]),
    ));
    let recipient = Host::serve_with(options).await;
    let seed = "historical receipt address captured from actual HTTP ingress";
    http_ingress(
        &recipient,
        &reqwest::Client::new(),
        key,
        "k07-over-limit-body-task",
        seed,
        Some("consultation"),
    )
    .await;

    let id = received_id(&recipient.journal, seed).await;
    let mut historical = room_events(&recipient.journal).await.into_iter().find_map(|(_, event)| {
        matches!(&event, RoomEvent::RecipientItemReceived { address, .. } if address.item().as_str() == id).then_some(event)
    }).expect("captured production receipt");
    let prefix = "Q07-oversized-";
    let body = format!("{}{}λ", prefix, "x".repeat(64 * 1024 - 1 - prefix.len()));
    assert_eq!(body.len(), 64 * 1024 + 1);
    let RoomEvent::RecipientItemReceived { content, .. } = &mut historical else {
        unreachable!("selected the received event");
    };
    *content = body.clone();
    // Live admission caps instructions at 64 KiB; cold historical journals
    // remain readable without exposing an oversized retained body.
    let legacy = journal().await;
    legacy
        .room
        .record_event(historical)
        .await
        .expect("persist historical replay input");
    let state = open_view(&legacy.path, &id).await;
    let surface = state
        .recipient_surface
        .expect("historical received item opens");
    assert!(surface.content.contains("65537"));
    assert!(!surface.content.contains(&body));
    // Prefixes shorter than the unique marker could match unrelated rendered
    // text such as the random item id, so the sweep starts at the marker.
    for (offset, character) in body
        .char_indices()
        .filter(|(offset, _)| *offset + 1 >= prefix.len())
    {
        let prefix_end = offset + character.len_utf8();
        assert!(
            !surface.content.contains(&body[..prefix_end]),
            "oversized historical body prefix of {prefix_end} bytes must not be disclosed"
        );
    }
    assert!(
        !surface.content.contains('λ'),
        "a byte ceiling in the middle of a multibyte code point must not expose it"
    );
}

#[tokio::test]
async fn k07_view_withholds_a_turn_over_448_kibibytes() {
    let recipient = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let body = format!("p{}z", "\n".repeat(64 * 1024 - 2));
    assert_eq!(body.len(), 64 * 1024);
    let mut sender = new_sender(vec![recipient.peer("recipient")]).await;
    let mut send_state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut send_state,
            &format!("/team send --type consultation recipient {body}"),
        )
        .await;
    settle_send(&mut sender, &mut send_state).await;

    let id = received_id(&recipient.journal, &body).await;
    let state = open_view(&recipient.journal.path, &id).await;
    let surface = state
        .recipient_surface
        .expect("large received item remains inspectable");
    assert!(surface.content.contains("65536"));
    assert!(!surface.content.contains("      p\n      \n"));
}
#[tokio::test]
async fn k07_view_keeps_distinct_principals_and_unrelated_threads_separate() {
    let key_one = "k07-view-principal-one-secret";
    let key_two = "k07-view-principal-two-secret";
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.security.auth = Some(rustain::adapters::a2a::auth::A2aServerAuth::ApiKey(
        rustain::adapters::a2a::auth::ApiKeySet::unbound(vec![key_one.into(), key_two.into()]),
    ));
    let certificate_dir = tempfile::tempdir().expect("TLS fixture directory");
    let certificate =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let cert_path = certificate_dir.path().join("cert.pem");
    let key_path = certificate_dir.path().join("key.pem");
    std::fs::write(&cert_path, certificate.cert.pem()).expect("fixture certificate");
    std::fs::write(&key_path, certificate.signing_key.serialize_pem())
        .expect("fixture private key");
    options.security.tls = Some(
        rustain::adapters::a2a::tls::load_tls_material(&cert_path, &key_path)
            .expect("TLS material"),
    );
    options.bind = "0.0.0.0:0";
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(
            reqwest::Certificate::from_pem(certificate.cert.pem().as_bytes())
                .expect("fixture trust anchor"),
        )
        .build()
        .expect("anchored TLS client");
    let recipient = Host::serve_with(options).await;
    let first_body = "first-authenticated-origin-only-context";
    http_ingress(
        &recipient,
        &client,
        key_one,
        "k07-first-origin",
        first_body,
        Some("consultation"),
    )
    .await;
    let first_id = received_id(&recipient.journal, first_body).await;

    let unrelated_body = "same-origin-unrelated-root";
    http_ingress(
        &recipient,
        &client,
        key_one,
        "k07-unrelated-root",
        unrelated_body,
        Some("consultation"),
    )
    .await;
    let unrelated_id = received_id(&recipient.journal, unrelated_body).await;

    let other_principal_body = "second-authenticated-origin-distinct-context";
    http_ingress(
        &recipient,
        &client,
        key_two,
        "k07-other-origin",
        other_principal_body,
        Some("consultation"),
    )
    .await;
    let second_id = received_id(&recipient.journal, other_principal_body).await;
    let events = room_events(&recipient.journal).await;
    let principal_for = |id: &str| {
        events
            .iter()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived { address, .. }
                    if address.item().as_str() == id =>
                {
                    Some(address.principal().clone())
                }
                _ => None,
            })
            .expect("HTTP ingress minted item principal")
    };
    assert_ne!(principal_for(&first_id), principal_for(&second_id));

    let rustain::domain::models::ItemPrincipal::A2aPseudonym(second_peer) =
        principal_for(&second_id)
    else {
        panic!("API-key fixture has a pseudonymous A2A principal");
    };
    let cross_principal_child = "cross-principal-linked-child";
    recipient
        .journal
        .room
        .record_event(RoomEvent::RecipientItemReceived {
            stories: None,
            address: rustain::domain::models::ItemAddress::from_a2a_ingress(
                second_peer,
                rustain::domain::models::ItemId::from_replay(cross_principal_child),
            ),
            task: "cross-principal-task".to_owned(),
            alias: None,
            message_type: rustain::domain::models::SemanticMessageType::Consultation,
            content: "cross-principal-linked-body".to_owned(),
            in_reply_to: Some(first_id.clone()),
            capability_hash: None,
            reply_capability_matched: false,
            authorship: rustain::domain::models::Authorship::Unknown,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
            selected_for: None,
        })
        .await
        .expect("journal linked cross-principal control");

    let first_view = open_view(&recipient.journal.path, &first_id).await;
    let first_content = &first_view
        .recipient_surface
        .expect("first received item local view")
        .content;
    assert!(first_content.contains(first_body));
    assert!(
        !first_content.contains(unrelated_body),
        "unrelated root is excluded"
    );
    assert!(
        !first_content.contains(other_principal_body),
        "different authenticated principal is excluded"
    );
    assert!(
        !first_content.contains("cross-principal-linked-body"),
        "K07: exact principal scope excludes a cross-principal linked item even within the same principal class"
    );

    let unrelated_view = open_view(&recipient.journal.path, &unrelated_id).await;
    let unrelated_content = &unrelated_view
        .recipient_surface
        .expect("same-principal second thread local view")
        .content;
    assert!(unrelated_content.contains(unrelated_body));
    assert!(!unrelated_content.contains(first_body));
    assert!(!unrelated_content.contains(other_principal_body));

    let second_view = open_view(&recipient.journal.path, &second_id).await;
    let second_content = &second_view
        .recipient_surface
        .expect("second principal local view")
        .content;
    assert!(second_content.contains(other_principal_body));
    assert!(!second_content.contains(first_body));
}
#[tokio::test]
async fn k07_view_reports_content_unavailable_after_socket_removal() {
    let mut recipient = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let body = "remove-through-the-real-recipient-socket";
    let mut sender = new_sender(vec![recipient.peer("recipient")]).await;
    let mut send_state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut send_state,
            &format!("/team send --type consultation recipient {body}"),
        )
        .await;
    settle_send(&mut sender, &mut send_state).await;
    let id = received_id(&recipient.journal, body).await;

    let socket = SocketService::start(&mut recipient).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    write_frame(
        &mut writer,
        &ClientFrame::RemoveRecipientItem {
            item_id: id.clone(),
        },
    )
    .await
    .expect("remove received item through attached operator socket");
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if room_events(&recipient.journal).await.iter().any(|(_, event)| {
            matches!(event, RoomEvent::RecipientItemRemoved { address } if address.item().as_str() == id)
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "durable socket removal must complete"
        );
        tokio::task::yield_now().await;
    }

    let state = open_view(&recipient.journal.path, &id).await;
    let surface = state
        .recipient_surface
        .expect("removed item remains inspectable");
    assert_eq!(surface.item_state, Some("removed"));
    assert!(
        !surface.content.contains(body),
        "removed body must not be exposed by local View"
    );
}
