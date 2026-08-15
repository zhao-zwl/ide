//! 模型后端命令：本地模型列表 / 切换本地模型。

use crate::config::save_vendor_config;
use crate::error::GuiResult;
use crate::grpc::client;
use crate::ipc::VendorKind;
use crate::state::AppState;
use tauri::{AppHandle, Manager};

/// 列出本地可用模型（llama.cpp 单模型形态：返回当前配置的本地 GGUF 模型标识）。
///
/// 不再依赖外部 CLI（`ollama list`）：本地为单权重形态，模型即 `VendorConfig`
/// 中的 `local_model`；缺失时返回空列表（前端据此隐藏本地模型切换）。
#[tauri::command]
pub async fn model_list_local(app: AppHandle) -> GuiResult<Vec<String>> {
    let state = app.state::<AppState>();
    let model = state.vendor.lock().unwrap().local_model.clone();
    if model.is_empty() {
        Ok(vec![])
    } else {
        Ok(vec![model])
    }
}

/// 切换本地模型名（持久化 + 重启 serve；serve 以模型名注入 env）。
#[tauri::command]
pub async fn set_local_model(app: AppHandle, model: String) -> GuiResult<()> {
    // `state` 必须先绑定为具名局部变量，否则 `State<'_, AppState>` 临时值在本条
    // `let` 语句结束就被丢弃，而 MutexGuard 还借着它 → E0716。
    // 块的尾表达式 `v.clone()` 经 Deref 走 `VendorConfig::clone`，传出的是 owned
    // 数据而非借用；守卫在块结束即 drop，早于下面的 .await（MutexGuard 非 Send）。
    let config = {
        let state = app.state::<AppState>();
        let mut v = state.vendor.lock().unwrap();
        v.local_model = model.clone();
        v.clone()
    };
    save_vendor_config(&app, &config)?;
    // 仅本地模式需要重启 serve；在线模式改 base_url/model 走 set_vendor_config。
    if config.kind == VendorKind::Local {
        crate::bootstrap::serve::restart_with_vendor(&app).await?;
        client::ensure_connected(&app, crate::model_backend::GRPC_ADDR).await;
    }
    Ok(())
}
