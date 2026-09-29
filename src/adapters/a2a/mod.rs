//! A2A AgentCard discovery adapter.
//!
//! Configuration parsing remains available without the `a2a` feature so startup
//! can reject configured peers loudly instead of silently omitting them.

#[cfg(feature = "a2a")]
pub(crate) const MESSAGE_TYPE_METADATA_KEY: &str = "x-rustain-message-type";
#[cfg(feature = "a2a")]
pub const RECIPIENT_ITEM_METADATA_KEY: &str = "x-rustain-item-id";
/// Story 19.16b `AC1(a)` / `A20` — the cross-host acknowledgement read verb.
///
/// Namespaced, because `message` and `tasks` are A2A **spec** nouns while
/// `items` is ours: an `items/*` method minted by a future A2A revision would
/// collide and we would have no namespace argument left. One constant, read by
/// both the served dispatch arm and the client transport, so a one-side
/// respelling cannot compile silently.
#[cfg(feature = "a2a")]
pub const ITEMS_LIST_METHOD: &str = "x-rustain-items/list";
/// Story 19.16d `AC1(a)` — the cross-host retract write verb, served by the
/// **recipient** host. Namespaced for the same reason as
/// [`ITEMS_LIST_METHOD`], and one constant for the same reason: the served
/// dispatch arm reads it here, and `19-16f`'s client transport must read the
/// same constant so a one-side respelling cannot compile silently.
#[cfg(feature = "a2a")]
pub const ITEMS_RETRACT_METHOD: &str = "x-rustain-items/retract";

pub mod config;

#[cfg(feature = "a2a")]
pub mod admission;
#[cfg(feature = "a2a")]
pub mod auth;
#[cfg(feature = "a2a")]
pub mod board;
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
