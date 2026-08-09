//! 模型后端命令：本地模型列表 / 切换本地模型。

use crate::config::save_vendor_config;
use crate::error::GuiResult;
use crate::grpc::client;
use crate::ipc::VendorKind;
use crate::state::AppState;
use std::process::Command;
use tauri::{AppHandle, Manager};

/// 列出本地 Ollama 已拉取的模型名（`ollama list`，离线/缺失二进制时返回空）。
#[tauri::command]
pub async fn model_list_local(app: AppHandle) -> GuiResult<Vec<String>> {
    use crate::bootstrap::sidecar::bin_path;
    let bin = bin_path(&app, "ollama");
    if !bin.exists() {
        return Ok(vec![]);
    }
    match Command::new(&bin).args(["list"]).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let names: Vec<String> = s
                .lines()
                .skip(1) // 跳过表头
                .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
                .filter(|n| !n.is_empty())
                .collect();
            Ok(names)
        }
        _ => Ok(vec![]),
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
