//! Tonic server bootstrap (T02 / T10 / T11 / T12 / T14).
//!
//! Starts the Core's gRPC `AgentService` together with the `HealthService`
//! readiness probe (T10). The host (Tauri shell or CLI) connects as a client;
//! the Core in turn may drive the host through `GrpcHostClient`.
//!
//! Two entry points:
//!   * [`serve`]        — demo/default stack (CLI host + mock NES backend).
//!   * [`serve_configured`] — production stack driven by [`CoreConfig`].
//!
//! v1.0 Stage A (T14): when governance is configured, an **admin TCP listener**
//! (`/metrics` + `/healthz` + `/console`) is spawned in-process on
//! `config.admin_addr`, fully separate from the frozen gRPC ProtoBus contract.
//! v1.0 Stage B (T13): the admin listener also serves the read-only enterprise
//! console (`/console`), driven by an [`AdminConsole`] adapter built from the
//! server principal + shared metrics.

use crate::admin::{serve_admin, AdminConsole};
use crate::agent::AgentServer;
use crate::config::CoreConfig;
use crate::health::health_server;
use crate::host::{CliHost, HostBridge};
use ide_core::v1::agent_service_server::AgentServiceServer as TonicAgentServer;
use std::sync::Arc;
use tonic::transport::Server;

/// Start the Core gRPC server on `addr` using the default M1+v0.5+v1.0 stack,
/// also spawning the admin `/metrics` + `/healthz` + `/console` listener on the
/// default address.
pub async fn serve(addr: &str) -> anyhow::Result<()> {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = AgentServer::default_stack(bridge);
    let console = Arc::new(AdminConsole::new(svc.principal(), svc.metrics()));
    let admin_addr = CoreConfig::default().admin_addr;
    tokio::spawn(async move { serve_admin(&admin_addr, console).await });
    serve_stack(addr, svc).await
}

/// Start the Core gRPC server on `addr`, building the stack from `config`
/// (selects the NES backend, applies single-tenant mode, attaches a Postgres
/// audit sink, chooses the SSO authenticator, and spawns the admin listener on
/// `config.admin_addr` serving the enterprise console). Used by the
/// `aidea serve` CLI subcommand (T09/T10/T11/T12/T13/T14).
pub async fn serve_configured(addr: &str, config: &CoreConfig) -> anyhow::Result<()> {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let svc = AgentServer::from_config(bridge, config).await;
    let console = Arc::new(AdminConsole::new(svc.principal(), svc.metrics()));
    let admin_addr = config.admin_addr.clone();
    tokio::spawn(async move { serve_admin(&admin_addr, console).await });
    serve_stack(addr, svc).await
}

/// Shared serve routine: register `AgentService` + `HealthService` and run.
async fn serve_stack(addr: &str, svc: AgentServer) -> anyhow::Result<()> {
    let agent_svc = TonicAgentServer::new(svc);
    let health_svc = health_server();
    let parsed: std::net::SocketAddr = addr.parse()?;
    println!("[core] AgentService + HealthService listening on {parsed}");
    Server::builder()
        .add_service(agent_svc)
        .add_service(health_svc)
        .serve(parsed)
        .await?;
    Ok(())
}
