//! Process-local, reference-only consented recipient-item context.

use std::collections::HashMap;
use std::path::Path;
use tokio::sync::Mutex;

use crate::adapters::policy::recipient_item::JournalRecipientItemProjection;
use crate::domain::models::{ItemAddress, RecipientItemState};
use crate::domain::ports::RoomJournalReader;
use anyhow::{Context, anyhow};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipientContextInclusion {
    Selected,
    Full,
    Partial,
    Skipped,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecipientContextDiagnostic {
    pub address: ItemAddress,
    pub inclusion: RecipientContextInclusion,
    pub estimated_tokens: usize,
    pub last_use_turn_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CurrentRecipientItem {
    pub address: ItemAddress,
    pub body: String,
    pub retracted: bool,
}

#[derive(Default)]
pub struct RecipientContextState {
    selections: Mutex<HashMap<String, ItemAddress>>,
    diagnostics: Mutex<HashMap<String, RecipientContextDiagnostic>>,
}

impl RecipientContextState {
    pub async fn select(&self, workspace: &Path, item_id: &str) -> anyhow::Result<bool> {
        let item = load_item(workspace, item_id).await?;
        let Some(item) = item else {
            return Err(anyhow!("recipient item not found or unavailable"));
        };
        let mut selections = self.selections.lock().await;
        if selections.contains_key(item_id) {
            return Ok(false);
        }
        selections.insert(item_id.to_owned(), item.address.clone());
        self.diagnostics.lock().await.insert(
            item_id.to_owned(),
            RecipientContextDiagnostic {
                address: item.address,
                inclusion: RecipientContextInclusion::Selected,
                estimated_tokens: 0,
                last_use_turn_id: None,
            },
        );
        Ok(true)
    }

    /// Remove the reference without resolving the journal or body.
    pub async fn deselect(&self, item_id: &str) -> bool {
        let removed = self.selections.lock().await.remove(item_id).is_some();
        if removed {
            self.diagnostics.lock().await.remove(item_id);
        }
        removed
    }

    pub async fn clear(&self) {
        self.selections.lock().await.clear();
        self.diagnostics.lock().await.clear();
    }

    pub async fn inventory(&self) -> Vec<RecipientContextDiagnostic> {
        let mut items: Vec<_> = self.diagnostics.lock().await.values().cloned().collect();
        items.sort_by(|a, b| a.address.item().as_str().cmp(b.address.item().as_str()));
        items
    }

    pub(crate) async fn selected(&self) -> Vec<(String, ItemAddress)> {
        let selections = self.selections.lock().await;
        let mut items: Vec<_> = selections
            .iter()
            .map(|(id, address)| (id.clone(), address.clone()))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    pub(crate) async fn record_use(
        &self,
        id: &str,
        inclusion: RecipientContextInclusion,
        estimated_tokens: usize,
        turn_id: &str,
    ) {
        if let Some(diagnostic) = self.diagnostics.lock().await.get_mut(id) {
            diagnostic.inclusion = inclusion;
            diagnostic.estimated_tokens = estimated_tokens;
            if inclusion == RecipientContextInclusion::Full
                || inclusion == RecipientContextInclusion::Partial
            {
                diagnostic.last_use_turn_id = Some(turn_id.to_owned());
            }
        }
    }

    pub(crate) async fn mark_unavailable(&self, id: &str) {
        if let Some(diagnostic) = self.diagnostics.lock().await.get_mut(id) {
            diagnostic.inclusion = RecipientContextInclusion::Unavailable;
            diagnostic.estimated_tokens = 0;
        }
    }
}

pub(crate) async fn resolve_current_items(
    workspace: &Path,
    selected: &[(String, ItemAddress)],
) -> Vec<(String, Option<CurrentRecipientItem>)> {
    if selected.is_empty() {
        return Vec::new();
    }
    let entries =
        match crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            workspace,
        )
        .load_entries()
        .await
        {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "recipient context journal unavailable");
                return selected.iter().map(|(id, _)| (id.clone(), None)).collect();
            }
        };
    let projection = JournalRecipientItemProjection::from_entries(&entries);
    selected
        .iter()
        .map(|(id, address)| {
            let view = projection.get(address);
            let current = view.and_then(|view| {
                if !crate::infrastructure::runtime::recipient_view::recipient_context_available(
                    &view,
                ) {
                    return None;
                }
                match view.state {
                    RecipientItemState::Received { content }
                    | RecipientItemState::Acknowledged { content } => Some(CurrentRecipientItem {
                        address: view.address,
                        body: content,
                        retracted: view.retracted_at_ms.is_some(),
                    }),
                    _ => None,
                }
            });
            (id.clone(), current)
        })
        .collect()
}

async fn load_item(
    workspace: &Path,
    item_id: &str,
) -> anyhow::Result<Option<CurrentRecipientItem>> {
    let reader =
        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            workspace,
        );
    let entries = reader
        .load_entries()
        .await
        .context("loading recipient context journal")?;
    let projection = JournalRecipientItemProjection::from_entries(&entries);
    let Some(view) = projection.find_by_id(item_id) else {
        return Ok(None);
    };
    if !crate::infrastructure::runtime::recipient_view::recipient_context_available(&view) {
        return Ok(None);
    }
    let body = match view.state {
        RecipientItemState::Received { content } | RecipientItemState::Acknowledged { content } => {
            content
        }
        _ => return Ok(None),
    };
    Ok(Some(CurrentRecipientItem {
        address: view.address,
        body,
        retracted: view.retracted_at_ms.is_some(),
    }))
}
