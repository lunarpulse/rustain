//! Provider-neutral Responses wire core, adapted from oh-my-pi.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

pub mod events;
pub mod repair;
pub mod request;
pub mod retry;
pub mod schema;
pub mod transport;

/// Request and event shape of one Responses backend family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// ChatGPT Codex backend: developer items, synthetic user item, Codex texts.
    #[default]
    Codex,
    /// Generic `openai-responses` backends (xAI): `instructions`, sampling and limits.
    Generic,
}

/// Provider-owned request policy; the core has no credential/session state.
/// The `Generic`-only methods are never consulted for `Dialect::Codex`.
pub trait WireProfile {
    fn dialect(&self) -> Dialect;
    fn prompt_cache_key(&self) -> Option<&str>;
    /// Sent by `Dialect::Codex` only.
    fn client_metadata(&self) -> Option<&serde_json::Value>;
    /// Provider kind whose stored replay items this request may carry back;
    /// `None` replays nothing.
    fn replay_provider(&self) -> Option<&str> {
        None
    }
    /// The stamp the transport puts on captured replay items; the replay
    /// provider unless a provider replays only part of its history.
    fn capture_provider(&self) -> Option<&str> {
        self.replay_provider()
    }
    /// `Generic`: the model reasons, so `include` requests encrypted reasoning.
    fn reasoning(&self) -> bool {
        false
    }
    /// `Generic`: the model accepts image input; otherwise images become a placeholder.
    fn vision(&self) -> bool {
        true
    }
    /// `Generic`: the already clamped `max_output_tokens`.
    fn max_output_tokens(&self) -> Option<u32> {
        None
    }
    /// `Generic`: the sampling temperature, sent only when present.
    fn temperature(&self) -> Option<f32> {
        None
    }
    /// `Generic`: flatten typeless required-key root unions and quarantine tools
    /// whose root still carries a non-object union.
    fn reject_root_object_union(&self) -> bool {
        false
    }
    /// `Generic`: how system texts reach the model.
    fn system_role(&self) -> SystemRole {
        SystemRole::Instructions
    }
}

/// Where a `Generic` request carries its system texts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemRole {
    /// Joined with a blank line into `instructions`.
    #[default]
    Instructions,
    /// One leading `{"role":"developer","content":<text>}` input item per text
    /// (reasoning models with a developer role); no `instructions`.
    Developer,
}

/// omp `normalizeOpenAIPromptCacheKey`: at most 64 UTF-16 units are kept,
/// longer identities become `pc_` + Bun.hash base 36.
pub fn prompt_cache_key(id: &str) -> String {
    if id.encode_utf16().count() <= 64 {
        id.to_owned()
    } else {
        format!("pc_{}", repair::stable_hash(id))
    }
}

#[cfg(test)]
mod tests;
