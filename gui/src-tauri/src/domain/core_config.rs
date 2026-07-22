//! 构造进程内复用的 [`CoreConfig`]（对齐 bundled PostgreSQL 与当前 vendor）。

use crate::config::get_secret;
use crate::ipc::VendorConfig;
use crate::model_backend::{DEFAULT_DB_URL, GRPC_ADDR};
use ide_core::config::{CoreConfig, LlmBackend, NesBackend};

/// 根据当前 vendor 构造进程内 `CoreConfig`：
///   * 数据库指向 bundled PostgreSQL（127.0.0.1:5432 / aidea）；
///   * 本地 → Ollama backend + `nes-tab:latest`；
///   * 在线 → OpenAI 兼容 backend，Key 从 keyring 读取（进程内瞬时持有）。
pub fn build_core_config(vendor: &VendorConfig) -> CoreConfig {
    let mut cfg = CoreConfig::default();
    cfg.database_url = DEFAULT_DB_URL.to_string();
    cfg.grpc_addr = GRPC_ADDR.to_string();
    cfg.tenant_id = "single".to_string();
    cfg.single_tenant = true;
    cfg.quest_max_steps = 8;
    cfg.quest_max_subtasks = 8;

    match vendor.kind {
        crate::ipc::VendorKind::Local => {
            cfg.llm_backend = LlmBackend::Ollama;
            cfg.nes_backend = NesBackend::Ollama;
            cfg.model_endpoint = "http://localhost:11434".to_string();
            cfg.model_name = vendor.local_model.clone();
        }
        crate::ipc::VendorKind::Online => {
            cfg.llm_backend = LlmBackend::OpenAi;
            cfg.nes_backend = NesBackend::OpenAi;
            if let Some(b) = &vendor.base_url {
                cfg.llm_base_url = b.clone();
            }
            cfg.llm_model = vendor
                .model
                .clone()
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            // 在线 Key 来自 keyring（GUI 拥有 serve 生命周期，Key 不落前端）。
            if let Ok(Some(key)) = get_secret("online_api_key") {
                cfg.llm_api_key = key;
            }
        }
    }
    cfg
}
