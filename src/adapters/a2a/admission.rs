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
pub use super::config::A2aAdmissionPolicy;

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
            let credential = match trust {
                SubmitterTrust::Loopback => "loopback",
                SubmitterTrust::ApiKey => "api-key",
            };
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
}
