use std::collections::VecDeque;

use crate::domain::errors::TurnQueueError;
use crate::domain::models::UserMessage;

#[derive(Clone)]
pub struct TurnQueue {
    pending: VecDeque<UserMessage>,
    max_pending: usize,
    merge_threshold: usize,
}

impl Default for TurnQueue {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            max_pending: 8,
            merge_threshold: 12_000,
        }
    }
}

impl TurnQueue {
    pub fn enqueue(&mut self, msg: UserMessage) -> Result<(), TurnQueueError> {
        if self.pending.len() >= self.max_pending {
            return Err(TurnQueueError::QueueFull);
        }

        // Merge text-only messages if both have no images and combined length fits
        if msg.images.is_empty() {
            if let Some(last) = self.pending.back_mut() {
                if last.images.is_empty()
                    && last.content.len() + 1 + msg.content.len() <= self.merge_threshold
                {
                    last.content.push('\n');
                    last.content.push_str(&msg.content);
                    return Ok(());
                }
            }
        }

        self.pending.push_back(msg);
        Ok(())
    }

    pub fn dequeue(&mut self) -> Option<UserMessage> {
        self.pending.pop_front()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.pending.len()
    }
}
