//! Streaming-text collection — shared helper for paths that buffer a
//! `Stream<Item = StreamChunk>` into a single `String`.
//!
//! Established Story 8.0a Phase 4 (Winston Decision Gate amendment) to
//! consolidate two pre-existing duplicates: `event_loop.rs::collect_completion_text`
//! (used by title generation) and `handlers/compaction.rs::collect_text_chunks`
//! (used by compaction summary). Both now route through this single domain helper.
//!
//! Returns `Err` on `StreamChunk::Error`; ignores non-Text chunks.

#![allow(dead_code)]

use crate::domain::models::StreamChunk;

/// Collect text content from a completion stream into a single `String`.
/// Returns `Err` on `StreamChunk::Error`; ignores non-Text chunks.
pub async fn collect_text(
    stream: impl futures::Stream<Item = StreamChunk>,
) -> anyhow::Result<String> {
    use futures::StreamExt;
    futures::pin_mut!(stream);

    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            StreamChunk::Text { content, .. } => {
                text.push_str(&content);
            }
            StreamChunk::Error { content } => {
                anyhow::bail!("Stream error: {}", content);
            }
            _ => {}
        }
    }
    Ok(text)
}
