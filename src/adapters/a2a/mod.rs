//! A2A AgentCard discovery adapter.
//!
//! Configuration parsing remains available without the `a2a` feature so startup
//! can reject configured peers loudly instead of silently omitting them.

#[cfg(feature = "a2a")]
pub(crate) const MESSAGE_TYPE_METADATA_KEY: &str = "x-rustain-message-type";
#[cfg(feature = "a2a")]
pub const RECIPIENT_ITEM_METADATA_KEY: &str = "x-rustain-item-id";

pub mod config;

#[cfg(feature = "a2a")]
pub mod admission;
#[cfg(feature = "a2a")]
pub mod auth;
#[cfg(feature = "a2a")]
pub mod card;
#[cfg(feature = "a2a")]
pub mod card_cache;
#[cfg(feature = "a2a")]
pub mod client;
#[cfg(feature = "a2a")]
pub mod driver;
#[cfg(feature = "a2a")]
pub mod egress;
#[cfg(feature = "a2a")]
pub mod endpoint;
#[cfg(feature = "a2a")]
pub mod error;
#[cfg(feature = "a2a")]
pub mod exec;
#[cfg(feature = "a2a")]
pub mod jsonrpc;
#[cfg(feature = "a2a")]
pub mod jws;
#[cfg(feature = "a2a")]
pub mod lifecycle;
#[cfg(feature = "a2a")]
pub mod projection;
#[cfg(feature = "a2a")]
pub mod provider;
#[cfg(feature = "a2a")]
pub mod send;
#[cfg(feature = "a2a")]
pub mod server;
#[cfg(feature = "a2a")]
pub mod task;
/// Story 19.14's keystone fixtures: locally generated certificates and a
/// recording loopback peer. ⛔ Test-only; never compiled into a shipped binary.
#[cfg(all(test, feature = "a2a"))]
pub(crate) mod test_fixtures;
#[cfg(feature = "a2a")]
pub mod tls;
pub mod transparency;
