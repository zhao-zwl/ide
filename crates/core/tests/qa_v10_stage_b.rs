//! QA verification tests for v1.0 Stage B (T12 SSO + T13 no-UI console).
//!
//! Complements `qa_v10_stage_a.rs` (TG1–TG6) with the Stage B acceptance
//! assertions called out in the QA brief that the engineer's in-module tests
//! did not fully close:
//!
//!   * TG-B1 — T12 gRPC transport integration: with SSO enabled, a request
//!     carrying NO bearer token, or a token signed with the WRONG secret, is
//!     rejected with gRPC `UNAUTHENTICATED`. A request with a VALID HS256 token
//!     is accepted and reaches the decision core (RunAgent/Chat too, proving
//!     the three privileged RPCs are consistent).
//!   * TG-B2 — T12 backward compatibility: with SSO disabled the M1 demo
//!     (`AgentServer::default_stack`) runs unchanged and the Noop authenticator
//!     skips the token entirely.
//!   * TG-B3 — T13 console is read-only: `ConsoleProvider::console_status`
//!     produces an identical snapshot on repeated calls (no hidden mutation of
//!     shared state) and `render_console` carries only read-only fields.
//!   * TG-B4 — T13 CLI `console` is a library-only path (zero `tonic`/`tauri`
//!     dependency) and the README documents that the high-fidelity GUI is NOT
//!     implemented.
//!   * TG-B5 — T12 dependency boundary (strengthens TG5): crates/core/Cargo.toml
//!     adds ONLY `hmac`/`sha2`/`base64`; `jsonwebtoken` (and axum/actix/rocket/
//!     oauth2) are forbidden.
//!   * TG-B6 — proto G6 freeze: `auth`/`console` are transport/management
//!     surfaces that do NOT appear in proto/ide_core.proto (no new service, no
//!     new message used by a frozen service); frozen services keep their RPC
//!     counts.
//!
//! No external services (DB / network / heavy deps): pure logic + file text
//! embedded via `include_str!`. Runnable under the OOM-constrained baseline.

use ide_core::admin::{render_console, AdminConsole, ConsoleProvider, ConsoleStatus};
use ide_core::agent::AgentServer;
use ide_core::auth::{Authenticator, Hs256Authenticator};
use ide_core::audit::{AuditSink, MockAuditSink};
use ide_core::chat::ChatEngine;
use ide_core::host::{CliHost, HostBridge};
use ide_core::metrics::Metrics;
use ide_core::permissions::PermissionSet;
use ide_core::planner::Planner;
use ide_core::principal::Principal;
use ide_core::v1::agent_service_server::AgentService;
use ide_core::v1::{AgentRequest, ChatRequest, NesRequest};
use ide_core::{CompletionBackend, MockOllamaClient};
use std::str::FromStr;
use std::sync::Arc;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request};

/// Future expiry (now + 1h) so minted HS256 tokens are valid at call time.
fn exp_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3600
}

/// Build an `AgentServer` backed by an HS256 authenticator (SSO enabled path)
/// without touching the network: it wires the M1 mock stack locally and pins the
/// shared secret. The `Noop`/Postgres paths are exercised by `default_stack`
/// and `from_config` respectively; here we isolate the SSO-on transport logic.
fn hs_server(bridge: Arc<HostBridge>, secret: &str) -> AgentServer {
    let principal = Principal::all("single", "core");
    let authn: Arc<dyn Authenticator> = Arc::new(Hs256Authenticator::new(secret, "", ""));
    let audit: Arc<dyn AuditSink> = Arc::new(MockAuditSink::new());
    let metrics = Arc::new(Metrics::new());
    let planner = Arc::new(
        Planner::with_defaults(bridge.clone(), 8)
            .with_governance(principal.clone(), audit.clone(), metrics.clone()),
    );
    let nes_backend: Arc<dyn CompletionBackend> = Arc::new(MockOllamaClient::new());
    let chat_engine = Arc::new(
        ChatEngine::new(planner.llm(), PermissionSet::all())
            .with_governance(principal.clone(), audit.clone(), metrics.clone()),
    );
    AgentServer::new(planner, nes_backend, chat_engine, authn, principal, audit, metrics)
}

fn with_bearer(req: &mut Request<NesRequest>, token: &str) {
    let val = MetadataValue::from_str(&format!("Bearer {token}")).unwrap();
    req.metadata_mut().insert("authorization", val);
}

// --- TG-B1: SSO-on transport integration -----------------------------------

#[tokio::test]
async fn sso_enabled_missing_token_rejected_unauthenticated() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = hs_server(bridge, "secret");
    let req = Request::new(NesRequest {
        session_id: "s".into(),
        document_uri: "d".into(),
        prefix: "fn main".into(),
        suffix: "".into(),
        language: "rust".into(),
    });
    // No `authorization` metadata -> missing token -> UNAUTHENTICATED.
    let err = svc.nes_complete(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::Unauthenticated,
        "missing bearer token with SSO on must be UNAUTHENTICATED"
    );
}

#[tokio::test]
async fn sso_enabled_wrong_secret_rejected_unauthenticated() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = hs_server(bridge, "secret"); // server expects "secret"
    let bad = ide_core::auth::mint_hs256_token(
        "attacker-secret",
        "t1",
        "u1",
        63,
        Some(exp_future()),
        None,
        None,
    );
    let mut req = Request::new(NesRequest {
        session_id: "s".into(),
        document_uri: "d".into(),
        prefix: "fn".into(),
        suffix: "".into(),
        language: "rust".into(),
    });
    with_bearer(&mut req, &bad);
    let err = svc.nes_complete(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::Unauthenticated,
        "token signed with wrong secret must be UNAUTHENTICATED"
    );
}

#[tokio::test]
async fn sso_enabled_valid_hs256_token_reaches_core() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = hs_server(bridge, "secret");
    let tok = ide_core::auth::mint_hs256_token(
        "secret", "acme", "alice", 63, Some(exp_future()), None, None,
    );
    let mut req = Request::new(NesRequest {
        session_id: "s".into(),
        document_uri: "d".into(),
        prefix: "fn main".into(),
        suffix: "".into(),
        language: "rust".into(),
    });
    with_bearer(&mut req, &tok);
    let resp = svc.nes_complete(req).await;
    assert!(
        resp.is_ok(),
        "valid HS256 bearer token must authenticate and reach the decision core"
    );
}

#[tokio::test]
async fn sso_enabled_valid_token_authorizes_run_agent() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = hs_server(bridge, "secret");
    let tok = ide_core::auth::mint_hs256_token(
        "secret", "acme", "alice", 63, Some(exp_future()), None, None,
    );
    let mut req = Request::new(AgentRequest {
        session_id: "s".into(),
        goal: "add retry logic".into(),
        project_id: "".into(),
    });
    with_bearer(&mut req, &tok);
    let resp = svc.run_agent(req).await;
    assert!(
        resp.is_ok(),
        "valid HS256 token must authorize the RunAgent RPC (per-request Principal injected)"
    );
}

#[tokio::test]
async fn sso_enabled_missing_token_rejected_on_chat_too() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = hs_server(bridge, "secret");
    let req = Request::new(ChatRequest {
        session_id: "s".into(),
        message: "hi".into(),
        attachments: vec![],
    });
    let err = svc.chat(req).await.unwrap_err();
    assert_eq!(
        err.code(),
        Code::Unauthenticated,
        "the Chat RPC must also reject a missing token (three RPCs consistent)"
    );
}

// --- TG-B2: SSO-off keeps the M1 demo running (Noop skip) ------------------

#[tokio::test]
async fn sso_disabled_demo_runs_noop_skips_token() {
    // `default_stack` uses the NoopAuthenticator (sso_enabled = false, the
    // default). The M1 demo must run unchanged even without any bearer token.
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = AgentServer::default_stack(bridge);
    let req = Request::new(NesRequest {
        session_id: "s".into(),
        document_uri: "d".into(),
        prefix: "fn main".into(),
        suffix: "".into(),
        language: "rust".into(),
    });
    let resp = svc.nes_complete(req).await;
    assert!(
        resp.is_ok(),
        "M1 demo (CliHost + MockLlm) must run with Noop authenticator (SSO off)"
    );
}

// --- TG-B3: console is read-only ------------------------------------------

#[test]
fn console_status_is_read_only_and_idempotent() {
    let principal = Principal::from_mask("acme", "alice", 0b100011); // 35 = Read|Generate|Commit
    let metrics = Arc::new(Metrics::new());
    metrics.inc_requests();
    metrics.inc_audit_events();
    let provider = AdminConsole::new(principal, metrics.clone());

    // Repeated calls yield identical snapshots => no hidden write/mutation of
    // shared state (the console provider is strictly read-only).
    let a = provider.console_status();
    let b = provider.console_status();
    assert_eq!(a.tenant_id, b.tenant_id);
    assert_eq!(a.user_id, b.user_id);
    assert_eq!(a.perm_mask, b.perm_mask);
    assert_eq!(a.requests, b.requests);
    assert_eq!(a.audit_events, b.audit_events);
    assert_eq!(a.completions, b.completions);
    assert_eq!(a.denials, b.denials);

    // Rendered text + embedded JSON carry only read-only fields.
    let out = render_console(&a);
    assert!(out.contains("tenant:        acme"));
    assert!(out.contains("user:          alice"));
    assert!(out.contains("perm_mask:     35 (Read, Generate, Commit)"));
    assert!(out.contains("audit_events:  1"));
    assert!(out.contains("\"tenant_id\":\"acme\""));
    assert!(out.contains("\"permissions\":\"Read, Generate, Commit\""));
}

// --- TG-B4: CLI console is library-only; GUI documented unimplemented ------

const CLI_TOML: &str = include_str!("../../../crates/cli/Cargo.toml");
const README: &str = include_str!("../../../README.md");

#[test]
fn cli_has_no_tonic_tauri_and_gui_documented_unimplemented() {
    let cli = CLI_TOML.to_lowercase();
    assert!(
        !cli.contains("tonic"),
        "aidea console must not link tonic directly (library-only path)"
    );
    assert!(
        !cli.contains("tauri"),
        "aidea console must not depend on the Tauri GUI"
    );
    assert!(!cli.contains("actix"), "no actix in the CLI crate");

    // README must explicitly state the high-fidelity console GUI is NOT built.
    assert!(
        README.contains("GUI 明确不实现") || README.contains("GUI不实现"),
        "README must document that the Tauri/React console GUI is not implemented"
    );
}

// --- TG-B5: T12 dependency boundary (strengthens TG5) ----------------------

const CORE_TOML: &str = include_str!("../Cargo.toml");

#[test]
fn core_only_adds_lightweight_crypto_for_sso() {
    // T12 SSO must add ONLY the three lightweight crypto crates.
    for dep in ["hmac", "sha2", "base64"] {
        assert!(CORE_TOML.contains(dep), "T12 SSO must add {dep}");
    }
    // Forbidden heavy dependencies must NOT be present.
    let lower = CORE_TOML.to_lowercase();
    for forbidden in ["jsonwebtoken", "axum", "actix", "rocket", "oauth2"] {
        assert!(
            !lower.contains(forbidden),
            "forbidden heavy dependency in core: {forbidden}"
        );
    }
    // Sanity: tokio-postgres (T11 audit sink) is still the only network dep.
    assert!(CORE_TOML.contains("tokio-postgres"));
}

// --- TG-B6: proto G6 freeze (auth/console must not enter the contract) -----

const PROTO: &str = include_str!("../../../proto/ide_core.proto");

#[test]
fn proto_freeze_auth_console_not_in_contract() {
    assert!(
        !PROTO.contains("service Auth"),
        "auth must NOT enter the frozen proto"
    );
    assert!(
        !PROTO.contains("service Console"),
        "console must NOT enter the frozen proto"
    );
    assert!(
        !PROTO.contains("ConsoleStatus"),
        "ConsoleStatus message must NOT enter the frozen proto"
    );
    assert!(
        !PROTO.contains("AuthToken"),
        "auth messages must NOT enter the frozen proto"
    );
    // Frozen services keep their exact RPC counts (no regression vs G6).
    let agent = extract_service(&PROTO, "AgentService");
    assert_eq!(count_rpcs(&agent), 4, "AgentService must stay at 4 RPCs");
    let host = extract_service(&PROTO, "HostService");
    assert_eq!(count_rpcs(&host), 3, "HostService must stay at 3 RPCs");
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
