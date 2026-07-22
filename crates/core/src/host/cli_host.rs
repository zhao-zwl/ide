//! CLI host stub — a stand-in IDE used to prove host decoupling (T01).
//!
//! It satisfies [`HostProvider`] by printing to stdout and reading from a simple
//! in-memory document store. No real IDE or gRPC is involved, so the Core can
//! run end-to-end against it — demonstrating that the Core never depends on a
//! concrete host implementation.

use crate::host::provider::{
    GhostText, HostError, HostEvent, HostProvider, TextEdit,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// In-memory document store keyed by URI.
#[derive(Default)]
struct DocStore {
    docs: HashMap<String, String>,
}

/// A minimal host that drives the Core from the command line.
pub struct CliHost {
    docs: Mutex<DocStore>,
}

impl Default for CliHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CliHost {
    /// Create an empty CLI host.
    pub fn new() -> Self {
        Self {
            docs: Mutex::new(DocStore::default()),
        }
    }

    /// Seed a document (used by demos / tests).
    pub fn seed(&self, uri: &str, content: &str) {
        self.docs
            .lock()
            .expect("doc store poisoned")
            .docs
            .insert(uri.to_string(), content.to_string());
    }
}

#[async_trait]
impl HostProvider for CliHost {
    async fn apply_edit(&self, edit: TextEdit) -> Result<(), HostError> {
        let mut store = self.docs.lock().expect("doc store poisoned");
        let doc = store
            .docs
            .get_mut(&edit.document_uri)
            .ok_or_else(|| HostError::DocumentNotFound(edit.document_uri.clone()))?;
        if doc.contains(&edit.old_text) {
            *doc = doc.replacen(&edit.old_text, &edit.new_text, 1);
            println!("[cli-host] applied edit to {}", edit.document_uri);
            Ok(())
        } else {
            Err(HostError::EditRejected(
                "old_text not found in document".to_string(),
            ))
        }
    }

    async fn read_document(&self, uri: &str) -> Result<String, HostError> {
        let store = self.docs.lock().expect("doc store poisoned");
        store
            .docs
            .get(uri)
            .cloned()
            .ok_or_else(|| HostError::DocumentNotFound(uri.to_string()))
    }

    async fn show_ghost_text(&self, ghost: GhostText) -> Result<(), HostError> {
        println!(
            "[cli-host] ghost@{}: \"{}\"",
            ghost.cursor_offset, ghost.suggestion
        );
        Ok(())
    }

    async fn request_approval(&self, action: &str) -> Result<bool, HostError> {
        // A real host opens a UI prompt; the CLI stub auto-approves
        // non-destructive actions and asks on destructive ones.
        let approved = !action.to_lowercase().contains("commit")
            && !action.to_lowercase().contains("delete");
        println!("[cli-host] approval({action}) => {approved}");
        Ok(approved)
    }

    fn emit_event(&self, event: HostEvent) {
        println!("[cli-host] {event}");
    }
}
