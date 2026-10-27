//! Codex session/turn metadata, adapted from oh-my-pi.
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

use crate::adapters::install_id::uuid;
use crate::domain::models::{CompletionOptions, Message, MessageRole};
use std::{collections::HashMap, sync::LazyLock};
use tokio::sync::Mutex;

static FALLBACK: LazyLock<String> = LazyLock::new(uuid);
const SESSION_LIMIT: usize = 256;

#[derive(Clone)]
pub(super) struct Snapshot {
    pub session_id: String,
    pub thread_id: String,
    pub window_id: String,
    pub turn_id: String,
    pub turn_started_at_unix_ms: i64,
    pub turn_state: Option<String>,
    pub models_etag: Option<String>,
}
struct Session {
    snapshot: Snapshot,
    used: u64,
}
#[derive(Default)]
struct State {
    entries: HashMap<String, Session>,
    clock: u64,
}
#[derive(Default)]
pub(super) struct Sessions(Mutex<State>);

pub(super) fn normalise(id: &str) -> String {
    if id.encode_utf16().count() <= 64 {
        id.to_owned()
    } else {
        format!("pc_{}", crate::adapters::responses::repair::stable_hash(id))
    }
}
impl Sessions {
    pub async fn begin(&self, messages: &[Message], options: &CompletionOptions) -> Snapshot {
        let id = normalise(
            options
                .session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&FALLBACK),
        );
        let new_turn = messages
            .last()
            .is_some_and(|m| m.role == MessageRole::User && m.tool_results.is_empty());
        let mut state = self.0.lock().await;
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let existed = state.entries.contains_key(&id);
        if !existed && state.entries.len() >= SESSION_LIMIT {
            if let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(id, _)| id.clone())
            {
                state.entries.remove(&oldest);
            }
        }
        let session = state.entries.entry(id.clone()).or_insert_with(|| Session {
            snapshot: Snapshot {
                session_id: id,
                thread_id: uuid(),
                window_id: uuid(),
                turn_id: uuid(),
                turn_started_at_unix_ms: crate::infrastructure::utils::auth_now_ms(),
                turn_state: None,
                models_etag: None,
            },
            used: clock,
        });
        session.used = clock;
        if new_turn && existed {
            session.snapshot.turn_id = uuid();
            session.snapshot.turn_started_at_unix_ms = crate::infrastructure::utils::auth_now_ms();
            session.snapshot.turn_state = None;
        }
        session.snapshot.clone()
    }
    pub async fn capture(&self, request: &mut Snapshot, headers: &reqwest::header::HeaderMap) {
        let turn = headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty());
        // As in omp, an empty value never replaces the last seen etag.
        let etag = headers
            .get("x-models-etag")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty());
        if request.turn_state.is_none() {
            request.turn_state = turn.map(str::to_owned);
        }
        if let Some(etag) = etag {
            request.models_etag = Some(etag.to_owned());
        }
        let mut state = self.0.lock().await;
        if let Some(session) = state.entries.get_mut(&request.session_id) {
            if session.snapshot.turn_id == request.turn_id && session.snapshot.turn_state.is_none()
            {
                session.snapshot.turn_state = request.turn_state.clone();
            }
            if let Some(etag) = etag {
                session.snapshot.models_etag = Some(etag.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(id: Option<String>) -> CompletionOptions {
        CompletionOptions {
            model: "m".into(),
            max_tokens: 1,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
            session_id: id,
        }
    }
    #[tokio::test]
    async fn lru_bound_and_process_fallback_are_stable() {
        let sessions = Sessions::default();
        let a = sessions.begin(&[], &options(None)).await;
        let b = sessions.begin(&[], &options(None)).await;
        assert_eq!(a.session_id, b.session_id);
        assert_eq!(a.thread_id, b.thread_id);
        for i in 0..257 {
            sessions.begin(&[], &options(Some(i.to_string()))).await;
        }
        assert_eq!(sessions.0.lock().await.entries.len(), 256);
        assert!(!sessions.0.lock().await.entries.contains_key("0"));
        sessions.begin(&[], &options(Some("1".into()))).await;
        sessions.begin(&[], &options(Some("258".into()))).await;
        assert!(sessions.0.lock().await.entries.contains_key("1"));
        assert!(!sessions.0.lock().await.entries.contains_key("2"));
    }
    #[test]
    fn wire_limit_counts_utf16_and_uses_reference_hash() {
        assert_eq!(normalise("short"), "short");
        assert!(normalise(&"🦀".repeat(33)).starts_with("pc_"));
        assert!(normalise(&"a".repeat(65)).len() < 64);
    }
}
