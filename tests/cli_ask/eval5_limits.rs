use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Output, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const EVAL5_MODEL: &str = "eval5-fixture-model";
const EVAL5_CANARY: &str = "local fixture write\n";
const EVAL5_PREFIX: &str = "before the boundary";
const EVAL5_FINAL: &str = "after the boundary";

struct Eval5Ask {
    root: tempfile::TempDir,
    server: MockServer,
    chat_path: &'static str,
}

impl Eval5Ask {
    async fn ollama(priced: bool, daily: Option<u32>) -> Self {
        Self::new("ollama", "/api/chat", "/api/tags", "", priced, daily).await
    }

    async fn new(
        kind: &str,
        chat_path: &'static str,
        readiness_path: &str,
        base_suffix: &str,
        priced: bool,
        daily: Option<u32>,
    ) -> Self {
        // Outside the system temp dir: the sandbox's /tmp writable root must
        // not contain HOME, or `--yolo` is refused as unbounded (Story 23.1).
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let server = MockServer::start().await;
        for dir in ["home", "workspace", "config", "data"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        // Both adapters are keyless; discovery is disabled and even a readiness
        // request is loopback and not a billable chat admission.
        let base_url = format!("{}{base_suffix}", server.uri());
        let mut config = format!(
            "model = {EVAL5_MODEL:?}\n[provider.{kind}]\nprovider_id = {kind:?}\n\
             model_id = {EVAL5_MODEL:?}\napi_key_env = \"\"\nsupports_tools = true\n\
             discover_models = false\nbase_url = {base_url:?}\n",
        );
        if priced {
            config.push_str(&format!(
                "[pricing.{EVAL5_MODEL}]\ninput_per_million = 1000000.0\n\
                 output_per_million = 2000000.0\ncache_read_per_million = 250000.0\n\
                 reasoning_per_million = 3000000.0\n"
            ));
        }
        if let Some(daily) = daily {
            config.push_str(&format!("[budget]\ndaily_limit_usd = {daily}.0\n"));
        }
        fs::write(root.path().join("config/config.toml"), config).unwrap();
        Mock::given(method("GET"))
            .and(path(readiness_path))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"models": [], "data": []})),
            )
            .mount(&server)
            .await;
        Self {
            root,
            server,
            chat_path,
        }
    }

    async fn script(&self, mime: &str, replies: impl IntoIterator<Item = String>) {
        for reply in replies {
            // Equal-priority mocks are tried in insertion order. Every POST,
            // even an unexpected one past the script, stays in the transcript.
            Mock::given(method("POST"))
                .and(path(self.chat_path))
                .respond_with(ResponseTemplate::new(200).set_body_raw(reply, mime))
                .up_to_n_times(1)
                .mount(&self.server)
                .await;
        }
    }

    async fn run(&self, format: &str, flags: &[&str]) -> Output {
        let root = self.root.path();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rustain"));
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("RUSTAIN_CONFIG_DIR", root.join("config"))
            .env("RUSTAIN_DATA_DIR", root.join("data"))
            .env("TZ", "UTC")
            .current_dir(root.join("workspace"))
            .args([
                "ask",
                "Exercise the local fixture",
                "--yolo",
                "--output-format",
                format,
            ])
            .args(flags)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .expect("fixture CLI did not exit")
            .expect("launch fixture CLI")
    }

    async fn assert_calls(&self, count: usize) {
        let requests = self.server.received_requests().await.unwrap();
        let calls: Vec<_> = requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == self.chat_path
            })
            .collect();
        assert_eq!(calls.len(), count, "actual chat POSTs: {calls:?}");
        for (index, call) in calls.iter().enumerate() {
            let body: Value = serde_json::from_slice(&call.body).unwrap();
            assert_eq!(body["model"], EVAL5_MODEL, "{body}");
            assert_eq!(body["stream"], true, "{body}");
            if index > 0 {
                let tool_results = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|msg| {
                        msg["role"] == "tool"
                            && msg["content"]
                                .as_str()
                                .is_some_and(|content| content.starts_with("Successfully wrote "))
                    })
                    .count();
                assert_eq!(
                    tool_results, index,
                    "continuation must carry real tool results: {body}"
                );
            }
        }
    }

    fn assert_canary(&self, name: &str, written: bool) {
        let read = fs::read_to_string(self.root.path().join("workspace").join(name));
        if written {
            assert_eq!(read.unwrap(), EVAL5_CANARY);
        } else {
            assert_eq!(
                read.unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "{name}"
            );
        }
    }

    fn ledger_dir(&self) -> PathBuf {
        self.root.path().join("data/usage")
    }

    fn seed(&self, rows: &str) -> PathBuf {
        let dir = self.ledger_dir();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("prior.jsonl");
        fs::write(&file, rows).unwrap();
        file
    }

    fn new_receipts(&self, call_count: usize) -> Vec<Value> {
        let dir = self.ledger_dir();
        let mut rows = Vec::new();
        if dir.is_dir() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|ext| ext == "jsonl")
                    && path.file_name().unwrap() != "prior.jsonl"
                {
                    let contents = fs::read_to_string(path).unwrap();
                    rows.extend(parse_ndjson(&contents));
                }
            }
        }
        assert_eq!(
            rows.len(),
            call_count,
            "unexpected ledger receipts: {rows:?}"
        );
        rows
    }
}

fn eval5_write_args(canary: &str) -> String {
    json!({"file_path": canary, "content": EVAL5_CANARY}).to_string()
}

fn eval5_ollama_reply(
    text: &str,
    canary: Option<&str>,
    prompt: Option<u32>,
    output: u32,
) -> String {
    let mut response = json!({
        "model": EVAL5_MODEL, "done": true, "done_reason": "stop",
        "message": {"role": "assistant", "content": text}, "eval_count": output,
    });
    if let Some(prompt) = prompt {
        response["prompt_eval_count"] = json!(prompt);
    }
    if let Some(canary) = canary {
        response["done_reason"] = json!("tool_calls");
        response["message"]["tool_calls"] = json!([{
            "id": canary, "type": "function",
            "function": {"name": "Write", "arguments": eval5_write_args(canary)},
        }]);
    }
    format!("{response}\n")
}

fn eval5_assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn eval5_assert_json(output: &Output, version: &str, reason: Option<&str>, stop: &str) -> Value {
    eval5_assert_exit(output, i32::from(reason.is_some()));
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["schema_version"], version, "{doc}");
    assert_eq!(
        doc.get("limit_reason").and_then(Value::as_str),
        reason,
        "{doc}"
    );
    assert_eq!(doc["stop_reason"], stop, "{doc}");
    assert!(doc.get("error").is_none(), "{doc}");
    if reason.is_none() {
        assert!(doc.get("limit_reason").is_none(), "{doc}");
    }
    doc
}

fn eval5_assert_stream(
    output: &Output,
    version: &str,
    reason: Option<&str>,
    stop: &str,
    saw_text: bool,
) {
    eval5_assert_exit(output, i32::from(reason.is_some()));
    let events = parse_ndjson(std::str::from_utf8(&output.stdout).unwrap());
    let (terminal, earlier) = events.split_last().expect("structured terminal event");
    assert_eq!(terminal["type"], "turn_complete", "{events:?}");
    if let Some(reason) = reason {
        assert_eq!(terminal["limit_reason"], reason, "{events:?}");
    } else {
        assert!(terminal.get("limit_reason").is_none(), "{events:?}");
    }
    assert_eq!(terminal["stop_reason"], stop, "{events:?}");
    for event in &events {
        assert_eq!(event["schema_version"], version, "{event}");
    }
    assert!(
        earlier.iter().all(|event| {
            event["type"] != "turn_complete"
                && event["type"] != "error"
                && event.get("limit_reason").is_none()
        }),
        "contradictory terminal output: {events:?}"
    );
    assert_eq!(
        earlier.iter().any(|event| event["content"] == EVAL5_PREFIX),
        saw_text,
        "{events:?}"
    );
    assert_eq!(
        earlier.iter().any(|event| event["content"] == EVAL5_FINAL),
        reason.is_none(),
        "{events:?}"
    );
}

#[tokio::test]
async fn eval5_cli_limit_arguments_are_validated_before_ask_execution() {
    let fixture = Eval5Ask::ollama(true, None).await;
    fixture
        .script(
            "application/x-ndjson",
            [eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0)],
        )
        .await;
    for invalid in [
        "--max-turns=0",
        "--max-turns=-1",
        "--max-turns=1.5",
        "--max-turns=18446744073709551616",
        "--max-duration=0",
        "--max-duration=-1",
        "--max-duration=NaN",
        "--max-duration=inf",
        "--max-duration=seconds",
        "--max-duration=18446744073709551616",
        "--max-duration=1e9999",
        "--max-cost=0",
        "--max-cost=-1",
        "--max-cost=NaN",
        "--max-cost=inf",
        "--max-cost=USD",
        "--max-cost=1e9999",
    ] {
        let output = fixture.run("json", &[invalid]).await;
        eval5_assert_exit(&output, 2);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid value") && output.stdout.is_empty(),
            "{invalid}: {stderr}"
        );
    }
    fixture.assert_calls(0).await;
    fixture.new_receipts(0);
    let output = fixture
        .run(
            "json",
            &["--max-turns=1", "--max-duration=60", "--max-cost=0.01"],
        )
        .await;
    let json = eval5_assert_json(&output, "1.4", None, "end_turn");
    assert_eq!(json["response"], EVAL5_FINAL);
    fixture.assert_calls(1).await;
    fixture.new_receipts(1);
}

#[tokio::test]
async fn eval5_ask_setup_errors_preserve_structured_shape_across_versions() {
    let fixture = Eval5Ask::ollama(true, None).await;
    let missing = "eval5-missing-context.txt";
    for format in ["json", "stream-json"] {
        let mut inactive_fields = None;
        for (flags, version) in [
            (&["--file", missing][..], "1.3"),
            (&["--file", missing, "--max-turns=1"][..], "1.4"),
        ] {
            let output = fixture.run(format, flags).await;
            eval5_assert_exit(&output, 1);
            let doc: Value = if format == "json" {
                let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert!(doc.get("response").is_some_and(Value::is_null), "{doc}");
                doc
            } else {
                let events = parse_ndjson(std::str::from_utf8(&output.stdout).unwrap());
                let [event]: [Value; 1] = events.try_into().expect("one setup error event");
                assert_eq!(event["type"], "error", "{event}");
                assert!(event.get("stop_reason").is_none(), "{event}");
                event
            };
            assert_eq!(doc["schema_version"], version, "{doc}");
            assert!(doc.get("limit_reason").is_none(), "{doc}");
            let error = doc["error"].as_object().expect("structured setup error");
            assert_eq!(error.len(), 1, "{doc}");
            assert!(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| {
                        message.contains("cannot read file") && message.contains(missing)
                    }),
                "{doc}"
            );
            let mut fields = std::collections::BTreeSet::new();
            collect_field_paths(&doc, "", &mut fields);
            if let Some(inactive_fields) = &inactive_fields {
                assert_eq!(&fields, inactive_fields, "{format} setup error fields");
            } else {
                inactive_fields = Some(fields);
            }
        }
    }
    fixture.assert_calls(0).await;
    fixture.new_receipts(0);
}

#[tokio::test]
async fn eval5_ask_max_turns_stops_before_next_provider_call() {
    let mut active_fields = None;
    let mut inactive_fields = None;
    for (format, flags, request_second_tool, calls, reason) in [
        ("json", &["--max-turns=2"][..], false, 2, None),
        ("json", &["--max-turns=2"][..], true, 2, Some("max_turns")),
        ("json", &["--max-turns=3"][..], true, 3, None),
        ("json", &[][..], true, 3, None),
        ("stream-json", &["--max-turns=3"][..], true, 3, None),
        ("stream-json", &[][..], true, 3, None),
    ] {
        let fixture = Eval5Ask::ollama(true, None).await;
        let second = if request_second_tool {
            eval5_ollama_reply(EVAL5_PREFIX, Some("second.txt"), Some(0), 0)
        } else {
            eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0)
        };
        fixture
            .script(
                "application/x-ndjson",
                [
                    eval5_ollama_reply(EVAL5_PREFIX, Some("first.txt"), Some(1), 0),
                    second,
                    eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0),
                ],
            )
            .await;
        let output = fixture.run(format, flags).await;
        let stop = if reason.is_some() {
            "tool_use"
        } else {
            "end_turn"
        };
        let version = if flags.is_empty() { "1.3" } else { "1.4" };
        if format == "json" {
            let doc = eval5_assert_json(&output, version, reason, stop);
            let response = doc["response"].as_str().unwrap();
            assert!(response.contains(EVAL5_PREFIX), "{doc}");
            assert_eq!(response.contains(EVAL5_FINAL), reason.is_none(), "{doc}");
            if request_second_tool && reason.is_none() {
                let mut fields = std::collections::BTreeSet::new();
                collect_field_paths(&doc, "", &mut fields);
                if flags.is_empty() {
                    inactive_fields = Some(fields);
                } else {
                    active_fields = Some(fields);
                }
            }
        } else {
            eval5_assert_stream(&output, version, reason, stop, true);
        }
        fixture.assert_calls(calls).await;
        fixture.assert_canary("first.txt", true);
        fixture.assert_canary("second.txt", calls == 3);
        fixture.new_receipts(calls);
    }
    assert_eq!(
        inactive_fields.expect("v1.3 ordinary JSON fields"),
        active_fields.expect("v1.4 ordinary JSON fields"),
        "a nonfiring limit must not change the ordinary JSON field set"
    );
}

#[cfg(feature = "openai")]
#[tokio::test]
async fn eval5_ask_cost_limit_stops_after_mixed_cache_call() {
    // 4 uncached input × $1, 4 cached × $0.25, 2 ordinary output × $2,
    // 2 reasoning × $3 = $15 exactly. Ollama does not carry cache counters.
    let chunk = json!({"choices": [{"index": 0, "delta": {
        "content": EVAL5_PREFIX, "tool_calls": [{"index": 0, "id": "write-canary",
            "type": "function", "function": {"name": "Write", "arguments": eval5_write_args("cost.txt")}}],
    }, "finish_reason": "tool_calls"}]});
    let usage = json!({"choices": [], "usage": {"prompt_tokens": 8, "completion_tokens": 4,
        "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 2}}});
    let first = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    let chunk = json!({"choices": [{"index": 0, "delta": {"content": EVAL5_PREFIX}, "finish_reason": "stop"}]});
    let final_at_limit = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    let chunk = json!({"choices": [{"index": 0, "delta": {"content": EVAL5_FINAL}, "finish_reason": "stop"}]});
    let usage = json!({"choices": [], "usage": {"prompt_tokens": 0, "completion_tokens": 0}});
    let last = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");

    for (format, flags, reply, blocked) in [
        ("json", &["--max-cost=16"][..], &first, false),
        (
            "stream-json",
            &["--max-cost=15", "--max-turns=1"][..],
            &first,
            true,
        ),
        ("text", &["--max-cost=14"][..], &first, true),
        ("json", &["--max-cost=15"][..], &final_at_limit, true),
    ] {
        let fixture = Eval5Ask::new(
            "openai-compatible",
            "/v1/chat/completions",
            "/v1/models",
            "/v1",
            true,
            None,
        )
        .await;
        fixture
            .script("text/event-stream", [reply.clone(), last.clone()])
            .await;
        let output = fixture.run(format, flags).await;
        match format {
            "stream-json" => {
                eval5_assert_stream(&output, "1.4", Some("max_cost"), "tool_use", true)
            }
            "text" => {
                eval5_assert_exit(&output, 1);
                let text = String::from_utf8(output.stdout).unwrap();
                let err = String::from_utf8(output.stderr).unwrap();
                assert!(
                    text.contains(EVAL5_PREFIX) && !text.contains(EVAL5_FINAL),
                    "{text}"
                );
                assert!(err.contains("Ask stopped: max_cost"), "{err}");
            }
            "json" if blocked => {
                let doc = eval5_assert_json(&output, "1.4", Some("max_cost"), "end_turn");
                assert_eq!(doc["response"], EVAL5_PREFIX, "{doc}");
            }
            _ => {
                let doc = eval5_assert_json(&output, "1.4", None, "end_turn");
                assert!(
                    doc["response"].as_str().unwrap().contains(EVAL5_FINAL),
                    "{doc}"
                );
            }
        }
        let calls = if blocked { 1 } else { 2 };
        fixture.assert_calls(calls).await;
        fixture.assert_canary("cost.txt", !blocked);
        let receipts = fixture.new_receipts(calls);
        assert!(
            receipts.iter().any(|row| row["provenance"] == "observed"
                && row["usage"]["tokensIn"] == 4
                && row["usage"]["cacheReadTokens"] == 4
                && row["usage"]["tokensOut"] == 4
                && row["usage"]["reasoningTokens"] == 2),
            "{receipts:?}"
        );
    }
}

#[tokio::test]
async fn eval5_ask_ollama_missing_counter_is_not_observed_zero() {
    let cost = &["--max-cost=2"][..];
    for (priced, prompt, flags, reason) in [
        (true, Some(0), cost, None),
        (true, None, cost, Some("accounting_error")),
        (false, Some(0), cost, Some("accounting_error")),
        (
            true,
            None,
            &["--max-cost=2", "--max-turns=1"][..],
            Some("accounting_error"),
        ),
    ] {
        let fixture = Eval5Ask::ollama(priced, None).await;
        fixture
            .script(
                "application/x-ndjson",
                [
                    eval5_ollama_reply(EVAL5_PREFIX, Some("zero.txt"), prompt, 0),
                    eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0),
                ],
            )
            .await;
        let output = fixture.run("json", flags).await;
        let allowed = reason.is_none();
        let doc = eval5_assert_json(
            &output,
            "1.4",
            reason,
            if allowed { "end_turn" } else { "tool_use" },
        );
        let response = doc["response"].as_str().unwrap();
        assert!(response.contains(EVAL5_PREFIX), "{doc}");
        assert_eq!(response.contains(EVAL5_FINAL), allowed, "{doc}");
        let calls = if allowed { 2 } else { 1 };
        fixture.assert_calls(calls).await;
        fixture.assert_canary("zero.txt", allowed);
        let receipts = fixture.new_receipts(calls);
        let provenance = if prompt.is_some() {
            "observed"
        } else {
            "unknown"
        };
        assert!(
            receipts.iter().all(|row| row["provenance"] == provenance
                && row["usage"]["tokensIn"] == 0
                && row["usage"]["tokensOut"] == 0),
            "{receipts:?}"
        );
    }
}

fn eval5_prior(model: &str, tokens: u32, provenance: Option<&str>, age_days: i64) -> String {
    let mut row = json!({
        "timestampMs": (chrono::Utc::now() - chrono::Duration::days(age_days)).timestamp_millis(),
        "sessionId": "prior", "conversationId": "prior", "providerId": "router", "model": model,
        "tier": "cheap_agentic", "stepKind": null, "escalationReason": "none",
        "usage": {"tokensIn": tokens, "tokensOut": 0, "parentCtx": 0}
    });
    if let Some(provenance) = provenance {
        row["provenance"] = json!(provenance);
    }
    format!("{row}\n")
}

#[tokio::test]
async fn eval5_ask_daily_budget_checks_prior_spend_and_the_settled_call() {
    for (daily, prior, tool, calls, reason, stop) in [
        (6, 4, true, 2, None, "end_turn"),
        (5, 5, true, 0, Some("daily_budget"), "cancelled"),
        (5, 4, true, 1, Some("daily_budget"), "tool_use"),
        (5, 4, false, 1, Some("daily_budget"), "end_turn"),
    ] {
        let fixture = Eval5Ask::ollama(true, Some(daily)).await;
        let seed = eval5_prior(EVAL5_MODEL, prior, Some("observed"), 0)
            + &eval5_prior(EVAL5_MODEL, 100, Some("observed"), 2);
        let prior_path = fixture.seed(&seed);
        fixture
            .script(
                "application/x-ndjson",
                [
                    eval5_ollama_reply(EVAL5_PREFIX, tool.then_some("daily.txt"), Some(1), 0),
                    eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0),
                ],
            )
            .await;
        let flags = match (calls, reason) {
            // Prior spend belongs only to the daily cap, not the invocation cap.
            (2, _) => &["--max-cost=2"][..],
            // Both paid limits hit exactly on the Nth call; daily must win.
            (1, Some(_)) => &["--max-cost=1", "--max-turns=1"][..],
            _ => &[][..],
        };
        let output = fixture.run("json", flags).await;
        let doc = eval5_assert_json(&output, "1.4", reason, stop);
        let response = doc["response"].as_str().unwrap();
        assert_eq!(response.contains(EVAL5_PREFIX), calls > 0, "{doc}");
        assert_eq!(response.contains(EVAL5_FINAL), calls == 2, "{doc}");
        fixture.assert_calls(calls).await;
        fixture.assert_canary("daily.txt", calls == 2);
        fixture.new_receipts(calls);
        assert_eq!(fs::read_to_string(prior_path).unwrap(), seed);
    }
}

#[tokio::test]
async fn eval5_ask_daily_budget_rejects_unpriced_unverified_or_malformed_rows() {
    let known_zero = eval5_prior(EVAL5_MODEL, 0, Some("observed"), 0);
    for (additional, allowed) in [
        (String::new(), true),
        (eval5_prior("eval5-unpriced", 0, Some("observed"), 0), false),
        (eval5_prior(EVAL5_MODEL, 0, None, 0), false),
        (eval5_prior(EVAL5_MODEL, 1, None, 0), false),
        (
            eval5_prior(EVAL5_MODEL, 5, Some("observed"), 0)
                + &eval5_prior(EVAL5_MODEL, 0, Some("unknown"), 0),
            false,
        ),
        ("{not valid JSONL}\n".into(), false),
    ] {
        let fixture = Eval5Ask::ollama(true, Some(5)).await;
        let seed = known_zero.clone() + &additional;
        let prior_path = fixture.seed(&seed);
        fixture
            .script(
                "application/x-ndjson",
                [
                    eval5_ollama_reply(EVAL5_PREFIX, Some("strict.txt"), Some(0), 0),
                    eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0),
                ],
            )
            .await;
        let calls = if allowed { 2 } else { 0 };
        if allowed {
            let output = fixture.run("json", &[]).await;
            let doc = eval5_assert_json(&output, "1.4", None, "end_turn");
            assert!(
                doc["response"].as_str().unwrap().contains(EVAL5_FINAL),
                "{doc}"
            );
        } else {
            let output = fixture.run("stream-json", &[]).await;
            eval5_assert_stream(&output, "1.4", Some("accounting_error"), "cancelled", false);
        }
        fixture.assert_calls(calls).await;
        fixture.assert_canary("strict.txt", allowed);
        fixture.new_receipts(calls);
        assert_eq!(fs::read_to_string(prior_path).unwrap(), seed);
    }
}

#[tokio::test]
async fn eval5_ask_daily_budget_fails_closed_on_ledger_path_error() {
    for broken in [false, true] {
        let fixture = Eval5Ask::ollama(true, Some(5)).await;
        let usage_dir = fixture.ledger_dir();
        if broken {
            // A file in place of the directory denies reads even when run as root.
            fs::write(&usage_dir, b"not a directory").unwrap();
        } else {
            fs::create_dir(&usage_dir).unwrap();
        }
        fixture
            .script(
                "application/x-ndjson",
                [
                    eval5_ollama_reply(EVAL5_PREFIX, Some("path.txt"), Some(0), 0),
                    eval5_ollama_reply(EVAL5_FINAL, None, Some(0), 0),
                ],
            )
            .await;
        let output = fixture.run("json", &[]).await;
        if broken {
            eval5_assert_json(&output, "1.4", Some("accounting_error"), "cancelled");
            assert_eq!(fs::read(&usage_dir).unwrap(), b"not a directory");
        } else {
            let doc = eval5_assert_json(&output, "1.4", None, "end_turn");
            assert!(
                doc["response"].as_str().unwrap().contains(EVAL5_FINAL),
                "{doc}"
            );
        }
        let calls = if broken { 0 } else { 2 };
        fixture.assert_calls(calls).await;
        fixture.assert_canary("path.txt", !broken);
        fixture.new_receipts(calls);
    }
}

/// Story 23.1 K8 (AC8): `rustain ask` prints the sandbox status as one stderr
/// line before the first provider request: when the provider fails the turn,
/// the "Sandbox off" line is the FIRST stderr line, ahead of the turn's own
/// failure text, and stdout keeps the selected format.
#[tokio::test]
async fn k8_ask_prints_the_sandbox_status_before_the_first_provider_request() {
    const NOTICE: &str =
        "Sandbox off ([sandbox].adapter = \"noop\"): tool commands run without OS confinement.";
    let fixture = Eval5Ask::ollama(false, None).await;
    Mock::given(method("POST"))
        .and(path(fixture.chat_path))
        .respond_with(ResponseTemplate::new(500).set_body_string("fixture failure"))
        .mount(&fixture.server)
        .await;

    let output = fixture.run("text", &["--sandbox-adapter", "noop"]).await;
    assert!(!output.status.success(), "a failed turn exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut lines = stderr.lines();
    // The session adds where the adapter came from after the status.
    assert!(
        lines.next().is_some_and(|l| l.starts_with(NOTICE)),
        "{stderr}"
    );
    assert!(
        lines.any(|line| !line.trim().is_empty()),
        "the turn's failure follows the notice: {stderr}"
    );

    let output = fixture
        .run("stream-json", &["--sandbox-adapter", "noop"])
        .await;
    assert!(!output.status.success(), "a failed turn exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.lines().next().is_some_and(|l| l.starts_with(NOTICE)),
        "{stderr}"
    );
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("stdout stays stream-json ({e}): {line}"));
    }
}

/// Story 23.1 review H1: a checkout's own `.rustain/config.toml` cannot switch
/// the sandbox off for `rustain ask --yolo`; the ignored key is named on the
/// status line.
#[tokio::test]
async fn h1_a_checkout_cannot_switch_the_sandbox_off() {
    let fixture = Eval5Ask::ollama(false, None).await;
    Mock::given(method("POST"))
        .and(path(fixture.chat_path))
        .respond_with(ResponseTemplate::new(500).set_body_string("fixture failure"))
        .mount(&fixture.server)
        .await;
    let dot = fixture.root.path().join("workspace/.rustain");
    fs::create_dir_all(&dot).unwrap();
    fs::write(dot.join("config.toml"), "[sandbox]\nadapter = \"noop\"\n").unwrap();
    let output = fixture.run("text", &[]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().next().unwrap_or_default();
    assert!(!first.starts_with("Sandbox off"), "{stderr}");
    assert!(
        stderr.contains("Ignored `[sandbox].adapter = \"noop\"`")
            && stderr.contains(".rustain/config.toml"),
        "{stderr}"
    );
}
