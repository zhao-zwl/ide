//! gRPC 客户端封装（GUI ⇄ 本地 aidea serve）。
//!
//! 复用 `ide_core::v1` 生成的 tonic 客户端（AgentService / HealthService），
//! GUI 作为独立 crate 依赖 `ide-core` 路径，**不重复 build proto**。

pub mod client;
pub mod stream;

use ide_core::v1::agent_service_client::AgentServiceClient;
use ide_core::v1::health_service_client::HealthServiceClient;
use tonic::transport::Channel;

/// 复用的 gRPC 客户端对（agent 流式 + health 探活）。tonic 客户端可 Clone。
#[derive(Clone)]
pub struct GrpcClients {
    pub agent: AgentServiceClient<Channel>,
    pub health: HealthServiceClient<Channel>,
}

impl GrpcClients {
    /// 连接到本地 serve（HTTP/2，IP 地址形式）。
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok(Self {
            agent: AgentServiceClient::new(channel.clone()),
            health: HealthServiceClient::new(channel),
        })
    }
}
