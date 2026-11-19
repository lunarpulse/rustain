use super::*;
use rustain::adapters::cli::ask::{AskOpts, run_ask_core};
use rustain::domain::errors::{ProviderError, StorageError, ToolError};
use rustain::domain::models::{
    AppConfig, ModelDescriptor, ToolDefinition, ToolResult, UsageLedgerEntry, UsageProvenance,
    pricing::PricingConfig,
};
use rustain::domain::ports::{LedgerReadMode, ProbeOutcome, ToolSetPort, UsageLedgerPort};
use serde_json::{Value, json};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{advance, timeout};
use tokio_util::sync::CancellationToken;

const LIMIT: Duration = Duration::from_secs(5);
// A virtual watchdog reports a stuck test; it is never the limit under test.
const WATCHDOG: Duration = Duration::from_secs(60);
const MODEL: &str = "eval5-interruption-fixture";
const PREFIX: &str = "first call text";
const DONE: &str = "continuation text";

type AskResult = (ExitCode, Vec<Value>, String);

struct Hold<T> {
    entered: oneshot::Sender<T>,
    release: oneshot::Receiver<()>,
}

impl<T> Hold<T> {
    fn new() -> (Self, oneshot::Receiver<T>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, held) = oneshot::channel();
        (
            Self {
                entered,
                release: held,
            },
            observed,
            release,
        )
    }

    async fn park(self, evidence: T) {
        assert!(self.entered.send(evidence).is_ok(), "observer lost");
        self.release
            .await
            .expect("test dropped the hold without releasing it");
    }
}

async fn at_hold<T>(
    mut run: Pin<&mut impl Future<Output = AskResult>>,
    entered: oneshot::Receiver<T>,
    phase: &str,
) -> T {
    tokio::select! {
        biased;
        result = run.as_mut() => panic!("ask returned before {phase} parked: {result:?}"),
        observed = timeout(WATCHDOG, entered) => observed
            .unwrap_or_else(|_| panic!("{phase} was never polled"))
            .unwrap_or_else(|_| panic!("{phase} dropped without being polled")),
    }
}

async fn settle_held(
    run: impl Future<Output = AskResult>,
    release: oneshot::Sender<()>,
    remaining: Duration,
    expires: bool,
    phase: &str,
) -> AskResult {
    let result = if expires {
        advance(remaining).await;
        // Allow timer-wheel rounding, not another provider/tool-sized budget.
        let result = timeout(Duration::from_millis(100), run)
            .await
            .unwrap_or_else(|_| panic!("ask did not retire {phase} at the run deadline"));
        assert!(release.is_closed(), "ask returned with {phase} still live");
        result
    } else {
        advance(remaining - Duration::from_secs(1)).await;
        release
            .send(())
            .unwrap_or_else(|_| panic!("{phase} retired before its deadline"));
        settled(run).await
    };
    terminal(&result, expires.then_some("max_duration"));
    result
}

async fn settled(run: impl Future<Output = AskResult>) -> AskResult {
    timeout(WATCHDOG, run)
        .await
        .expect("ask did not settle and return")
}

#[derive(Clone, Default)]
struct LiveOutput(Arc<Mutex<Vec<u8>>>);

impl LiveOutput {
    fn events(&self) -> Vec<Value> {
        parse_ndjson(std::str::from_utf8(&self.0.lock().unwrap()).unwrap())
    }
}

impl Write for LiveOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn run(
    provider: Arc<dyn StreamingProvider>,
    ledger: Arc<dyn UsageLedgerPort>,
    tools: Option<Arc<FixtureTools>>,
    opts: AskOpts,
    config: AppConfig,
    mut out: LiveOutput,
) -> AskResult {
    let mut core = build_test_core(provider.clone());
    core.ledger = ledger;
    if let Some(tools) = tools {
        core.tool_scheduler = ToolScheduler::new(
            core.security.clone(),
            tools.clone(),
            core.approval.clone(),
            16,
        );
        core.tools = tools;
    }
    let mut err = Cursor::new(Vec::new());
    let exit = run_ask_core(
        provider,
        core,
        opts,
        &config,
        Path::new("/tmp/test-ask"),
        &mut out,
        &mut err,
    )
    .await;
    (
        exit,
        out.events(),
        String::from_utf8(err.into_inner()).unwrap(),
    )
}

fn opts() -> AskOpts {
    let mut opts = make_opts_with_format("test execution limits", OutputFormat::StreamJson);
    opts.model_override = Some(MODEL.into());
    opts
}

fn pricing() -> AppConfig {
    let mut config = default_config();
    config.pricing.insert(
        MODEL.into(),
        PricingConfig {
            input_per_million: 1_000_000.0,
            output_per_million: 2_000_000.0,
            cache_creation_per_million: Some(1_500_000.0),
            cache_read_per_million: Some(250_000.0),
            reasoning_per_million: Some(3_000_000.0),
        },
    );
    config
}

fn text(content: &str) -> StreamChunk {
    StreamChunk::Text {
        content: content.into(),
        parent_tool_use_id: None,
    }
}

fn end() -> StreamChunk {
    StreamChunk::TurnComplete {
        stop_reason: StopReason::EndTurn,
    }
}

fn usage(input_tokens: u32, output_tokens: u32, counters_observed: bool) -> StreamChunk {
    StreamChunk::Usage {
        usage: UsageInfo {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
            counters_observed,
        },
        session_id: None,
    }
}

fn with_tool(usage: StreamChunk) -> Vec<StreamChunk> {
    vec![
        text(PREFIX),
        StreamChunk::ToolUse {
            id: "read-fixture".into(),
            name: "Read".into(),
            input: json!({"file_path": "fixture.txt"}),
        },
        usage,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

fn terminal(result: &AskResult, reason: Option<&str>) {
    let (exit, events, err) = result;
    let expected = if reason.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    };
    assert_eq!(*exit, expected, "{events:?}\n{err}");
    let last = events.last().expect("ask emitted no terminal event");
    assert_eq!(last["type"], "turn_complete", "{events:?}\n{err}");
    assert_eq!(last["schema_version"], "1.4", "{last}");
    assert_eq!(last["limit_reason"].as_str(), reason, "{last}\n{err}");
    if reason.is_none() {
        assert_eq!(last["stop_reason"], "end_turn", "{last}");
    }
    assert_eq!(count_events(events, "turn_complete"), 1, "{events:?}");
}

fn count_events(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|event| event["type"] == kind).count()
}

fn has_text(events: &[Value], content: &str) -> bool {
    events
        .iter()
        .any(|event| event["type"] == "text" && event["content"] == content)
}

fn observed(row: &UsageLedgerEntry, input: u32, output: u32) {
    assert_eq!(row.model, MODEL);
    assert_eq!(row.provenance, Some(UsageProvenance::Observed), "{row:?}");
    assert_eq!(
        (row.usage.tokens_in, row.usage.tokens_out),
        (input, output),
        "{row:?}"
    );
}

struct HeldProvider {
    script: ScriptedProvider,
    attempts: AtomicUsize,
    open: Mutex<Option<Hold<()>>>,
    stream_tail: Mutex<Option<Hold<()>>>,
}

impl HeldProvider {
    fn new(
        calls: Vec<Vec<StreamChunk>>,
        open: Option<Hold<()>>,
        stream_tail: Option<Hold<()>>,
    ) -> Self {
        Self {
            script: ScriptedProvider::new(calls),
            attempts: AtomicUsize::new(0),
            open: Mutex::new(open),
            stream_tail: Mutex::new(stream_tail),
        }
    }
}

#[async_trait]
impl StreamingProvider for HeldProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, ProviderError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let open = self.open.lock().unwrap().take();
        if let Some(open) = open {
            open.park(()).await;
        }
        let stream = self.script.stream_completion(messages, options).await?;
        let tail = self.stream_tail.lock().unwrap().take();
        Ok(match tail {
            Some(tail) => Box::pin(stream.chain(stream::once(async move {
                tail.park(()).await;
                end()
            }))),
            None => stream,
        })
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        self.script.abort().await
    }

    fn provider_id(&self) -> String {
        self.script.provider_id()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        self.script.list_models()
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        self.script.health_check().await
    }

    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        self.script.connectivity_probe().await
    }
}

#[derive(Default)]
struct FixtureTools {
    hold: Mutex<Option<Hold<()>>>,
    entered: AtomicUsize,
    completed: AtomicUsize,
}

impl FixtureTools {
    fn calls(&self) -> (usize, usize) {
        (
            self.entered.load(Ordering::Relaxed),
            self.completed.load(Ordering::Relaxed),
        )
    }
}

#[async_trait]
impl ToolSetPort for FixtureTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Read".into(),
            description: "Read a fixture without accessing the filesystem".into(),
            input_schema: json!({"type": "object"}),
            parallel_safe: false,
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        assert_eq!(name, "Read");
        assert_eq!(input["file_path"], "fixture.txt");
        self.entered.fetch_add(1, Ordering::Relaxed);
        let hold = self.hold.lock().unwrap().take();
        if let Some(hold) = hold {
            hold.park(()).await;
        }
        self.completed.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "fixture tool completed".into(),
            is_error: false,
            images: vec![],
        })
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_duration_retires_blocked_provider_open() {
    for expires in [false, true] {
        let (hold, entered, release) = Hold::new();
        let provider = Arc::new(HeldProvider::new(
            vec![vec![text(DONE), usage(1, 0, true), end()]],
            Some(hold),
            None,
        ));
        let ledger = Arc::new(RecordingLedger::default());
        let mut opts = opts();
        opts.limits.max_duration = Some(LIMIT);
        let run = run(
            provider.clone(),
            ledger.clone(),
            None,
            opts,
            default_config(),
            LiveOutput::default(),
        );
        tokio::pin!(run);
        at_hold(run.as_mut(), entered, "provider open").await;
        assert_eq!(provider.attempts.load(Ordering::Relaxed), 1);
        assert_eq!(provider.script.served(), 0, "open was not released");
        assert!(ledger.entries.lock().unwrap().is_empty());
        let result = settle_held(run.as_mut(), release, LIMIT, expires, "provider open").await;
        assert_eq!(
            provider.attempts.load(Ordering::Relaxed),
            1,
            "unexpected retry"
        );
        assert_eq!(provider.script.served(), usize::from(!expires));
        assert_eq!(has_text(&result.1, DONE), !expires);
        let rows = ledger.entries.lock().unwrap();
        assert_eq!(rows.len(), usize::from(!expires), "{rows:?}");
        if let Some(row) = rows.first() {
            observed(row, 1, 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_interrupted_stream_settles_the_newest_observed_snapshot_once() {
    for expires in [false, true] {
        let (hold, entered, release) = Hold::new();
        let mut chunks = vec![text(PREFIX), usage(2, 1, true), usage(7, 3, true)];
        if expires {
            // The more recent unproved frame must not replace the latest observed counters.
            chunks.push(usage(99, 0, false));
        }
        let provider = Arc::new(HeldProvider::new(vec![chunks], None, Some(hold)));
        let ledger = Arc::new(RecordingLedger::default());
        let mut opts = opts();
        opts.limits.max_duration = Some(LIMIT);
        let run = run(
            provider.clone(),
            ledger.clone(),
            None,
            opts,
            default_config(),
            LiveOutput::default(),
        );
        tokio::pin!(run);
        at_hold(run.as_mut(), entered, "stream tail after usage snapshots").await;
        assert_eq!(provider.script.served(), 1);
        assert!(
            ledger.entries.lock().unwrap().is_empty(),
            "call has not settled yet"
        );
        let result = settle_held(run.as_mut(), release, LIMIT, expires, "stream").await;
        assert!(
            has_text(&result.1, PREFIX),
            "already observed text was lost"
        );
        assert_eq!(count_events(&result.1, "call_usage"), 1, "{:?}", result.1);
        assert_eq!(provider.attempts.load(Ordering::Relaxed), 1);
        let rows = ledger.entries.lock().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the admitted call must settle once: {rows:?}"
        );
        observed(&rows[0], 7, 3);
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_interrupted_unknown_usage_requires_a_money_limit_receipt() {
    for (mode, turns, cost, daily) in [
        ("duration only", None, None, None),
        ("duration and turns", NonZeroU64::new(1), None, None),
        ("duration and cost", None, Some(10.0), None),
        ("duration and daily", None, None, Some(10.0)),
    ] {
        let (hold, entered, release) = Hold::new();
        let provider = Arc::new(HeldProvider::new(
            vec![vec![text(PREFIX), usage(99, 0, false)]],
            None,
            Some(hold),
        ));
        let ledger = Arc::new(RecordingLedger::default());
        let mut opts = opts();
        opts.limits.max_duration = Some(LIMIT);
        opts.limits.max_turns = turns;
        opts.limits.max_cost_usd = cost;
        let mut config = pricing();
        config.budget.daily_limit_usd = daily;
        let run = run(
            provider.clone(),
            ledger.clone(),
            None,
            opts,
            config,
            LiveOutput::default(),
        );
        tokio::pin!(run);
        at_hold(run.as_mut(), entered, mode).await;
        let result = settle_held(run.as_mut(), release, LIMIT, true, mode).await;
        assert_eq!(provider.attempts.load(Ordering::Relaxed), 1, "{mode}");
        assert!(has_text(&result.1, PREFIX), "{mode}: lost observed text");
        let rows = ledger.entries.lock().unwrap();
        assert_eq!(
            rows.len(),
            usize::from(cost.is_some() || daily.is_some()),
            "{mode}: {rows:?}"
        );
        if let Some(row) = rows.first() {
            assert_eq!(
                row.provenance,
                Some(UsageProvenance::Unknown),
                "{mode}: {row:?}"
            );
            assert_eq!(
                (row.usage.tokens_in, row.usage.tokens_out),
                (0, 0),
                "{mode}: unproved counts: {row:?}"
            );
        }
    }
}

fn normalized_usage(counts: (u32, u32, u32, u32, u32)) -> UsageInfo {
    let (input, output, written, read, reasoning) = counts;
    UsageInfo {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: Some(written),
        cache_read_input_tokens: Some(read),
        reasoning_tokens: Some(reasoning),
        counters_observed: true,
    }
}

async fn held_attempts(
    chunks: Vec<StreamChunk>,
    opts: AskOpts,
    config: AppConfig,
    expires: bool,
    phase: &str,
) -> (AskResult, Vec<UsageLedgerEntry>) {
    let (hold, entered, release) = Hold::new();
    let provider = Arc::new(HeldProvider::new(vec![chunks], None, Some(hold)));
    let ledger = Arc::new(RecordingLedger::default());
    let run = run(
        provider.clone(),
        ledger.clone(),
        None,
        opts,
        config,
        LiveOutput::default(),
    );
    tokio::pin!(run);
    at_hold(run.as_mut(), entered, phase).await;
    assert!(ledger.entries.lock().unwrap().is_empty(), "{phase}");
    let result = if expires {
        settle_held(run.as_mut(), release, LIMIT, true, phase).await
    } else {
        release.send(()).unwrap();
        settled(run).await
    };
    assert_eq!(provider.attempts.load(Ordering::Relaxed), 1, "{phase}");
    let rows = std::mem::take(&mut *ledger.entries.lock().unwrap());
    (result, rows)
}

#[tokio::test(start_paused = true)]
async fn eval5_retry_spend_reaches_each_ceiling_without_changing_display() {
    // The abandoned attempt costs $19; the displayed attempt costs $24.
    for (invocation, daily, reason) in [
        (Some(43.0), None, Some("max_cost")),
        (None, Some(43.0), Some("daily_budget")),
        (Some(44.0), None, None),
        (None, Some(44.0), None),
        (None, None, None),
    ] {
        let prior = normalized_usage((2, 3, 6, 4, 1));
        let final_usage = normalized_usage((7, 5, 2, 8, 2));
        let mut display = final_usage.clone();
        // Some transports prove the current display only via AccountingUsage.
        display.counters_observed = daily.is_some();
        let chunks = vec![
            StreamChunk::AccountingUsage {
                usage: prior.clone(),
            },
            StreamChunk::AccountingRetry { usage: Some(prior) },
            text(DONE),
            StreamChunk::Usage {
                usage: display,
                session_id: None,
            },
            StreamChunk::AccountingUsage { usage: final_usage },
        ];
        let mut opts = opts();
        opts.limits.max_cost_usd = invocation;
        let mut config = pricing();
        config.budget.daily_limit_usd = daily;
        let (result, rows) = held_attempts(chunks, opts, config, false, "two attempts").await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        if invocation.is_some() || daily.is_some() {
            terminal(&result, reason);
            observed(row, 9, 8);
            assert_eq!(row.usage.cache_creation_tokens, Some(8));
            assert_eq!(row.usage.cache_read_tokens, Some(12));
            assert_eq!(row.usage.reasoning_tokens, Some(3));
        } else {
            assert_eq!(result.0, ExitCode::SUCCESS);
            assert_eq!(result.1.last().unwrap()["schema_version"], "1.3");
            assert_eq!(row.provenance, Some(UsageProvenance::Unknown));
            assert_eq!((row.usage.tokens_in, row.usage.tokens_out), (7, 5));
            assert_eq!(row.usage.cache_creation_tokens, Some(2));
            assert_eq!(row.usage.cache_read_tokens, Some(8));
            assert_eq!(row.usage.reasoning_tokens, Some(2));
        }
        assert!(has_text(&result.1, DONE));
        assert_eq!(count_events(&result.1, "call_usage"), 1);
        let displayed = result.1.iter().find(|e| e["type"] == "call_usage").unwrap();
        assert_eq!(displayed["usage"]["input_tokens"], 7);
        assert_eq!(displayed["usage"]["output_tokens"], 5);
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_retry_unknown_or_overflowed_attempt_prevents_a_proved_total() {
    for (phase, prior, final_observed) in [
        ("unproved abandoned attempt", None, true),
        ("unproved final attempt", Some(2), false),
        ("unrepresentable sum", Some(u32::MAX), true),
    ] {
        let mut opts = opts();
        opts.limits.max_cost_usd = Some(100.0);
        let chunks = vec![
            StreamChunk::AccountingUsage {
                usage: normalized_usage((2, 3, 0, 0, 0)),
            },
            StreamChunk::AccountingRetry {
                usage: prior.map(|input| normalized_usage((input, 3, 0, 0, 0))),
            },
            text(DONE),
            usage(1, 0, final_observed),
        ];
        let (result, rows) = held_attempts(chunks, opts, pricing(), false, phase).await;
        terminal(&result, Some("accounting_error"));
        assert_eq!(rows.len(), 1, "{phase}: {rows:?}");
        assert_eq!(
            rows[0].provenance,
            Some(UsageProvenance::Unknown),
            "{phase}"
        );
        assert_eq!(rows[0].usage.tokens_in, 1, "{phase}: no saturated total");
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_retry_cancellation_distinguishes_not_started_from_unproved() {
    for (phase, started, final_proved, money) in [
        ("still in retry backoff", false, false, true),
        ("replacement opened without proof", true, false, true),
        (
            "unproved replacement without a money limit",
            true,
            false,
            false,
        ),
        ("replacement reported explicit zero", true, true, true),
    ] {
        let prior = normalized_usage((2, 3, 0, 0, 0));
        let mut chunks = vec![StreamChunk::AccountingUsage {
            usage: prior.clone(),
        }];
        if started {
            chunks.push(StreamChunk::AccountingRetry { usage: Some(prior) });
        }
        if final_proved {
            chunks.push(StreamChunk::AccountingUsage {
                usage: normalized_usage((0, 0, 0, 0, 0)),
            });
        }
        let mut opts = opts();
        opts.limits.max_duration = Some(LIMIT);
        opts.limits.max_cost_usd = money.then_some(100.0);
        let (_, rows) = held_attempts(chunks, opts, pricing(), true, phase).await;
        assert_eq!(rows.len(), usize::from(money), "{phase}: {rows:?}");
        if let Some(row) = rows.first() {
            if !started || final_proved {
                observed(row, 2, 3);
            } else {
                assert_eq!(row.provenance, Some(UsageProvenance::Unknown), "{phase}");
                assert_eq!((row.usage.tokens_in, row.usage.tokens_out), (0, 0));
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_duration_retires_a_dispatched_tool_without_continuation() {
    const OPEN_TIME: Duration = Duration::from_secs(2);
    for expires in [false, true] {
        let (hold, entered, release) = Hold::new();
        let tools = Arc::new(FixtureTools {
            hold: Mutex::new(Some(hold)),
            ..Default::default()
        });
        let (open_hold, opening, open_release) = Hold::new();
        let provider = Arc::new(HeldProvider::new(
            vec![
                with_tool(usage(1, 2, true)),
                vec![text(DONE), usage(0, 0, true), end()],
            ],
            Some(open_hold),
            None,
        ));
        let ledger = Arc::new(RecordingLedger::default());
        let mut opts = opts();
        opts.limits.max_duration = Some(LIMIT);
        opts.limits.max_turns = NonZeroU64::new(2);
        let run = run(
            provider.clone(),
            ledger.clone(),
            Some(tools.clone()),
            opts,
            default_config(),
            LiveOutput::default(),
        );
        tokio::pin!(run);
        at_hold(run.as_mut(), opening, "provider open before tool").await;
        advance(OPEN_TIME).await;
        open_release
            .send(())
            .expect("provider should have time left");
        at_hold(run.as_mut(), entered, "real scheduler's tool dispatch").await;
        assert_eq!(tools.calls(), (1, 0));
        assert_eq!(provider.script.served(), 1, "tool has not returned");
        assert_eq!(
            ledger.entries.lock().unwrap().len(),
            1,
            "provider settled before dispatch"
        );
        let result = settle_held(
            run.as_mut(),
            release,
            LIMIT - OPEN_TIME,
            expires,
            "tool future",
        )
        .await;
        assert!(has_text(&result.1, PREFIX));
        assert_eq!(has_text(&result.1, DONE), !expires);
        assert_eq!(tools.calls(), (1, usize::from(!expires)));
        let expected_calls = if expires { 1 } else { 2 };
        assert_eq!(provider.script.served(), expected_calls);
        assert_eq!(provider.attempts.load(Ordering::Relaxed), expected_calls);
        let rows = ledger.entries.lock().unwrap();
        assert_eq!(
            rows.len(),
            expected_calls,
            "no receipt for a denied continuation"
        );
        observed(&rows[0], 1, 2);
    }
}

/// Test-only ledger with a real on-disk commit point. Observe an append before
/// accepting or rejecting it, then let strict snapshots read the actual file.
struct GatedLedger {
    file: tempfile::NamedTempFile,
    first: Mutex<Option<Hold<UsageLedgerEntry>>>,
    fail_first: bool,
    snapshots: Mutex<Vec<usize>>,
    attempts: AtomicUsize,
}

impl GatedLedger {
    fn rows(&self) -> Vec<UsageLedgerEntry> {
        let data = std::fs::read_to_string(self.file.path()).expect("read fixture JSONL");
        data.lines()
            .map(|line| serde_json::from_str(line).expect("durable receipt JSON"))
            .collect()
    }
}

#[async_trait]
impl UsageLedgerPort for GatedLedger {
    async fn append(&self, row: UsageLedgerEntry) -> Result<(), StorageError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let first = self.first.lock().unwrap().take();
        if let Some(first) = first {
            first.park(row.clone()).await;
            if self.fail_first {
                return Err(StorageError::IoError("fixture append failed".into()));
            }
        }
        // Synchronous fixture I/O introduces no additional virtual-clock waits.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.file.path())
            .unwrap();
        serde_json::to_writer(&mut file, &row).unwrap();
        writeln!(file).unwrap();
        file.sync_data().unwrap();
        Ok(())
    }

    async fn read_session(&self, session_id: &str) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        Ok(self
            .rows()
            .into_iter()
            .filter(|row| row.session_id == session_id)
            .collect())
    }

    async fn read_since(
        &self,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        let LedgerReadMode::Strict { end_unix_ms } = mode else {
            panic!("ask daily accounting must be strict");
        };
        let rows: Vec<_> = self
            .rows()
            .into_iter()
            .filter(|row| (since_unix_ms..end_unix_ms).contains(&row.timestamp_ms))
            .collect();
        self.snapshots.lock().unwrap().push(rows.len());
        Ok(rows)
    }
}

#[tokio::test(start_paused = true)]
async fn eval5_daily_append_is_a_settlement_barrier() {
    // 2 uncached input × $1, 3 cache writes × $1.50, 4 cache hits × $0.25,
    // 1 ordinary output × $2, and 1 reasoning token × $3.
    const EXACT: f64 = 12.50;
    for (case, daily, invocation, turns, fails, expires, reason) in [
        ("room after settlement", 14.0, None, 2, false, false, None),
        (
            "exact daily",
            EXACT,
            None,
            2,
            false,
            false,
            Some("daily_budget"),
        ),
        (
            "cost vs turns",
            14.0,
            Some(EXACT),
            1,
            false,
            false,
            Some("max_cost"),
        ),
        (
            "append error",
            EXACT,
            None,
            2,
            true,
            false,
            Some("accounting_error"),
        ),
        (
            "expiry vs daily",
            EXACT,
            None,
            2,
            false,
            true,
            Some("max_duration"),
        ),
        (
            "expiry vs error",
            EXACT,
            None,
            2,
            true,
            true,
            Some("max_duration"),
        ),
    ] {
        let mixed = StreamChunk::Usage {
            usage: UsageInfo {
                input_tokens: 2,
                output_tokens: 2,
                cache_creation_input_tokens: Some(3),
                cache_read_input_tokens: Some(4),
                reasoning_tokens: Some(1),
                counters_observed: true,
            },
            session_id: None,
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            with_tool(mixed),
            vec![text(DONE), usage(0, 0, true), end()],
        ]));
        let tools = Arc::new(FixtureTools::default());
        let (hold, entered, release) = Hold::new();
        let ledger = Arc::new(GatedLedger {
            file: tempfile::NamedTempFile::new().unwrap(),
            first: Mutex::new(Some(hold)),
            fail_first: fails,
            snapshots: Mutex::new(Vec::new()),
            attempts: AtomicUsize::new(0),
        });
        let mut config = pricing();
        config.budget.daily_limit_usd = Some(daily);
        let mut opts = opts();
        opts.limits.max_duration = expires.then_some(LIMIT);
        opts.limits.max_cost_usd = invocation;
        opts.limits.max_turns = NonZeroU64::new(turns);
        let output = LiveOutput::default();
        let run = run(
            provider.clone(),
            ledger.clone(),
            Some(tools.clone()),
            opts,
            config,
            output.clone(),
        );
        tokio::pin!(run);
        let observed_row = at_hold(run.as_mut(), entered, case).await;
        observed(&observed_row, 2, 2);
        advance(if expires { LIMIT } else { Duration::ZERO }).await;
        assert!(
            futures::poll!(run.as_mut()).is_pending(),
            "{case}: ask returned with append pending"
        );
        assert_eq!(provider.served(), 1, "{case}: early continuation");
        assert_eq!(tools.calls(), (0, 0), "{case}: early tool dispatch");
        assert!(
            ledger.rows().is_empty(),
            "{case}: row committed before release"
        );
        assert_eq!(
            *ledger.snapshots.lock().unwrap(),
            [0],
            "{case}: early budget snapshot"
        );
        assert_eq!(
            count_events(&output.events(), "call_usage"),
            0,
            "{case}: premature call boundary"
        );
        release
            .send(())
            .expect("append was dropped instead of settling");
        let result = settled(run.as_mut()).await;
        terminal(&result, reason);
        assert!(has_text(&result.1, PREFIX), "{case}: lost observed output");
        let continued = reason.is_none();
        assert_eq!(has_text(&result.1, DONE), continued, "{case}");
        let additional = usize::from(continued);
        assert_eq!(provider.served(), 1 + additional, "{case}");
        assert_eq!(tools.calls(), (additional, additional), "{case}");
        assert_eq!(
            ledger.attempts.load(Ordering::Relaxed),
            1 + additional,
            "{case}"
        );
        let rows = ledger.rows();
        assert_eq!(
            rows.len(),
            usize::from(!fails) + additional,
            "{case}: {rows:?}"
        );
        if let Some(row) = rows.first() {
            observed(row, 2, 2);
            assert_eq!(row.usage.cache_creation_tokens, Some(3));
            assert_eq!(row.usage.cache_read_tokens, Some(4));
            assert_eq!(row.usage.reasoning_tokens, Some(1));
        }
        if continued {
            observed(&rows[1], 0, 0);
        }
        if !expires && !fails {
            let snapshots = ledger.snapshots.lock().unwrap();
            assert!(
                snapshots.windows(2).any(|w| w == [0, 1]),
                "{case}: boundary did not read the single committed row: {snapshots:?}"
            );
        }
    }
}
