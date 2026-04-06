//! Tests for port traits and NoOp adapter implementations.
//! Verifies that all NoOp structs implement their port traits
//! and return expected default values.

use std::path::Path;

use rustain::adapters::noop::{
    NoOpChannel, NoOpContext, NoOpMemory, NoOpPersona, NoOpProvider, NoOpScheduler, NoOpSecurity,
    NoOpSession, NoOpStorage, NoOpToolSet,
};
use rustain::domain::errors::{ProviderError, ToolError};
use rustain::domain::models::{
    ApprovalDecision, CompletionOptions, Message, MessageRole, PathAccessType, PermissionMode,
};
use rustain::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, ProviderPort, SchedulerPort, SecurityPort,
    SessionPort, StoragePort, ToolSetPort,
};

// Covers: NFR1 (hexagonal architecture conformance)
#[tokio::test]
async fn test_noop_provider_returns_error() {
    let provider = NoOpProvider;
    let messages = vec![Message {
        role: MessageRole::User,
        content: "hello".to_string(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
    }];
    let options = CompletionOptions {
        model: "test".to_string(),
        max_tokens: 100,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
    };
    let result = provider.stream_completion(messages, options).await;
    assert!(result.is_err());
    match result {
        Err(ProviderError::Other(msg)) => assert!(msg.contains("NoOp provider")),
        Err(e) => panic!("Expected ProviderError::Other, got: {:?}", e),
        Ok(_) => panic!("Expected error from NoOp provider"),
    }
}

// Covers: NFR1 (hexagonal architecture conformance)
#[tokio::test]
async fn test_noop_provider_abort_ok() {
    let provider = NoOpProvider;
    assert!(provider.abort().await.is_ok());
}

// Covers: NFR1 (hexagonal architecture conformance)
#[test]
fn test_noop_provider_id() {
    let provider = NoOpProvider;
    assert_eq!(provider.provider_id(), "noop");
}

// Covers: NFR1 (hexagonal architecture conformance)
#[tokio::test]
async fn test_noop_storage_operations() {
    let storage = NoOpStorage;
    // save silently discards
    let conv = rustain::domain::models::Conversation {
        id: "test".into(),
        title: "Test".into(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };
    assert!(storage.save_conversation(&conv).await.is_ok());
    // load returns None
    assert!(storage.load_conversation("test").await.unwrap().is_none());
    // list returns empty
    assert!(storage.list_conversations().await.unwrap().is_empty());
}

// Covers: NFR1 (hexagonal architecture conformance)
#[tokio::test]
async fn test_noop_security_allows_all() {
    let security = NoOpSecurity;
    assert!(security.check_blocklist("rm -rf /").is_ok());
    assert_eq!(
        security
            .check_workspace_access(
                Path::new("/tmp"),
                rustain::domain::models::FileOperation::Read
            )
            .unwrap(),
        PathAccessType::Workspace
    );
    assert_eq!(
        security
            .request_permission("bash", &serde_json::json!({}))
            .await
            .unwrap(),
        ApprovalDecision::Allow
    );
    assert_eq!(security.current_mode(), PermissionMode::Normal);
    security.set_mode(PermissionMode::Yolo); // no-op, doesn't panic
}

// Covers: NFR1 (hexagonal architecture conformance)
#[tokio::test]
async fn test_noop_toolset_no_tools() {
    let toolset = NoOpToolSet;
    assert!(toolset.available_tools().is_empty());
    let result = toolset.execute("bash", serde_json::json!({})).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
}

// Covers: NFR1 (hexagonal architecture conformance)
#[test]
fn test_noop_persona_empty_prompt() {
    let persona = NoOpPersona;
    assert!(persona.system_prompt(Path::new("/workspace")).is_empty());
}

// Covers: NFR1 (hexagonal architecture conformance)
#[test]
fn test_noop_empty_ports_instantiate() {
    // These ports have no methods — just verify they can be instantiated
    // and satisfy trait bounds (Send + Sync).
    let _memory: Box<dyn MemoryPort> = Box::new(NoOpMemory);
    let _session: Box<dyn SessionPort> = Box::new(NoOpSession);
    let _channel: Box<dyn ChannelPort> = Box::new(NoOpChannel);
    let _scheduler: Box<dyn SchedulerPort> = Box::new(NoOpScheduler);
    let _context: Box<dyn ContextPort> = Box::new(NoOpContext);
}

// Covers: NFR1 (hexagonal architecture conformance)
#[test]
fn test_port_traits_are_object_safe() {
    // Verify all port traits can be used as trait objects (required for ArcSwap<Box<dyn Port>>)
    fn assert_object_safe<T: ?Sized>() {}
    assert_object_safe::<dyn ProviderPort>();
    assert_object_safe::<dyn StoragePort>();
    assert_object_safe::<dyn SecurityPort>();
    assert_object_safe::<dyn ToolSetPort>();
    assert_object_safe::<dyn PersonaPort>();
    assert_object_safe::<dyn MemoryPort>();
    assert_object_safe::<dyn SessionPort>();
    assert_object_safe::<dyn ChannelPort>();
    assert_object_safe::<dyn SchedulerPort>();
    assert_object_safe::<dyn ContextPort>();
}
