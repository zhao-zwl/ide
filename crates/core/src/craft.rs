//! Craft mode — human-led editing (T07, F3-base).
//!
//! The user edits inside the IDE; the Agent only *proposes* suggestions,
//! completions and explanations. No file is changed unless the user confirms.
//!
//! This module implements the ApplyEdit flow using the proto
//! [`ApplyEditRequest`](crate::v1::ApplyEditRequest) shape via the existing
//! [`HostProvider::apply_edit`](crate::host::provider::HostProvider::apply_edit)
//! (already implemented by `CliHost` and `GrpcHostClient`). Every state
//! transition is gated behind the six-bit permission mask — reusing
//! [`permissions`]: `Modify` for file edits, `Execute` for commands, `Commit`
//! for VCS commits.
//!
//! State machine per proposal: `Suggestion -> PendingConfirm -> Applied | Rejected`.

use crate::audit::{AuditEvent, AuditSink, MockAuditSink};
use crate::host::provider::{HostEvent, TextEdit};
use crate::host::HostBridge;
use crate::metrics::Metrics;
use crate::permissions::{Permission, PermissionDenied, PermissionSet};
use crate::principal::{AuditAction, Principal, check_permission};
use serde_json::json;
use std::sync::Arc;

/// Lifecycle of a Craft proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftState {
    /// Proposed by the Agent; not yet reviewed by the user.
    Suggestion,
    /// Under user review (UI has opened the diff).
    PendingConfirm,
    /// Confirmed and applied to the host.
    Applied,
    /// Rejected by the user; never applied.
    Rejected,
}

/// What kind of action a proposal represents — drives which permission bit gates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// A file text edit — gated by `Modify`.
    FileEdit,
    /// Running a command — gated by `Execute`.
    RunCommand,
    /// A VCS commit — gated by `Commit`.
    Commit,
}

impl EditKind {
    /// The six-bit permission required to apply this kind of proposal.
    pub fn required_permission(self) -> Permission {
        match self {
            EditKind::FileEdit => Permission::Modify,
            EditKind::RunCommand => Permission::Execute,
            EditKind::Commit => Permission::Commit,
        }
    }

    /// Stable string form (mirrors the `craft_proposals.kind` CHECK constraint
    /// added in `migrations/0002_v05.sql`).
    pub fn as_str(self) -> &'static str {
        match self {
            EditKind::FileEdit => "FileEdit",
            EditKind::RunCommand => "RunCommand",
            EditKind::Commit => "Commit",
        }
    }
}

/// A single Agent-proposed edit awaiting user confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftProposal {
    pub id: String,
    pub document_uri: String,
    pub old_text: String,
    pub new_text: String,
    pub rationale: String,
    pub kind: EditKind,
    pub state: CraftState,
}

impl CraftProposal {
    /// Create a proposal in the `Suggestion` state (no file change yet).
    pub fn new(
        id: &str,
        document_uri: &str,
        old_text: &str,
        new_text: &str,
        rationale: &str,
        kind: EditKind,
    ) -> Self {
        Self {
            id: id.to_string(),
            document_uri: document_uri.to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
            rationale: rationale.to_string(),
            kind,
            state: CraftState::Suggestion,
        }
    }

    /// Mark the proposal as under review (UI opened the diff).
    pub fn mark_pending(&mut self) {
        if self.state == CraftState::Suggestion {
            self.state = CraftState::PendingConfirm;
        }
    }
}

/// Craft engine: propose, gate (permissions), and apply edits — always behind
/// explicit user confirmation.
pub struct CraftEngine {
    bridge: Arc<HostBridge>,
    perms: PermissionSet,
    /// v1.0 — security identity authorizing Modify/Execute/Commit (T11).
    principal: Principal,
    /// v1.0 — append-only audit sink (T11).
    audit: Arc<dyn AuditSink>,
    /// v1.0 — observability counters (T14).
    metrics: Arc<Metrics>,
}

impl CraftEngine {
    /// Build a craft engine bound to a host bridge and a permission set.
    pub fn new(bridge: Arc<HostBridge>, perms: PermissionSet) -> Self {
        Self {
            bridge,
            perms,
            principal: Principal::from_mask("single", "craft", perms.mask()),
            audit: Arc::new(MockAuditSink::new()),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Override the governance triple (principal / audit sink / metrics).
    pub fn with_governance(
        mut self,
        principal: Principal,
        audit: Arc<dyn AuditSink>,
        metrics: Arc<Metrics>,
    ) -> Self {
        self.perms = principal.perms;
        self.principal = principal;
        self.audit = audit;
        self.metrics = metrics;
        self
    }

    /// Propose an edit. Returns a `Suggestion`-state proposal; no file changes
    /// happen until [`confirm`](CraftEngine::confirm).
    pub fn propose(
        &self,
        document_uri: &str,
        old_text: &str,
        new_text: &str,
        rationale: &str,
        kind: EditKind,
    ) -> CraftProposal {
        CraftProposal::new(&format!("p-{}", fast_id()), document_uri, old_text, new_text, rationale, kind)
    }

    /// Check the proposal's required permission bit against the engine's
    /// principal. Mirrors the enforcement performed inside [`confirm`].
    pub fn check_permissions(&self, proposal: &CraftProposal) -> Result<(), PermissionDenied> {
        check_permission(&self.principal, proposal.kind.required_permission())
    }

    /// Confirm + apply. Verifies permissions, then (for `FileEdit`) applies the
    /// edit through the host. Transitions `Suggestion`/`PendingConfirm` ->
    /// `Applied`. Returns an error (permission denied or host rejection) without
    /// changing state on failure. Every outcome (granted or denied) is written
    /// to the append-only audit log (T11).
    pub async fn confirm(&self, proposal: &mut CraftProposal) -> anyhow::Result<CraftState> {
        let action = proposal.kind.required_permission();
        match check_permission(&self.principal, action) {
            Ok(()) => {
                self.audit(
                    AuditAction::from_permission(action),
                    true,
                    json!({ "kind": proposal.kind.as_str(), "uri": proposal.document_uri }),
                )
                .await;
            }
            Err(e) => {
                self.metrics.inc_denials();
                self.audit(
                    AuditAction::from_permission(action),
                    false,
                    json!({
                        "kind": proposal.kind.as_str(),
                        "uri": proposal.document_uri,
                        "reason": e.to_string()
                    }),
                )
                .await;
                return Err(e.into());
            }
        }

        match proposal.kind {
            EditKind::FileEdit => {
                self.metrics.inc_tool_calls();
                self.bridge
                    .apply_edit(TextEdit {
                        document_uri: proposal.document_uri.clone(),
                        old_text: proposal.old_text.clone(),
                        new_text: proposal.new_text.clone(),
                    })
                    .await?;
            }
            EditKind::RunCommand | EditKind::Commit => {
                // Gated by the permission bit above; the actual execution is
                // deferred to host tooling (T18). Core's job here is to enforce
                // the bit and emit an auditable event.
                self.metrics.inc_tool_calls();
                self.bridge.emit_event(HostEvent::Status(format!(
                    "craft {} approved: {}",
                    proposal.kind.as_str(),
                    proposal.document_uri
                )));
            }
        }
        proposal.state = CraftState::Applied;
        Ok(proposal.state)
    }

    /// Append an audit event (fire-and-forget; errors are logged, never fatal).
    async fn audit(&self, action: AuditAction, granted: bool, payload: serde_json::Value) {
        let ev = AuditEvent::new(&self.principal, action, granted, payload);
        if let Err(e) = self.audit.record(&ev).await {
            tracing::warn!("craft audit record failed: {e}");
        }
    }

    /// Reject a proposal (unless it has already been applied).
    pub fn reject(proposal: &mut CraftProposal) {
        if proposal.state != CraftState::Applied {
            proposal.state = CraftState::Rejected;
        }
    }
}

/// A multi-file Craft plan: a batch of proposals the user reviews as one diff.
#[derive(Debug, Default)]
pub struct CraftSession {
    pub proposals: Vec<CraftProposal>,
}

impl CraftSession {
    pub fn new() -> Self {
        Self {
            proposals: Vec::new(),
        }
    }

    /// Add a proposal to the plan.
    pub fn add(&mut self, p: CraftProposal) {
        self.proposals.push(p);
    }

    /// Proposals still awaiting a decision.
    pub fn pending(&self) -> Vec<&CraftProposal> {
        self.proposals
            .iter()
            .filter(|p| p.state == CraftState::Suggestion || p.state == CraftState::PendingConfirm)
            .collect()
    }

    /// Confirm every pending proposal through `engine`. Returns one result per
    /// proposal, preserving order.
    pub async fn confirm_all(&mut self, engine: &CraftEngine) -> Vec<anyhow::Result<CraftState>> {
        let mut out = Vec::with_capacity(self.proposals.len());
        for p in self.proposals.iter_mut() {
            out.push(engine.confirm(p).await);
        }
        out
    }

    /// Reject every proposal in the plan.
    pub fn reject_all(&mut self) {
        for p in self.proposals.iter_mut() {
            CraftEngine::reject(p);
        }
    }
}

/// Cheap, deterministic id for demo proposals (no external `uuid` dependency).
fn fast_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{CliHost, HostBridge, HostProvider};

    fn engine(perms: PermissionSet) -> CraftEngine {
        let host = Arc::new(CliHost::new());
        let bridge = Arc::new(HostBridge::new(host));
        CraftEngine::new(bridge, perms)
    }

    #[tokio::test]
    async fn file_edit_applied_after_confirm() {
        let host = Arc::new(CliHost::new());
        host.seed("mem://a.rs", "let x = 1;");
        let bridge = Arc::new(HostBridge::new(host.clone()));
        let eng = CraftEngine::new(bridge, PermissionSet::all());
        let mut p = eng.propose("mem://a.rs", "let x = 1;", "let x = 2;", "bump value", EditKind::FileEdit);
        assert_eq!(p.state, CraftState::Suggestion);
        p.mark_pending();
        let st = eng.confirm(&mut p).await.unwrap();
        assert_eq!(st, CraftState::Applied);
        assert_eq!(host.read_document("mem://a.rs").await.unwrap(), "let x = 2;");
    }

    #[tokio::test]
    async fn missing_modify_permission_blocks_edit() {
        let host = Arc::new(CliHost::new());
        host.seed("mem://a.rs", "let x = 1;");
        let bridge = Arc::new(HostBridge::new(host));
        // Read granted, Modify missing.
        let eng = CraftEngine::new(bridge, PermissionSet::empty().grant(Permission::Read));
        let mut p = eng.propose("mem://a.rs", "let x = 1;", "let x = 9;", "x", EditKind::FileEdit);
        let err = eng.confirm(&mut p).await.unwrap_err();
        assert!(err.to_string().contains("permission denied"));
        // State is unchanged on denial.
        assert_eq!(p.state, CraftState::Suggestion);
    }

    #[tokio::test]
    async fn run_command_requires_execute_bit() {
        let eng = engine(PermissionSet::empty());
        let mut p = eng.propose("sh", "", "cargo test", "run tests", EditKind::RunCommand);
        assert!(eng.confirm(&mut p).await.is_err());

        let eng2 = engine(PermissionSet::all());
        let mut p2 = eng2.propose("sh", "", "cargo test", "run tests", EditKind::RunCommand);
        assert_eq!(eng2.confirm(&mut p2).await.unwrap(), CraftState::Applied);
    }

    #[tokio::test]
    async fn commit_requires_commit_bit() {
        let eng = engine(PermissionSet::empty().grant(Permission::Modify));
        let mut p = eng.propose("vcs", "", "commit -a", "snapshot", EditKind::Commit);
        assert!(eng.confirm(&mut p).await.is_err());

        let eng2 = engine(PermissionSet::all());
        let mut p2 = eng2.propose("vcs", "", "commit -a", "snapshot", EditKind::Commit);
        assert_eq!(eng2.confirm(&mut p2).await.unwrap(), CraftState::Applied);
    }

    #[test]
    fn reject_transitions_state() {
        let eng = engine(PermissionSet::all());
        let mut p = eng.propose("mem://a.rs", "a", "b", "r", EditKind::FileEdit);
        CraftEngine::reject(&mut p);
        assert_eq!(p.state, CraftState::Rejected);
    }

    #[tokio::test]
    async fn multi_file_session_confirm_all() {
        let host = Arc::new(CliHost::new());
        host.seed("mem://a.rs", "A");
        host.seed("mem://b.rs", "B");
        let bridge = Arc::new(HostBridge::new(host.clone()));
        let eng = CraftEngine::new(bridge, PermissionSet::all());
        let mut session = CraftSession::new();
        session.add(eng.propose("mem://a.rs", "A", "A2", "a", EditKind::FileEdit));
        session.add(eng.propose("mem://b.rs", "B", "B2", "b", EditKind::FileEdit));
        assert_eq!(session.pending().len(), 2);
        let results = session.confirm_all(&eng).await;
        assert!(results.iter().all(|r| r.is_ok()));
        assert_eq!(host.read_document("mem://a.rs").await.unwrap(), "A2");
        assert_eq!(host.read_document("mem://b.rs").await.unwrap(), "B2");
    }
}
