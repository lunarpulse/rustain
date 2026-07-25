//! A2A AgentCard discovery adapter.
//!
//! Configuration parsing remains available without the `a2a` feature so startup
//! can reject configured peers loudly instead of silently omitting them.

pub mod config;

#[cfg(feature = "a2a")]
pub mod card;
#[cfg(feature = "a2a")]
pub mod client;
#[cfg(feature = "a2a")]
pub mod driver;
#[cfg(feature = "a2a")]
pub mod endpoint;
#[cfg(feature = "a2a")]
pub mod error;
#[cfg(feature = "a2a")]
pub mod jsonrpc;
#[cfg(feature = "a2a")]
pub mod jws;
#[cfg(feature = "a2a")]
pub mod lifecycle;
#[cfg(feature = "a2a")]
pub mod provider;
#[cfg(feature = "a2a")]
pub mod server;
#[cfg(feature = "a2a")]
pub mod task;
