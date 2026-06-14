use std::io::Cursor;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use rustain::adapters::cli::ask::OutputFormat;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpSecurity, NoOpStorage, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::models::{
    CompletionOptions, FileContextProvenance, Message, MessageRole, PermissionMode, StopReason,
    StreamChunk, UsageInfo,
};
use rustain::domain::ports::SecurityPort;
use rustain::domain::ports::StreamingProvider;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::message_builder::ResolvedFileContext;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;
use sha2::{Digest, Sha256};
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}
fn make_opts(query: &str) -> rustain::adapters::cli::ask::AskOpts {
    rustain::adapters::cli::ask::AskOpts {
        query: query.to_string(),
        files: vec![],
        yolo: false,
        final_message_only: false,
        output_format: rustain::adapters::cli::ask::OutputFormat::Text,
        model_override: None,
        session_id: None,
        dry_run: false,
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
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

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
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
// ================== Story 13.1b: Structured Output & Schema Versioning ==================

/// Generic provider that emits a configured sequence of chunks.
struct ChunkProvider {
    chunks: Vec<StreamChunk>,
}

impl ChunkProvider {
    fn new(chunks: Vec<StreamChunk>) -> Self {
        Self { chunks }
    }
}

#[async_trait]
impl StreamingProvider for ChunkProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let chunks = self.chunks.clone();
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "mock-chunk".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}

fn make_opts_with_format(query: &str, fmt: OutputFormat) -> rustain::adapters::cli::ask::AskOpts {
    rustain::adapters::cli::ask::AskOpts {
        query: query.to_string(),
        files: vec![],
        yolo: false,
        final_message_only: false,
        output_format: fmt,
        model_override: None,
        session_id: None,
        dry_run: false,
    }
}

async fn run_ask_core_test(
    provider: Arc<dyn StreamingProvider>,
    opts: rustain::adapters::cli::ask::AskOpts,
) -> (ExitCode, String, String) {
    let core = build_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask");
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
    let out = String::from_utf8(stdout.into_inner()).unwrap();
    let err = String::from_utf8(stderr.into_inner()).unwrap();
    (exit, out, err)
}

/// P0-13.1b-text-identity — default mode and explicit `--output-format text` are identical.
#[tokio::test]
async fn p0_13_1b_text_identity() {
    let provider = Arc::new(SimpleTextProvider {
        text: "hello text identity".into(),
    });
    let default_opts = make_opts("test query");
    let text_opts = make_opts_with_format("test query", OutputFormat::Text);

    let (exit1, out1, _) = run_ask_core_test(provider.clone(), default_opts).await;
    let (exit2, out2, _) = run_ask_core_test(provider, text_opts).await;

    assert_eq!(exit1, ExitCode::SUCCESS);
    assert_eq!(exit2, ExitCode::SUCCESS);
    assert_eq!(
        out1, out2,
        "default text output must match explicit --output-format text"
    );
}

/// P0-13.1b-json-shape — single JSON document with the v1.0 schema fields.
#[tokio::test]
async fn p0_13_1b_json_shape() {
    let usage = UsageInfo {
        input_tokens: 10,
        output_tokens: 20,
        cache_creation_input_tokens: Some(1),
        cache_read_input_tokens: Some(2),
        reasoning_tokens: Some(3),
    };
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "JSON shape response".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::Usage {
            usage,
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_opts_with_format("test", OutputFormat::Json);
    opts.yolo = true;

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).expect("stdout must be valid JSON");
    assert_eq!(doc["schema_version"], "1.1");
    assert_eq!(doc["response"], "JSON shape response");
    assert!(
        !doc["model"].as_str().unwrap().is_empty(),
        "model must be the resolved concrete model string"
    );
    assert_eq!(doc["stop_reason"], "end_turn");
    assert_eq!(doc["usage"]["input_tokens"], 10);
    assert_eq!(doc["usage"]["output_tokens"], 20);
    assert_eq!(doc["usage"]["cache_creation_input_tokens"], 1);
    assert_eq!(doc["usage"]["cache_read_input_tokens"], 2);
    assert_eq!(doc["usage"]["reasoning_tokens"], 3);
    assert!(doc["tool_calls"].is_array());
    assert_eq!(doc["tool_calls"].as_array().unwrap().len(), 0);
    assert!(
        !doc["session_id"].as_str().unwrap().is_empty(),
        "session_id must be a real conversation id"
    );
    assert_eq!(doc["deny_count"], 0);
    assert!(
        !doc.as_object().unwrap().contains_key("error"),
        "success JSON must not contain an error object"
    );
    assert!(
        err.contains("Session saved"),
        "resume hint must stay on stderr, got: {}",
        err
    );
}

/// P0-13.1b-G1-usage-matrix — capture Usage regardless of chunk ordering.
#[tokio::test]
async fn p0_13_1b_g1_usage_matrix() {
    let usage = UsageInfo {
        input_tokens: 1,
        output_tokens: 2,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    };
    let cases: Vec<(&str, Vec<StreamChunk>)> = vec![
        (
            "after",
            vec![
                StreamChunk::Text {
                    content: "a".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
                StreamChunk::Usage {
                    usage: usage.clone(),
                    session_id: None,
                },
            ],
        ),
        (
            "before",
            vec![
                StreamChunk::Text {
                    content: "a".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::Usage {
                    usage: usage.clone(),
                    session_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ),
        (
            "interleaved",
            vec![
                StreamChunk::Text {
                    content: "a".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::Usage {
                    usage: usage.clone(),
                    session_id: None,
                },
                StreamChunk::Text {
                    content: "b".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ),
        (
            "none",
            vec![
                StreamChunk::Text {
                    content: "a".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ),
        (
            "twice",
            vec![
                StreamChunk::Text {
                    content: "a".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::Usage {
                    usage: UsageInfo {
                        input_tokens: 1,
                        output_tokens: 2,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    session_id: None,
                },
                StreamChunk::Usage {
                    usage: UsageInfo {
                        input_tokens: 3,
                        output_tokens: 4,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    session_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ),
    ];

    for (name, chunks) in cases {
        let provider = Arc::new(ChunkProvider::new(chunks));
        let opts = make_opts_with_format("test", OutputFormat::Json);
        let (exit, out, _) = run_ask_core_test(provider, opts).await;
        assert_eq!(exit, ExitCode::SUCCESS, "case {} failed", name);

        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        if name == "none" {
            assert!(
                doc["usage"].is_null(),
                "case {}: usage must be explicit null, got {:?}",
                name,
                doc["usage"]
            );
        } else if name == "twice" {
            assert_eq!(
                doc["usage"]["input_tokens"], 3,
                "case {}: last usage wins",
                name
            );
            assert_eq!(
                doc["usage"]["output_tokens"], 4,
                "case {}: last usage wins",
                name
            );
        } else {
            assert_eq!(doc["usage"]["input_tokens"], 1, "case {}", name);
            assert_eq!(doc["usage"]["output_tokens"], 2, "case {}", name);
        }
        assert!(out.contains('a'), "case {}: response text intact", name);
    }
}

/// P0-13.1b-G3-stream-ndjson + mid-stream error.
#[tokio::test]
async fn p0_13_1b_g3_stream_ndjson_and_midstream_error() {
    // Happy path: every line is a parseable object, no embedded newlines,
    // terminal line is turn_complete.
    let usage = UsageInfo {
        input_tokens: 5,
        output_tokens: 6,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: None,
    };
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "hello".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "t1".into(),
            name: "tool".into(),
            input: serde_json::json!({"x": 1}),
        },
        StreamChunk::ToolResult {
            id: "t1".into(),
            content: "r1".into(),
            is_error: false,
        },
        StreamChunk::Usage {
            usage,
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_opts_with_format("test", OutputFormat::StreamJson);
    opts.yolo = true;

    let (exit, out, _err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let lines: Vec<&str> = out.trim_end().split('\n').collect();
    assert!(
        lines.len() >= 2,
        "expected >=2 NDJSON lines, got {}",
        lines.len()
    );
    for line in &lines {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "line not parseable: {}",
            line
        );
        assert!(
            !line.contains('\n'),
            "compact JSON must not contain embedded newline: {}",
            line
        );
        let ev: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(ev["schema_version"], "1.1");
    }
    let types: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(types.contains(&"text".to_string()));
    assert!(types.contains(&"tool_use".to_string()));
    assert!(types.contains(&"tool_result".to_string()));
    assert!(types.contains(&"usage".to_string()));
    assert_eq!(types.last().unwrap(), "turn_complete");

    // Mid-stream error: prior NDJSON lines still parse, terminal line is error.
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "part1".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::Text {
            content: "part2".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::Error {
            content: "boom".into(),
        },
    ]));
    let opts = make_opts_with_format("test", OutputFormat::StreamJson);
    let (exit, out, _err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::FAILURE);

    let lines: Vec<&str> = out.trim_end().split('\n').collect();
    assert!(
        lines.len() >= 2,
        "expected prior NDJSON lines plus error, got {:?}",
        lines
    );
    for line in &lines {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "line not parseable: {}",
            line
        );
    }
    let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(last["type"], "error");
    assert!(last["error"]["message"].as_str().unwrap().contains("boom"));
}

/// P0-13.1b-G2-deny-fields + exit-zero — auto-denied tool surfaces deny_count on stdout.
#[tokio::test]
async fn p0_13_1b_g2_deny_fields() {
    let provider = Arc::new(ToolUseProvider::new());
    let mut opts = make_opts_with_format("use a tool", OutputFormat::Json);
    opts.yolo = false;

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(
        exit,
        ExitCode::SUCCESS,
        "deny must keep exit 0, stderr: {}",
        err
    );

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        doc["deny_count"].as_u64().unwrap() >= 1,
        "deny_count must surface on stdout, got {}",
        doc["deny_count"]
    );
}

/// P0-13.1b-G2-stream-json-deny — stream-json turn_complete carries deny_count.
#[tokio::test]
async fn p0_13_1b_g2_stream_json_deny_fields() {
    let provider = Arc::new(ToolUseProvider::new());
    let mut opts = make_opts_with_format("use a tool", OutputFormat::StreamJson);
    opts.yolo = false;

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(
        exit,
        ExitCode::SUCCESS,
        "deny must keep exit 0 in stream-json, stderr: {}",
        err
    );

    // Find the turn_complete line and assert deny_count >= 1
    let lines: Vec<&str> = out.trim().lines().collect();
    let tc_line = lines
        .iter()
        .find(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .map(|v| v["type"] == "turn_complete")
                .unwrap_or(false)
        })
        .expect("stream-json must contain a turn_complete event");
    let tc: serde_json::Value = serde_json::from_str(tc_line).unwrap();
    assert!(
        tc["deny_count"].as_u64().unwrap() >= 1,
        "stream-json turn_complete must carry deny_count >= 1, got: {}",
        tc["deny_count"]
    );
}

/// P0-13.1b-stdout-purity-under-denials — json stdout contains only the JSON doc.
#[tokio::test]
async fn p0_13_1b_stdout_purity_under_denials() {
    let provider = Arc::new(ToolUseProvider::new());
    let mut opts = make_opts_with_format("use a tool", OutputFormat::Json);
    opts.yolo = false;

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    assert!(
        serde_json::from_str::<serde_json::Value>(&out).is_ok(),
        "stdout must be a single JSON document, got: {}",
        out
    );
    assert!(
        !out.contains("requires approval"),
        "deny narration must not leak to stdout"
    );
    assert!(
        !out.contains("auto-denied"),
        "deny summary must not leak to stdout"
    );
    assert!(
        !out.contains("Session saved"),
        "resume hint must not leak to stdout"
    );
    assert!(
        err.contains("requires approval") || err.contains("auto-denied"),
        "stderr must still carry human-readable denial info, got: {}",
        err
    );
}

/// P0-13.1b-json-error — turn failure emits a structured error document in json mode.
#[tokio::test]
async fn p0_13_1b_json_error() {
    let provider = Arc::new(ErrorProvider);
    let opts = make_opts_with_format("test", OutputFormat::Json);

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::FAILURE);

    let doc: serde_json::Value =
        serde_json::from_str(&out).expect("json error must be parseable stdout");
    assert_eq!(doc["schema_version"], "1.1");
    assert!(
        doc["response"].is_null(),
        "error envelope: response must be null"
    );
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("simulated provider failure")
    );
    // F-7: verify full envelope parity — same fields as success (AC6)
    assert!(
        doc.get("model").is_some(),
        "error envelope must carry model"
    );
    assert!(
        doc.get("stop_reason").is_some(),
        "error envelope must carry stop_reason"
    );
    assert!(
        doc.get("usage").is_some(),
        "error envelope must carry usage"
    );
    assert!(
        doc.get("tool_calls").is_some(),
        "error envelope must carry tool_calls"
    );
    assert!(
        doc.get("session_id").is_some(),
        "error envelope must carry session_id"
    );
    assert!(
        doc.get("deny_count").is_some(),
        "error envelope must carry deny_count"
    );
    assert!(err.contains("Error"));
}

/// Collect every field path from a serialized JSON value.
/// Arrays are represented with `[]` so nested object keys under arrays are captured.
fn collect_field_paths(
    value: &serde_json::Value,
    prefix: &str,
    paths: &mut std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                paths.insert(p.clone());
                collect_field_paths(v, &p, paths);
            }
        }
        serde_json::Value::Array(arr) => {
            if !prefix.is_empty() {
                paths.insert(format!("{}[]", prefix));
            }
            for item in arr {
                collect_field_paths(item, &format!("{}[]", prefix), paths);
            }
        }
        _ => {}
    }
}

/// P0-13.1b/c-G4-schema-fingerprint — the v1.1 field set is pinned.
/// Story 13.1c additive bump 1.0→1.1: dry_run, plan, tools_would_use (always-present, O3).
/// turn_complete now carries dry_run + tools_would_use.
const SCHEMA_FINGERPRINT: &str = "9287240bc07389a3dddf14ecbe68d36fdd12d37c7ec8a80b34c880b6fe847f9f";
#[tokio::test]
async fn p0_13_1b_g4_schema_fingerprint() {
    let usage = UsageInfo {
        input_tokens: 1,
        output_tokens: 2,
        cache_creation_input_tokens: Some(3),
        cache_read_input_tokens: Some(4),
        reasoning_tokens: Some(5),
    };
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "fp".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "tc1".into(),
            name: "tc_name".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "tc1".into(),
            content: "res".into(),
            is_error: false,
        },
        StreamChunk::Usage {
            usage,
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_opts_with_format("test", OutputFormat::Json);
    opts.yolo = true;

    let (exit, out, _) = run_ask_core_test(provider.clone(), opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let mut paths = std::collections::BTreeSet::new();
    collect_field_paths(&doc, "", &mut paths);
    // Include the full stream-json field-path set (F-8: was type-only, now field paths).
    let opts_stream = make_opts_with_format("test", OutputFormat::StreamJson);
    let (_exit_s, out_s, _) = run_ask_core_test(provider, opts_stream).await;
    for line in out_s.trim().lines() {
        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(t) = ev["type"].as_str() {
                collect_field_paths(&ev, &format!("stream:{}", t), &mut paths);
            }
        }
    }

    let joined = paths.iter().cloned().collect::<Vec<_>>().join("\n");
    let hash = hex::encode(Sha256::digest(&joined));
    assert_eq!(
        hash, SCHEMA_FINGERPRINT,
        "field set changed — if additive bump minor, if breaking bump major, then update the fingerprint.\nfields:\n{}",
        joined
    );
}

/// Assert that every object key in a JSON value matches snake_case.
fn assert_all_keys_snake_case(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                assert!(
                    k.chars()
                        .next()
                        .map(|c| c.is_ascii_lowercase())
                        .unwrap_or(false)
                        && k.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "key '{}' is not snake_case",
                    k
                );
                assert_all_keys_snake_case(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                assert_all_keys_snake_case(item);
            }
        }
        _ => {}
    }
}

/// P0-13.1b-G5-snake-case-lint — every key in json and stream-json is snake_case.
#[tokio::test]
async fn p0_13_1b_g5_snake_case_lint() {
    let usage = UsageInfo {
        input_tokens: 1,
        output_tokens: 2,
        cache_creation_input_tokens: Some(3),
        cache_read_input_tokens: Some(4),
        reasoning_tokens: Some(5),
    };
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "hi".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "t".into(),
            name: "n".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "t".into(),
            content: "r".into(),
            is_error: false,
        },
        StreamChunk::Usage {
            usage,
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));

    let mut opts_json = make_opts_with_format("test", OutputFormat::Json);
    opts_json.yolo = true;
    let (exit, out, _) = run_ask_core_test(provider.clone(), opts_json).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    let doc = serde_json::from_str::<serde_json::Value>(&out).unwrap();
    assert_all_keys_snake_case(&doc);

    let mut opts_stream = make_opts_with_format("test", OutputFormat::StreamJson);
    opts_stream.yolo = true;
    let (exit2, out2, _) = run_ask_core_test(provider, opts_stream).await;
    assert_eq!(exit2, ExitCode::SUCCESS);
    for line in out2.trim_end().split('\n') {
        let ev = serde_json::from_str::<serde_json::Value>(line).unwrap();
        assert_all_keys_snake_case(&ev);
    }
}

/// P0-13.1b-final-message-json — `--final-message-only` narrows response to final block.
#[tokio::test]
async fn p0_13_1b_final_message_json() {
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "first block".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "t".into(),
            name: "n".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "t".into(),
            content: "r".into(),
            is_error: false,
        },
        StreamChunk::Text {
            content: "final block".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_opts_with_format("test", OutputFormat::Json);
    opts.yolo = true;
    opts.final_message_only = true;

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["response"], "final block");
    assert!(!err.contains("Session saved"), "stderr must be quieted");
}

/// P0-13.1b-no-format-flag — `--format` is intentionally absent.
#[test]
fn p0_13_1b_no_format_flag() {
    let mut cmd = assert_cmd::Command::cargo_bin("rustain").unwrap();
    cmd.args(["ask", "q", "--format", "json"]);
    cmd.assert().failure();
}

/// P1-13.1b-G7-empty-response — pure tool turn produces response: "" (not missing).
#[tokio::test]
async fn p1_13_1b_g7_empty_response() {
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::ToolUse {
            id: "t".into(),
            name: "n".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "t".into(),
            content: "r".into(),
            is_error: false,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_opts_with_format("test", OutputFormat::Json);
    opts.yolo = true;

    let (exit, out, _) = run_ask_core_test(provider.clone(), opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    let doc = serde_json::from_str::<serde_json::Value>(&out).unwrap();
    assert_eq!(doc["response"], "");

    let opts_stream = make_opts_with_format("test", OutputFormat::StreamJson);
    let (exit2, out2, _) = run_ask_core_test(provider, opts_stream).await;
    assert_eq!(exit2, ExitCode::SUCCESS);
    assert!(out2.contains(r#""type":"turn_complete""#));
}

/// P1-13.1b-G8-escaping — JSON/UTF-8 escaping round-trips correctly.
#[tokio::test]
async fn p1_13_1b_g8_escaping() {
    let text = "line1\nline2\t\"quoted\" 🎉";
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: text.into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let opts = make_opts_with_format("test", OutputFormat::Json);

    let (exit, out, _) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    let doc = serde_json::from_str::<serde_json::Value>(&out).unwrap();
    assert_eq!(doc["response"].as_str().unwrap(), text);
}

/// P1-13.1b-G12-framing — trailing newline discipline.
#[tokio::test]
async fn p1_13_1b_g12_framing() {
    let provider = Arc::new(SimpleTextProvider { text: "hi".into() });

    let opts_json = make_opts_with_format("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_core_test(provider.clone(), opts_json).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(out.ends_with('\n'), "single JSON doc must end with newline");

    let opts_stream = make_opts_with_format("test", OutputFormat::StreamJson);
    let (exit2, out2, _) = run_ask_core_test(provider, opts_stream).await;
    assert_eq!(exit2, ExitCode::SUCCESS);
    assert!(
        out2.ends_with('\n'),
        "NDJSON stream must end with final newline"
    );
}

/// P0-13.1b-json-error-unreadable-file — pre-turn file failure emits structured error.
#[test]
fn p0_13_1b_json_error_unreadable_file() {
    let mut cmd = assert_cmd::Command::cargo_bin("rustain").unwrap();
    cmd.args([
        "ask",
        "q",
        "--output-format",
        "json",
        "--file",
        "/nonexistent/path/that/does/not/exist.txt",
    ]);
    let output = cmd.output().unwrap();
    assert!(!output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let doc = serde_json::from_str::<serde_json::Value>(&stdout)
        .expect("stdout must contain structured error document");
    assert_eq!(doc["schema_version"], "1.1");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read file")
    );
}
// ================== Story 13.1c: Dry-Run Plan Mode ==================

/// ToolSet that handles `propose_plan` by parsing the plan and emitting PlanProposed
/// on the captured event_tx. All other tools return NotFound.
struct PlanToolSet {
    event_tx: tokio::sync::mpsc::UnboundedSender<rustain::domain::events::AppEvent>,
}

impl PlanToolSet {
    fn new(
        event_tx: tokio::sync::mpsc::UnboundedSender<rustain::domain::events::AppEvent>,
    ) -> Self {
        Self { event_tx }
    }
}

#[async_trait]
impl rustain::domain::ports::ToolSetPort for PlanToolSet {
    fn available_tools(&self) -> Vec<rustain::domain::models::ToolDefinition> {
        vec![rustain::domain::models::ToolDefinition {
            name: "propose_plan".to_string(),
            description: "Propose a structured plan.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "description": {"type": "string"}
                            }
                        }
                    },
                    "estimated_tool_calls": {"type": "number"},
                    "estimated_seconds": {"type": "number"}
                },
                "required": ["title", "tasks"]
            }),
            parallel_safe: false,
        }]
    }
    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<rustain::domain::models::ToolResult, rustain::domain::errors::ToolError> {
        if tool_name == "propose_plan" {
            use rustain::domain::services::plan_parser::parse_plan_input;
            let plan = parse_plan_input(&input, "test-plan-id")
                .map_err(|e| rustain::domain::errors::ToolError::InvalidInput(e.to_string()))?;
            let _ = self
                .event_tx
                .send(rustain::domain::events::AppEvent::PlanProposed {
                    conversation_id: String::new(),
                    plan,
                });
            Ok(rustain::domain::models::ToolResult {
                tool_use_id: String::new(),
                content: "Plan proposed for user approval.".to_string(),
                is_error: false,
            })
        } else {
            Err(rustain::domain::errors::ToolError::NotFound(
                tool_name.into(),
            ))
        }
    }
}

/// Build a CliCore that handles `propose_plan` via PlanToolSet.
fn build_plan_test_core(provider: Arc<dyn StreamingProvider>) -> CliCore {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let security: Arc<dyn rustain::domain::ports::SecurityPort> =
        Arc::new(SecurityAdapter::new(std::env::current_dir().unwrap()));
    let tools: Arc<dyn rustain::domain::ports::ToolSetPort> =
        Arc::new(PlanToolSet::new(event_tx.clone()));
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

fn make_dry_run_opts(query: &str, fmt: OutputFormat) -> rustain::adapters::cli::ask::AskOpts {
    rustain::adapters::cli::ask::AskOpts {
        query: query.to_string(),
        files: vec![],
        yolo: false,
        final_message_only: false,
        output_format: fmt,
        model_override: None,
        session_id: None,
        dry_run: true,
    }
}

async fn run_ask_plan_test(
    provider: Arc<dyn StreamingProvider>,
    opts: rustain::adapters::cli::ask::AskOpts,
) -> (ExitCode, String, String) {
    let core = build_plan_test_core(provider.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-ask-dryrun");
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
    let out = String::from_utf8(stdout.into_inner()).unwrap();
    let err = String::from_utf8(stderr.into_inner()).unwrap();
    (exit, out, err)
}

/// Provider that emits a propose_plan ToolUse (iteration 0), then text + EndTurn (iteration 1).
struct ProposePlanProvider {
    call_count: std::sync::atomic::AtomicUsize,
    plan_input: serde_json::Value,
    extra_tool_uses: Vec<(String, String, serde_json::Value)>, // (id, name, input)
}

impl ProposePlanProvider {
    fn with_plan(plan_input: serde_json::Value) -> Self {
        Self {
            call_count: std::sync::atomic::AtomicUsize::new(0),
            plan_input,
            extra_tool_uses: vec![],
        }
    }

    fn with_plan_and_tools(
        plan_input: serde_json::Value,
        extra: Vec<(String, String, serde_json::Value)>,
    ) -> Self {
        Self {
            call_count: std::sync::atomic::AtomicUsize::new(0),
            plan_input,
            extra_tool_uses: extra,
        }
    }
}

#[async_trait]
impl StreamingProvider for ProposePlanProvider {
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
            let mut v = Vec::new();
            // Extra tool uses first (e.g. Write, Bash — will be denied in plan mode)
            for (id, name, input) in &self.extra_tool_uses {
                v.push(StreamChunk::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            v.push(StreamChunk::ToolUse {
                id: "plan_call_1".into(),
                name: "propose_plan".into(),
                input: self.plan_input.clone(),
            });
            v.push(StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse,
            });
            v
        } else {
            vec![
                StreamChunk::Text {
                    content: "Plan proposed.".into(),
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
        "mock-propose-plan".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}

fn sample_plan_input() -> serde_json::Value {
    serde_json::json!({
        "title": "Refactor auth module",
        "tasks": [
            {"title": "Extract interfaces", "description": "Move auth contracts to ports"},
            {"title": "Implement adapters", "description": "Create concrete implementations"}
        ],
        "estimated_tool_calls": 5,
        "estimated_seconds": 40
    })
}

/// P0-13.1c-yolo-conflict — `--dry-run --yolo` → clap rejects.
#[test]
fn p0_13_1c_yolo_conflict() {
    use clap::Parser;
    use rustain::adapters::cli::commands::Cli;
    let result = Cli::try_parse_from(["rustain", "ask", "test query", "--dry-run", "--yolo"]);
    assert!(result.is_err(), "--dry-run --yolo must be rejected by clap");
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot be used with") || msg.contains("conflict"),
        "error must mention conflict: {}",
        msg
    );
}

/// P0-13.1c-always-present (O3 shape) — normal ask has dry_run: false, plan: null, tools_would_use: [].
#[tokio::test]
async fn p0_13_1c_always_present() {
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "hello".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let opts = make_opts_with_format("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    // All three keys MUST be present (not absent)
    assert!(doc.get("dry_run").is_some(), "dry_run key must be present");
    assert!(doc.get("plan").is_some(), "plan key must be present");
    assert!(
        doc.get("tools_would_use").is_some(),
        "tools_would_use key must be present"
    );
    // Values on normal ask
    assert_eq!(doc["dry_run"], false);
    assert!(doc["plan"].is_null(), "plan must be null on normal ask");
    assert_eq!(doc["tools_would_use"], serde_json::json!([]));
}

/// P0-13.1c-no-plan-fallback — no propose_plan → text shows prose + tools line; json plan == null.
#[tokio::test]
async fn p0_13_1c_no_plan_fallback() {
    // Provider emits text only, no propose_plan
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "I would refactor the auth module by...".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));

    // Text mode
    let opts = make_dry_run_opts("plan something", OutputFormat::Text);
    let (exit, out, _) = run_ask_core_test(provider.clone(), opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(
        out.contains("I would refactor"),
        "should contain prose fallback"
    );
    assert!(
        out.contains("Tools attempted: (none)"),
        "should have attempted-tools line"
    );

    // Json mode
    let opts = make_dry_run_opts("plan something", OutputFormat::Json);
    let (exit, out, _) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["dry_run"], true);
    assert!(
        doc["plan"].is_null(),
        "plan must be null when no propose_plan"
    );
    assert!(
        doc["response"]
            .as_str()
            .unwrap()
            .contains("I would refactor")
    );
}

/// P0-13.1c-json-plan + schema 1.1 — dry-run json with plan: parse doc, check fields, G5 lint.
#[tokio::test]
async fn p0_13_1c_json_plan_and_schema_1_1() {
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        sample_plan_input(),
        vec![
            (
                "w1".into(),
                "Write".into(),
                serde_json::json!({"path": "/tmp/x"}),
            ),
            (
                "b1".into(),
                "Bash".into(),
                serde_json::json!({"command": "echo"}),
            ),
        ],
    ));
    let opts = make_dry_run_opts("refactor auth", OutputFormat::Json);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["schema_version"], "1.1");
    assert_eq!(doc["dry_run"], true);
    assert!(!doc["plan"].is_null(), "plan must be non-null");
    assert_eq!(doc["plan"]["title"], "Refactor auth module");

    // Snake keys
    assert!(
        doc["plan"].get("estimated_effort").is_some(),
        "must have estimated_effort (snake)"
    );
    assert!(
        doc["plan"]["tasks"][0].get("depends_on").is_some(),
        "must have depends_on (snake)"
    );
    assert!(
        doc["plan"]["tasks"][0].get("sub_tasks").is_some(),
        "must have sub_tasks (snake)"
    );

    // tools_would_use sorted, excluding plan-control
    let tools: Vec<String> = doc["tools_would_use"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tools,
        vec!["Bash", "Write"],
        "tools_would_use should be sorted, plan-control excluded"
    );

    // G5 snake-lint over the entire doc
    assert_all_keys_snake_case(&doc);
}

/// P0-13.1c-text-plan — dry-run text renders title, numbered tasks, effort, tools line.
#[tokio::test]
async fn p0_13_1c_text_plan() {
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        sample_plan_input(),
        vec![("w1".into(), "Write".into(), serde_json::json!({}))],
    ));
    let opts = make_dry_run_opts("refactor auth", OutputFormat::Text);
    let (exit, out, err) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    // Title
    assert!(
        out.contains("Refactor auth module"),
        "stdout should have plan title"
    );
    // Numbered tasks
    assert!(out.contains("1. Extract interfaces"), "should have task 1");
    assert!(out.contains("2. Implement adapters"), "should have task 2");
    // Effort estimate
    assert!(
        out.contains("Estimated:"),
        "should have estimated effort line"
    );
    assert!(
        out.contains("~5 tool calls"),
        "should have tool call estimate"
    );
    // Attempted-tools line
    assert!(
        out.contains("Tools attempted: Write"),
        "should have attempted-tools line"
    );
    // Purity: narration on stderr, not stdout
    assert!(!out.contains("Session saved"), "no resume hint in stdout");
    // No resume hint anywhere (read-only)
    assert!(
        !err.contains("Session saved"),
        "no resume hint in stderr (read-only)"
    );
    assert!(!err.contains("Resume with"), "no resume hint");
}

/// P0-13.1c-read-only (AC7) — dry-run writes NO session state.
#[tokio::test]
async fn p0_13_1c_read_only() {
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "dry run output".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let opts = make_dry_run_opts("test read only", OutputFormat::Text);
    let (exit, _out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);
    // No session-save warning
    assert!(
        !err.contains("Session saved"),
        "dry-run must not save session"
    );
    assert!(
        !err.contains("Resume with"),
        "dry-run must not emit resume hint"
    );
}

/// P0-13.1c-deny-count-vs-tools-would-use — deny_count == 0 AND tools_would_use has attempted tools.
#[tokio::test]
async fn p0_13_1c_deny_count_vs_tools_would_use() {
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        sample_plan_input(),
        vec![
            (
                "w1".into(),
                "Write".into(),
                serde_json::json!({"path": "/tmp/x"}),
            ),
            (
                "b1".into(),
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
            ),
        ],
    ));
    let opts = make_dry_run_opts("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    // deny_count is structurally 0 in dry-run (plan-mode denials short-circuit before ApprovalRuntime)
    assert_eq!(doc["deny_count"], 0, "deny_count must be 0 in pure dry-run");
    // tools_would_use captures the attempted tools
    let tools: Vec<String> = doc["tools_would_use"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tools,
        vec!["Bash", "Write"],
        "tools_would_use must contain attempted tools"
    );
}

/// P0-13.1c-denied-tool-in-set — a single tool: side-effect absent AND name in tools_would_use.
#[tokio::test]
async fn p0_13_1c_denied_tool_in_set() {
    // Provider emits Write ToolUse targeting a path inside a temp directory.
    let tmp = tempfile::tempdir().unwrap();
    let target_path = tmp.path().join("denied-test-file");
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        sample_plan_input(),
        vec![(
            "w1".into(),
            "Write".into(),
            serde_json::json!({"path": target_path.to_string_lossy()}),
        )],
    ));
    let opts = make_dry_run_opts("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let tools: Vec<String> = doc["tools_would_use"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        tools.contains(&"Write".to_string()),
        "Write must be in tools_would_use"
    );
    // The file must NOT exist (side-effect absent)
    assert!(
        !target_path.exists(),
        "denied tool must not have written the file"
    );
}

/// P0-13.1c-stream-plan — stream-json: plan_proposed line + turn_complete with dry_run/tools_would_use.
#[tokio::test]
async fn p0_13_1c_stream_plan() {
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        sample_plan_input(),
        vec![("w1".into(), "Write".into(), serde_json::json!({}))],
    ));
    let opts = make_dry_run_opts("test", OutputFormat::StreamJson);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let lines: Vec<serde_json::Value> = out
        .trim()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Every line has schema_version 1.1
    for line in &lines {
        assert_eq!(
            line["schema_version"], "1.1",
            "every line must have schema 1.1"
        );
    }

    // Find plan_proposed event
    let plan_proposed = lines.iter().find(|l| l["type"] == "plan_proposed");
    assert!(
        plan_proposed.is_some(),
        "must have a plan_proposed event line"
    );
    let pp = plan_proposed.unwrap();
    assert_eq!(pp["dry_run"], true);
    assert!(!pp["plan"].is_null(), "plan_proposed must carry a plan");
    assert_eq!(pp["plan"]["title"], "Refactor auth module");
    // Snake case in plan
    assert_all_keys_snake_case(pp);

    // Find turn_complete
    let turn_complete = lines.iter().find(|l| l["type"] == "turn_complete");
    assert!(turn_complete.is_some(), "must have turn_complete");
    let tc = turn_complete.unwrap();
    assert_eq!(tc["dry_run"], true);
    assert!(
        tc["tools_would_use"].is_array(),
        "turn_complete must carry tools_would_use"
    );
}

/// P0-13.1c-last-plan-wins — two propose_plan calls → the second is rendered.
#[tokio::test]
async fn p0_13_1c_last_plan_wins() {
    // Provider that emits two propose_plan calls in iteration 0, then ends in iteration 1.
    struct TwoPlanProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl StreamingProvider for TwoPlanProvider {
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
                        id: "p1".into(),
                        name: "propose_plan".into(),
                        input: serde_json::json!({"title": "First plan", "tasks": [{"title": "A"}]}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ]
            } else if n == 1 {
                vec![
                    StreamChunk::ToolUse {
                        id: "p2".into(),
                        name: "propose_plan".into(),
                        input: serde_json::json!({"title": "Second plan", "tasks": [{"title": "B"}]}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ]
            } else {
                vec![
                    StreamChunk::Text {
                        content: "done".into(),
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
            "mock-two-plan".into()
        }
        fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
            Ok(())
        }

        async fn connectivity_probe(
            &self,
        ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError>
        {
            Ok(rustain::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    let provider = Arc::new(TwoPlanProvider {
        call_count: std::sync::atomic::AtomicUsize::new(0),
    });
    let opts = make_dry_run_opts("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["plan"]["title"], "Second plan", "last plan must win");
}

/// P0-13.1c-bounded-timeout (O4 hang guard) — the dry-run turn reaches EndTurn within a bounded timeout.
#[tokio::test]
async fn p0_13_1c_bounded_timeout() {
    let provider = Arc::new(ProposePlanProvider::with_plan(sample_plan_input()));
    let opts = make_dry_run_opts("test timeout", OutputFormat::Text);

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        run_ask_plan_test(provider, opts).await
    })
    .await;
    assert!(
        result.is_ok(),
        "dry-run must complete within bounded timeout"
    );
    let (exit, _, _) = result.unwrap();
    assert_eq!(exit, ExitCode::SUCCESS);
}

/// P0-13.1c-addendum-present (O7) — dry_run_reminder() string present in system prompt when dry-run.
#[tokio::test]
async fn p0_13_1c_addendum_present() {
    use std::sync::Mutex;

    /// Provider that captures the system prompt from options.
    struct SystemPromptCapture {
        captured: Mutex<Option<String>>,
    }
    #[async_trait]
    impl StreamingProvider for SystemPromptCapture {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            options: CompletionOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, StreamChunk>,
            rustain::domain::errors::ProviderError,
        > {
            *self.captured.lock().unwrap() = Some(options.system_prompt.clone());
            let chunks = vec![
                StreamChunk::Text {
                    content: "ok".into(),
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
            "mock-capture".into()
        }
        fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
            Ok(())
        }

        async fn connectivity_probe(
            &self,
        ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError>
        {
            Ok(rustain::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    let addendum = rustain::domain::services::plan_mode_injector::dry_run_reminder();

    // With dry-run: addendum present
    let provider_dr = Arc::new(SystemPromptCapture {
        captured: Mutex::new(None),
    });
    let opts = make_dry_run_opts("test", OutputFormat::Text);
    let core = build_test_core(provider_dr.clone());
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-addendum");
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let _ = rustain::adapters::cli::ask::run_ask_core(
        provider_dr.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;
    let prompt_dr = provider_dr.captured.lock().unwrap().clone().unwrap();
    assert!(
        prompt_dr.contains(addendum),
        "dry-run system prompt must contain the addendum"
    );

    // Without dry-run: addendum absent
    let provider_no = Arc::new(SystemPromptCapture {
        captured: Mutex::new(None),
    });
    let opts = make_opts("test");
    let core = build_test_core(provider_no.clone());
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let _ = rustain::adapters::cli::ask::run_ask_core(
        provider_no.clone(),
        core,
        opts,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;
    let prompt_no = provider_no.captured.lock().unwrap().clone().unwrap();
    assert!(
        !prompt_no.contains(addendum),
        "normal system prompt must NOT contain the addendum"
    );
}

/// P0-13.1c-tools-line-divergence — plan content line and tools_would_use are independent.
#[tokio::test]
async fn p0_13_1c_tools_line_divergence() {
    // Plan with a "Tools:" field in its description that differs from attempted ToolUse.
    let plan = serde_json::json!({
        "title": "Divergence test",
        "tasks": [{"title": "Use grep and sed", "description": "Tools: grep, sed"}]
    });
    let provider = Arc::new(ProposePlanProvider::with_plan_and_tools(
        plan,
        vec![("w1".into(), "Write".into(), serde_json::json!({}))],
    ));
    let opts = make_dry_run_opts("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_plan_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let tools: Vec<String> = doc["tools_would_use"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    // tools_would_use is from ToolUse chunks, not from plan description content
    assert_eq!(
        tools,
        vec!["Write"],
        "tools_would_use independent from plan text"
    );
    assert!(
        doc["plan"]["tasks"][0]["description"]
            .as_str()
            .unwrap()
            .contains("grep"),
        "plan content has its own tools reference"
    );
}

/// P0-13.1c-no-mutate-differential — same mock: without dry-run tool executes (if available),
/// with dry-run it doesn't. Since NoOpToolSet doesn't actually execute tools, we verify
/// through deny_count and permission behavior.
#[tokio::test]
async fn p0_13_1c_no_mutate_differential() {
    // Provider emits dummy_tool ToolUse.
    let provider_normal = Arc::new(ToolUseProvider::new());
    let provider_dryrun = Arc::new(ToolUseProvider::new());

    // Normal+yolo mode: SecurityAdapter in Yolo so tool executes (even if NotFound).
    let security_yolo = Arc::new(SecurityAdapter::new(std::env::current_dir().unwrap()));
    security_yolo.set_mode(PermissionMode::Yolo);
    let core = build_test_core_with_security(provider_normal.clone(), security_yolo);
    let mut opts_normal = make_opts_with_format("test", OutputFormat::Json);
    opts_normal.yolo = true;
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-differential");
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let exit_n = rustain::adapters::cli::ask::run_ask_core(
        provider_normal,
        core,
        opts_normal,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(exit_n, ExitCode::SUCCESS);
    let out_n = String::from_utf8(stdout.into_inner()).unwrap();
    let doc_n: serde_json::Value = serde_json::from_str(&out_n).unwrap();
    // In normal+yolo, tool_calls should have result (even if error from NotFound)
    assert!(
        !doc_n["tool_calls"].as_array().unwrap().is_empty(),
        "normal mode should have tool_calls"
    );
    // Normal runs must report tools_would_use: [] (O3 always-present).
    assert_eq!(
        doc_n["tools_would_use"],
        serde_json::json!([]),
        "normal mode tools_would_use must be empty"
    );

    // Dry-run mode: tool should be denied by Plan mode gate
    let security = Arc::new(SecurityAdapter::new(std::env::current_dir().unwrap()));
    let core = build_test_core_with_security(provider_dryrun.clone(), security);
    let opts_dr = make_dry_run_opts("test", OutputFormat::Json);
    let config = default_config();
    let workspace = std::path::Path::new("/tmp/test-differential");
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let exit_d = rustain::adapters::cli::ask::run_ask_core(
        provider_dryrun,
        core,
        opts_dr,
        &config,
        workspace,
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(exit_d, ExitCode::SUCCESS);
    let out_d = String::from_utf8(stdout.into_inner()).unwrap();
    let doc_d: serde_json::Value = serde_json::from_str(&out_d).unwrap();

    assert_eq!(doc_d["dry_run"], true, "must be dry_run");
    // In dry-run the tool was attempted but the plan gate blocked it.
    // The tool name appears in tools_would_use
    let tools: Vec<String> = doc_d["tools_would_use"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        tools.contains(&"dummy_tool".to_string()),
        "dry-run tools_would_use must contain the attempted tool"
    );
}

/// P0-13.1c-final-message-only-dry-run — `--dry-run --final-message-only` narrows text output
/// to the final assistant block plus tools line, and suppresses resume hint.
#[tokio::test]
async fn p0_13_1c_final_message_only_dry_run() {
    // Provider emits a tool use (resets last_block_start) then final text.
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "Reasoning about the refactor...".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "t1".into(),
            name: "propose_plan".into(),
            input: sample_plan_input(),
        },
        StreamChunk::ToolResult {
            id: "t1".into(),
            content: "Plan proposed.".into(),
            is_error: false,
        },
        StreamChunk::Text {
            content: "Final plan paragraph.".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let mut opts = make_dry_run_opts("plan refactor", OutputFormat::Text);
    opts.final_message_only = true;
    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    // Text is narrowed to final message after the tool block.
    assert!(
        out.contains("Final plan paragraph"),
        "stdout must contain final message"
    );
    assert!(
        !out.contains("Reasoning about the refactor"),
        "stdout must NOT contain earlier reasoning block"
    );
    // Attempted-tools line present.
    assert!(
        out.contains("Tools attempted: (none)"),
        "must render tools line"
    );
    // Read-only: no resume hint anywhere.
    assert!(!err.contains("Session saved"), "no resume hint in stderr");
    assert!(!err.contains("Resume with"), "no resume hint");
}

/// P0-13.1c-13.1b-regression — existing 13.1b tests pass; only sanctioned diffs are schema_version + fingerprint.
#[tokio::test]
async fn p0_13_1c_13_1b_regression() {
    // Re-run a representative 13.1b test to confirm the only change is schema_version
    let provider = Arc::new(ChunkProvider::new(vec![
        StreamChunk::Text {
            content: "regression check".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]));
    let opts = make_opts_with_format("test", OutputFormat::Json);
    let (exit, out, _) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::SUCCESS);

    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    // Schema version bumped
    assert_eq!(doc["schema_version"], "1.1");
    // All existing fields still present
    assert!(doc.get("response").is_some());
    assert!(doc.get("model").is_some());
    assert!(doc.get("stop_reason").is_some());
    assert!(doc.get("tool_calls").is_some());
    assert!(doc.get("session_id").is_some());
    assert!(doc.get("deny_count").is_some());
    // New fields present with correct defaults for non-dry-run
    assert_eq!(doc["dry_run"], false);
    assert!(doc["plan"].is_null());
    assert_eq!(doc["tools_would_use"], serde_json::json!([]));
}
// ================== Story 13.2: Offline Ask Tests ==================

struct OfflineAskProvider;

#[async_trait]
impl StreamingProvider for OfflineAskProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        Err(rustain::domain::errors::ProviderError::Offline(
            "connection refused".into(),
        ))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "offline-mock".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Err(rustain::domain::errors::ProviderError::Offline(
            "connection refused".into(),
        ))
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Err(rustain::domain::errors::ProviderError::Offline(
            "connection refused".into(),
        ))
    }
}

/// P0-13.2-AC3 — offline provider → specific error message on stderr, non-zero exit.
#[tokio::test]
async fn test_ask_offline_error() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(OfflineAskProvider);
    let opts = make_opts("test offline");

    let (exit, _out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::FAILURE, "offline should exit non-zero");
    let expected_msg =
        "✗ No provider available (offline). Use --dry-run for plan-only, or configure a local LLM.";
    assert!(
        err.contains(expected_msg),
        "stderr should contain AC3 message.\nActual stderr: {}",
        err
    );
}

/// P0-13.2-AC3-json — offline with `--output-format json` → structured ErrorOut on stdout.
#[tokio::test]
async fn test_ask_offline_error_json() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(OfflineAskProvider);
    let opts = make_opts_with_format("test offline", OutputFormat::Json);

    let (exit, out, err) = run_ask_core_test(provider, opts).await;
    assert_eq!(exit, ExitCode::FAILURE, "offline should exit non-zero");

    // stderr carries the human message
    let expected_msg =
        "✗ No provider available (offline). Use --dry-run for plan-only, or configure a local LLM.";
    assert!(
        err.contains(expected_msg),
        "stderr should contain AC3 message.\nActual stderr: {}",
        err
    );

    // stdout carries a valid JSON error envelope
    let doc: serde_json::Value =
        serde_json::from_str(&out).expect("json error must be parseable stdout");
    let error_msg = doc["error"]["message"]
        .as_str()
        .expect("error.message should exist");
    assert!(
        error_msg.contains("No provider available (offline)"),
        "JSON error message should contain offline text, got: {}",
        error_msg
    );
}

/// P0-13.2-dry-run-offline — `--dry-run` with an offline provider does not error
/// on the streaming path; the plan turn still receives the offline error since
/// stream_completion is called. This verifies dry-run follows the same offline path.
#[tokio::test]
async fn test_ask_offline_dry_run() {
    let provider: Arc<dyn StreamingProvider> = Arc::new(OfflineAskProvider);
    let opts = make_dry_run_opts("plan something", OutputFormat::Json);

    let (exit, out, err) = run_ask_core_test(provider, opts).await;

    // Dry-run still calls stream_completion, so it gets the offline error.
    // The offline message is emitted regardless of dry-run mode.
    // This is the expected behavior: offline blocks even dry-run since
    // the model is needed to generate the plan.
    assert_eq!(
        exit,
        ExitCode::FAILURE,
        "dry-run + offline should still fail"
    );
    let expected_msg = "No provider available (offline)";
    assert!(
        err.contains(expected_msg),
        "stderr should mention offline.\nActual stderr: {}",
        err
    );
    // If JSON output is present, verify it's valid
    if !out.trim().is_empty() {
        let _doc: serde_json::Value =
            serde_json::from_str(&out).expect("JSON output should be valid if present");
    }
}
