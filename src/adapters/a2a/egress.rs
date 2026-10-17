//! Shared outbound A2A composition for standalone and daemon roots.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use crate::domain::events::AppEvent;
use crate::domain::models::A2aPeerSpec;
use crate::domain::ports::{CapabilityProvider, RoomJournal};
use crate::infrastructure::subagent::NodeTree;

use super::client::A2aClientAdapter;
use super::driver::A2aDelegationRuntime;
use super::item_capability::ItemCapabilityKeyState;
use super::provider::A2aProvider;

/// The one outbound A2A runtime shared by a composition root's provider and
/// capability composite. AgentCard refreshes are deliberately un-awaited: a
/// peer that is still offline at boot remains unavailable until a later boot.
#[derive(Clone)]
pub struct A2aEgress {
    provider: Arc<A2aProvider>,
    runtime: Arc<A2aDelegationRuntime>,
}

impl A2aEgress {
    /// Compose the outbound runtime. Story 19.18c `A3`/`PQ1`: the root loads
    /// `item_capability` (the one loader helper,
    /// [`super::item_capability::load_item_capability_key`]) and hands it in;
    /// ⛔ compose never touches the filesystem for it, and an unusable key is
    /// never a compose error.
    pub fn compose(
        peers: Vec<A2aPeerSpec>,
        node_tree: NodeTree,
        journal: Arc<dyn RoomJournal>,
        journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        event_tx: UnboundedSender<AppEvent>,
        item_capability: ItemCapabilityKeyState,
    ) -> Result<Self> {
        Self::compose_with_clock(
            peers,
            node_tree,
            journal,
            journal_reader,
            event_tx,
            item_capability,
            Arc::new(crate::domain::clock::SystemClock::default()),
        )
    }

    /// [`Self::compose`] with the wall clock an action board's age is measured
    /// against supplied (Story 19.21 `AC3(c)`, `A14`). Production reaches this
    /// through [`Self::compose`] with the system clock; a keystone injects a
    /// controllable clock, so an age is a deterministic function of the
    /// journal's recorded time and this clock — never a sleep.
    pub fn compose_with_clock(
        peers: Vec<A2aPeerSpec>,
        node_tree: NodeTree,
        journal: Arc<dyn RoomJournal>,
        journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        event_tx: UnboundedSender<AppEvent>,
        item_capability: ItemCapabilityKeyState,
        clock: Arc<dyn crate::domain::clock::Clock>,
    ) -> Result<Self> {
        let mut bindings = Vec::with_capacity(peers.len());
        for spec in peers {
            let client = Arc::new(A2aClientAdapter::new(&spec, None).map_err(|error| {
                anyhow::anyhow!("A2A peer {:?} configuration failed: {error}", spec.id)
            })?);
            bindings.push((spec, client));
        }

        let provider = Arc::new(A2aProvider::new(bindings));
        let runtime = Arc::new(
            A2aDelegationRuntime::new(node_tree, journal, event_tx.clone())
                .with_peer_bindings(provider.peer_bindings())
                .with_journal_reader(journal_reader)
                .with_item_capability(item_capability)
                .with_clock(clock),
        );
        provider.set_delegation_runtime(runtime.clone());

        for (spec, client) in provider.peer_bindings().iter() {
            let spec = spec.clone();
            let client = client.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                match client.refresh_agent_card(&spec).await {
                    Ok(()) => {
                        let skill_count = client
                            .ready_card()
                            .await
                            .map(|(card, _)| card.skills.len())
                            .unwrap_or(0);
                        let _ = event_tx.send(AppEvent::A2aCatalogChanged {
                            peer_id: spec.id,
                            skill_count,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            peer_id = %spec.id,
                            %error,
                            "A2A AgentCard refresh failed"
                        );
                    }
                }
            });
        }

        Ok(Self { provider, runtime })
    }

    pub fn install(
        &self,
        composite: &CompositeToolsetAdapter,
        proposal_sink: Option<Arc<dyn crate::domain::ports::TeamProposalSink>>,
    ) {
        composite.set_a2a_provider(self.provider.clone() as Arc<dyn CapabilityProvider>);
        composite.set_team_proposal_tool(Arc::new(
            super::team_proposal_tool::TeamProposalTool::new(self.runtime.clone(), proposal_sink),
        ));
    }

    pub fn provider(&self) -> &Arc<A2aProvider> {
        &self.provider
    }

    pub fn runtime(&self) -> &Arc<A2aDelegationRuntime> {
        &self.runtime
    }

    /// Await every boot card fetch's **completion** (not its success) — the
    /// `settled` signal, event-driven, so a test or caller never sleeps or
    /// polls (Story 19.16b review: the board keystones need the slots to have
    /// left `Pending` before `collect_board` reads them).
    pub async fn await_cards_settled(&self) {
        for (_, client) in self.provider.peer_bindings().iter() {
            client.await_settled().await;
        }
    }
}

/// Story 19.14 `AC3` — a configured anchor is the **sole** trust for that peer,
/// on an explicitly chosen backend.
///
/// Front door: `A2aEgress::compose`, observed in-crate through `peer_bindings`.
/// ⛔ Never `A2aClientAdapter::new` with a hand-built spec and ⛔ never a
/// hand-built `ClientBuilder` — the thing under test is what *production*
/// composition hands the builder.
#[cfg(test)]
mod trust_anchor_tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::adapters::a2a::client::TrustStepKind;
    use crate::adapters::a2a::error::{AnchorCause, AnchorFailure};
    use crate::adapters::a2a::test_fixtures::{
        PeerFixture, RpcAnswer, TestLeaf, Validity, leaf_issued_by, self_signed_leaf, test_ca,
        unreachable_origin, write_pem,
    };
    use crate::adapters::a2a::{client::CardSlot, send::deliver_text};
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, RecipientOutcome, RedactedUrl, RoomEvent,
    };
    use crate::domain::ports::{RoomJournal, RoomJournalError};

    use super::*;

    const SETTLE: Duration = Duration::from_secs(20);

    struct AcceptingJournal;

    #[async_trait]
    impl RoomJournal for AcceptingJournal {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            Ok(())
        }
    }

    #[async_trait]
    impl crate::domain::ports::RoomJournalReader for AcceptingJournal {
        async fn load_entries(
            &self,
        ) -> Result<Vec<crate::domain::models::JournalEntry>, RoomJournalError> {
            Ok(Vec::new())
        }

        async fn latest_seq(&self) -> Result<u64, RoomJournalError> {
            Ok(0)
        }
    }

    fn peer(alias: &str, origin: &str, anchor: Option<std::path::PathBuf>) -> A2aPeerSpec {
        A2aPeerSpec::new(alias, RedactedUrl::from(origin), A2aPeerSource::Workspace)
            .with_ca_cert(anchor)
    }

    /// Compose through production and wait for every boot fetch to settle.
    ///
    /// ⛔ No sleep and no poll loop: a failed boot fetch emits nothing, so the
    /// settle signal is the only event-driven way to know it finished. The outer
    /// timeout turns a hang into a failure instead of a hang.
    async fn composed(peers: Vec<A2aPeerSpec>) -> A2aEgress {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        // Story 19.18c: a production-shaped `Ready` key from a throwaway
        // store — these keystones are about trust anchors, not capabilities.
        let keys = tempfile::tempdir().expect("item capability key dir");
        let egress = A2aEgress::compose(
            peers,
            NodeTree::new(),
            Arc::new(AcceptingJournal),
            Arc::new(AcceptingJournal),
            event_tx,
            crate::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
                .path()
                .to_path_buf())),
        )
        .expect("compose must not fail for an unloadable anchor or an unset variable");
        for (_, client) in egress.provider().peer_bindings().iter() {
            tokio::time::timeout(SETTLE, client.await_settled())
                .await
                .expect("boot card fetch settled");
        }
        egress
    }

    async fn slot(egress: &A2aEgress, alias: &str) -> CardSlot {
        let bindings = egress.provider().peer_bindings();
        let (_, client) = bindings
            .iter()
            .find(|(spec, _)| spec.id == alias)
            .expect("peer is bound");
        client.card_slot().await
    }

    /// The retained validation sub-cause, if the slot holds one.
    async fn validation_failure(egress: &A2aEgress, alias: &str) -> Option<AnchorFailure> {
        match slot(egress, alias).await {
            CardSlot::AnchorRefused(AnchorCause::Validation(failure)) => Some(failure),
            _ => None,
        }
    }

    fn trust_record(egress: &A2aEgress, alias: &str) -> Vec<TrustStepKind> {
        let bindings = egress.provider().peer_bindings();
        let (_, client) = bindings
            .iter()
            .find(|(spec, _)| spec.id == alias)
            .expect("peer is bound");
        client.trust_steps().to_vec()
    }

    /// The typed refusal cause delivered to the command renderer.
    async fn refusal(egress: &A2aEgress, alias: &str) -> String {
        let RecipientOutcome::Unreachable { cause: Some(cause) } =
            deliver_text(egress.runtime(), alias, "hello", None, None).await
        else {
            panic!("an anchored refusal must be an unreachable outcome with a cause");
        };
        cause.as_str().to_owned()
    }

    /// `AC3` part 3 and clause (a): the ONLY evidence that an anchored peer does
    /// not *also* trust the platform roots. `reqwest` exposes no root-store
    /// inspection, so a behavioural test cannot see it — a different local CA is
    /// refused either way.
    #[tokio::test]
    async fn an_anchored_peer_forces_rustls_drops_built_in_roots_and_adds_one_root_per_block() {
        let ca = test_ca("19-14 record CA", Validity::Current);
        let leaf = leaf_issued_by(&ca, "19-14 record leaf", "localhost", Validity::Current);
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        // Two CERTIFICATE blocks in one file: the record must count blocks, not
        // files.
        let second = test_ca("19-14 record CA two", Validity::Current);
        let bundle = write_pem(
            dir.path(),
            "ca.pem",
            &format!("{}{}", ca.anchor_pem, second.anchor_pem),
        );

        let egress = composed(vec![peer("anchored", &fixture.origin, Some(bundle))]).await;

        assert_eq!(
            trust_record(&egress, "anchored"),
            vec![
                TrustStepKind::ForceRustls,
                TrustStepKind::BuiltInRoots(false),
                TrustStepKind::AddRoot,
                TrustStepKind::AddRoot,
            ],
            "an anchored peer must force rustls, drop the built-in roots, and add \
             exactly one root per CERTIFICATE block"
        );
        assert!(
            matches!(slot(&egress, "anchored").await, CardSlot::Ready(..)),
            "the anchor validates a server presenting a certificate issued under it"
        );
    }

    /// `AC3` clause (b)/(c): an unpinned peer's builder is untouched, which is
    /// also the evidence of record for the "force rustls for every peer" mutant.
    #[tokio::test]
    async fn an_unpinned_peer_has_no_trust_step_applied_at_all() {
        let fixture = PeerFixture::plaintext().await;
        let egress = composed(vec![peer("unpinned", &fixture.origin, None)]).await;

        assert!(
            trust_record(&egress, "unpinned").is_empty(),
            "⛔ no trust setting may reach the builder for a peer without `caCert`"
        );
        assert!(
            matches!(slot(&egress, "unpinned").await, CardSlot::Ready(..)),
            "an unpinned loopback peer is unchanged and still discovers its card"
        );
    }

    /// `AC3` part 2: the behavioural pair. ⚠ This pair CANNOT show the platform
    /// roots are gone — a different local CA is refused either way — which is
    /// why part 3's record exists.
    #[tokio::test]
    async fn a_leaf_from_a_different_ca_is_a_mismatch_while_the_anchors_own_leaf_succeeds() {
        let ca = test_ca("19-14 pair CA", Validity::Current);
        let other = test_ca("19-14 pair other CA", Validity::Current);
        let foreign = leaf_issued_by(
            &other,
            "19-14 pair foreign leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&foreign).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);

        let egress = composed(vec![peer("foreign", &fixture.origin, Some(anchor))]).await;

        assert_eq!(
            validation_failure(&egress, "foreign").await,
            Some(AnchorFailure::Mismatch),
            "a leaf from an unrelated CA is `UnknownIssuer`, which maps to Mismatch"
        );
        assert_eq!(
            refusal(&egress, "foreign").await,
            "foreign's certificate does not match the pinned anchor"
        );
    }

    /// `AC3(g)` — characterization, through production. ⚠ An **expired root**
    /// still anchors a valid leaf (RFC 5280 §6.1.1; webpki's trust anchor omits
    /// validity). ⛔ Not enforced here — `DF-19-14-ANCHOR-EXPIRY-UNENFORCED`.
    /// Also backend-discriminating: native-tls refuses this pair.
    #[tokio::test]
    async fn an_expired_root_still_anchors_a_valid_leaf() {
        let ca = test_ca("19-14 expired root CA", Validity::Expired);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 expired root leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);

        let egress = composed(vec![peer("expired-root", &fixture.origin, Some(anchor))]).await;

        assert!(
            matches!(slot(&egress, "expired-root").await, CardSlot::Ready(..)),
            "an expired root anchor is STILL TRUSTED — documented, not enforced"
        );
    }

    /// `AC3(g)` — the other half: an expired **leaf** under a valid anchor is
    /// refused, and as `OutsideValidity` rather than a generic mismatch. ⚠ The
    /// fixture is a real expired certificate, so rustls emits `ExpiredContext`.
    #[tokio::test]
    async fn an_expired_leaf_under_a_valid_anchor_is_outside_its_validity() {
        let ca = test_ca("19-14 expired leaf CA", Validity::Current);
        let leaf = leaf_issued_by(&ca, "19-14 expired leaf", "localhost", Validity::Expired);
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);

        let egress = composed(vec![peer("stale", &fixture.origin, Some(anchor))]).await;

        assert_eq!(
            validation_failure(&egress, "stale").await,
            Some(AnchorFailure::OutsideValidity),
            "`ExpiredContext` must not collapse into Mismatch"
        );
        assert_eq!(
            refusal(&egress, "stale").await,
            "stale's certificate has expired or is not yet valid"
        );
    }

    /// `AC3(h)` row: a **non-CA** self-signed leaf pinned as itself validates.
    #[tokio::test]
    async fn a_non_ca_self_signed_leaf_pinned_as_itself_validates() {
        let leaf = self_signed_leaf("19-14 self leaf", "localhost", rcgen::IsCa::ExplicitNoCa);
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &leaf.cert_pem);

        let egress = composed(vec![peer("self-signed", &fixture.origin, Some(anchor))]).await;

        assert!(
            matches!(slot(&egress, "self-signed").await, CardSlot::Ready(..)),
            "a non-CA self-signed leaf pinned as itself is a working anchor form"
        );
    }

    /// `AC3(h)` row + the version-split ratchet: a `CA:TRUE` self-signed
    /// certificate — what `openssl req -x509` produces **by default** — pinned as
    /// itself is REFUSED. Asserts the **variant**, so a webpki split that made the
    /// downcast return `None` would be caught here rather than silently degrading
    /// form 8 to form 5. Also backend-discriminating: native-tls accepts this.
    #[tokio::test]
    async fn a_ca_true_self_signed_certificate_pinned_as_itself_is_refused() {
        let leaf = self_signed_leaf(
            "19-14 ca-true leaf",
            "localhost",
            rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained),
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &leaf.cert_pem);

        let egress = composed(vec![peer("ca-true", &fixture.origin, Some(anchor))]).await;

        assert_eq!(
            validation_failure(&egress, "ca-true").await,
            Some(AnchorFailure::CaAsServerCert),
            "a CA certificate served as its own end-entity certificate is \
             `CaUsedAsEndEntity`, reached ONLY by downcasting to `webpki::Error`"
        );
        assert_eq!(
            refusal(&egress, "ca-true").await,
            "ca-true presents a CA certificate as its server certificate"
        );
    }

    /// Form 7: a leaf issued for another hostname is in the anchor class, and it
    /// is its own sub-cause — ⛔ not "does not match the pinned anchor", which
    /// would send the operator to re-pin a file that is fine.
    #[tokio::test]
    async fn a_leaf_for_another_hostname_is_refused_as_wrong_name() {
        let ca = test_ca("19-14 wrong-name CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 wrong-name leaf",
            "elsewhere.invalid",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);

        let egress = composed(vec![peer("misnamed", &fixture.origin, Some(anchor))]).await;

        assert_eq!(
            validation_failure(&egress, "misnamed").await,
            Some(AnchorFailure::WrongName),
        );
        assert_eq!(
            refusal(&egress, "misnamed").await,
            "misnamed's certificate is not valid for its roster address"
        );
    }

    /// `AC3(f)` / `AC4(c′)`: a file with zero `CERTIFICATE` blocks is form 9 with
    /// `pem_tls`'s own reason — ⛔ never a successful load, ⛔ never form 5 — and
    /// **no client is built**, which is provable only from the server's side:
    /// a fallback client's boot handshake would reach the socket.
    #[tokio::test]
    async fn a_pem_with_no_certificate_block_refuses_without_ever_reaching_the_socket() {
        let ca = test_ca("19-14 empty-bundle CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 empty-bundle leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        // A key-only PEM: `Certificate::from_pem_bundle` would answer `Ok(0)` for
        // this, install zero anchors, and then report every handshake as a
        // mismatch.
        let anchor = write_pem(dir.path(), "key-only.pem", &leaf.key_pem);

        let egress = composed(vec![peer("unloadable", &fixture.origin, Some(anchor))]).await;

        let CardSlot::AnchorRefused(AnchorCause::Unloadable { reason }) =
            slot(&egress, "unloadable").await
        else {
            panic!("a key-only PEM must retain an Unloadable cause, not a validation failure");
        };
        assert!(
            reason.contains("contains no CERTIFICATE block") && reason.contains("key-only.pem"),
            "the reason must be pem_tls's shipped vocabulary, naming the path: {reason}"
        );
        assert_eq!(
            refusal(&egress, "unloadable").await,
            format!("unloadable's pinned anchor could not be loaded: {reason}")
        );
        assert_eq!(
            fixture.accepted_connections(),
            0,
            "⛔ no client is built when the anchor cannot load: a platform-roots \
             fallback would reach this socket before failing"
        );
        assert!(
            trust_record(&egress, "unloadable").is_empty(),
            "no step list survives a load failure"
        );
    }

    /// `AC4` positive control: an anchored peer that is simply **unreachable** at
    /// boot leaves the slot `Unavailable` and keeps today's `CardNotCached` — ⛔ it
    /// is not reported as a certificate failure. A refused TCP connect is also
    /// `is_connect() == true`, so this is the row that proves the classifier
    /// distinguishes them.
    #[tokio::test]
    async fn an_unreachable_anchored_peer_keeps_todays_card_not_cached_refusal() {
        let ca = test_ca("19-14 unreachable CA", Validity::Current);
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);
        let origin = unreachable_origin("https").await;

        let egress = composed(vec![peer("offline", &origin, Some(anchor))]).await;

        assert!(
            matches!(slot(&egress, "offline").await, CardSlot::Unavailable),
            "a connect refusal is NOT an anchor failure"
        );
        assert!(
            refusal(&egress, "offline")
                .await
                .contains("is configured but its AgentCard is not cached"),
            "the shipped CardNotCached text is byte-identical for a non-anchor failure"
        );
    }

    /// `AC4(c)`: the classifier runs on the RPC's own handshake too, not only on
    /// the boot card GET — a certificate rotated **after** a successful boot is
    /// refused by name.
    #[tokio::test]
    async fn a_certificate_rotated_after_boot_is_classified_on_the_rpc_handshake() {
        let ca = test_ca("19-14 rotation CA", Validity::Current);
        let leaf = leaf_issued_by(&ca, "19-14 rotation leaf", "localhost", Validity::Current);
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);

        let egress = composed(vec![peer("rotated", &fixture.origin, Some(anchor))]).await;
        assert!(
            matches!(slot(&egress, "rotated").await, CardSlot::Ready(..)),
            "boot must validate before the rotation means anything"
        );

        // A FRESH ServerConfig under a different CA. ⚠ Reusing the old config
        // would let TLS 1.3 resumption skip certificate validation entirely.
        let rogue = test_ca("19-14 rotation rogue CA", Validity::Current);
        let rotated: TestLeaf = leaf_issued_by(
            &rogue,
            "19-14 rotation rogue leaf",
            "localhost",
            Validity::Current,
        );
        fixture.rotate(&rotated).await;
        fixture.answer_with(RpcAnswer::Completed).await;

        assert_eq!(
            refusal(&egress, "rotated").await,
            "rotated's certificate does not match the pinned anchor",
            "the RPC's own handshake must be classified, not just the boot GET"
        );
    }
}

/// Story 19.18c in-crate units through `A2aEgress::compose`: the compose half
/// of `K15`, and the `K03`/`K06b` wire-shape units on the extended
/// `PeerFixture` (`P11`). ⛔ Wire shape only — the recorded request body —
/// never an end-to-end claim; those live in
/// `tests/conformance_19_18c_a2a_item_capability.rs`.
#[cfg(test)]
mod item_capability_tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::adapters::a2a::item_capability::{
        ItemCapabilityKeyState, ItemCapabilityKeyStore, derive_item_capability,
        load_item_capability_key,
    };
    use crate::adapters::a2a::lifecycle::A2aTaskTransport;
    use crate::adapters::a2a::test_fixtures::PeerFixture;
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, DispatchAct, JournalEntry, JournalRecord, RedactedUrl,
        RoomEvent, alias_pseudonym,
    };
    use crate::domain::ports::{RoomJournal, RoomJournalError, RoomJournalReader};

    use super::*;

    const SETTLE: Duration = Duration::from_secs(20);

    /// A sender journal holding seeded rows; appends are kept too.
    #[derive(Default)]
    struct SeededJournal(tokio::sync::Mutex<Vec<JournalEntry>>);

    #[async_trait]
    impl RoomJournal for SeededJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            let mut rows = self.0.lock().await;
            let seq = rows.len() as u64 + 1;
            rows.push(JournalEntry::new(seq, JournalRecord::Room(event), 0));
            Ok(())
        }
    }

    #[async_trait]
    impl RoomJournalReader for SeededJournal {
        async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
            Ok(self.0.lock().await.clone())
        }

        async fn latest_seq(&self) -> Result<u64, RoomJournalError> {
            Ok(self.0.lock().await.len() as u64)
        }
    }

    fn peer(alias: &str, origin: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(alias, RedactedUrl::from(origin), A2aPeerSource::Workspace)
    }

    async fn compose(
        peers: Vec<A2aPeerSpec>,
        journal: Arc<SeededJournal>,
        state: ItemCapabilityKeyState,
    ) -> A2aEgress {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let egress = A2aEgress::compose(
            peers,
            NodeTree::new(),
            journal.clone(),
            journal,
            event_tx,
            state,
        )
        .expect("an unusable key is never a compose error");
        tokio::time::timeout(SETTLE, egress.await_cards_settled())
            .await
            .expect("boot card fetch settled");
        egress
    }

    fn dispatched(peer: &str, task: &str, endpoint: Option<&str>) -> RoomEvent {
        RoomEvent::RemoteEnvelopeDispatched {
            peer: alias_pseudonym(peer),
            task: Some(task.to_owned()),
            bytes: 1,
            act: DispatchAct::Task,
            content_hash: None,
            endpoint: endpoint.map(str::to_owned),
            action: None,
            authorship: crate::domain::models::ReplyAuthorship::Unknown,
        }
    }

    /// `K15` (compose half, `P6`/`P19`) — an unusable key composes `Ok`; the
    /// refusal is per send, credentialed peers only (`PQ1`).
    #[tokio::test]
    async fn k15_an_unusable_key_composes_and_refuses_only_credentialed_peers() {
        let fixture = PeerFixture::plaintext().await;
        let credentialed = peer("b", &fixture.origin).with_auth(Some("UNSET_19_18C".to_owned()));
        let open = peer("l", &fixture.origin);
        let egress = compose(
            vec![credentialed.clone(), open.clone()],
            Arc::new(SeededJournal::default()),
            ItemCapabilityKeyState::Unusable {
                path: "/keys/item-capability.key".to_owned(),
                cause: "item capability key file must be exactly 32 bytes, found 31".to_owned(),
            },
        )
        .await;
        let refusal = egress
            .runtime()
            .outbound_item_key(&credentialed)
            .expect_err("a credentialed peer is refused");
        assert_eq!(
            refusal.to_string(),
            "this host's item key can't be read (/keys/item-capability.key: item capability key \
             file must be exactly 32 bytes, found 31). Fix its permissions (chmod 600) or restore \
             it; don't delete it."
        );
        assert!(
            egress
                .runtime()
                .outbound_item_key(&open)
                .expect("an uncredentialed peer sends legacy")
                .is_none()
        );
    }

    /// `K03` (`A9`, `P8`) — a transport bound to its own `(messageId, c)`
    /// presents `c` in `params.metadata` for THAT id only, on `tasks/get` and
    /// `tasks/cancel`; a peer-chosen id gets nothing. `M04` → RED: derive and
    /// attach for any polled id. `M20b` → RED: bind `tasks_get` only.
    #[tokio::test]
    async fn k03_a_bound_transport_presents_its_capability_for_its_own_id_only() {
        let fixture = PeerFixture::plaintext().await;
        let keys = tempfile::tempdir().expect("keys");
        let key = ItemCapabilityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("key");
        let (message, own) = super::super::driver::build_message(
            &serde_json::json!({ "message": "q" }),
            super::super::driver::MessageCarriers {
                item_key: Some(&key),
                ..Default::default()
            },
        );
        let own_id = message["message"]["messageId"]
            .as_str()
            .expect("id")
            .to_owned();
        let client =
            Arc::new(A2aClientAdapter::new(&peer("b", &fixture.origin), None).expect("client"));
        let transport =
            super::super::driver::TaskClient::new(client, format!("{}/", fixture.origin))
                .with_own_task(own.expect("bound"));
        transport.tasks_get(&own_id).await.expect("get");
        transport.tasks_get("rustain-2").await.expect("get");
        transport.tasks_cancel(&own_id).await.expect("cancel");
        transport.tasks_cancel("peer-chosen").await.expect("cancel");
        let expected = derive_item_capability(&key, &own_id);
        let gets = fixture.calls("tasks/get").await;
        let cancels = fixture.calls("tasks/cancel").await;
        assert_eq!(
            gets[0]["params"],
            serde_json::json!({
                "id": own_id,
                "metadata": { "x-rustain-item-capability": expected.as_wire_str() },
            })
        );
        assert_eq!(gets[1]["params"], serde_json::json!({ "id": "rustain-2" }));
        assert_eq!(
            cancels[0]["params"]["metadata"]["x-rustain-item-capability"],
            expected.as_wire_str()
        );
        assert_eq!(
            cancels[1]["params"],
            serde_json::json!({ "id": "peer-chosen" })
        );
    }

    /// `K06b` (`A9`/`A10`, `P13` positive control) — the resolver derives
    /// only for a task this host dispatched to THIS peer: a served task not
    /// in the ledger and a task dispatched to another peer yield no
    /// capability; an `rr_` id with no observed row yields none WITHOUT a
    /// list call; an own task yields `derive(K, task)` and the recorded
    /// thread request carries it. `M12` (front-door kill is `K06c`).
    #[tokio::test]
    async fn k06b_the_resolver_derives_only_for_an_own_task_of_this_peer() {
        let fixture = PeerFixture::plaintext().await;
        fixture
            .answer_method(
                crate::adapters::a2a::ITEMS_LIST_METHOD,
                serde_json::json!({ "items": [
                    { "itemId": "ri_own", "task": "t-own" },
                    { "itemId": "ri_foreign", "task": "t-foreign" },
                    { "itemId": "ri_other", "task": "t-other" },
                ]}),
            )
            .await;
        let journal = Arc::new(SeededJournal::default());
        journal
            .record_event(dispatched("b", "t-own", Some("any-origin")))
            .await
            .expect("seed");
        journal
            .record_event(dispatched("c", "t-other", Some("another-origin")))
            .await
            .expect("seed");
        let keys = tempfile::tempdir().expect("keys");
        let egress = compose(
            vec![peer("b", &fixture.origin)],
            journal,
            load_item_capability_key(Ok(keys.path().to_path_buf())),
        )
        .await;
        let key = ItemCapabilityKeyStore::new(keys.path())
            .load()
            .expect("key");
        let thread_params = || async {
            fixture
                .calls(crate::adapters::a2a::ITEMS_THREAD_METHOD)
                .await
                .last()
                .expect("a thread request")["params"]
                .clone()
        };

        let _ = super::super::send::read_thread(egress.runtime(), "b", "rr_unobserved").await;
        assert!(
            fixture
                .calls(crate::adapters::a2a::ITEMS_LIST_METHOD)
                .await
                .is_empty(),
            "an unobserved reply turn resolves nothing and reads no list"
        );
        assert_eq!(
            thread_params().await,
            serde_json::json!({ "itemId": "rr_unobserved" })
        );
        for foreign in ["ri_foreign", "ri_other"] {
            let _ = super::super::send::read_thread(egress.runtime(), "b", foreign).await;
            assert_eq!(
                thread_params().await,
                serde_json::json!({ "itemId": foreign }),
                "{foreign}: not this host's dispatch to THIS peer"
            );
        }
        let _ = super::super::send::read_thread(egress.runtime(), "b", "ri_own").await;
        assert_eq!(
            thread_params().await,
            serde_json::json!({
                "itemId": "ri_own",
                "capability": derive_item_capability(&key, "t-own").as_wire_str(),
            }),
            "positive control: an own task for this peer"
        );
    }
}
