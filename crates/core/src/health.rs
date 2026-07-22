//! Health check service (T10 — private-deployment base).
//!
//! Appended *minimally* to the ProtoBus contract as `HealthService.Check`
//! (see `proto/ide_core.proto`: `HealthCheckRequest` / `HealthCheckResponse` /
//! `HealthStatus`). This respects the G6 contract freeze — a single read-only
//! readiness probe is added without touching the frozen `AgentService` RPCs.
//!
//! An orchestrator (docker-compose, k8s) probes Core readiness over gRPC via
//! `grpc_health_probe -addr=<core>:50051`. A TCP liveness fallback is also
//! acceptable for the slice.

use async_trait::async_trait;
use ide_core::v1::health_service_server::{HealthService, HealthServiceServer};
use ide_core::v1::{HealthCheckRequest, HealthCheckResponse, HealthStatus};

/// Readiness implementation: always reports `SERVING`. (Wiring DB/model pings
/// is a v1.0 concern; the slice verifies the gRPC health surface exists.)
pub struct HealthServiceImpl;

#[async_trait]
impl HealthService for HealthServiceImpl {
    async fn check(
        &self,
        _request: tonic::Request<HealthCheckRequest>,
    ) -> Result<tonic::Response<HealthCheckResponse>, tonic::Status> {
        Ok(tonic::Response::new(HealthCheckResponse {
            status: HealthStatus::Serving as i32,
            detail: "ok".to_string(),
        }))
    }
}

/// Build the tonic `HealthServer` for registration alongside `AgentService`.
pub fn health_server() -> HealthServiceServer<HealthServiceImpl> {
    HealthServiceServer::new(HealthServiceImpl)
}
