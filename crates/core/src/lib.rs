//! Agentic IDE decision Core — M1 minimal runnable slice (v0.1 kernel) +
//! v0.5 Stage A (T05/T06/T07).
//!
//! This crate is deliberately **host-agnostic**: it depends only on the
//! [`host::HostProvider`] abstraction and injected capability traits
//! ([`Llm`], [`ToolExecutor`], [`Validator`]). The ProtoBus (`tonic` server in
//! [`server`] / [`agent`]) and the NES probe (via the `ide-probe` crate) are
//! layered on top, proving the T01–T04 M1 主轴 (host-decoupling + ProtoBus +
//! Core + NES probe).
//!
//! Decision Core (four-layer architecture):
//! ```text
//!   Planner (ReAct loop)
//!     ├─ Llm            (reasoning + action selection)
//!     ├─ ToolExecutor   (run actions -> observations)
//!     ├─ ContextManager (rolling working memory + vectorization + T05 sources)
//!     ├─ Validator      (safety rail / stop condition)
//!     └─ HostBridge     (drive the IDE host via HostProvider)
//! ```
//!
//! v0.5 Stage A extends the Core with:
//!   * [`context_manager`] + [`retrieval`] — T05 multi-source context, tiered
//!     token-budget trimming, and vector/hybrid retrieval.
//!   * [`chat`] — T06 multi-turn chat reusing the same `Llm` as the Planner,
//!     with tool-call suggestions over the frozen `AgentService.Chat`.
//!   * [`craft`] — T07 human-led editing: propose/gate/apply edits behind the
//!     six-bit permission mask, never touching files without confirmation.
//!
//! v1.0 Stage A extends the Core with enterprise hardening (T11 / T14):
//!   * [`principal`] — T11 security identity (`Principal`) threading through the
//!     whole call chain + the six [`AuditAction`]s + [`check_permission`].
//!   * [`audit`] — T11 append-only audit logging via the [`AuditSink`] trait
//!     (`MockAuditSink` for tests/offline, `PgAuditSink` for Postgres).
//!   * [`metrics`] — T14 atomic counters + histograms rendered as Prometheus
//!     text via [`Metrics::render_prometheus`].
//!   * [`admin`] — T14 a tiny `tokio::net` TCP listener serving `/metrics`,
//!     `/healthz` and (v1.0B) `/console` (no axum/actix; outside `.proto`).
//!
//! v1.0 Stage B (T12 / T13) extends the Core with enterprise access + console:
//!   * [`auth`] — T12 pluggable [`Authenticator`] (`NoopAuthenticator` dev /
//!     `Hs256Authenticator` SSO) deriving a per-request [`Principal`] from a
//!     bearer token at the gRPC boundary.
//!   * [`admin`]'s `/console` route + the `aidea console` CLI subcommand form
//!     the *no-UI* enterprise console (T13); the high-fidelity GUI is deferred.
//!
//! v1.5 Stage A (T15 / T16) — autonomous agent + self-healing:
//!   * [`quest`] — T15 autonomous agent: decomposes a high-level `goal` into an
//!     ordered [`SubTask`] list, then runs each subtask through the *existing*
//!     [`Planner`] (reused, not reimplemented) with the governance triple
//!     injected, so the six-bit mask is still enforced. An approval gate
//!     (`auto_commit`) collects `Execute`/`Commit` class actions as pending
//!     approvals when false, and runs them autonomously when true.
//!   * [`self_heal`] — T16自愈 executor: wraps a tool so a failed run is
//!     auto-repaired by an [`Llm`]-generated patch and re-run, with a
//!     `max_repair_attempts` circuit breaker. Reused by [`quest`] per subtask.
//!
//! v1.5 Stage B (T17 / T18) — code knowledge graph + engineering:
//!   * [`ckg`] — T17 code knowledge graph: lightweight (no `tree-sitter` /
//!     `regex`) symbol + edge extraction into an in-memory [`CkgIndex`]
//!     ([`InMemoryCkg`]), with an optional [`PgCkgStore`] persisting to the
//!     `0004_v15.sql` tables. Used to enrich context *beyond* pure-text
//!     similarity (pull in callers/callees/containers of a referenced symbol).
//!   * [`engineering`] — T18 build/test/git automation: [`GitClient`] and
//!     [`BuildRunner`] shell out to `git`/`cargo` via `tokio::process::Command`,
//!     and [`ShellTool`] plugs the runners into the v1.5A [`SelfHeal`] executor
//!     so a failed build/test yields real stderr that an LLM patch can repair
//!     (and into a [`Quest`](crate::quest::Quest) as its base executor).

// Allow intra-crate references via the published crate name `ide_core`.
// The gRPC layer and the QA tests import the Core by name, so `ide_core::v1::…`
// must resolve from *inside* the crate too.
extern crate self as ide_core;

pub mod admin;
pub mod agent;
pub mod audit;
pub mod auth;
pub mod chat;
pub mod collab;
pub mod config;
pub mod context_manager;
pub mod craft;
pub mod health;
pub mod host;
pub mod llm;
pub mod metrics;
pub mod permissions;
pub mod planner;
pub mod principal;
pub mod quest;
pub mod retrieval;
pub mod server;
pub mod security;
pub mod self_heal;
pub mod ckg;
pub mod engineering;
pub mod tool_executor;
pub mod validator;

// Re-export the NES probe surface so the `aidea` CLI (and other consumers) can
// drive the Core without depending on `ide-probe` directly. Keeps the dual-form
// IDE (desktop + CLI) sharing one decision Core and one probe client (T08/T09).
pub use ide_probe::{
    derive_rule_candidates, Candidate, CompletionBackend, CompletionItem, CompletionProvider,
    LspContext, MockOllamaClient, NesClient, OllamaClient, OllamaConfig, ProbeCompletionProvider,
    RuleBasedBackend, rank_completions, speed_test,
};

pub use config::{CoreConfig, LlmBackend, NesBackend};
pub use agent::{build_llm, default_nes_backend};
pub use llm::{MockLlm, OllamaLlm, OpenAiLlm};

// v1.0 Stage A (T11 / T14) public surface.
pub use audit::{AuditEvent, AuditSink, MockAuditSink, PgAuditSink};
pub use auth::{AuthError, Authenticator, Hs256Authenticator, NoopAuthenticator};
pub use metrics::{Histogram, Metrics, SharedMetrics};
pub use principal::{AuditAction, Principal, check_permission};

// v1.5 Stage A (T15 / T16) public surface.
pub use quest::{
    GoalDecomposer, LlmGoalDecomposer, PendingApproval, Quest, QuestConfig, QuestReport, SubTask,
    SubTaskStatus, parse_subtasks,
};
pub use self_heal::{parse_patch, RepairOutcome, SelfHeal, SelfHealingExecutor};

// v1.5 Stage B (T17 / T18) public surface.
pub use ckg::{
    CkgIndex, Edge, EdgeKind, InMemoryCkg, PgCkgStore, RelatedSymbol, Symbol, SymbolKind,
};
pub use engineering::{
    BuildResult, BuildRunner, GitClient, GitResult, RunOutput, ShellTool, run_shell,
};

// v2.0 Stage A (T19 / T20) public surface.
pub use collab::{
    Comment, CommentStore, InMemoryCommentStore, Lock, LockStore, InMemoryLockStore, PgCommentStore,
    SET_TENANT_LOCAL_SQL as COLLAB_SET_TENANT_LOCAL_SQL,
};
pub use security::{PgSecretStore, SET_TENANT_LOCAL_SQL as SECURITY_SET_TENANT_LOCAL_SQL};

/// Generated ProtoBus types from `proto/ide_core.proto`.
///
/// Re-exported as `ide_core::v1` so both in-crate and external callers use the
/// same path (`ide_core::v1::agent_server::Agent`, etc.).
pub mod proto {
    //! Compiled gRPC types for the ProtoBus contract.
    pub mod ide_core {
        tonic::include_proto!("ide_core");
    }
}

pub use proto::ide_core as v1;
