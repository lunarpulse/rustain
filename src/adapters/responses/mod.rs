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
pub mod schema;

/// Provider-owned affinity and metadata; the core has no credential/session state.
pub trait WireProfile {
    fn prompt_cache_key(&self) -> Option<&str>;
    fn client_metadata(&self) -> Option<&serde_json::Value>;
    /// Provider kind whose stored replay items this request may carry back
    /// (Story 20.1d1b); `None` replays nothing.
    fn replay_provider(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests;
