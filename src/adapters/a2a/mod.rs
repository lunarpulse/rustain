//! A2A AgentCard discovery adapter.
//!
//! Configuration parsing remains available without the `a2a` feature so startup
//! can reject configured peers loudly instead of silently omitting them.

#[cfg(feature = "a2a")]
pub(crate) const MESSAGE_TYPE_METADATA_KEY: &str = "x-rustain-message-type";
#[cfg(feature = "a2a")]
pub const RECIPIENT_ITEM_METADATA_KEY: &str = "x-rustain-item-id";
#[cfg(feature = "a2a")]
pub const AUTHORSHIP_METADATA_KEY: &str = "x-rustain-authorship";
#[cfg(feature = "a2a")]
pub const SEND_AUTHORIZATION_METADATA_KEY: &str = "x-rustain-send-authorization";
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
/// Story 19.18 `A5` — the follow-up carrier: a `message/send` metadata key
/// naming the item or reply-turn id this new item answers. Written by
/// `build_message` from a separate Rust parameter (⛔ never from
/// model-controlled `input`), read by the served `message_send`. One constant
/// for both sides, so a one-side respelling cannot compile silently.
#[cfg(feature = "a2a")]
pub const REPLY_TO_METADATA_KEY: &str = "x-rustain-in-reply-to";
/// Story 19.18c `A12`/`P2` — the per-item capability carrier on every
/// outbound `message/send` when this host's key is usable, and the
/// `tasks/get`/`tasks/cancel` carrier under `params.metadata` (`P8`). Frozen
/// at the v1 43-character shape: a new format ships under a new name.
#[cfg(feature = "a2a")]
pub const ITEM_CAPABILITY_METADATA_KEY: &str = "x-rustain-item-capability";
/// Story 19.18c `A12` — on a follow-up, the capability of the reference's
/// nearest item. ⛔ Only this carrier counts as presenting a capability for
/// the reference (`A7` step 1).
#[cfg(feature = "a2a")]
pub const REPLY_CAPABILITY_METADATA_KEY: &str = "x-rustain-reply-capability";
/// Story 19.18 `AC4(a)` — the per-item thread read, served by the
/// ANSWERING host. Namespaced for the same reason as [`ITEMS_LIST_METHOD`].
/// Side-effect free; one operator act on one item, ⛔ never a poll.
#[cfg(feature = "a2a")]
pub const ITEMS_THREAD_METHOD: &str = "x-rustain-items/thread";

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
pub mod item_capability;
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
#[cfg(feature = "a2a")]
pub mod team_proposal_tool;
/// Story 19.14's keystone fixtures: locally generated certificates and a
/// recording loopback peer. ⛔ Test-only; never compiled into a shipped binary.
#[cfg(all(test, feature = "a2a"))]
pub(crate) mod test_fixtures;
#[cfg(feature = "a2a")]
pub mod tls;
pub mod transparency;
