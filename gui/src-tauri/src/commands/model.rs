//! 模型后端命令：本地模型列表 / 切换本地模型。

use crate::config::save_vendor_config;
use crate::error::{GuiError, GuiResult};
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
    let config = {
        let mut v = app.state::<AppState>().vendor.lock().unwrap();
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
