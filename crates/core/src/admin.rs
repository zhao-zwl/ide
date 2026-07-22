//! Admin HTTP/Text listener (T14 — observability endpoint).
//!
//! Exposes `/metrics` (Prometheus text), `/healthz` (liveness) and — new in
//! v1.0 Stage B (T13) — `/console` (read-only enterprise console status) on a
//! *plain* TCP socket (no axum/actix HTTP framework, per the v1.0 constraint).
//! The route decision and response rendering are pure functions so they can be
//! unit-tested without binding a socket. The admin port is separate from the
//! gRPC ProtoBus and never part of the frozen `.proto` contract.
//!
//! The GUI enterprise console (Tauri/React high-fidelity UI) is **not**
//! implemented in this stage — see README §v1.0B. The "no-UI console" is
//! exactly: the `admin` `/console` endpoint here + the `aidea console` CLI
//! subcommand (T13).

use crate::metrics::{Metrics, SharedMetrics};
use crate::principal::Principal;
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Admin route resolved from a request path (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRoute {
    /// `GET /metrics` — Prometheus exposition text.
    Metrics,
    /// `GET /healthz` — liveness probe.
    Healthz,
    /// `GET /console` — read-only enterprise console status (T13).
    Console,
    /// Anything else -> 404.
    NotFound,
}

/// Resolve an HTTP request path to an [`AdminRoute`] (pure; ignores query
/// string and method).
pub fn route_admin(path: &str) -> AdminRoute {
    // Strip query string / fragment; default to root.
    let path = path.split(['?', '#']).next().unwrap_or("/");
    match path {
        "/metrics" => AdminRoute::Metrics,
        "/healthz" => AdminRoute::Healthz,
        "/console" => AdminRoute::Console,
        _ => AdminRoute::NotFound,
    }
}

/// Build the (status, body) for a non-console route (pure; testable without a
/// socket). Console responses are produced separately by [`render_console`].
pub fn render_admin(route: AdminRoute, metrics: &Metrics) -> (u16, String) {
    match route {
        AdminRoute::Metrics => (200, metrics.render_prometheus()),
        AdminRoute::Healthz => (200, "ok\n".to_string()),
        // Console is rendered via `render_console`; fall back to 404 here.
        AdminRoute::Console => (200, String::new()),
        AdminRoute::NotFound => (404, "not found\n".to_string()),
    }
}

/// Read-only snapshot of the current tenant status for the enterprise console
/// (T13). Constructed by [`ConsoleProvider::console_status`] and rendered by
/// [`render_console`]. Building it performs **no writes** — it only reads the
/// shared identity + metrics.
#[derive(Debug, Clone)]
pub struct ConsoleStatus {
    /// Single-tenant tenant identifier.
    pub tenant_id: String,
    /// Acting user / service identity.
    pub user_id: String,
    /// Six-bit permission mask (0..63).
    pub perm_mask: u32,
    /// Total append-only audit events recorded so far.
    pub audit_events: u64,
    /// gRPC/CLI requests handled.
    pub requests: u64,
    /// Tool / action executions.
    pub tool_calls: u64,
    /// LLM think/plan invocations.
    pub llm_calls: u64,
    /// NES completions produced.
    pub completions: u64,
    /// Actions denied by the six-bit mask.
    pub denials: u64,
    /// Request latency p95 (ms).
    pub request_p95_ms: f64,
}

/// Render the console status as a human-readable text block with an embedded
/// JSON object for machine consumers. Pure; testable without a socket (T13).
pub fn render_console(status: &ConsoleStatus) -> String {
    let perms = crate::permissions::PermissionSet::from_mask(status.perm_mask)
        .labels()
        .join(", ");
    let body = json!({
        "tenant_id": status.tenant_id,
        "user_id": status.user_id,
        "perm_mask": status.perm_mask,
        "permissions": perms,
        "audit_events": status.audit_events,
        "metrics": {
            "requests": status.requests,
            "tool_calls": status.tool_calls,
            "llm_calls": status.llm_calls,
            "completions": status.completions,
            "denials": status.denials,
            "request_p95_ms": status.request_p95_ms,
        },
    });
    format!(
        "=== Agentic IDE Enterprise Console ===\n\
         tenant:        {}\n\
         user:          {}\n\
         perm_mask:     {} ({})\n\
         audit_events:  {}\n\
         requests:      {}\n\
         tool_calls:    {}\n\
         llm_calls:     {}\n\
         completions:   {}\n\
         denials:       {}\n\
         request_p95:   {:.1} ms\n\
         --- json ---\n\
         {}\n",
        status.tenant_id,
        status.user_id,
        status.perm_mask,
        perms,
        status.audit_events,
        status.requests,
        status.tool_calls,
        status.llm_calls,
        status.completions,
        status.denials,
        status.request_p95_ms,
        body,
    )
}

/// Supplies the admin listener with the shared state it needs to render
/// `/metrics`, `/healthz` and `/console`: a live [`Metrics`] handle plus a
/// read-only [`ConsoleStatus`] snapshot. Implemented by the server console
/// adapter in `server.rs` (T13), keeping the admin listener decoupled from the
/// gRPC service internals.
pub trait ConsoleProvider: Send + Sync {
    /// The shared metrics registry (for `/metrics`).
    fn metrics(&self) -> SharedMetrics;
    /// A read-only snapshot of the current tenant status (for `/console`).
    fn console_status(&self) -> ConsoleStatus;
}

/// Default adapter wiring a configured [`Principal`] + shared [`Metrics`] into
/// the [`ConsoleProvider`] surface used by the admin listener (T13).
pub struct AdminConsole {
    principal: Principal,
    metrics: SharedMetrics,
}

impl AdminConsole {
    /// Build an adapter from the server identity and the shared metrics handle.
    pub fn new(principal: Principal, metrics: SharedMetrics) -> Self {
        Self { principal, metrics }
    }
}

impl ConsoleProvider for AdminConsole {
    fn metrics(&self) -> SharedMetrics {
        self.metrics.clone()
    }

    fn console_status(&self) -> ConsoleStatus {
        let m = &*self.metrics;
        ConsoleStatus {
            tenant_id: self.principal.tenant_id.clone(),
            user_id: self.principal.user_id.clone(),
            perm_mask: self.principal.perm_mask(),
            audit_events: m.audit_events(),
            requests: m.requests(),
            tool_calls: m.tool_calls(),
            llm_calls: m.llm_calls(),
            completions: m.completions(),
            denials: m.denials(),
            request_p95_ms: m.request_latency.p95_ms(),
        }
    }
}

/// Render a minimal HTTP/1.1 response for a non-console route.
fn http_response(route: AdminRoute, metrics: &Metrics) -> String {
    let (status, body) = render_admin(route, metrics);
    let reason = if status == 200 { "OK" } else { "Not Found" };
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Render a minimal HTTP/1.1 response for the `/console` route.
fn http_response_console(status: &ConsoleStatus) -> String {
    let body = render_console(status);
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Start the blocking admin TCP listener. Serves `/metrics`, `/healthz` and
/// `/console` as plain-text HTTP. Runs forever; the caller normally
/// `tokio::spawn`s it. `provider` supplies the live metrics + console snapshot.
pub async fn serve_admin(addr: &str, provider: Arc<dyn ConsoleProvider>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("admin listener serving /metrics, /healthz and /console on {addr}");
    loop {
        let (socket, _peer) = listener.accept().await?;
        let provider = provider.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(socket, provider).await {
                tracing::debug!("admin conn error: {e}");
            }
        });
    }
}

/// Handle a single admin connection: read one request line, route, respond.
async fn handle_conn(mut socket: TcpStream, provider: Arc<dyn ConsoleProvider>) -> anyhow::Result<()> {
    let mut buf = [0u8; 2048];
    let n = socket.read(&mut buf).await?;
    if n == 0 {
        return Ok(()); // empty request; nothing to do
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    // First line: "GET /console HTTP/1.1"
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let route = route_admin(path);
    let resp = match route {
        AdminRoute::Console => {
            let status = provider.console_status();
            http_response_console(&status)
        }
        _ => {
            let metrics = provider.metrics();
            http_response(route, &metrics)
        }
    };
    socket.write_all(resp.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_resolve() {
        assert_eq!(route_admin("/metrics"), AdminRoute::Metrics);
        assert_eq!(route_admin("/healthz"), AdminRoute::Healthz);
        assert_eq!(route_admin("/console"), AdminRoute::Console);
        assert_eq!(route_admin("/console?foo=1"), AdminRoute::Console);
        assert_eq!(route_admin("/metrics?foo=1"), AdminRoute::Metrics);
        assert_eq!(route_admin("/unknown"), AdminRoute::NotFound);
        assert_eq!(route_admin("/"), AdminRoute::NotFound);
    }

    #[test]
    fn render_produces_status_and_body() {
        let m = Metrics::new();
        m.inc_requests();
        let (status, body) = render_admin(AdminRoute::Metrics, &m);
        assert_eq!(status, 200);
        assert!(body.contains("aidea_requests_total 1"));

        let (h_status, h_body) = render_admin(AdminRoute::Healthz, &m);
        assert_eq!(h_status, 200);
        assert_eq!(h_body, "ok\n");

        let (n_status, _) = render_admin(AdminRoute::NotFound, &m);
        assert_eq!(n_status, 404);
    }

    #[test]
    fn http_response_is_well_formed() {
        let m = Metrics::new();
        let resp = http_response(AdminRoute::Metrics, &m);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Type: text/plain"));
        // Blank line separates headers from body.
        assert!(resp.contains("\r\n\r\n"));
        // Content-Length reflects the body length.
        let expected_len = render_admin(AdminRoute::Metrics, &m).1.len();
        assert!(resp.contains(&format!("Content-Length: {expected_len}")));
    }

    #[test]
    fn console_status_renders_text_and_json() {
        let status = ConsoleStatus {
            tenant_id: "acme".into(),
            user_id: "alice".into(),
            // 0b100011 == Read | Generate | Commit.
            perm_mask: 0b100011,
            audit_events: 7,
            requests: 100,
            tool_calls: 10,
            llm_calls: 50,
            completions: 5,
            denials: 2,
            request_p95_ms: 12.3,
        };
        let out = render_console(&status);
        assert!(out.contains("tenant:        acme"));
        assert!(out.contains("perm_mask:     35 (Read, Generate, Commit)"));
        assert!(out.contains("audit_events:  7"));
        // Embedded JSON for machine consumers.
        assert!(out.contains("\"tenant_id\":\"acme\""));
        assert!(out.contains("\"permissions\":\"Read, Generate, Commit\""));
    }

    #[test]
    fn console_provider_builds_snapshot() {
        let principal = Principal::from_mask("t", "u", 63);
        let metrics = Arc::new(Metrics::new());
        metrics.inc_requests();
        metrics.inc_audit_events();
        let provider = AdminConsole::new(principal, metrics.clone());
        let status = provider.console_status();
        assert_eq!(status.tenant_id, "t");
        assert_eq!(status.perm_mask, 63);
        assert_eq!(status.requests, 1);
        assert_eq!(status.audit_events, 1);
        let resp = http_response_console(&status);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Type: text/plain"));
        // Content-Length must match the rendered console body.
        let expected_len = render_console(&status).len();
        assert!(resp.contains(&format!("Content-Length: {expected_len}")));
    }
}
