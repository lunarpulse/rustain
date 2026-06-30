//! Compatibility re-export for the Story 14.4 local message bus.
//!
//! The concrete implementation lives beside the subagent node registry because it
//! resolves local node handles. Keeping this file as a re-export preserves the
//! `infrastructure::agent_message_bus::LocalMessageBus` path without creating a
//! second registry consumer.

pub use crate::infrastructure::subagent::LocalMessageBus;
