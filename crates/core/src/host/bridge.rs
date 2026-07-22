//! HostBridge — the single point the Core uses to talk to a host.
//!
//! `HostBridge` wraps any [`HostProvider`] and adds:
//!   * a uniform facade with convenience helpers,
//!   * graceful approval handling (transport failures ⇒ denied, never panics),
//!   * a swap point: changing the concrete provider (CLI stub ↔ gRPC client)
//!     requires no changes inside the decision Core.

use crate::host::provider::{GhostText, HostError, HostEvent, HostProvider, TextEdit};
use std::sync::Arc;

/// Mediator between the decision Core and a concrete `HostProvider`.
pub struct HostBridge {
    provider: Arc<dyn HostProvider>,
}

impl HostBridge {
    /// Build a bridge over the given host provider.
    pub fn new(provider: Arc<dyn HostProvider>) -> Self {
        Self { provider }
    }

    /// Read a document through the underlying provider.
    pub async fn read_document(&self, uri: &str) -> Result<String, HostError> {
        self.provider.read_document(uri).await
    }

    /// Apply a text edit through the underlying provider.
    pub async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError> {
        self.provider.apply_edit(edit).await
    }

    /// Show a ghost completion through the underlying provider.
    pub async fn show_ghost_text(&self, ghost: GhostText) -> Result<(), HostError> {
        self.provider.show_ghost_text(ghost).await
    }

    /// Request approval; returns `false` on denial **or** transport failure.
    ///
    /// Never fails: a broken host must not crash the Core's safety rail.
    pub async fn request_approval(&self, action: &str) -> bool {
        match self.provider.request_approval(action).await {
            Ok(approved) => approved,
            Err(e) => {
                self.emit_event(HostEvent::Log(format!("approval error: {e}")));
                false
            }
        }
    }

    /// Emit an event (delegated to the provider).
    pub fn emit_event(&self, event: HostEvent) {
        self.provider.emit_event(event);
    }
}

#[async_trait::async_trait]
impl HostProvider for HostBridge {
    async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError> {
        self.provider.apply_edit(edit).await
    }
    async fn read_document(&self, uri: &str) -> Result<String, HostError> {
        self.provider.read_document(uri).await
    }
    async fn show_ghost_text(&self, ghost: GhostText) -> Result<(), HostError> {
        self.provider.show_ghost_text(ghost).await
    }
    async fn request_approval(&self, action: &str) -> Result<bool, HostError> {
        self.provider.request_approval(action).await
    }
    fn emit_event(&self, event: HostEvent) {
        self.provider.emit_event(event);
    }
}
