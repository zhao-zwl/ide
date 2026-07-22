//! QA verification tests for v1.0 Stage A (T11 six-rights + T14 observability).
//!
//! Complements the engineer's in-module unit tests by covering the gaps called
//! out in the QA brief:
//!   * TG1/TG2 — governance-wiring regression: the M1 default stack
//!     (`AgentServer::default_stack`, full six-bit Principal + in-memory sink)
//!     still constructs and runs after T11/T14 injection, and the
//!     `with_governance` triple actually routes audits + metrics on the happy
//!     path. No custom LLM doubles are needed — we reuse the Planner's own
//!     `MockLlm` via `Planner::llm()`.
//!   * TG3 — migrations/0003_v10.sql is *purely incremental*: it only
//!     `CREATE OR REPLACE FUNCTION`s and never ALTERs/DROPs the 0001/0002
//!     objects.
//!   * TG4 — proto/ide_core.proto is frozen at G6: no admin/metrics
//!     service leaked in, and AgentService/HostService keep exactly their
//!     frozen RPC sets.
//!   * TG5 — crates/core/Cargo.toml dependency boundary: only
//!     `tokio-postgres` is added (no TLS); no axum/actix/external-metrics
//!     framework.
//!   * TG6 — config defaults: `admin_addr` default + `perm_mask` clamp to
//!     the six-bit domain (0..63), mirroring the SQL `perm_mask` domain.
//!
//! No external services (DB / network / heavy deps): pure logic + file text
//! embedded via `include_str!`. Runnable under the OOM-constrained baseline.

use ide_core::agent::AgentServer;
use ide_core::audit::MockAuditSink;
use ide_core::chat::{ChatEngine, ChatSession};
use ide_core::config::CoreConfig;
use ide_core::host::{CliHost, HostBridge};
use ide_core::metrics::Metrics;
use ide_core::permissions::PermissionSet;
use ide_core::planner::Planner;
use ide_core::principal::{AuditAction, Principal};
use std::sync::Arc;

// --- TG1: M1 default stack survives governance injection -------------------
#[test]
fn default_stack_constructs_with_full_principal_and_memory_sink() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    // The M1 demo default stack: full six-bit Principal + in-memory audit sink.
    let svc = AgentServer::default_stack(bridge);
    // Governance is wired: a metrics handle is exposed and usable.
    let m = svc.metrics();
    m.inc_requests();
    assert_eq!(m.requests(), 1);
}

// --- TG2: with_governance routes audit + metrics on the happy path --------
#[tokio::test]
async fn planner_with_governance_runs_and_counts_tool_calls() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let audit = Arc::new(MockAuditSink::new());
    let metrics = Arc::new(Metrics::new());
    let principal = Principal::all("single", "tester");
    let planner = Planner::with_defaults(bridge, 6)
        .with_governance(principal, audit.clone(), metrics.clone());
    let trace = planner.run("goal").await.unwrap();
    // MockLlm plans `inspect` each step (non-privileged => no audit, but a
    // counted tool call), so the loop terminates on budget and metrics move.
    assert_eq!(trace.final_answer, "budget exhausted");
    assert!(metrics.tool_calls() >= 1, "tool calls must be counted");
    // inspect is non-privileged: no audit events expected.
    assert_eq!(audit.count(), 0);
}

#[tokio::test]
async fn chat_engine_with_governance_audits_generate_and_counts_llm() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let planner = Planner::with_defaults(bridge, 8);
    let llm = planner.llm(); // reuse the existing MockLlm — no custom double needed
    let audit = Arc::new(MockAuditSink::new());
    let metrics = Arc::new(Metrics::new());
    let engine = ChatEngine::new(llm, PermissionSet::all()).with_governance(
        Principal::all("single", "tester"),
        audit.clone(),
        metrics.clone(),
    );
    let mut session = ChatSession::new(2048, 16);
    let _ = engine.reply(&mut session, "hi", &[]).await.unwrap();
    // T11: a granted Generate audit was written.
    assert_eq!(audit.count_action(AuditAction::Generate), 1);
    assert_eq!(audit.count_denied(AuditAction::Generate), 0);
    // T14: the LLM call counter advanced.
    assert_eq!(metrics.llm_calls(), 1);
}

#[tokio::test]
async fn chat_engine_denied_without_generate_bit() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let planner = Planner::with_defaults(bridge, 8);
    let llm = planner.llm();
    let audit = Arc::new(MockAuditSink::new());
    let metrics = Arc::new(Metrics::new());
    // No Generate bit => the boundary check must deny before any LLM call.
    let engine = ChatEngine::new(llm, PermissionSet::empty()).with_governance(
        Principal::new("single", "tester", PermissionSet::empty()),
        audit.clone(),
        metrics.clone(),
    );
    let mut session = ChatSession::new(2048, 16);
    let err = engine.reply(&mut session, "hi", &[]).await.unwrap_err();
    assert!(err.to_string().contains("permission denied"));
    assert_eq!(audit.count_denied(AuditAction::Generate), 1);
    assert_eq!(metrics.llm_calls(), 0);
    assert_eq!(metrics.denials(), 1);
}

// --- TG3: 0003 is purely incremental (no ALTER/DROP of 0001/0002) --------
const MIG_0003: &str = include_str!("../../../migrations/0003_v10.sql");

#[test]
fn mig_0003_is_purely_incremental() {
    assert!(
        MIG_0003.contains("CREATE OR REPLACE FUNCTION"),
        "0003 must add objects via CREATE OR REPLACE FUNCTION"
    );
    for forbidden in ["ALTER TABLE", "DROP TABLE", "TRUNCATE", "CREATE TABLE"] {
        assert!(
            !MIG_0003.to_uppercase().contains(forbidden),
            "0003 must not contain {forbidden} (must not mutate 0001/0002)"
        );
    }
    // The two added helpers target the 0001 `audit` table only.
    assert!(MIG_0003.contains("log_audit"));
    assert!(MIG_0003.contains("log_audit_event"));
}

// --- TG4: proto contract frozen at G6 (no admin/metrics service) ----------
const PROTO: &str = include_str!("../../../proto/ide_core.proto");

#[test]
fn proto_contract_frozen_no_admin_metrics_service() {
    // No new admin/metrics surface leaked into the frozen gRPC contract.
    assert!(
        !PROTO.contains("service Admin"),
        "AdminService must not enter the frozen proto"
    );
    assert!(
        !PROTO.contains("service Metrics"),
        "MetricsService must not enter the frozen proto"
    );
    assert!(!PROTO.contains("rpc Admin"), "no Admin RPC");
    assert!(!PROTO.contains("rpc Metrics"), "no Metrics RPC");

    // AgentService keeps exactly its frozen four RPCs.
    let agent = extract_service(&PROTO, "AgentService");
    for rpc in ["Ping", "NesComplete", "RunAgent", "Chat"] {
        assert!(
            agent.contains(&format!("rpc {rpc}")),
            "AgentService must keep rpc {rpc}"
        );
    }
    assert_eq!(count_rpcs(&agent), 4, "AgentService must not gain RPCs");

    // HostService keeps exactly its three frozen RPCs.
    let host = extract_service(&PROTO, "HostService");
    for rpc in ["ApplyEdit", "ShowGhostText", "ReadDocument"] {
        assert!(
            host.contains(&format!("rpc {rpc}")),
            "HostService must keep rpc {rpc}"
        );
    }
    assert_eq!(count_rpcs(&host), 3, "HostService must not gain RPCs");
}

fn extract_service(src: &str, name: &str) -> String {
    let marker = format!("service {name} {{");
    let start = src.find(&marker).expect("service must exist");
    let after = &src[start..];
    let end = after.find("\n}").unwrap_or(after.len());
    after[..end].to_string()
}

fn count_rpcs(svc: &str) -> usize {
    svc.lines()
        .filter(|l| l.trim_start().starts_with("rpc "))
        .count()
}

// --- TG5: core Cargo.toml dependency boundary -----------------------------
const CORE_TOML: &str = include_str!("../Cargo.toml");

#[test]
fn core_cargo_only_adds_tokio_postgres() {
    assert!(
        CORE_TOML.contains("tokio-postgres"),
        "T11 audit sink needs tokio-postgres"
    );
    assert!(
        CORE_TOML.contains("runtime-tokio"),
        "must use tokio runtime (no blocking driver)"
    );
    assert!(
        !CORE_TOML.to_lowercase().contains("default-features = true"),
        "tokio-postgres must disable default TLS features"
    );
    for forbidden in ["axum", "actix", "prometheus", "hyper", "reqwest", "rocket"] {
        assert!(
            !CORE_TOML.to_lowercase().contains(forbidden),
            "forbidden dependency in core: {forbidden}"
        );
    }
    // tracing / tracing-subscriber are reused, not a metrics/http framework.
    assert!(CORE_TOML.contains("tracing"));
}

// --- TG6: config defaults + mask clamp (T11/T14 wiring) -------------------
#[test]
fn config_defaults_admin_addr_and_clamp_mask() {
    let c = CoreConfig::default();
    assert_eq!(c.admin_addr, "127.0.0.1:9090");
    // Over-grant is clamped to the six-bit domain (0..63).
    let mut m = std::collections::HashMap::new();
    m.insert("PERM_MASK".to_string(), "255".to_string());
    let c = CoreConfig::from_map(&m);
    assert_eq!(c.principal().perm_mask(), 63);
}
