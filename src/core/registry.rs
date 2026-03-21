use std::collections::HashMap;

use anyhow::Result;

use crate::types::capability::{
    ActivatedCapability, Capability, CapabilityEvent, CapabilityInput, CapabilityProvider,
    MentionCategory, SessionContext, WorkspaceConfig,
};
use tokio::sync::mpsc;

/// Central registry of all capability providers.
///
/// At startup, each protocol registers its provider. The registry
/// handles discovery, @mention grouping, and routing execution
/// to the correct provider — protocol-agnostically.
///
/// Adding a new protocol:
///   registry.register(Box::new(FutureProtocolProvider::new()));
///   // That's it. @FutureProtocol/ appears in mentions.
pub struct CapabilityRegistry {
    providers: Vec<Box<dyn CapabilityProvider>>,
    capabilities: Vec<Capability>,
    activated: HashMap<String, ActivatedCapability>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            capabilities: Vec::new(),
            activated: HashMap::new(),
        }
    }

    /// Register a new protocol provider
    pub fn register(&mut self, provider: Box<dyn CapabilityProvider>) {
        self.providers.push(provider);
    }

    /// Discover all capabilities from all registered providers
    pub async fn discover_all(&mut self, config: &WorkspaceConfig) -> Result<()> {
        self.capabilities.clear();
        for provider in &self.providers {
            match provider.discover(config).await {
                Ok(caps) => self.capabilities.extend(caps),
                Err(e) => {
                    tracing::warn!(
                        "Failed to discover capabilities from {}: {}",
                        provider.protocol(),
                        e
                    );
                }
            }
        }
        Ok(())
    }

    /// Get all discovered capabilities
    pub fn all(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Get capabilities grouped by @mention category
    pub fn for_mention(&self) -> HashMap<MentionCategory, Vec<&Capability>> {
        let mut grouped: HashMap<MentionCategory, Vec<&Capability>> = HashMap::new();
        for provider in &self.providers {
            let category = provider.mention_category();
            let caps: Vec<&Capability> = self
                .capabilities
                .iter()
                .filter(|c| c.protocol == provider.protocol())
                .collect();
            if !caps.is_empty() {
                grouped.entry(category).or_default().extend(caps);
            }
        }
        grouped
    }

    /// Resolve a mention string (e.g., "@MCP/db-tools") to a capability
    pub fn resolve_mention(&self, mention: &str) -> Option<&Capability> {
        // Parse "@Category/name" format
        let mention = mention.trim_start_matches('@');
        let parts: Vec<&str> = mention.splitn(2, '/').collect();
        if parts.len() != 2 {
            return None;
        }
        let (category_str, name) = (parts[0], parts[1]);

        self.capabilities.iter().find(|c| {
            let provider = self.providers.iter().find(|p| p.protocol() == c.protocol);
            if let Some(provider) = provider {
                provider.mention_category().display_name() == category_str && c.name == name
            } else {
                false
            }
        })
    }

    /// Activate a capability and store it
    pub async fn activate(
        &mut self,
        capability_id: &str,
        session: &SessionContext,
    ) -> Result<()> {
        let cap = self
            .capabilities
            .iter()
            .find(|c| c.id == capability_id)
            .ok_or_else(|| anyhow::anyhow!("Capability not found: {}", capability_id))?
            .clone();

        let provider = self
            .providers
            .iter()
            .find(|p| p.protocol() == cap.protocol)
            .ok_or_else(|| anyhow::anyhow!("Provider not found: {}", cap.protocol))?;

        let activated = provider.activate(&cap, session).await?;
        self.activated.insert(capability_id.to_string(), activated);
        Ok(())
    }

    /// Execute an activated capability
    pub async fn execute(
        &self,
        capability_id: &str,
        input: CapabilityInput,
        tx: &mpsc::UnboundedSender<CapabilityEvent>,
    ) -> Result<()> {
        let activated = self
            .activated
            .get(capability_id)
            .ok_or_else(|| anyhow::anyhow!("Capability not activated: {}", capability_id))?;

        let cap = self
            .capabilities
            .iter()
            .find(|c| c.id == capability_id)
            .ok_or_else(|| anyhow::anyhow!("Capability not found: {}", capability_id))?;

        let provider = self
            .providers
            .iter()
            .find(|p| p.protocol() == cap.protocol)
            .ok_or_else(|| anyhow::anyhow!("Provider not found: {}", cap.protocol))?;

        provider.execute(activated, input, tx).await
    }

    /// Get the number of registered providers
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Get the number of discovered capabilities
    pub fn capability_count(&self) -> usize {
        self.capabilities.len()
    }
}
