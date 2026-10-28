//! Cloud Code Assist project discovery and onboarding through the real login.
use super::auth::{
    ACCESS, EMAIL, Env, Google, LOAD, ONBOARD, OPERATION, PROVIDER, Sleeps, TOKEN, USERINFO, env,
    google, hits, login_with, mount, ok, stored_oauth, token_body, ui,
};
use crate::conformance_20_1d_codex_subscription::boundaries::{raw_listener, read_request};
use rustain::adapters::auth::flows::{self, FlowCtx};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::{Arc, atomic::Ordering};
use tokio::io::AsyncWriteExt;
use wiremock::{MockServer, Request, ResponseTemplate};

const PROJECT_REQUIRED: &str = "This account requires setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca";
const NO_PROJECT: &str = "Could not discover or provision a Google Cloud project. Try setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca";

struct Outcome {
    result: Result<String, String>,
    requests: Vec<Request>,
    progress: Vec<String>,
    sleeps: usize,
    other_sleeps: usize,
}
impl Outcome {
    fn bodies(&self, path: &str) -> Vec<String> {
        hits(&self.requests, path)
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect()
    }
    fn count(&self, path: &str) -> usize {
        hits(&self.requests, path).len()
    }
}

/// One real login with `google`, under `vars`; returns the stored project or the error.
async fn run(google: Google, vars: &[(&'static str, Option<&str>)]) -> Outcome {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let _vars = Env::set(vars);
    mount(&server, google).await;
    let (ui, mut seen) = ui(&[]);
    let sleeps = Sleeps::default();
    let (result, _) = login_with(&server, ui, &mut seen, sleeps.sleeper(None)).await;
    let rows = stored_oauth(PROVIDER).await;
    let result = match result {
        Ok(login) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].identity.project_id, login.identity.project_id);
            Ok(login.identity.project_id.unwrap())
        }
        Err(error) => {
            assert!(rows.is_empty(), "a failed login stores nothing");
            Err(error.to_string())
        }
    };
    Outcome {
        result,
        requests: server.received_requests().await.unwrap(),
        progress: seen.progress(),
        sleeps: sleeps.count.load(Ordering::SeqCst),
        other_sleeps: sleeps.other.load(Ordering::SeqCst),
    }
}
fn with_load(load: ResponseTemplate) -> Google {
    Google {
        load,
        ..google(ACCESS, "onboarded-project", EMAIL)
    }
}
fn done(project: Option<&str>) -> ResponseTemplate {
    match project {
        Some(project) => {
            ok(json!({"done": true, "response": {"cloudaicompanionProject": {"id": project}}}))
        }
        None => ok(json!({"done": true, "response": {}})),
    }
}
fn pending() -> ResponseTemplate {
    ok(json!({"name": "operations/fixture-op", "done": false}))
}
fn load_with(project: &str) -> String {
    format!(
        r#"{{"cloudaicompanionProject":"{project}","metadata":{{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":"{project}"}}}}"#
    )
}

#[tokio::test]
#[serial]
async fn k03_existing_tier_uses_its_project_then_the_env_project_else_fails() {
    let outcome = run(
        with_load(ok(json!({"currentTier": {"id": "standard-tier"}, "cloudaicompanionProject": "existing-project"}))),
        &[],
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Ok("existing-project"));
    assert_eq!(outcome.bodies(LOAD), [super::auth::FREE_LOAD_BODY]);
    assert_eq!(outcome.count(ONBOARD), 0);
    // The `{id}` form discovery accepts.
    let outcome = run(
        with_load(ok(
            json!({"currentTier": {}, "cloudaicompanionProject": {"id": "object-project"}}),
        )),
        &[],
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Ok("object-project"));

    let outcome = run(
        with_load(ok(json!({"currentTier": {"id": "standard-tier"}}))),
        &[("GOOGLE_CLOUD_PROJECT", Some("env-project"))],
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Ok("env-project"));
    assert_eq!(outcome.bodies(LOAD), [load_with("env-project")]);
    assert_eq!(outcome.count(ONBOARD), 0);

    let outcome = run(
        with_load(ok(json!({"currentTier": {"id": "standard-tier"}}))),
        &[],
    )
    .await;
    assert_eq!(outcome.result, Err(PROJECT_REQUIRED.into()));
    assert_eq!(outcome.count(ONBOARD), 0);
}

#[tokio::test]
#[serial]
async fn k03_tier_choice_free_default_idless_default_and_legacy() {
    let free = r#"{"tierId":"free-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#;
    // Default free tier; an immediately `done` operation is not polled.
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "standard-tier"}, {"id": "free-tier", "isDefault": true}]}),
    ));
    google.onboard = done(Some("free-project"));
    let outcome = run(google, &[]).await;
    assert_eq!(outcome.result.as_deref(), Ok("free-project"));
    assert_eq!(outcome.bodies(ONBOARD), [free]);
    assert_eq!(outcome.count(OPERATION), 0);
    assert_eq!(outcome.sleeps, 0);
    assert_eq!(
        outcome.progress,
        [
            "Checking for existing Cloud Code Assist project...",
            "Provisioning Cloud Code Assist project (this may take a moment)..."
        ]
    );

    // A default tier without an id is the free tier.
    let mut google = with_load(ok(json!({"allowedTiers": [{"isDefault": true}]})));
    google.onboard = done(Some("idless-project"));
    let outcome = run(google, &[]).await;
    assert_eq!(outcome.result.as_deref(), Ok("idless-project"));
    assert_eq!(outcome.bodies(ONBOARD), [free]);

    // Absent tiers are the legacy (non-free) tier: no env project fails BEFORE onboarding.
    let outcome = run(with_load(ok(json!({}))), &[]).await;
    assert_eq!(outcome.result, Err(PROJECT_REQUIRED.into()));
    assert_eq!(outcome.count(ONBOARD), 0);
    let outcome = run(with_load(ok(json!({"allowedTiers": []}))), &[]).await;
    assert_eq!(outcome.result, Err(PROJECT_REQUIRED.into()));
    assert_eq!(outcome.count(ONBOARD), 0);
}

#[tokio::test]
#[serial]
async fn k03_non_free_tier_without_env_project_never_onboards() {
    let outcome = run(
        with_load(ok(
            json!({"allowedTiers": [{"id": "standard-tier", "isDefault": true}]}),
        )),
        &[],
    )
    .await;
    assert_eq!(outcome.result, Err(PROJECT_REQUIRED.into()));
    assert_eq!(
        outcome.count(ONBOARD),
        0,
        "configuration error precedes onboarding"
    );
    assert_eq!(
        outcome.progress,
        ["Checking for existing Cloud Code Assist project..."]
    );
}

#[tokio::test]
#[serial]
async fn k03_non_free_tier_with_env_project_onboards_it_in_omp_key_order() {
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "standard-tier", "isDefault": true}]}),
    ));
    google.onboard = done(None);
    // A blank `GOOGLE_CLOUD_PROJECT` falls through to `GOOGLE_CLOUD_PROJECT_ID`.
    let outcome = run(
        google,
        &[
            ("GOOGLE_CLOUD_PROJECT", Some("  ")),
            ("GOOGLE_CLOUD_PROJECT_ID", Some("paid-project")),
        ],
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Ok("paid-project"));
    assert_eq!(outcome.bodies(LOAD), [load_with("paid-project")]);
    assert_eq!(
        outcome.bodies(ONBOARD),
        [
            r#"{"tierId":"standard-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":"paid-project"},"cloudaicompanionProject":"paid-project"}"#
        ]
    );
}

#[tokio::test]
#[serial]
async fn k03_vpc_sc_rejection_is_the_standard_tier() {
    let vpc = ResponseTemplate::new(403).set_body_json(
        json!({"error": {"code": 403, "details": [{"reason": "SECURITY_POLICY_VIOLATED"}]}}),
    );
    let outcome = run(
        with_load(vpc.clone()),
        &[("GOOGLE_CLOUD_PROJECT", Some("vpc-project"))],
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Ok("vpc-project"));
    assert_eq!(outcome.count(ONBOARD), 0);
    let outcome = run(with_load(vpc), &[]).await;
    assert_eq!(outcome.result, Err(PROJECT_REQUIRED.into()));
}

#[tokio::test]
#[serial]
async fn k03_failures_name_the_step_status_and_redacted_message() {
    let outcome = run(
        with_load(
            ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "backend down"}})),
        ),
        &[],
    )
    .await;
    assert_eq!(
        outcome.result,
        Err("loadCodeAssist failed: 500 Internal Server Error: backend down".into())
    );
    // A non-JSON body is cut to 500 characters with the bearer removed first.
    let body = format!("{ACCESS} {}", "x".repeat(600));
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.onboard = ResponseTemplate::new(400).set_body_string(body);
    let outcome = run(google, &[]).await;
    let expected: String = format!("[redacted] {}", "x".repeat(600))
        .chars()
        .take(500)
        .collect();
    assert_eq!(
        outcome.result,
        Err(format!("onboardUser failed: 400 Bad Request: {expected}"))
    );
    // A 2xx that is not JSON.
    let outcome = run(
        with_load(ResponseTemplate::new(200).set_body_string("<html>")),
        &[],
    )
    .await;
    assert_eq!(
        outcome.result,
        Err("loadCodeAssist returned invalid JSON".into())
    );
}

#[tokio::test]
#[serial]
async fn k03_validation_required_names_the_account_and_only_shows_https_urls() {
    let rejection = |url: &str| {
        ResponseTemplate::new(403).set_body_json(
            json!({"error": {"code": 403, "message": "verify", "details": [
                {"reason": "VALIDATION_REQUIRED", "metadata": {"validation_url": url}}
            ]}}),
        )
    };
    let outcome = run(
        with_load(rejection("https://accounts.google.com/signin/continue?x=1")),
        &[],
    )
    .await;
    assert_eq!(
        outcome.result,
        Err(format!(
            "Account verification required for {EMAIL}. Visit https://accounts.google.com/signin/continue?x=1 to continue, then sign in again."
        ))
    );
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.onboard = rejection("http://phish.example/continue");
    let outcome = run(google, &[]).await;
    assert_eq!(
        outcome.result,
        Err("onboardUser failed: 403 Forbidden: verify".into())
    );
}

#[tokio::test]
#[serial]
async fn k03_operation_polls_until_done_and_reports_attempts() {
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.polls = vec![pending(), pending(), done(Some("polled-project"))];
    let outcome = run(google, &[]).await;
    assert_eq!(outcome.result.as_deref(), Ok("polled-project"));
    assert_eq!(outcome.count(OPERATION), 3);
    assert_eq!(outcome.sleeps, 2);
    assert_eq!(outcome.other_sleeps, 0);
    assert_eq!(
        outcome.progress[2..],
        [
            "Waiting for project provisioning (attempt 2/24)...",
            "Waiting for project provisioning (attempt 3/24)..."
        ]
    );
    for request in hits(&outcome.requests, OPERATION) {
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {ACCESS}").as_str()
        );
    }

    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.polls = vec![pending(), ResponseTemplate::new(500)];
    let outcome = run(google, &[]).await;
    assert_eq!(
        outcome.result,
        Err("Failed to poll operation: 500 Internal Server Error".into())
    );
    assert_eq!(outcome.count(OPERATION), 2);
}

#[tokio::test]
#[serial]
async fn k03_a_stuck_operation_stops_after_24_gets_and_23_sleeps() {
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.polls = (0..30).map(|_| pending()).collect();
    let outcome = run(google, &[]).await;
    assert_eq!(
        outcome.result,
        Err("Project provisioning did not complete after 24 attempts".into())
    );
    assert_eq!(outcome.count(OPERATION), 24);
    assert_eq!(outcome.sleeps, 23);
    assert_eq!(outcome.other_sleeps, 0, "every wait is 5000 ms");
    let waits: Vec<_> = outcome
        .progress
        .iter()
        .filter(|line| line.starts_with("Waiting for project provisioning"))
        .cloned()
        .collect();
    assert_eq!(waits.len(), 23);
    assert_eq!(
        waits[0],
        "Waiting for project provisioning (attempt 2/24)..."
    );
    assert_eq!(
        waits[22],
        "Waiting for project provisioning (attempt 24/24)..."
    );
}

#[tokio::test]
#[serial]
async fn k03_no_project_anywhere_fails_with_omp_text() {
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.onboard = done(None);
    let outcome = run(google, &[]).await;
    assert_eq!(outcome.result, Err(NO_PROJECT.into()));
}

#[tokio::test]
#[serial]
async fn k03_cancel_during_the_third_sleep_stops_polling_and_stores_nothing() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let mut google = with_load(ok(
        json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]}),
    ));
    google.polls = (0..30).map(|_| pending()).collect();
    mount(&server, google).await;
    let (ui, mut seen) = ui(&[]);
    let cancel = ui.cancel.clone();
    let sleeps = Sleeps::default();
    let (result, _) = login_with(&server, ui, &mut seen, sleeps.sleeper(Some((3, cancel)))).await;
    assert_eq!(result.unwrap_err().to_string(), "Login cancelled");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(hits(&requests, OPERATION).len(), 3);
    assert_eq!(sleeps.count.load(Ordering::SeqCst), 3);
    assert!(stored_oauth(PROVIDER).await.is_empty());
}

/// A raw origin that serves the token and userinfo, then holds `loadCodeAssist`.
#[tokio::test]
#[serial]
async fn k03_cancel_during_an_in_flight_request_sends_nothing_further() {
    let (listener, origin) = raw_listener().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &origin);
    let (ui, mut seen) = ui(&[]);
    let cancel = ui.cancel.clone();
    let ctx = FlowCtx::new(ui, Arc::new(|| super::auth::NOW));
    let store = super::auth::store();
    let server = async {
        let mut paths = Vec::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let target = request.target.split('?').next().unwrap().to_owned();
            paths.push(target.clone());
            let body: Value = match target.as_str() {
                TOKEN => token_body(ACCESS, Some(super::auth::REFRESH)),
                USERINFO => json!({"email": EMAIL}),
                LOAD => {
                    // Held open: the login is cancelled while this request is in flight.
                    cancel.cancel();
                    // The client must drop the connection and open no other one.
                    let mut byte = [0];
                    use tokio::io::AsyncReadExt;
                    let read = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        socket.read(&mut byte),
                    )
                    .await
                    .expect("the cancelled request must close its connection");
                    assert_eq!(read.unwrap(), 0);
                    crate::conformance_20_1d_codex_subscription::boundaries::assert_no_connection(
                        &listener,
                    );
                    return paths;
                }
                other => panic!("unexpected request {other}"),
            };
            let body = body.to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    };
    let browser = async {
        let (authorize, _) = seen.urls.recv().await.unwrap();
        let authorize = url::Url::parse(&authorize).unwrap();
        let pairs = super::auth::query(&authorize);
        let find = |key: &str| pairs.iter().find(|(k, _)| k == key).unwrap().1.clone();
        let mut callback = url::Url::parse(&find("redirect_uri")).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", super::auth::CODE)
            .append_pair("state", &find("state"));
        super::auth::no_redirect_client()
            .get(callback)
            .send()
            .await
            .unwrap();
    };
    let (result, paths, ()) = tokio::join!(
        flows::login(PROVIDER, &super::auth::METHOD, &ctx, &store),
        server,
        browser
    );
    assert_eq!(result.unwrap_err().to_string(), "Login cancelled");
    assert_eq!(paths, [TOKEN, USERINFO, LOAD]);
    assert!(stored_oauth(PROVIDER).await.is_empty());
}

#[tokio::test]
#[serial]
async fn k03_an_unrefreshable_token_response_never_starts_provisioning() {
    let mut google = google(ACCESS, "p", EMAIL);
    google.token = ok(json!({"access_token": ACCESS, "refresh_token": "", "expires_in": 3600}));
    let outcome = run(google, &[]).await;
    assert_eq!(
        outcome.result,
        Err("No refresh token received. Please try again.".into())
    );
    assert_eq!(outcome.count(LOAD), 0);
}
