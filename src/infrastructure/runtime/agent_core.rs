//! AgentCore — central runtime holder of port adapters.
//!
//! Each of the 7 port dimensions has an Arc<ArcSwap<Arc<dyn PortTrait>>>
//! slot. Adapters are composed by `infrastructure::composition::*` from a
//! `ProfileSelection` and stored here. Future stories (8.4) swap individual
//! slots atomically without touching unaffected ports.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::domain::ports::{
    AgentMessageBus, ChannelPort, ContextAssemblerPort, ContextPort, MemoryPort, PersonaPort,
    SandboxManager, SchedulerPort, SessionPort, SkillExposurePort, ToolExposurePort, ToolSetPort,
};

pub struct AgentCore {
    pub persona: Arc<ArcSwap<Arc<dyn PersonaPort>>>,
    pub memory: Arc<ArcSwap<Arc<dyn MemoryPort>>>,
    pub session: Arc<ArcSwap<Arc<dyn SessionPort>>>,
    pub tools: Arc<ArcSwap<Arc<dyn ToolSetPort>>>,
    pub channels: Arc<ArcSwap<Arc<dyn ChannelPort>>>,
    pub scheduler: Arc<ArcSwap<Arc<dyn SchedulerPort>>>,
    pub context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
    /// Story 14.4 — send-side status-aware local/remote message delivery seam.
    pub agent_message_bus: Arc<ArcSwap<Arc<dyn AgentMessageBus>>>,
    /// Story 11.0a — per-turn Message-tier context assembler (ADR-10-4): the
    /// `Conversation -> Vec<Message>` wire-payload builder. Option-wrapped like
    /// `tool_exposure`/`skill_exposure`: `Some(StaticPassthroughAssembler)` is the
    /// behaviour-preserving default; `None` is the reserved eval/replay bypass
    /// (architecture.md:245) where the call site falls back to `build_api_messages`
    /// directly. Unreachable in 11.0a's live TUI paths (all bind `Some`); becomes
    /// observable at Story 11.6 when the default is `WindowingAssembler` and eval
    /// opts out via `None` to get raw passthrough.
    pub context_assembler: Arc<ArcSwap<Option<Arc<dyn ContextAssemblerPort>>>>,
    /// Story 9.4 — per-turn tool exposure strategy. `None` for headless / eval
    /// path per ADR-09-01 v2.1 §W1 (Disabled is NOT a trait impl; the eval
    /// harness binds None).
    pub tool_exposure: Arc<ArcSwap<Option<Arc<dyn ToolExposurePort>>>>,
    /// Story 9.6 — per-turn skill exposure strategy. `None` for headless / eval
    /// path per ADR-09-01 v2.1 §W1 (inherited — Disabled is NOT a trait impl).
    pub skill_exposure: Arc<ArcSwap<Option<Arc<dyn SkillExposurePort>>>>,
    /// Story 9.5 — OS-level sandbox enforcement (ADR-06-04). Defaults to
    /// `NoOpSandbox` on macOS/Windows and on Linux without the `sandbox` cargo
    /// feature; binds `LandlockSandbox` on Linux with the feature enabled and
    /// kernel ABI >= v3.
    ///
    /// NOT wrapped in `Option<_>` (unlike `tool_exposure` / `skill_exposure`)
    /// because `NoOpSandbox` is the always-composable default — there is no
    /// "headless / eval" path that wants NO sandbox-binding at all; the eval
    /// harness wants `NoOpSandbox` explicitly.
    pub sandbox: Arc<ArcSwap<Arc<dyn SandboxManager>>>,
    /// Story 9.7 Phase B — shared merged BM25 index for meta-search.
    /// `None` when the `meta-search` feature is compiled but no `[search]`
    /// knob is "on" per ADR-09-01 v2.1 §W1 inherited.
    ///
    /// Per spec AC-9-7-11 and party-mode consensus 3/4 (2026-05-24):
    /// `ArcSwap<Option<Arc<MergedIndex>>>` NOT `Arc<ArcSwap<...>>` because
    /// `MergedIndex` is the shared state being swapped — the outer `Arc`
    /// would be redundant (ArcSwap already provides atomic-swap semantics).
    #[cfg(feature = "meta-search")]
    pub merged_index: ArcSwap<Option<Arc<crate::infrastructure::search::MergedIndex>>>,
}

impl AgentCore {
    /// Test-only convenience constructor — all 7 ports wired to NoOp adapters.
    /// Public+`#[doc(hidden)]` so integration tests in `tests/` can reach it.
    #[doc(hidden)]
    pub fn test_noop() -> Self {
        use crate::adapters::noop::{
            NoOpChannel, NoOpContext, NoOpMemory, NoOpPersona, NoOpScheduler, NoOpSession,
            NoOpToolSet,
        };
        use crate::adapters::sandbox::NoOpSandbox;
        use crate::infrastructure::context::StaticPassthroughAssembler;
        Self {
            persona: Self::wrap(Arc::new(NoOpPersona) as Arc<dyn PersonaPort>),
            memory: Self::wrap(Arc::new(NoOpMemory) as Arc<dyn MemoryPort>),
            session: Self::wrap(Arc::new(NoOpSession) as Arc<dyn SessionPort>),
            tools: Self::wrap(Arc::new(NoOpToolSet) as Arc<dyn ToolSetPort>),
            channels: Self::wrap(Arc::new(NoOpChannel) as Arc<dyn ChannelPort>),
            scheduler: Self::wrap(Arc::new(NoOpScheduler) as Arc<dyn SchedulerPort>),
            context: Self::wrap(Arc::new(NoOpContext) as Arc<dyn ContextPort>),
            agent_message_bus: Self::wrap(Arc::new(
                crate::infrastructure::agent_message_bus::LocalMessageBus::new(
                    Default::default(),
                    Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
                ),
            ) as Arc<dyn AgentMessageBus>),
            // Asymmetry vs tool_exposure (which defaults None in noop):
            // StaticPassthroughAssembler IS the behaviour-preserving default —
            // there is no "no assembler" TUI path; None is only the eval bypass.
            context_assembler: Self::wrap_optional(Some(
                Arc::new(StaticPassthroughAssembler) as Arc<dyn ContextAssemblerPort>
            )),
            tool_exposure: Self::wrap_optional(None as Option<Arc<dyn ToolExposurePort>>),
            skill_exposure: Self::wrap_optional(None as Option<Arc<dyn SkillExposurePort>>),
            sandbox: Self::wrap(Arc::new(NoOpSandbox) as Arc<dyn SandboxManager>),
            #[cfg(feature = "meta-search")]
            merged_index: ArcSwap::from_pointee(
                None as Option<Arc<crate::infrastructure::search::MergedIndex>>,
            ),
        }
    }

    pub(crate) fn wrap<T: ?Sized>(arc: Arc<T>) -> Arc<ArcSwap<Arc<T>>> {
        Arc::new(ArcSwap::from_pointee(arc))
    }

    pub(crate) fn wrap_optional<T: ?Sized>(arc: Option<Arc<T>>) -> Arc<ArcSwap<Option<Arc<T>>>> {
        Arc::new(ArcSwap::from_pointee(arc))
    }

    pub fn store_for_port(&self, built: crate::infrastructure::composition::BuiltAdapter) {
        use crate::infrastructure::composition::BuiltAdapter;
        match built {
            BuiltAdapter::Persona(arc) => self.persona.store(Arc::new(arc)),
            BuiltAdapter::Memory(arc) => self.memory.store(Arc::new(arc)),
            BuiltAdapter::Session(arc) => self.session.store(Arc::new(arc)),
            BuiltAdapter::Tools(arc) => self.tools.store(Arc::new(arc)),
            BuiltAdapter::Channels(arc) => self.channels.store(Arc::new(arc)),
            BuiltAdapter::Scheduler(arc) => self.scheduler.store(Arc::new(arc)),
            BuiltAdapter::Context(arc) => self.context.store(Arc::new(arc)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_constructs_all_eleven_slots() {
        use crate::adapters::sandbox::SandboxAdapterKind;
        let core = AgentCore::test_noop();
        // Each load_full() must return a non-null Arc
        let p = core.persona.load_full();
        let m = core.memory.load_full();
        let s = core.session.load_full();
        let t = core.tools.load_full();
        let ch = core.channels.load_full();
        let sc = core.scheduler.load_full();
        let cx = core.context.load_full();
        let ca = core.context_assembler.load_full();
        let te = core.tool_exposure.load_full();
        let se = core.skill_exposure.load_full();
        let sb = core.sandbox.load_full();
        assert!(Arc::strong_count(&p) >= 1);
        assert!(Arc::strong_count(&m) >= 1);
        assert!(Arc::strong_count(&s) >= 1);
        assert!(Arc::strong_count(&t) >= 1);
        assert!(Arc::strong_count(&ch) >= 1);
        assert!(Arc::strong_count(&sc) >= 1);
        assert!(Arc::strong_count(&cx) >= 1);
        assert!(
            ca.is_some(),
            "context_assembler defaults to Some(StaticPassthroughAssembler) in noop agent \
             (passthrough IS the behaviour-preserving default; None is only the eval bypass)"
        );
        assert!(te.is_none(), "tool_exposure defaults to None in noop agent");
        assert!(
            se.is_none(),
            "skill_exposure defaults to None in noop agent"
        );
        // Sandbox defaults to NoOpSandbox in test_noop()
        assert_eq!(sb.kind(), SandboxAdapterKind::NoOp);
        #[cfg(feature = "meta-search")]
        {
            let mi = core.merged_index.load_full();
            assert!(mi.is_none(), "merged_index defaults to None in noop agent");
        }
    }
}
