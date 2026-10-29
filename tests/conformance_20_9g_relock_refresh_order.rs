//! Relock and overlapping discovery through the live TUI and periodic timer.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

use crate::conformance_20_1e2_tui_accounts::common::{
    Held, K1, K2, Tui, World, accept, activate_openai, answer_models, bearer, config_with,
    key_login, listing, openai, stub, turn_bearer, until_accounts, until_changed,
};
use rustain::adapters::model_catalog_cache::RefreshCheckpoint;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::infrastructure::model_discovery::rejected_notice;
use serial_test::serial;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use wiremock::MockServer;

struct KeyUi(&'static str);
impl rustain::adapters::auth::flows::LoginUi for KeyUi {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("unexpected browser");
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("unexpected device code");
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(
        &self,
        _: &str,
    ) -> futures::future::BoxFuture<'_, Option<rustain::domain::models::SecretString>> {
        Box::pin(async { Some(rustain::domain::models::SecretString::new(self.0.into())) })
    }
    fn manual_input(&self, _: &str) -> Option<futures::future::BoxFuture<'static, Option<String>>> {
        None
    }
    fn prompt_line(&self, _: &str) -> futures::future::BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
    fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }
}

async fn store_login(tui: &Tui, listener: &TcpListener, key: &'static str) {
    use rustain::adapters::auth::flows::{self, FlowCtx};
    use rustain::domain::models::AuthMethod;
    use std::sync::Arc;
    let context = FlowCtx::new(Arc::new(KeyUi(key)), Arc::new(|| 0))
        .with_validation_config(tui.app.provider["openai"].clone());
    let store = tui.ctx.store.clone();
    let login = tokio::spawn(async move {
        flows::login("openai", &AuthMethod::ApiKey, &context, &store)
            .await
            .unwrap()
    });
    assert_eq!(answer_models(listener).await, key);
    assert!(login.await.unwrap().validated);
}

async fn reject(mut held: Held) {
    held.stream
        .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    held.stream.shutdown().await.unwrap();
}

async fn request(listener: &TcpListener, key: &str) -> Held {
    let held = accept(listener).await;
    assert_eq!(held.path(), "/openai/v1/models");
    assert_eq!(bearer(&held), Some(format!("Bearer {key}")));
    held
}

async fn begin_login(tui: &mut Tui, listener: &TcpListener, key: &str) -> Held {
    tui.submit("/login openai");
    tui.until("masked field", Tui::field_pending).await;
    tui.paste(key);
    tui.key(DomainKey::Enter);
    assert_eq!(answer_models(listener).await, key);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    request(listener, key).await
}

async fn logout(tui: &mut Tui) {
    tui.submit("/logout openai");
    until_accounts(tui).await;
    tui.key(DomainKey::Enter);
    until_changed(tui).await;
    assert!(
        tui.live
            .discovery_targets()
            .iter()
            .all(|t| t.provider_id != "openai")
    );
    assert!(
        tui.live
            .registry()
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "openai")
    );
}

fn tick(tui: &Tui) -> tokio::task::JoinHandle<()> {
    let live = tui.live.clone();
    tokio::spawn(async move { live.periodic_tick().await })
}

fn target(tui: &Tui) -> rustain::adapters::model_catalog_cache::DiscoveryTarget {
    tui.live
        .discovery_targets()
        .iter()
        .find(|t| t.provider_id == "openai")
        .unwrap()
        .clone()
}

fn models(tui: &Tui) -> Vec<String> {
    target(tui)
        .provider
        .list_models()
        .iter()
        .map(|m| m.model_id.clone())
        .collect()
}

fn no_rejection(tui: &Tui) {
    assert!(
        !tui.event_log
            .iter()
            .any(|e| e.contains("rejected its credential")),
        "{:?}",
        tui.event_log
    );
}

#[tokio::test]
#[serial]
async fn k1_relock_relogin_retires_prelogout_refresh() {
    for schedule in 0..3 {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        let (listener, base) = stub().await;
        let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
        key_login(&mut tui, &listener, K1).await;
        let a = tick(&tui);
        let held_a = request(&listener, K1).await;
        logout(&mut tui).await;
        if schedule == 2 {
            reject(held_a).await;
            a.await.unwrap();
            tui.drain();
            assert_eq!(tui.live.registry().discovery_status("openai"), None);
            assert!(tui.live.router().get_provider("openai").is_none());
            assert!(
                tui.live
                    .registry()
                    .list_locked()
                    .iter()
                    .any(|p| p.provider_id == "openai")
            );
            no_rejection(&tui);
            continue;
        }
        let held_b = begin_login(&mut tui, &listener, K2).await;
        if schedule == 0 {
            reject(held_a).await;
            a.await.unwrap();
            tui.drain();
            assert_eq!(
                tui.live.registry().discovery_status("openai"),
                Some(DiscoveryStatus::Idle)
            );
            no_rejection(&tui);
            held_b.json(&listing(&["new-only"])).await;
            tui.until_login("new discovery", |e| {
                matches!(e, LoginEvent::Discovered { .. })
            })
            .await;
        } else {
            held_b.json(&listing(&["new-only"])).await;
            tui.until_login("new discovery", |e| {
                matches!(e, LoginEvent::Discovered { .. })
            })
            .await;
            reject(held_a).await;
            a.await.unwrap();
        }
        tui.drain();
        assert_eq!(
            tui.live.registry().discovery_status("openai"),
            Some(DiscoveryStatus::Ok)
        );
        assert_eq!(models(&tui), ["new-only"]);
        no_rejection(&tui);
        let model = activate_openai(&mut tui).await;
        assert_eq!(
            turn_bearer(&tui, &listener, &model).await,
            format!("Bearer {K2}")
        );
    }
}

#[tokio::test]
#[serial]
async fn k3_newest_started_attempt_wins_and_current_rejection_is_deduplicated() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    tui.event_log.clear();
    let baseline = models(&tui);
    let cache_before = tui.live.cache().load().await;
    let a = tick(&tui);
    let held_a = request(&listener, K1).await;
    let b = tick(&tui);
    let held_b = request(&listener, K1).await;
    reject(held_a).await;
    a.await.unwrap();
    tui.drain();
    assert_eq!(models(&tui), baseline);
    assert_eq!(
        serde_json::to_value(tui.live.cache().load().await).unwrap(),
        serde_json::to_value(cache_before).unwrap()
    );
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
    no_rejection(&tui);
    held_b.json(&listing(&["newest-only"])).await;
    b.await.unwrap();
    assert_eq!(models(&tui), ["newest-only"]);

    let a = tick(&tui);
    let held_a = request(&listener, K1).await;
    let b = tick(&tui);
    let held_b = request(&listener, K1).await;
    held_a.json(&listing(&["obsolete-only"])).await;
    a.await.unwrap();
    assert_eq!(models(&tui), ["newest-only"]);
    held_b.json(&listing(&[])).await;
    b.await.unwrap();
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Empty)
    );
    assert!(!models(&tui).iter().any(|m| m == "obsolete-only"));

    tui.drain();
    tui.event_log.clear();
    let expected = rejected_notice(&target(&tui));
    for n in 0..2 {
        let task = tick(&tui);
        reject(request(&listener, K1).await).await;
        task.await.unwrap();
        tui.drain();
        assert_eq!(
            tui.live.registry().discovery_status("openai"),
            Some(DiscoveryStatus::Unauthenticated)
        );
        assert_eq!(
            tui.event_log
                .iter()
                .filter(|e| e.contains(&expected))
                .count(),
            1,
            "iteration {n}"
        );
    }
}

#[tokio::test]
#[serial]
async fn k4a_retirement_before_publication_suppresses_rejection() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    tui.event_log.clear();
    let pause = target(&tui)
        .refresh_order
        .pause_at(RefreshCheckpoint::BeforePublication)
        .await;
    let a = tick(&tui);
    reject(request(&listener, K1).await).await;
    pause.reached().await;
    logout(&mut tui).await;
    pause.release();
    a.await.unwrap();
    tui.drain();
    assert_eq!(tui.live.registry().discovery_status("openai"), None);
    no_rejection(&tui);
}

#[tokio::test]
#[serial]
async fn k5_noop_registration_preserves_inflight_attempt() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let before = tui.ctx.store.snapshot().await.unwrap();
    let a = tick(&tui);
    let held = request(&listener, K1).await;
    assert!(
        tui.live
            .register_change("openai", &tui.app, &before)
            .await
            .is_empty()
    );
    held.json(&listing(&["noop-current"])).await;
    a.await.unwrap();
    assert_eq!(models(&tui), ["noop-current"]);
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
    assert!(!tui.state.model_selector.refreshing.contains("openai"));
}

#[tokio::test]
#[serial]
async fn k4b_cancelled_refresh_keeps_retirement_behind_blocking_commit() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let old = target(&tui);
    let writer = old
        .refresh_order
        .pause_at(RefreshCheckpoint::BlockingCommit)
        .await;
    let committed = old
        .refresh_order
        .pause_at(RefreshCheckpoint::BlockingCommitFinished)
        .await;
    let waiting = old
        .refresh_order
        .pause_at(RefreshCheckpoint::RetirementWaiting)
        .await;
    let a = tick(&tui);
    request(&listener, K1)
        .await
        .json(&listing(&["commit-before-retire"]))
        .await;
    writer.reached().await;
    a.abort();
    assert!(a.await.unwrap_err().is_cancelled());
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Enter);
    waiting.reached().await;
    waiting.release();
    writer.release();
    committed.reached().await;
    let cache = tui.live.cache().load().await;
    assert!(
        cache.providers["openai"]
            .models
            .iter()
            .any(|m| m.model_id == "commit-before-retire")
    );
    committed.release();
    until_changed(&mut tui).await;
    assert_eq!(
        serde_json::to_value(tui.live.cache().load().await).unwrap(),
        serde_json::to_value(cache).unwrap()
    );
    assert!(
        tui.live
            .discovery_targets()
            .iter()
            .all(|t| t.provider_id != "openai")
    );
}

#[tokio::test]
#[serial]
async fn k4c_delayed_login_completion_is_suppressed_after_relock() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    let held = begin_login(&mut tui, &listener, K1).await;
    let order = target(&tui).refresh_order;
    let before = order.pause_at(RefreshCheckpoint::BeforeCompletion).await;
    let finished = order.pause_at(RefreshCheckpoint::CompletionFinished).await;
    held.json(&listing(&["completion-obsolete"])).await;
    before.reached().await;
    tui.drain();
    tui.event_log.clear();
    logout(&mut tui).await;
    before.release();
    finished.reached().await;
    finished.release();
    tui.drain();
    assert!(
        !tui.event_log.iter().any(|e| e.contains("Discovered")),
        "{:?}",
        tui.event_log
    );
    assert!(!tui.feedback().contains("completion-obsolete"));
}

#[tokio::test]
#[serial]
async fn k6_fresh_cache_and_retired_snapshot_are_fenced() {
    use rustain::adapters::tui::refresh_tracker::RefreshTracker;
    use rustain::infrastructure::model_discovery::refresh_and_announce;

    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut config = openai(&base, true);
    config.cache_ttl_seconds = 3600;
    let mut tui = Tui::new(config_with(&[("openai", config)]));
    key_login(&mut tui, &listener, K1).await;
    let old = target(&tui);
    let tracker = RefreshTracker::new();
    let report = tokio::time::timeout(
        crate::conformance_20_1e_tui_login::common::GUARD,
        refresh_and_announce(
            &old,
            tui.live.cache(),
            tui.live.registry(),
            false,
            &tui.ctx.events,
            &tracker,
        ),
    )
    .await
    .expect("fresh cache must complete without a network response");
    assert!(report.published);
    assert_eq!(report.status, DiscoveryStatus::Ok);

    let pause = old
        .refresh_order
        .pause_at(RefreshCheckpoint::BeforePublication)
        .await;
    let live = tui.live.clone();
    let events = tui.ctx.events.clone();
    let old_for_task = old.clone();
    let cache_task = tokio::spawn(async move {
        refresh_and_announce(
            &old_for_task,
            live.cache(),
            live.registry(),
            false,
            &events,
            &tracker,
        )
        .await
    });
    pause.reached().await;
    logout(&mut tui).await;
    pause.release();
    assert!(!cache_task.await.unwrap().published);
    assert_eq!(tui.live.registry().discovery_status("openai"), None);
    let held_b = begin_login(&mut tui, &listener, K2).await;
    held_b.json(&listing(&["replacement-current"])).await;
    tui.until_login("replacement discovery", |e| {
        matches!(e, LoginEvent::Discovered { .. })
    })
    .await;
    let tracker = RefreshTracker::new();
    let stale = refresh_and_announce(
        &old,
        tui.live.cache(),
        tui.live.registry(),
        true,
        &tui.ctx.events,
        &tracker,
    )
    .await;
    assert!(!stale.published);
    assert_eq!(models(&tui), ["replacement-current"]);
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
}

#[tokio::test]
#[serial]
async fn k2_replacement_invalidates_before_refresh_starts() {
    for outcome in 0..3 {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        let (listener, base) = stub().await;
        let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
        key_login(&mut tui, &listener, K1).await;
        let before = tui.ctx.store.snapshot().await.unwrap();
        let a = tick(&tui);
        let held_a = request(&listener, K1).await;
        store_login(&tui, &listener, K2).await;
        let replacements = tui.live.register_change("openai", &tui.app, &before).await;
        assert_eq!(replacements.len(), 1);
        let baseline = models(&tui);
        let cache_before = tui.live.cache().load().await;
        tui.drain();
        tui.event_log.clear();
        match outcome {
            0 => held_a.json(&listing(&["obsolete-success"])).await,
            1 => held_a.json(&listing(&[])).await,
            _ => reject(held_a).await,
        }
        a.await.unwrap();
        tui.drain();
        assert_eq!(
            tui.live.registry().discovery_status("openai"),
            Some(DiscoveryStatus::Idle)
        );
        assert_eq!(models(&tui), baseline);
        assert_eq!(
            serde_json::to_value(tui.live.cache().load().await).unwrap(),
            serde_json::to_value(cache_before).unwrap()
        );
        no_rejection(&tui);
        assert!(
            !tui.event_log.iter().any(|e| e.contains("SystemNotice")),
            "{:?}",
            tui.event_log
        );
        let live = tui.live.clone();
        let b = tokio::spawn(async move { live.refresh_changed(replacements).await });
        request(&listener, K2)
            .await
            .json(&listing(&["registered-new"]))
            .await;
        assert!(b.await.unwrap()[0].1.published);
        assert_eq!(models(&tui), ["registered-new"]);
        assert_eq!(
            tui.live.registry().discovery_status("openai"),
            Some(DiscoveryStatus::Ok)
        );
    }
}

#[tokio::test]
#[serial]
async fn k4a_admitted_publication_precedes_retirement() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    tui.event_log.clear();
    let old = target(&tui);
    let announcement = old
        .refresh_order
        .pause_at(RefreshCheckpoint::BeforeAnnouncement)
        .await;
    let waiting = old
        .refresh_order
        .pause_at(RefreshCheckpoint::RetirementWaiting)
        .await;
    let task = tick(&tui);
    reject(request(&listener, K1).await).await;
    announcement.reached().await;
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Unauthenticated),
        "pause is after status publication and immediately before its warning"
    );
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Enter);
    waiting.reached().await;
    waiting.release();
    announcement.release();
    task.await.unwrap();
    until_changed(&mut tui).await;
    tui.drain();
    let warning = tui
        .event_log
        .iter()
        .position(|e| e.contains(&rejected_notice(&old)))
        .expect("admitted rejection warning");
    let retirement = tui
        .event_log
        .iter()
        .position(|e| e.contains("AccountsChanged"))
        .expect("retirement acknowledgement");
    assert!(
        warning < retirement,
        "warning must precede retirement: {:?}",
        tui.event_log
    );
    assert_eq!(tui.live.registry().discovery_status("openai"), None);
}

#[tokio::test]
#[serial]
async fn k3_cancelled_successor_does_not_resurrect_old_success() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let baseline = models(&tui);
    let a = tick(&tui);
    let held_a = request(&listener, K1).await;
    let b = tick(&tui);
    let held_b = request(&listener, K1).await;
    b.abort();
    assert!(b.await.unwrap_err().is_cancelled());
    drop(held_b);
    held_a
        .json(&listing(&["cancelled-successor-obsolete"]))
        .await;
    a.await.unwrap();
    tui.drain();
    assert_eq!(models(&tui), baseline);
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
    no_rejection(&tui);
}

#[tokio::test]
#[serial]
async fn k5_scoped_live_memory_survives_failed_persist_and_replacement() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let lock = world.dir.path().join("models_cache.json.lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    let refresh = tick(&tui);
    request(&listener, K1)
        .await
        .json(&listing(&["memory-newest"]))
        .await;
    refresh.await.unwrap();
    assert_eq!(models(&tui), ["memory-newest"]);
    assert!(
        !tui.live.cache().load().await.providers["openai"]
            .models
            .iter()
            .any(|m| m.model_id == "memory-newest")
    );
    let before = tui.ctx.store.snapshot().await.unwrap();
    store_login(&tui, &listener, K1).await;
    let replacements = tui.live.register_change("openai", &tui.app, &before).await;
    assert_eq!(replacements.len(), 1);
    tui.live.remerge().await;
    assert_eq!(models(&tui), ["memory-newest"]);
    let before = tui.ctx.store.snapshot().await.unwrap();
    store_login(&tui, &listener, K2).await;
    let replacements = tui.live.register_change("openai", &tui.app, &before).await;
    assert_eq!(replacements.len(), 1);
    tui.live.remerge().await;
    assert!(!models(&tui).iter().any(|m| m == "memory-newest"));
    assert!(
        tui.live
            .discovery_targets()
            .iter()
            .any(|t| t.provider_id == "openai")
    );
    assert!(!tui.state.model_selector.refreshing.contains("openai"));
    let live = tui.live.clone();
    let b = tokio::spawn(async move { live.refresh_changed(replacements).await });
    request(&listener, K2)
        .await
        .json(&listing(&["scoped-new"]))
        .await;
    assert!(b.await.unwrap()[0].1.published);
    assert_eq!(models(&tui), ["scoped-new"]);
}

#[tokio::test]
#[serial]
async fn k5_other_provider_remains_current_when_openai_relocks() {
    use crate::conformance_20_1e_tui_login::common::stubai;
    use rustain::adapters::tui::refresh_tracker::RefreshTracker;
    use rustain::infrastructure::model_discovery::refresh_and_announce;

    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("STUBAI_API_KEY", "independent-key");
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[
        ("openai", openai(&base, true)),
        ("stubai", stubai(&base)),
    ]));
    key_login(&mut tui, &listener, K1).await;
    let q = tui
        .live
        .discovery_targets()
        .iter()
        .find(|t| t.provider_id == "stubai")
        .unwrap()
        .clone();
    let live = tui.live.clone();
    let events = tui.ctx.events.clone();
    let tracker = RefreshTracker::new();
    let observed = tracker.clone();
    let q_task = tokio::spawn(async move {
        refresh_and_announce(&q, live.cache(), live.registry(), true, &events, &tracker).await
    });
    let held = accept(&listener).await;
    assert_eq!(held.path(), "/stubai/v1/models");
    assert_eq!(bearer(&held), Some("Bearer independent-key".into()));
    assert!(observed.contains("stubai"));
    logout(&mut tui).await;
    held.json(&listing(&["independent-current"])).await;
    assert!(q_task.await.unwrap().published);
    assert!(!observed.contains("stubai"));
    assert_eq!(
        tui.live.registry().discovery_status("stubai"),
        Some(DiscoveryStatus::Ok)
    );
    let q = tui
        .live
        .discovery_targets()
        .iter()
        .find(|t| t.provider_id == "stubai")
        .unwrap()
        .clone();
    assert_eq!(
        q.provider
            .list_models()
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["independent-current"]
    );
}

#[tokio::test]
#[serial]
async fn k6_fresh_cache_loses_to_later_admitted_attempt() {
    use rustain::adapters::tui::refresh_tracker::RefreshTracker;
    use rustain::infrastructure::model_discovery::refresh_and_announce;
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut cfg = openai(&base, true);
    cfg.cache_ttl_seconds = 3600;
    let mut tui = Tui::new(config_with(&[("openai", cfg)]));
    key_login(&mut tui, &listener, K1).await;
    let old = target(&tui);
    let pause = old
        .refresh_order
        .pause_at(RefreshCheckpoint::BeforePublication)
        .await;
    let live = tui.live.clone();
    let events = tui.ctx.events.clone();
    let a = tokio::spawn(async move {
        refresh_and_announce(
            &old,
            live.cache(),
            live.registry(),
            false,
            &events,
            &RefreshTracker::new(),
        )
        .await
    });
    pause.reached().await;
    let b = tick(&tui);
    let held = request(&listener, K1).await;
    let baseline = models(&tui);
    pause.release();
    assert!(!a.await.unwrap().published);
    assert_eq!(models(&tui), baseline);
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
    held.json(&listing(&["cache-successor"])).await;
    b.await.unwrap();
    assert_eq!(models(&tui), ["cache-successor"]);
}

#[tokio::test]
#[serial]
async fn k4b_retirement_first_excludes_obsolete_successful_commit() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let cache_before = tui.live.cache().load().await;
    let order = target(&tui).refresh_order;
    let pause = order.pause_at(RefreshCheckpoint::BeforePublication).await;
    let writer = order.pause_at(RefreshCheckpoint::BlockingCommit).await;
    let task = tick(&tui);
    request(&listener, K1)
        .await
        .json(&listing(&["must-not-commit"]))
        .await;
    pause.reached().await;
    logout(&mut tui).await;
    pause.release();
    task.await.unwrap();
    assert!(
        !writer.was_reached(),
        "retired result must not enter the blocking writer"
    );
    assert_eq!(
        serde_json::to_value(tui.live.cache().load().await).unwrap(),
        serde_json::to_value(cache_before).unwrap()
    );
    assert_eq!(tui.live.registry().discovery_status("openai"), None);
    tui.drain();
    no_rejection(&tui);
}

#[tokio::test]
#[serial]
async fn k4c_current_login_completion_is_emitted() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    let held = begin_login(&mut tui, &listener, K1).await;
    let order = target(&tui).refresh_order;
    let before = order.pause_at(RefreshCheckpoint::BeforeCompletion).await;
    let finished = order.pause_at(RefreshCheckpoint::CompletionFinished).await;
    held.json(&listing(&["completion-current"])).await;
    before.reached().await;
    before.release();
    finished.reached().await;
    finished.release();
    tui.until_login("current discovery line", |e| {
        matches!(e, LoginEvent::Discovered { .. })
    })
    .await;
    assert_eq!(models(&tui), ["completion-current"]);
}

#[tokio::test]
#[serial]
async fn k4a_successor_admission_fences_paused_rejection() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    tui.event_log.clear();
    let pause = target(&tui)
        .refresh_order
        .pause_at(RefreshCheckpoint::BeforePublication)
        .await;
    let a = tick(&tui);
    reject(request(&listener, K1).await).await;
    pause.reached().await;
    let b = tick(&tui);
    let held_b = request(&listener, K1).await;
    pause.release();
    a.await.unwrap();
    tui.drain();
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );
    no_rejection(&tui);
    held_b.json(&listing(&["admitted-successor"])).await;
    b.await.unwrap();
    assert_eq!(models(&tui), ["admitted-successor"]);
}

#[tokio::test]
#[serial]
async fn k4b_current_successful_blocking_commit_publishes() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let order = target(&tui).refresh_order;
    let writer = order.pause_at(RefreshCheckpoint::BlockingCommit).await;
    let committed = order
        .pause_at(RefreshCheckpoint::BlockingCommitFinished)
        .await;
    let task = tick(&tui);
    request(&listener, K1)
        .await
        .json(&listing(&["current-commit"]))
        .await;
    writer.reached().await;
    writer.release();
    committed.reached().await;
    committed.release();
    task.await.unwrap();
    assert_eq!(models(&tui), ["current-commit"]);
    assert!(
        tui.live.cache().load().await.providers["openai"]
            .models
            .iter()
            .any(|m| m.model_id == "current-commit")
    );
}
