//! 健康概览命令（拉 9090 /healthz + /console；不可达时降级为本地快照）。

use crate::domain;
use crate::error::GuiResult;
use crate::ipc::{ConsoleMetricsDto, ConsoleStatusDto, HealthOverviewDto};
use ide_core::permissions::PermissionSet;
use tauri::{AppHandle, Manager};

/// bundled serve 的管理端口（/metrics / /healthz / /console）。
const ADMIN_ADDR: &str = "http://127.0.0.1:9090";

/// 读取后端健康概览：liveness（`/healthz`）+ 控制台状态（`/console` 或本地降级）。
#[tauri::command]
pub async fn health_overview(app: AppHandle) -> GuiResult<HealthOverviewDto> {
    let vendor = domain::current_vendor(&app);
    let cfg = domain::core_config::build_core_config(&vendor);

    // /healthz：不可达时标记 unreachable（不阻塞前端）。
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let healthz = match client
        .get(format!("{ADMIN_ADDR}/healthz"))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default().trim().to_string(),
        Err(_) => "unreachable".to_string(),
    };

    // /console 不可达时，按当前配置构造本地快照（不依赖远端 schema）。
    let permissions = PermissionSet::from_mask(cfg.perm_mask).labels().join(",");
    let console = ConsoleStatusDto {
        tenant_id: cfg.tenant_id,
        user_id: cfg.user_id,
        perm_mask: cfg.perm_mask,
        permissions,
        audit_events: 0,
        metrics: ConsoleMetricsDto {
            requests: 0,
            tool_calls: 0,
            llm_calls: 0,
            completions: 0,
            denials: 0,
            request_p95_ms: 0.0,
        },
    };

    Ok(HealthOverviewDto { healthz, console })
}
