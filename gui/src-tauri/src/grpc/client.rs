//! gRPC 连接建立与就绪探测。

use crate::grpc::GrpcClients;
use crate::ipc::ConnStatus;
use crate::state::AppState;
use ide_core::v1::{HealthCheckRequest, HealthStatus};
use tauri::{AppHandle, Manager};

/// 尝试连接 serve；成功则把客户端写入 [`AppState`] 并置 `Connected`。
/// 失败不抛错，仅更新 `conn_status`（bootstrap 阶段会反复重试）。
pub async fn ensure_connected(app: &AppHandle, addr: &str) -> bool {
    match GrpcClients::connect(addr).await {
        Ok(clients) => {
            let state = app.state::<AppState>();
            *state.grpc.lock().unwrap() = Some(clients);
            *state.conn_status.lock().unwrap() = ConnStatus::Connected;
            true
        }
        Err(e) => {
            tracing::debug!("grpc not yet reachable at {addr}: {e}");
            *app.state::<AppState>().conn_status.lock().unwrap() = ConnStatus::Error;
            false
        }
    }
}

/// 阻塞等待 serve 进入 SERVING 状态（带总超时与轮询间隔），供 bootstrap 收尾。
pub async fn wait_until_ready(addr: &str, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    let interval = std::time::Duration::from_millis(500);
    loop {
        if let Ok(clients) = GrpcClients::connect(addr).await {
            if health_serving(&clients).await {
                return true;
            }
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// 调一次 Health.Check，判断是否 SERVING。
async fn health_serving(clients: &GrpcClients) -> bool {
    match clients
        .health
        .clone()
        .check(HealthCheckRequest::default())
        .await
    {
        // proto 枚举字段在 prost 中映射为 i32，与枚举变体比较需转型。
        Ok(resp) => resp.into_inner().status == HealthStatus::Serving as i32,
        Err(_) => false,
    }
}
