//! Channel adapters.

#[cfg(any(feature = "telegram", feature = "slack"))]
pub mod sanitize;
#[cfg(feature = "slack")]
pub mod slack;
#[cfg(feature = "telegram")]
pub mod telegram;
