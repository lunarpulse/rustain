use std::io::Cursor;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpSecurity, NoOpStorage, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::models::{
    CompletionOptions, FileContextProvenance, Message, MessageRole, PermissionMode, StopReason,
    StreamChunk,
};
use rustain::domain::ports::SecurityPort;
use rustain::domain::ports::StreamingProvider;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::message_builder::ResolvedFileContext;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;

fn default_config() -> rustain::domain::models::AppConfig {
    rustain::domain::models::AppConfig::default()
}

fn build_test_core(provider: Arc<dyn StreamingProvider>) -> CliCore {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let security: Arc<dyn rustain::domain::ports::SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn rustain::domain::ports::ToolSetPort> = Arc::new(NoOpToolSet);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(NoOpStorage);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let ledger: Arc<dyn rustain::domain::ports::UsageLedgerPort> = Arc::new(NoOpUsageLedger);

    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
    }
}

struct SimpleTextProvider {
    text: String,
}

#[async_trait]
impl StreamingProvider for SimpleTextProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let text = self.text.clone();
        let chunks = vec![
            StreamChunk::Text {
                content: text,
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-simple".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}
struct ModelCaptureProvider {
    text: String,
    seen_model: Mutex<Option<String>>,
}

impl ModelCaptureProvider {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            seen_model: Mutex::new(None),
        }
    }
}

#[async_trait]
impl StreamingProvider for ModelCaptureProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        *self.seen_model.lock().unwrap() = Some(options.model.clone());
        let text = self.text.clone();
        let chunks = vec![
            StreamChunk::Text {
                content: text,
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-model-capture".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

struct MessageCaptureProvider {
    text: String,
    seen_messages: Mutex<Option<Vec<Message>>>,
}

impl MessageCaptureProvider {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            seen_messages: Mutex::new(None),
        }
    }
}

#[async_trait]
impl StreamingProvider for MessageCaptureProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        *self.seen_messages.lock().unwrap() = Some(messages);
        let text = self.text.clone();
        let chunks = vec![
            StreamChunk::Text {
                content: text,
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-message-capture".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

struct ErrorProvider;

#[async_trait]
impl StreamingProvider for ErrorProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        Err(rustain::domain::errors::ProviderError::ConnectionFailed(
            "simulated provider failure".into(),
        ))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-error".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

fn make_opts(query: &str) -> rustain::adapters::cli::ask::AskOpts {
    rustain::adapters::cli::ask::AskOpts {
        query: query.to_string(),
        files: vec![],
        yolo: false,
        final_message_only: false,
        model_override: None,
        session_id: None,
    }
}

/// P0-13.1a-O1 — run_ask_core constructs + runs with the mock and asserts final text,
/// without importing any daemon/supervision symbol.
#[tokio::test]
async fn p0_o1_basic_ask_core_no_daemon_symbols() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider {
        text: "Hello from the assistant".into(),
    });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("test query");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(
        output.contains("Hello from the assistant"),
        "got: {}",
        output
    );
}

/// P0-13.1a-O2c — happy path: tool-less turn → exit 0, final text on stdout.
#[tokio::test]
async fn p0_o2c_happy_path_exit_zero() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider {
        text: "Response text".into(),
    });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("hello");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(output.contains("Response text"));
}

/// P0-13.1a-O2d — turn-failure path: mock provider errors → non-zero exit, error on stderr.
#[tokio::test]
async fn p0_o2d_provider_error_nonzero_exit() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(ErrorProvider);
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("this will fail");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::FAILURE);
    let stderr_str = String::from_utf8(stderr.into_inner()).unwrap();
    assert!(stderr_str.contains("Error"), "stderr: {}", stderr_str);
}

/// P0-13.1a-O4a — default mode: stdout contains ONLY assistant text, no narration substrings.
#[tokio::test]
async fn p0_o4a_stdout_assistant_text_only() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider {
        text: "Pure assistant output".into(),
    });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("test");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert_eq!(output.trim(), "Pure assistant output");

    let stderr_str = String::from_utf8(stderr.into_inner()).unwrap();
    assert!(
        stderr_str.contains("Session saved"),
        "resume hint should be on stderr"
    );
}

/// P0-13.1a-O4b — --final-message-only: resume hint and deny-count suppressed on stderr.
#[tokio::test]
async fn p0_o4b_final_message_only_quiet_stderr() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider {
        text: "Final block text".into(),
    });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let mut opts = make_opts("test");
    opts.final_message_only = true;

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(output.contains("Final block text"));

    let stderr_str = String::from_utf8(stderr.into_inner()).unwrap();
    assert!(
        !stderr_str.contains("Session saved"),
        "resume hint must be suppressed under --final-message-only"
    );
}

/// P0-13.1a-model — --model override reaches the resolved model.
#[tokio::test]
async fn p0_model_override() {
    let provider: Arc<ModelCaptureProvider> = Arc::new(ModelCaptureProvider::new("ok"));
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let mut opts = make_opts("test");
    opts.model_override = Some("claude-haiku".into());

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let seen = provider.seen_model.lock().unwrap().clone();
    assert_eq!(
        seen,
        Some("claude-haiku".to_string()),
        "model override must reach CompletionOptions"
    );
}

/// P0-13.1a-file — --file and piped stdin produce XML <file> prefix, query is NOT concatenated.
#[tokio::test]
async fn p0_file_context_not_concatenated() {
    let provider: Arc<MessageCaptureProvider> =
        Arc::new(MessageCaptureProvider::new("file review done"));
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let mut opts = make_opts("review this");
    opts.files = vec![
        ResolvedFileContext {
            path: "test.rs".into(),
            content: "fn main() {}".into(),
            provenance: FileContextProvenance::UserProvided,
        },
        ResolvedFileContext {
            path: "<stdin>".into(),
            content: "piped content".into(),
            provenance: FileContextProvenance::UserProvided,
        },
    ];

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(output.contains("file review done"));

    let messages = provider
        .seen_messages
        .lock()
        .unwrap()
        .clone()
        .expect("provider should have received messages");
    let user_content = &messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .expect("user message must exist")
        .content;
    assert!(
        user_content.contains("<file path=\"test.rs\">"),
        "user message must contain XML file prefix, got: {}",
        user_content
    );
    assert!(
        user_content.contains("<file path=\"&lt;stdin&gt;\">"),
        "stdin context must also be wrapped in XML file block, got: {}",
        user_content
    );
    assert!(
        user_content.contains("</file>\n\nreview this"),
        "query must follow file blocks, not be concatenated into them, got: {}",
        user_content
    );
}

struct ToolUseProvider {
    call_count: std::sync::atomic::AtomicUsize,
}

impl ToolUseProvider {
    fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl StreamingProvider for ToolUseProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let chunks = if n == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "tool_call_1".into(),
                    name: "dummy_tool".into(),
                    input: serde_json::json!({}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![
                StreamChunk::Text {
                    content: "after denial".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-tool-use".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

struct BashToolUseProvider {
    call_count: std::sync::atomic::AtomicUsize,
}

impl BashToolUseProvider {
    fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl StreamingProvider for BashToolUseProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let chunks = if n == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "bash_call_1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "rm -rf /"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![
                StreamChunk::Text {
                    content: "ok done".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-bash-tool".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

fn build_test_core_with_security(
    provider: Arc<dyn StreamingProvider>,
    security: Arc<dyn SecurityPort>,
) -> CliCore {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let tools: Arc<dyn rustain::domain::ports::ToolSetPort> = Arc::new(NoOpToolSet);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(NoOpStorage);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let ledger: Arc<dyn rustain::domain::ports::UsageLedgerPort> = Arc::new(NoOpUsageLedger);

    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
    }
}

/// P0-13.1a-O2a — auto-deny: tool call in non-yolo → auto-rejected, exit 0, deny-count on stderr.
#[tokio::test]
async fn p0_o2a_auto_deny_path() {
    let provider: Arc<ToolUseProvider> = Arc::new(ToolUseProvider::new());
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("use a tool");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let stderr_str = String::from_utf8(stderr.into_inner()).unwrap();
    assert!(
        stderr_str.contains("Tool 'dummy_tool' requires approval. Use --yolo for auto-approve or --dry-run for plan only."),
        "expected per-tool deny message on stderr, got: {}",
        stderr_str
    );
    assert!(
        stderr_str.contains("1 tool call(s) auto-denied"),
        "expected deny-count on stderr, got: {}",
        stderr_str
    );
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(
        output.contains("after denial"),
        "turn should continue after denied tool, got: {}",
        output
    );
    let calls = provider
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        calls, 2,
        "provider should be called once for tool turn and once after denial"
    );
}

/// P0-13.1a-O3 — --file with outside-workspace absolute path: UserProvided provenance bypasses
/// workspace gating; the context reaches the provider.
#[tokio::test]
async fn p0_o3_file_outside_workspace() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider {
        text: "reviewed external file".into(),
    });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask-workspace");
    let mut opts = make_opts("review this external file");
    opts.files = vec![ResolvedFileContext {
        path: "/etc/outside-workspace/config.toml".into(),
        content: "external_key = true".into(),
        provenance: FileContextProvenance::UserProvided,
    }];

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(output.contains("reviewed external file"));
}

/// P0-13.1a-O2b — yolo + blocklist: blocklist check (step 2) fires before mode×risk (step 3.5),
/// so even in Yolo mode a blocklisted Bash command is denied. Pins permission_chain.rs ordering.
#[tokio::test]
async fn p0_o2b_yolo_blocklist_ordering() {
    let provider: Arc<BashToolUseProvider> = Arc::new(BashToolUseProvider::new());
    // Use the real production SecurityAdapter so the blocklist and mode gating
    // in permission_chain.rs are exercised end-to-end. Yolo mode would normally
    // auto-approve an Elevated tool, but the blocklist check (step 2) fires
    // before mode×risk gating (step 3.5), so the Bash command is still denied.
    let security = SecurityAdapter::new(std::env::current_dir().unwrap());
    security.set_mode(PermissionMode::Yolo);
    let security: Arc<dyn SecurityPort> = Arc::new(security);
    let core = build_test_core_with_security(provider.clone(), security);
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let mut opts = make_opts("run a bash command");
    opts.yolo = true;

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(
        exit,
        ExitCode::SUCCESS,
        "turn should complete despite denial"
    );
    let output = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(
        output.contains("ok done"),
        "provider should continue after denied tool: {}",
        output
    );
    let calls = provider
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        calls, 2,
        "provider should be called once for tool turn and once after blocklist denial"
    );
}

/// P0-13.1a-session — new session persisted; resume hint on stderr uses the real nanoid id.
#[tokio::test]
async fn p0_session_resume_hint() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(SimpleTextProvider { text: "ok".into() });
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
    let opts = make_opts("test");

    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());

    let exit = rustain::adapters::cli::ask::run_ask_core(
        provider,
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(exit, ExitCode::SUCCESS);
    let stderr_str = String::from_utf8(stderr.into_inner()).unwrap();
    assert!(
        stderr_str.contains("Session saved. Resume with: rustain --session "),
        "expected resume hint on stderr, got: {}",
        stderr_str
    );
    let session_line = stderr_str
        .lines()
        .find(|l| l.contains("Session saved"))
        .unwrap();
    let id = session_line.split("--session ").nth(1).unwrap().trim();
    assert!(
        id.len() == 21,
        "expected 21-char nanoid, got len={}: '{}'",
        id.len(),
        id
    );
}
