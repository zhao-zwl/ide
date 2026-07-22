//! Host capability abstraction — the contract the decision Core depends on.
//!
//! The Core must not depend on any concrete IDE (Tauri / VS Code / CLI).
//! [`HostProvider`] defines the minimal capability surface the Core needs to
//! drive a host. Concrete hosts (CLI stub, gRPC client, Tauri shell) implement
//! this trait, which is the heart of the T01 host-decoupling layer.

use async_trait::async_trait;
use std::fmt;

/// Errors raised when a host cannot fulfill a capability request.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The referenced document is not open / does not exist on the host.
    #[error("document not found: {0}")]
    DocumentNotFound(String),
    /// The host rejected an edit (e.g. conflict or policy).
    #[error("edit rejected by host: {0}")]
    EditRejected(String),
    /// Transport-level failure talking to a remote host over the ProtoBus.
    #[error("host transport error: {0}")]
    Transport(String),
    /// The user (or host policy) denied an approval request.
    #[error("approval denied for action: {0}")]
    ApprovalDenied(String),
}

/// A text edit applied to a host document (`old_text` → `new_text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub document_uri: String,
    pub old_text: String,
    pub new_text: String,
}

/// Inline "ghost" (grey) completion shown to the user before acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostText {
    pub document_uri: String,
    /// Cursor offset (char index) where the suggestion is anchored.
    pub cursor_offset: usize,
    /// Suggested text rendered after the cursor.
    pub suggestion: String,
}

/// Telemetry / UI events emitted by the Core toward the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A thinking / planning status line.
    Status(String),
    /// A structured log line.
    Log(String),
    /// A completion was accepted by the user.
    CompletionAccepted(String),
}

impl fmt::Display for HostEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostEvent::Status(s) => write!(f, "[status] {s}"),
            HostEvent::Log(s) => write!(f, "[log] {s}"),
            HostEvent::CompletionAccepted(s) => write!(f, "[accepted] {s}"),
        }
    }
}

/// Capability surface the decision Core requires from a host.
///
/// Implemented by [`super::CliHost`] (stub), [`super::GrpcHostClient`]
/// (remote host over the ProtoBus) and, in production, the Tauri host. The Core
/// only ever talks to this trait, never to a concrete IDE.
#[async_trait]
pub trait HostProvider: Send + Sync {
    /// Apply a text edit to a document.
    async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError>;

    /// Read the full text of a document.
    async fn read_document(&self, uri: &str) -> Result<String, HostError>;

    /// Render an inline ghost (grey) completion.
    async fn show_ghost_text(&self, ghost: GhostText) -> Result<(), HostError>;

    /// Ask the user to approve a potentially risky action (e.g. `git commit`).
    async fn request_approval(&self, action: &str) -> Result<bool, HostError>;

    /// Emit a telemetry / status event (fire-and-forget).
    fn emit_event(&self, event: HostEvent);
}
