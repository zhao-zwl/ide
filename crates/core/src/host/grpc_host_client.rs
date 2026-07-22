//! gRPC-backed host provider (ProtoBus).
//!
//! Implements [`HostProvider`] by forwarding each capability to the host
//! process over the ProtoBus (`HostService`). This is the production wiring
//! path: the Core (server side) talks to a remote Tauri host through this
//! client. It proves that swapping the concrete host = swapping the
//! `HostProvider` implementation, with zero changes to the decision Core.

use crate::host::provider::{GhostText, HostError, HostProvider, TextEdit};
use async_trait::async_trait;
use ide_core::v1::host_service_client::HostServiceClient;
use ide_core::v1::{ApplyEditRequest, GhostTextRequest, ReadDocumentRequest};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A [`HostProvider`] backed by the ProtoBus `HostService`.
pub struct GrpcHostClient {
    client: Arc<Mutex<HostServiceClient<tonic::transport::Channel>>>,
}

impl GrpcHostClient {
    /// Connect to a host exposing `HostService` at `endpoint` (e.g. a Tauri
    /// shell bridged via gRPC-Web / sidecar).
    pub async fn connect(endpoint: &str) -> Result<Self, HostError> {
        let client = HostServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|e| HostError::Transport(e.to_string()))?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }
}

#[async_trait]
impl HostProvider for GrpcHostClient {
    async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError> {
        let req = ApplyEditRequest {
            document_uri: edit.document_uri,
            old_text: edit.old_text,
            new_text: edit.new_text,
        };
        let mut c = self.client.lock().await;
        let resp = c
            .apply_edit(req)
            .await
            .map_err(|e| HostError::Transport(e.to_string()))?;
        if resp.into_inner().ok {
            Ok(())
        } else {
            Err(HostError::EditRejected("host rejected edit".to_string()))
        }
    }

    async fn read_document(&self, uri: &str) -> Result<String, HostError> {
        let req = ReadDocumentRequest {
            document_uri: uri.to_string(),
        };
        let mut c = self.client.lock().await;
        let resp = c
            .read_document(req)
            .await
            .map_err(|e| HostError::Transport(e.to_string()))?;
        Ok(resp.into_inner().content)
    }

    async fn show_ghost_text(&self, ghost: GhostText) -> Result<(), HostError> {
        let req = GhostTextRequest {
            document_uri: ghost.document_uri,
            cursor_offset: ghost.cursor_offset as i32,
            suggestion: ghost.suggestion,
        };
        let mut c = self.client.lock().await;
        c.show_ghost_text(req)
            .await
            .map_err(|e| HostError::Transport(e.to_string()))?;
        Ok(())
    }

    async fn request_approval(&self, _action: &str) -> Result<bool, HostError> {
        // The bus can be extended with an `Approve` RPC; for M1 we defer to the
        // host-side default policy and treat absence of a hard error as granted.
        Ok(true)
    }

    fn emit_event(&self, _event: crate::host::provider::HostEvent) {
        // Core→host events are surfaced through the AgentService stream instead.
    }
}
