//! Real-binary proof; build the binary with `test-instrumentation` before running.
//! `RUSTAIN_TEST_CODEX_BINARY` selects that binary explicitly. The cargo-bin
//! fallback has the SAME instrumentation requirement, checked before any callback.
//! `RUSTAIN_TEST_CODEX_EVIDENCE_DIR` retains command/output/request evidence.

use super::auth::jwt;
use rustain::{adapters::auth_store::FileAuthStore, domain::ports::AuthStorePort};
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const NOW: i64 = 1_800_000_000_000;
const RESPONSE_PATH: &str = "/backend-api/codex/responses";
const FIXTURE_TEXT: &str = "hello Codex CLI fixture 🦀";
const CODE: &str = "CODEX-CLI-FIXTURE-CODE";
const REFRESH: &str = "CODEX-CLI-FIXTURE-REFRESH";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Fixture {
    pub(crate) home: tempfile::TempDir,
    pub(crate) evidence: PathBuf,
    binary: PathBuf,
    env: BTreeMap<&'static str, OsString>,
}

impl Fixture {
    pub(crate) fn new(server: &MockServer) -> Self {
        let home = tempfile::tempdir().unwrap();
        let evidence = std::env::var_os("RUSTAIN_TEST_CODEX_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                tempfile::Builder::new()
                    .prefix("rustain-codex-cli-evidence-")
                    .tempdir()
                    .unwrap()
                    .keep()
            });
        std::fs::create_dir_all(&evidence).unwrap();
        let evidence = evidence.canonicalize().unwrap();
        // A reused evidence destination must not make a failed rerun look passed.
        let passed = evidence.join("passed.json");
        if passed.exists() {
            std::fs::remove_file(passed).unwrap();
        }
        let explicit_binary = std::env::var_os("RUSTAIN_TEST_CODEX_BINARY");
        let binary = explicit_binary
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| assert_cmd::cargo::cargo_bin("rustain"))
            .canonicalize()
            .expect("build the real rustain binary with --features test-instrumentation first");
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let callback_port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut env = BTreeMap::new();
        for (key, value) in [
            ("HOME", home.path().to_owned().into_os_string()),
            (
                "RUSTAIN_DATA_DIR",
                home.path().join("data").into_os_string(),
            ),
            (
                "RUSTAIN_CONFIG_DIR",
                home.path().join("config").into_os_string(),
            ),
            (
                "XDG_CONFIG_HOME",
                home.path().join("xdg-config").into_os_string(),
            ),
            (
                "XDG_DATA_HOME",
                home.path().join("xdg-data").into_os_string(),
            ),
            (
                "XDG_CACHE_HOME",
                home.path().join("xdg-cache").into_os_string(),
            ),
            ("RUSTAIN_TEST_AUTH_ORIGIN", server.uri().into()),
            (
                "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
                callback_port.to_string().into(),
            ),
            ("RUSTAIN_TEST_NOW_MS", NOW.to_string().into()),
            ("RUSTAIN_TEST_CODEX_RETRY_SCALE", "0".into()),
            ("RUSTAIN_MODELS_DEV_URL", server.uri().into()),
            ("NO_PROXY", "*".into()),
        ] {
            env.insert(key, value);
        }
        let fixture = Self {
            home,
            evidence,
            binary,
            env,
        };
        std::fs::create_dir_all(fixture.home.path().join("config")).unwrap();
        let config = "model = \"gpt-5.6-luna\"\n\n[provider.openai-codex]\nkind = \"openai-codex\"\nprovider_id = \"openai-codex\"\nmodel_id = \"gpt-5.6-luna\"\napi_key_env = \"OPENAI_CODEX_OAUTH_TOKEN\"\nenabled = true\ndiscover_models = true\n";
        std::fs::write(fixture.home.path().join("config/config.toml"), config).unwrap();
        std::fs::write(fixture.evidence.join("config.toml"), config).unwrap();
        fixture.json("binary.json", &json!({
            "path": fixture.binary,
            "selection": if explicit_binary.is_some() { "RUSTAIN_TEST_CODEX_BINARY" } else { "assert_cmd::cargo::cargo_bin" },
            "required_build_feature": "test-instrumentation",
            "instrumentation_verified_by": "exact printed authorize URL origin before any launch or callback request",
            "bytes": std::fs::metadata(&fixture.binary).unwrap().len(),
        }));
        println!("Codex CLI evidence: {}", fixture.evidence.display());
        fixture
    }

    pub(crate) fn json(&self, file: &str, value: &Value) {
        std::fs::write(
            self.evidence.join(file),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn command(&self, label: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .env_clear()
            .envs(&self.env)
            .current_dir(self.home.path())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        self.json(&format!("{label}.command.json"), &json!({
            "program": self.binary, "args": args, "cwd": self.home.path(),
            "env_clear": true,
            "env": self.env.iter().map(|(key, value)| (*key, value.to_string_lossy().into_owned())).collect::<BTreeMap<_, _>>(),
            "stdin": "null", "timeout_seconds": COMMAND_TIMEOUT.as_secs(),
        }));
        command
    }

    fn capture(&self, label: &str, output: &Output, stderr: &[u8]) {
        std::fs::write(
            self.evidence.join(format!("{label}.stdout")),
            &output.stdout,
        )
        .unwrap();
        std::fs::write(self.evidence.join(format!("{label}.stderr")), stderr).unwrap();
        self.json(
            &format!("{label}.exit.json"),
            &json!({"success": output.status.success(), "code": output.status.code()}),
        );
    }

    pub(crate) async fn run(&self, label: &str, args: &[&str]) -> Output {
        let output = tokio::time::timeout(COMMAND_TIMEOUT, self.command(label, args).output())
            .await
            .unwrap_or_else(|_| panic!("{label} exceeded {COMMAND_TIMEOUT:?}"))
            .unwrap_or_else(|error| panic!("cannot execute {label}: {error}"));
        self.capture(label, &output, &output.stderr);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub(crate) async fn requests(&self, server: &MockServer, label: &str) -> Vec<Request> {
        let requests = server.received_requests().await.unwrap();
        let values: Vec<_> = requests.iter().map(|request| json!({
            "method": request.method.as_str(), "url": request.url.as_str(),
            "headers": request.headers.iter().map(|(key, value)| (key.as_str(), value.to_str().unwrap())).collect::<BTreeMap<_, _>>(),
            "body_utf8": String::from_utf8_lossy(&request.body),
        })).collect();
        self.json(&format!("{label}.requests.json"), &json!(values));
        requests
    }
}

fn provider_row(value: &Value) -> &Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == "openai-codex")
        .expect("Codex row")
}

pub(crate) async fn login(fixture: &Fixture, server: &MockServer) -> (Output, String) {
    let mut child = fixture
        .command("login", &["auth", "login", "openai-codex"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let (authorize, launch) = tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut authorize = None;
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "login exited before URLs: {transcript}"
            );
            transcript.push_str(&line);
            std::fs::write(fixture.evidence.join("login.stderr"), &transcript).unwrap();
            assert!(transcript.len() <= 65_536, "unbounded login stderr");
            if line.trim().starts_with("http") {
                authorize = Some(url::Url::parse(line.trim()).unwrap());
            }
            if let Some(launch) = line.trim().strip_prefix("Local browser launch: ") {
                break (
                    authorize.expect("authorize URL must precede launch URL"),
                    url::Url::parse(launch).unwrap(),
                );
            }
        }
    })
    .await
    .expect("login did not print URLs within its deadline");

    // This is the safety gate: never drive even /launch before proving that the
    // actual running binary rebased its printed authorization URL to our stub.
    let origin = url::Url::parse(&server.uri()).unwrap();
    assert_eq!(origin.scheme(), "http");
    assert_eq!(origin.host_str(), Some("127.0.0.1"));
    assert_eq!(
        authorize.origin(),
        origin.origin(),
        "binary MUST be built with test-instrumentation: {authorize}"
    );
    assert_eq!(authorize.path(), "/oauth/authorize");
    let query: BTreeMap<_, _> = authorize.query_pairs().into_owned().collect();
    assert_eq!(query["originator"], "omp");
    let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
    let port: u16 = fixture.env["RUSTAIN_TEST_AUTH_CALLBACK_PORT"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    for url in [&callback, &launch] {
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(port));
        assert!(url.username().is_empty());
        assert!(url.password().is_none());
    }
    assert_eq!(callback.path(), "/auth/callback");
    assert_eq!(launch.path(), "/launch");
    assert!(!query["state"].is_empty());
    fixture.json(
        "safety.json",
        &json!({
            "authorize_url_from_child_stderr": authorize.as_str(),
            "expected_loopback_origin": origin.origin().ascii_serialization(),
            "actual_authorize_origin": authorize.origin().ascii_serialization(),
            "exact_loopback_origin_asserted_before_any_http": true,
            "launch_url_from_child_stderr": launch.as_str(),
            "callback_before_fixture_code": callback.as_str(),
            "http_actions_started": false,
        }),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client.get(launch.clone()).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], authorize.as_str());
    fixture.json("launch.json", &json!({"url": launch.as_str(), "status": response.status().as_u16(), "location": response.headers()["location"].to_str().unwrap()}));
    callback
        .query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", &query["state"]);
    let response = client.get(callback.clone()).send().await.unwrap();
    assert!(response.status().is_success());
    fixture.json("callback.json", &json!({"url": callback.as_str(), "state_matches_authorize": true, "status": response.status().as_u16()}));
    let (output, read) = tokio::time::timeout(COMMAND_TIMEOUT, async {
        tokio::join!(
            child.wait_with_output(),
            stderr.read_to_string(&mut transcript)
        )
    })
    .await
    .expect("login did not exit within its deadline");
    read.unwrap();
    let output = output.unwrap();
    fixture.capture("login", &output, transcript.as_bytes());
    assert!(output.status.success(), "{transcript}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Logged in to ChatGPT Plus/Pro (Codex Subscription) as operator@example.test (plus)"
    );
    (output, transcript)
}

#[tokio::test]
#[serial]
async fn k10_real_debug_binary_login_status_ask_logout() {
    // No sleeps or polling: URL publication gates callback, and each subprocess
    // is bounded, including shutdown. The outer deadline also bounds the flow.
    tokio::time::timeout(Duration::from_secs(120), async {
        let server = MockServer::start().await;
        let fixture = Fixture::new(&server);
        let access = jwt(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"cli-access-account", "chatgpt_plan_type":"Plus ", "chatgpt_data_residency":"eu"},
            "https://api.openai.com/profile": {"email":" Operator@EXAMPLE.test "},
        }));
        Mock::given(method("POST")).and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":access,"refresh_token":REFRESH,"expires_in":3600})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
        Mock::given(method("GET")).and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"gpt-5.6-luna"}]}))).mount(&server).await;
        let sse = format!("data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":FIXTURE_TEXT}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":2}}}));
        Mock::given(method("POST")).and(path(RESPONSE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server).await;

        let (login, transcript) = login(&fixture, &server).await;
        // Story 20.1d1b: login now refreshes the live Codex roster exactly once.
        assert!(
            transcript.contains("Discovered 1 models for ChatGPT Plus/Pro (Codex Subscription)."),
            "missing post-login discovery line: {transcript}"
        );
        let requests = fixture.requests(&server, "login").await;
        assert_eq!(requests.iter().filter(|request| request.method == "POST").count(), 1);
        assert_eq!(requests.iter().filter(|request| request.method == "GET" && request.url.path().ends_with("/models")).count(), 1, "login discovers models once");
        let token = requests.iter().find(|request| request.url.path() == "/oauth/token").unwrap();
        assert_eq!(token.headers["content-type"], "application/x-www-form-urlencoded");
        let fields: Vec<_> = url::form_urlencoded::parse(&token.body).into_owned().collect();
        assert_eq!(fields.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(), ["grant_type","client_id","code","redirect_uri","code_verifier"]);
        assert_eq!(fields[0].1, "authorization_code");
        assert_eq!(fields[1].1, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(fields[2].1, CODE);
        assert!(!fields[4].1.is_empty());
        let store = FileAuthStore::at(fixture.home.path().join("data/auth.json"));
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.rows("openai-codex").len(), 1);
        std::fs::copy(fixture.home.path().join("data/auth.json"), fixture.evidence.join("auth-after-login.json")).unwrap();

        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json)["source"], "oauth");
        let ask = fixture.run("ask", &["ask", "hi", "--output-format", "json"]).await;
        let ask_json: Value = serde_json::from_slice(&ask.stdout).unwrap();
        assert_eq!(ask_json["response"], FIXTURE_TEXT);
        let requests = fixture.requests(&server, "ask").await;
        let responses: Vec<_> = requests.iter().filter(|request| request.method == "POST" && request.url.path() == RESPONSE_PATH).collect();
        assert_eq!(responses.len(), 1, "one actual Codex request");
        assert_eq!(requests.iter().filter(|request| request.url.path().ends_with("/chat/completions")).count(), 0, "Chat fallback is forbidden");
        assert_eq!(requests.iter().filter(|request| request.url.path() == "/oauth/token").count(), 1, "fresh login must not refresh");
        let request = responses[0];
        for (key, value) in [
            ("authorization", format!("Bearer {access}")), ("originator", "omp".into()),
            ("user-agent", "omp/18.12.6".into()), ("version", "0.159.0".into()),
            ("openai-beta", "responses=experimental".into()), ("chatgpt-account-id", "cli-access-account".into()),
            ("x-openai-internal-codex-residency", "eu".into()), ("x-codex-routing-hint", "model=gpt-5.6-luna".into()),
            ("accept", "text/event-stream".into()), ("content-type", "application/json".into()),
        ] { assert_eq!(request.headers.get(key).unwrap().to_str().unwrap(), value, "{key}"); }
        assert!(!request.headers.contains_key("x-api-key"));
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["model"], "gpt-5.6-luna");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body["input"].as_array().unwrap().iter().any(|item| item["role"] == "user" && item["content"].as_array().is_some_and(|content| content.iter().any(|part| part["type"] == "input_text" && part["text"] == "hi"))));
        for absent in ["messages","temperature","top_p","max_output_tokens","max_completion_tokens","stop","parallel_tool_calls","reasoning"] {
            assert!(body.get(absent).is_none(), "unexpected Chat/sampling field {absent}");
        }
        let session = body["prompt_cache_key"].as_str().unwrap();
        assert!(!session.is_empty());
        for key in ["conversation_id","session_id","x-client-request-id"] {
            assert_eq!(request.headers.get(key).unwrap().to_str().unwrap(), session, "{key}");
        }
        let metadata = &body["client_metadata"];
        for (field, header) in [("session_id","session-id"),("thread_id","thread-id"),("x-codex-window-id","x-codex-window-id"),("x-codex-turn-metadata","x-codex-turn-metadata")] {
            assert_eq!(metadata[field].as_str().unwrap(), request.headers.get(header).unwrap().to_str().unwrap(), "{field}");
        }
        let turn: Value = serde_json::from_str(metadata["x-codex-turn-metadata"].as_str().unwrap()).unwrap();
        assert_eq!(turn["request_kind"], "turn");
        assert_eq!(turn["installation_id"], metadata["x-codex-installation-id"]);
        assert_eq!(turn["session_id"], metadata["session_id"]);
        assert_eq!(turn["thread_id"], metadata["thread_id"]);
        assert_eq!(turn["window_id"], metadata["x-codex-window-id"]);
        assert_eq!(turn["turn_id"], metadata["turn_id"]);
        assert!(turn["turn_started_at_unix_ms"].as_u64().is_some());
        assert_eq!(metadata["x-codex-installation-id"].as_str().unwrap(), std::fs::read_to_string(fixture.home.path().join("data/install-id")).unwrap().trim());
        fixture.json("codex-request-body.json", &body);

        let logout = fixture.run("logout", &["auth", "logout", "openai-codex"]).await;
        assert_eq!(String::from_utf8_lossy(&logout.stdout).trim(), "Logged out of openai-codex.");
        assert!(store.snapshot().await.unwrap().rows("openai-codex").is_empty());
        std::fs::copy(fixture.home.path().join("data/auth.json"), fixture.evidence.join("auth-after-logout.json")).unwrap();
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(provider_row(&list_json)["configured"], false);
        assert_eq!(provider_row(&list_json)["is_default"], true, "the explicit home config must remain active");
        let final_requests = fixture.requests(&server, "final").await;
        assert_eq!(final_requests.len(), requests.len(), "logout/list must not make provider requests");
        let all_output = format!("{transcript}{}{}{}{}{}{}{}{}{}",
            String::from_utf8_lossy(&login.stdout), String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr),
            String::from_utf8_lossy(&ask.stdout), String::from_utf8_lossy(&ask.stderr), String::from_utf8_lossy(&logout.stdout),
            String::from_utf8_lossy(&logout.stderr), String::from_utf8_lossy(&list.stdout), String::from_utf8_lossy(&list.stderr));
        for secret in [&access, REFRESH, CODE, fields[4].1.as_str()] {
            assert!(!all_output.contains(secret), "fixture credential leaked into CLI output");
        }
        // Written only after every runtime assertion above succeeded. This is
        // not a substitute for cargo's result; failed runs have no pass marker.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d_codex_subscription::cli::k10_real_debug_binary_login_status_ask_logout",
            "login_exit": login.status.code(), "status_exit": status.status.code(), "ask_exit": ask.status.code(),
            "logout_exit": logout.status.code(), "list_exit": list.status.code(),
            "response": ask_json["response"], "status_source": provider_row(&status_json)["source"],
            "configured_after_logout": provider_row(&list_json)["configured"],
            "codex_response_posts": responses.len(), "login_model_gets": 1, "chat_completions_requests": 0,
            "fixture_secrets_only": true,
        }));
    }).await.expect("CLI fixture exceeded its structural 120-second deadline");
}
