//! Inbound-task admission — a Decision-Core (Story 18.0 pattern).
//!
//! [`admit`] is effect-free and value-returning: no I/O, no lock, no clock, no
//! `NodeTree` mutation, no journal write. Every effect the verdict implies is
//! lifted into the async shell in [`super::server`]. That split is what makes
//! NFR70 ("verify identity and authority **before** any mutation") provable
//! rather than asserted: a refusal cannot mutate anything, because the function
//! that decides refusals cannot mutate anything.

/// Re-exported so callers name one policy type. It lives in
/// [`super::config`] because that module parses without the `a2a` feature and a
/// misconfigured build must fail loudly rather than ignore the operator.
pub use super::config::{A2aAdmissionPolicy, ThreadReads};

/// Whether the submitter cleared the transport's credential gate, and how.
///
/// This is an *admission* signal only. It never decides what a task may do —
/// the executed node's authority is the same local `Peer` authority regardless.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitterTrust {
    /// Reached us over the plaintext loopback socket; no credential required.
    Loopback,
    /// Presented a valid API key on a TLS-protected non-loopback socket.
    ApiKey,
}

/// The effect-free inputs of an admission decision.
#[derive(Debug, Clone, Copy)]
pub struct AdmissionRequest<'a> {
    /// The submitted instruction text, already extracted from the A2A message.
    pub text: &'a str,
    /// Whether an execution core is wired behind this listener. A discovery-only
    /// deployment answers a *policy verdict*, not a lie about capability.
    pub executor_available: bool,
}

/// What the shell must do. Exhaustive by construction: there is no "maybe".
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionVerdict {
    /// Register the peer node and drive the turn now.
    Accept,
    /// A human must approve first. The shell raises the approval, answers
    /// `auth-required`, and resumes on grant — it does **not** wait here.
    AcceptPendingApproval,
    /// Refuse. `reason` is disclosed verbatim to the submitter, so it must name
    /// the policy, never host state.
    Reject { reason: String },
}

/// The maximum instruction length we will admit. Bounded here rather than in the
/// shell so the bound is testable without a socket; the body limit upstream is a
/// byte cap on the whole request, which a single enormous instruction fits under.
pub const MAX_TASK_TEXT_BYTES: usize = 64 * 1024;

// Story 19.18 `AC2(b)` — a reply turn is never larger than an inbound item.
const _: () = assert!(crate::domain::models::MAX_REPLY_BYTES <= MAX_TASK_TEXT_BYTES);

/// Decide whether to admit an inbound task. Pure.
#[must_use]
pub fn admit(
    request: &AdmissionRequest<'_>,
    policy: A2aAdmissionPolicy,
    trust: SubmitterTrust,
) -> AdmissionVerdict {
    // Order matters and is deliberate: capability, then well-formedness, then
    // policy. A discovery-only endpoint must not report "denied by policy" for a
    // request it could never have run, and policy must not be consulted for a
    // request that is not a task at all.
    if !request.executor_available {
        return AdmissionVerdict::Reject {
            reason: "this A2A endpoint serves discovery only; task execution requires an \
                     execution core (run with `rustain daemon start --serve-a2a=<addr>`)"
                .to_owned(),
        };
    }
    if request.text.trim().is_empty() {
        return AdmissionVerdict::Reject {
            reason: "inbound task carries no instruction text".to_owned(),
        };
    }
    if request.text.len() > MAX_TASK_TEXT_BYTES {
        return AdmissionVerdict::Reject {
            reason: format!(
                "inbound task instruction exceeds the {MAX_TASK_TEXT_BYTES}-byte admission limit"
            ),
        };
    }
    match policy {
        A2aAdmissionPolicy::Deny => {
            // `trust` is named so an operator reading a peer's transcript can
            // tell "your key was fine, the policy said no" apart from "your key
            // was wrong" — the latter never reaches this function at all.
            let credential = credential_label(trust);
            AdmissionVerdict::Reject {
                reason: format!(
                    "inbound A2A task acceptance is disabled by policy \
                     (`server.admission` = \"deny\"); credential accepted: {credential}"
                ),
            }
        }
        A2aAdmissionPolicy::Ask => AdmissionVerdict::AcceptPendingApproval,
        A2aAdmissionPolicy::Allow => AdmissionVerdict::Accept,
    }
}

/// Decide whether a peer may retract an item on this host (Story 19.16d,
/// `AC1(g)`). Pure, like [`admit`], and consulted **before** the retract
/// touches the fold or the journal (NFR70).
///
/// - `deny` refuses: an operator who set it to shut out a peer must not find
///   that peer can still write to their journal.
/// - `ask` refuses too, naming why: the shipped approval rail is task-shaped
///   end to end (a pending task record, `auth-required`, a watcher) and a
///   retract has no task. ⛔ Never silently treated as `allow`.
/// - `allow` admits.
///
/// There is no capability gate: a retract writes only the journal, which a
/// discovery-only endpoint has too. `Err` carries the reason, disclosed
/// verbatim to the caller, so it names the policy and never host state.
///
/// # Errors
///
/// Returns the refusal reason under `deny` and `ask`.
pub fn admit_item_retract(policy: A2aAdmissionPolicy, trust: SubmitterTrust) -> Result<(), String> {
    let credential = credential_label(trust);
    match policy {
        A2aAdmissionPolicy::Deny => Err(format!(
            "inbound A2A item retraction is disabled by policy (`server.admission` = \
             \"deny\"); credential accepted: {credential}"
        )),
        A2aAdmissionPolicy::Ask => Err(format!(
            "inbound A2A item retraction is disabled by policy (`server.admission` = \
             \"ask\"): the ask policy has no approval shape for a non-task verb; credential \
             accepted: {credential}"
        )),
        A2aAdmissionPolicy::Allow => Ok(()),
    }
}

/// Story 19.18 `AC12` — the one fixed refusal a thread read or a follow-up
/// reference receives from an API-key caller while thread access is off.
pub const THREAD_ACCESS_OFF: &str = "thread access for API-key callers is off on this host — \
     its operator can enable it with a2a.json server.threadReads = \"credential-scoped\"";

/// Decide whether a caller may read a thread or post a follow-up reference
/// (Story 19.18 `AC12`, `A17`). Pure, and consulted **before** any fold,
/// lookup or mutation, so a refused caller learns nothing about whether the
/// id exists (`M44`).
///
/// - Loopback → allowed: every local caller already runs code here, and the
///   response discloses the collapse (19.16b `A13`).
/// - API key + `credential-scoped` → allowed.
/// - API key + `off` or a value this build cannot name → refused. ⛔ Never
///   fail open on `Unknown` (`M46`).
///
/// # Errors
///
/// Returns [`THREAD_ACCESS_OFF`], disclosed verbatim, when refused.
pub fn admit_thread_access(
    trust: SubmitterTrust,
    setting: ThreadReads,
) -> Result<(), &'static str> {
    match (trust, setting) {
        (SubmitterTrust::Loopback, _) | (SubmitterTrust::ApiKey, ThreadReads::CredentialScoped) => {
            Ok(())
        }
        (SubmitterTrust::ApiKey, ThreadReads::Off | ThreadReads::Unknown) => Err(THREAD_ACCESS_OFF),
    }
}

/// Story 19.18c `A7` — the served verbs a per-item capability gates, grouped
/// by the rule their **legacy** items keep.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityGatedVerb {
    /// `x-rustain-items/thread` and a follow-up's reference: a legacy item
    /// keeps 19.18's `threadReads` gate.
    ThreadOrFollowUp,
    /// `x-rustain-items/retract`, `tasks/get`, `tasks/cancel`: a legacy item
    /// keeps today's credential-scoped behaviour.
    RetractOrTask,
}

/// Story 19.18c `A7` step 5 — what the served verb does after the lookup.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityVerdict {
    Serve,
    /// Byte-identical to an id that does not exist.
    NotFound,
    /// [`THREAD_ACCESS_OFF`]: a legacy item under `threadReads = off`.
    AccessOff,
}

/// Story 19.18c `A7` step 2 — the one refusal decided **before** any lookup:
/// an API-key caller that presents **no** capability on a thread read or a
/// follow-up reference meets 19.18's gate unchanged, so the refusal still
/// reveals nothing about the id (19.18 `M44`). A caller that presents one is
/// classified after the lookup, by [`post_lookup`]. Pure; exhaustive over
/// [`SubmitterTrust`] with no wildcard, so a new trust class must decide.
///
/// # Errors
///
/// [`THREAD_ACCESS_OFF`] for a capability-less API-key thread read or
/// follow-up reference under `off` or an unrecognised setting.
pub fn pre_lookup(
    trust: SubmitterTrust,
    verb: CapabilityGatedVerb,
    presented: bool,
    setting: ThreadReads,
) -> Result<(), &'static str> {
    match trust {
        SubmitterTrust::Loopback => Ok(()),
        SubmitterTrust::ApiKey => match verb {
            CapabilityGatedVerb::ThreadOrFollowUp if !presented => {
                admit_thread_access(trust, setting)
            }
            CapabilityGatedVerb::ThreadOrFollowUp | CapabilityGatedVerb::RetractOrTask => Ok(()),
        },
    }
}

/// Story 19.18c `A7` steps 4–5 — decide a served verb on its nearest item
/// (`A8`), after the principal-scoped lookup found it. `matched` is the one
/// compare's result, computed independently of trust (`P7`).
///
/// - Loopback → serve (no principal separation exists there, 19.16b `A13`).
/// - API key, capability-bearing item → serve iff `matched`, else not-found.
/// - API key, legacy item → thread/follow-up keep the `threadReads` gate;
///   retract and `tasks/*` stay credential-scoped (disclosed).
///
/// Pure; exhaustive over [`SubmitterTrust`] with no wildcard.
#[must_use]
pub fn post_lookup(
    trust: SubmitterTrust,
    verb: CapabilityGatedVerb,
    item_hash: Option<&str>,
    matched: bool,
    setting: ThreadReads,
) -> CapabilityVerdict {
    match trust {
        SubmitterTrust::Loopback => CapabilityVerdict::Serve,
        SubmitterTrust::ApiKey => match (item_hash, verb) {
            (Some(_), _) if matched => CapabilityVerdict::Serve,
            (Some(_), _) => CapabilityVerdict::NotFound,
            (None, CapabilityGatedVerb::ThreadOrFollowUp) => {
                match admit_thread_access(trust, setting) {
                    Ok(()) => CapabilityVerdict::Serve,
                    Err(_) => CapabilityVerdict::AccessOff,
                }
            }
            (None, CapabilityGatedVerb::RetractOrTask) => CapabilityVerdict::Serve,
        },
    }
}

fn credential_label(trust: SubmitterTrust) -> &'static str {
    match trust {
        SubmitterTrust::Loopback => "loopback",
        SubmitterTrust::ApiKey => "api-key",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(text: &str) -> AdmissionRequest<'_> {
        AdmissionRequest {
            text,
            executor_available: true,
        }
    }

    #[test]
    fn policy_selects_the_verdict_and_nothing_else_does() {
        let req = request("summarize the corpus");
        assert_eq!(
            admit(&req, A2aAdmissionPolicy::Allow, SubmitterTrust::ApiKey),
            AdmissionVerdict::Accept
        );
        assert_eq!(
            admit(&req, A2aAdmissionPolicy::Ask, SubmitterTrust::ApiKey),
            AdmissionVerdict::AcceptPendingApproval
        );
        assert!(matches!(
            admit(&req, A2aAdmissionPolicy::Deny, SubmitterTrust::ApiKey),
            AdmissionVerdict::Reject { .. }
        ));
    }

    #[test]
    fn the_default_policy_refuses() {
        // A fresh config that says nothing about admission must not execute a
        // stranger's instructions.
        assert!(matches!(
            admit(
                &request("rm -rf /"),
                A2aAdmissionPolicy::default(),
                SubmitterTrust::Loopback
            ),
            AdmissionVerdict::Reject { .. }
        ));
    }

    #[test]
    fn a_discovery_only_endpoint_refuses_before_consulting_policy() {
        let req = AdmissionRequest {
            text: "do the thing",
            executor_available: false,
        };
        let AdmissionVerdict::Reject { reason } =
            admit(&req, A2aAdmissionPolicy::Allow, SubmitterTrust::Loopback)
        else {
            panic!("a discovery-only endpoint must refuse even under `allow`");
        };
        assert!(reason.contains("discovery only"), "reason={reason}");
    }

    #[test]
    fn empty_and_oversized_instructions_are_refused_under_every_policy() {
        for policy in [
            A2aAdmissionPolicy::Allow,
            A2aAdmissionPolicy::Ask,
            A2aAdmissionPolicy::Deny,
        ] {
            assert!(matches!(
                admit(&request("   \n\t "), policy, SubmitterTrust::ApiKey),
                AdmissionVerdict::Reject { .. }
            ));
            let huge = "x".repeat(MAX_TASK_TEXT_BYTES + 1);
            assert!(matches!(
                admit(&request(&huge), policy, SubmitterTrust::ApiKey),
                AdmissionVerdict::Reject { .. }
            ));
        }
    }

    #[test]
    fn policy_deserializes_from_the_documented_lowercase_spellings() {
        for (raw, expected) in [
            ("\"deny\"", A2aAdmissionPolicy::Deny),
            ("\"ask\"", A2aAdmissionPolicy::Ask),
            ("\"allow\"", A2aAdmissionPolicy::Allow),
        ] {
            let parsed: A2aAdmissionPolicy = serde_json::from_str(raw).expect(raw);
            assert_eq!(parsed, expected);
        }
        assert!(serde_json::from_str::<A2aAdmissionPolicy>("\"yolo\"").is_err());
    }

    /// Story 19.18 `K21` (in-crate half) — the gate over every
    /// `(trust, setting)` cell. `M45` (default on) and `M46` (`Unknown` on)
    /// both turn this RED.
    #[test]
    fn thread_access_is_refused_by_default_for_api_key_callers() {
        assert_eq!(ThreadReads::default(), ThreadReads::Off);
        for setting in [
            ThreadReads::Off,
            ThreadReads::CredentialScoped,
            ThreadReads::Unknown,
        ] {
            assert_eq!(
                admit_thread_access(SubmitterTrust::Loopback, setting),
                Ok(()),
                "loopback is served under {setting:?}"
            );
        }
        assert_eq!(
            admit_thread_access(SubmitterTrust::ApiKey, ThreadReads::default()),
            Err(THREAD_ACCESS_OFF)
        );
        assert_eq!(
            admit_thread_access(SubmitterTrust::ApiKey, ThreadReads::Unknown),
            Err(THREAD_ACCESS_OFF)
        );
        assert_eq!(
            admit_thread_access(SubmitterTrust::ApiKey, ThreadReads::CredentialScoped),
            Ok(())
        );
        for (raw, expected) in [
            ("\"off\"", ThreadReads::Off),
            ("\"credential-scoped\"", ThreadReads::CredentialScoped),
            ("\"shared\"", ThreadReads::Unknown),
        ] {
            assert_eq!(
                serde_json::from_str::<ThreadReads>(raw).expect(raw),
                expected
            );
        }
    }

    /// Story 19.18c `A7` (pure half of `K05`/`K08`, served decision table):
    /// every `(trust, verb, class, presented/matched, setting)` cell.
    #[test]
    fn the_capability_gate_follows_the_served_decision_table() {
        use CapabilityGatedVerb::{RetractOrTask, ThreadOrFollowUp};
        use CapabilityVerdict::{AccessOff, NotFound, Serve};
        let hash = Some("00");
        for setting in [
            ThreadReads::Off,
            ThreadReads::CredentialScoped,
            ThreadReads::Unknown,
        ] {
            for verb in [ThreadOrFollowUp, RetractOrTask] {
                for presented in [false, true] {
                    assert_eq!(
                        pre_lookup(SubmitterTrust::Loopback, verb, presented, setting),
                        Ok(())
                    );
                }
                for (item, matched) in [(hash, true), (hash, false), (None, false)] {
                    assert_eq!(
                        post_lookup(SubmitterTrust::Loopback, verb, item, matched, setting),
                        Serve,
                        "loopback is served ({verb:?}, {item:?}, {matched})"
                    );
                }
                assert_eq!(
                    post_lookup(SubmitterTrust::ApiKey, verb, hash, true, setting),
                    Serve
                );
                assert_eq!(
                    post_lookup(SubmitterTrust::ApiKey, verb, hash, false, setting),
                    NotFound,
                    "a capability-bearing item without its capability is not-found"
                );
            }
            assert_eq!(
                pre_lookup(SubmitterTrust::ApiKey, RetractOrTask, false, setting),
                Ok(())
            );
            assert_eq!(
                pre_lookup(SubmitterTrust::ApiKey, ThreadOrFollowUp, true, setting),
                Ok(()),
                "a presented capability is classified after the lookup (`M09`)"
            );
            assert_eq!(
                post_lookup(SubmitterTrust::ApiKey, RetractOrTask, None, false, setting),
                Serve,
                "a legacy retract / tasks/* stays credential-scoped"
            );
        }
        assert_eq!(
            pre_lookup(
                SubmitterTrust::ApiKey,
                ThreadOrFollowUp,
                false,
                ThreadReads::Off
            ),
            Err(THREAD_ACCESS_OFF),
            "19.18's pre-lookup gate binds capability-less API-key reads"
        );
        assert_eq!(
            pre_lookup(
                SubmitterTrust::ApiKey,
                ThreadOrFollowUp,
                false,
                ThreadReads::CredentialScoped
            ),
            Ok(())
        );
        assert_eq!(
            post_lookup(
                SubmitterTrust::ApiKey,
                ThreadOrFollowUp,
                None,
                true,
                ThreadReads::Off
            ),
            AccessOff,
            "a legacy item under off is refused even when a capability was presented (`M14`)"
        );
        assert_eq!(
            post_lookup(
                SubmitterTrust::ApiKey,
                ThreadOrFollowUp,
                None,
                false,
                ThreadReads::CredentialScoped
            ),
            Serve
        );
    }
}
