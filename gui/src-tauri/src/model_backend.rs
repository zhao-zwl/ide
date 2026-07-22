//! 模型后端（vendor）配置 → `aidea serve` 启动环境变量 + 在线厂商连通测试。
//!
//! GUI 拥有 `aidea serve` 的生命周期，在启动时通过环境变量注入后端选择，包括
//! 在线厂商 API Key（来自 keyring）。**无需新增 gRPC RPC 传 key**。运行时切换 =
//! 重启 serve 进程（见 `bootstrap::serve`）。

use crate::error::{GuiError, GuiResult};
use crate::ipc::{VendorConfig, VendorKind};

/// 本地 gRPC 地址（GUI 拉起的 serve 强制 IPv4，规避 [::1] 绑定差异）。
pub const GRPC_ADDR: &str = "127.0.0.1:50051";
/// bundled PostgreSQL 连接串（与 bootstrap 创建的库/角色一致）。
pub const DEFAULT_DB_URL: &str = "postgres://aidea:aidea@127.0.0.1:5432/aidea";
/// 在线厂商默认模型（当用户未指定时使用）。
pub const DEFAULT_ONLINE_MODEL: &str = "gpt-4o-mini";

/// 把 [`VendorConfig`] 翻译为 `aidea serve` 的启动环境变量列表。
///
/// `api_key` 来自 keyring，仅在 online 时注入（本地模式不需要）。
pub fn to_serve_env(cfg: &VendorConfig, api_key: Option<&str>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("AIDEA_GRPC_ADDR".to_string(), GRPC_ADDR.to_string()),
        ("AIDEA_DATABASE_URL".to_string(), DEFAULT_DB_URL.to_string()),
    ];
    match cfg.kind {
        VendorKind::Local => {
            env.push(("AIDEA_LLM_BACKEND".to_string(), "ollama".to_string()));
            env.push(("AIDEA_NES_BACKEND".to_string(), "ollama".to_string()));
            env.push(("AIDEA_LLM_MODEL".to_string(), cfg.local_model.clone()));
            env.push(("AIDEA_MODEL_NAME".to_string(), cfg.local_model.clone()));
        }
        VendorKind::Online => {
            env.push(("AIDEA_LLM_BACKEND".to_string(), "openai".to_string()));
            env.push(("AIDEA_NES_BACKEND".to_string(), "openai".to_string()));
            if let Some(base) = &cfg.base_url {
                env.push(("AIDEA_LLM_BASE_URL".to_string(), base.clone()));
            }
            let model = cfg
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_ONLINE_MODEL.to_string());
            env.push(("AIDEA_LLM_MODEL".to_string(), model));
            if let Some(key) = api_key {
                if !key.trim().is_empty() {
                    env.push(("AIDEA_LLM_API_KEY".to_string(), key.to_string()));
                }
            }
        }
    }
    env
}

/// 在线厂商连通测试：GUI 侧临时从 keyring 读 Key 直接探活 OpenAI 兼容
/// `/v1/chat/completions`（前端不持久化）。发送一条极短请求验证可达 + 鉴权。
pub async fn test_vendor(base_url: &str, api_key: Option<&str>) -> GuiResult<bool> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/chat/completions");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| GuiError::vendor(format!("http client error: {e}")))?;

    let body = serde_json::json!({
        "model": DEFAULT_ONLINE_MODEL,
        "messages": [{"role": "user", "content": "ping"}],
        "stream": false,
        "max_tokens": 4,
    });

    let mut req = client.post(&url).json(&body);
    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            req = req.bearer_auth(key.trim());
        }
    }

    match req.send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(e) => {
            tracing::warn!("vendor test failed: {e}");
            Ok(false)
        }
    }
}
