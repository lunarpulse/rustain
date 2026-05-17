//! AgentCore — central runtime holder of port adapters.
//!
//! Each of the 7 port dimensions has an Arc<ArcSwap<Arc<dyn PortTrait>>>
//! slot. Adapters are composed by `infrastructure::composition::*` from a
//! `ProfileSelection` and stored here. Future stories (8.4) swap individual
//! slots atomically without touching unaffected ports.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::domain::ports::{
    ChannelPort, ContextPort, MemoryPort, PersonaPort, SchedulerPort, SessionPort, ToolSetPort,
};

pub struct AgentCore {
    pub persona: Arc<ArcSwap<Arc<dyn PersonaPort>>>,
    pub memory: Arc<ArcSwap<Arc<dyn MemoryPort>>>,
    pub session: Arc<ArcSwap<Arc<dyn SessionPort>>>,
    pub tools: Arc<ArcSwap<Arc<dyn ToolSetPort>>>,
    pub channels: Arc<ArcSwap<Arc<dyn ChannelPort>>>,
    pub scheduler: Arc<ArcSwap<Arc<dyn SchedulerPort>>>,
    pub context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
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
        Self {
            persona: Self::wrap(Arc::new(NoOpPersona) as Arc<dyn PersonaPort>),
            memory: Self::wrap(Arc::new(NoOpMemory) as Arc<dyn MemoryPort>),
            session: Self::wrap(Arc::new(NoOpSession) as Arc<dyn SessionPort>),
            tools: Self::wrap(Arc::new(NoOpToolSet) as Arc<dyn ToolSetPort>),
            channels: Self::wrap(Arc::new(NoOpChannel) as Arc<dyn ChannelPort>),
            scheduler: Self::wrap(Arc::new(NoOpScheduler) as Arc<dyn SchedulerPort>),
            context: Self::wrap(Arc::new(NoOpContext) as Arc<dyn ContextPort>),
        }
    }

    pub(crate) fn wrap<T: ?Sized>(arc: Arc<T>) -> Arc<ArcSwap<Arc<T>>> {
        Arc::new(ArcSwap::from_pointee(arc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_noop_constructs_all_seven_ports() {
        let core = AgentCore::test_noop();
        // Each load_full() must return a non-null Arc
        let p = core.persona.load_full();
        let m = core.memory.load_full();
        let s = core.session.load_full();
        let t = core.tools.load_full();
        let ch = core.channels.load_full();
        let sc = core.scheduler.load_full();
        let cx = core.context.load_full();
        assert!(Arc::strong_count(&p) >= 1);
        assert!(Arc::strong_count(&m) >= 1);
        assert!(Arc::strong_count(&s) >= 1);
        assert!(Arc::strong_count(&t) >= 1);
        assert!(Arc::strong_count(&ch) >= 1);
        assert!(Arc::strong_count(&sc) >= 1);
        assert!(Arc::strong_count(&cx) >= 1);
    }
}
