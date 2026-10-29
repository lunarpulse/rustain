use super::*;
use rustain::adapters::tui::handlers::transcript_target::{
    handle_fork_confirm, handle_rewind_confirm, handle_rewind_fork_instead,
};
use rustain::domain::ports::StoragePort;

#[tokio::test]
async fn fork_and_rewind_target_durable_row_after_display_before_and_after_reload() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSystemStorage::new(dir.path().join("sessions"));
    let mut conversation = Conversation {
        id: "targets".into(),
        ..Default::default()
    };
    conversation.messages.push(ChatMessage {
        id: "before".into(),
        role: MessageRole::User,
        content: "before".into(),
        ..Default::default()
    });
    let mut state = TuiState::new(80, 24);
    let plan = Plan {
        id: "plan".into(),
        title: "temporary".into(),
        tasks: vec![],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
    };
    handle_plan_proposed(
        &mut conversation,
        &mut state,
        "targets",
        plan,
        PermissionMode::Normal,
        None,
    );
    conversation.messages.push(ChatMessage {
        id: "target".into(),
        role: MessageRole::Assistant,
        content: "target".into(),
        ..Default::default()
    });
    conversation.messages.push(ChatMessage {
        id: "tail".into(),
        role: MessageRole::Assistant,
        content: "tail".into(),
        ..Default::default()
    });
    store
        .save_conversation_with_exit(&conversation, true)
        .await
        .unwrap();
    let initial = store.load_conversation("targets").await.unwrap().unwrap();
    let original = serde_json::to_value(&initial).unwrap();
    assert!(handle_fork_confirm(&store, &conversation, 1).await.is_err());
    assert!(
        handle_rewind_confirm(&store, &conversation, 1)
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(store.load_conversation("targets").await.unwrap().unwrap()).unwrap(),
        original
    );
    state.pending_rewind_index = Some(2);
    let (live_index, result) = handle_rewind_fork_instead(&store, &conversation, &mut state)
        .await
        .unwrap();
    assert_eq!(live_index, 2);
    assert!(state.pending_rewind_index.is_none());
    let forked = store
        .load_conversation(&result.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        forked
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["before", "target"]
    );
    let fork_before = handle_fork_confirm(&store, &conversation, 2).await.unwrap();
    let fork_after = handle_fork_confirm(&store, &initial, 1).await.unwrap();
    for fork in [fork_before, fork_after] {
        let forked = store.load_conversation(&fork).await.unwrap().unwrap();
        assert_eq!(
            forked
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["before", "target"]
        );
        assert!(
            forked.plans.contains_key("plan"),
            "durable plan data has no transient host dependency"
        );
    }
    let result = handle_rewind_confirm(&store, &conversation, 2)
        .await
        .unwrap();
    assert_eq!(
        result
            .conversation
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["before", "target"]
    );
    assert!(result.conversation.plans.contains_key("plan"));
    assert_eq!(
        result
            .conversation
            .turns
            .iter()
            .map(|turn| turn.id.0.as_str())
            .collect::<Vec<_>>(),
        ["target"]
    );
    store
        .save_conversation_with_exit(&conversation, true)
        .await
        .unwrap();
    let reloaded = store.load_conversation("targets").await.unwrap().unwrap();
    let result = handle_rewind_confirm(&store, &reloaded, 1).await.unwrap();
    assert_eq!(
        result
            .conversation
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["before", "target"]
    );
    assert!(result.conversation.plans.contains_key("plan"));
    assert_eq!(
        result
            .conversation
            .turns
            .iter()
            .map(|turn| turn.id.0.as_str())
            .collect::<Vec<_>>(),
        ["target"]
    );
}
