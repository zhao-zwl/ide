//! aidea serve sidecar 编排：以 vendor 配置注入的环境变量拉起 serve，并轮询
//! Health.Check 直至 SERVING。支持运行时「停旧 serve → 以新 env 重启」以切换
//! 模型后端（决策 #A 的 local/online 切换）。

use crate::bootstrap::sidecar::bin_path;
use crate::bootstrap::emit_progress;
use crate::config::get_secret;
use crate::error::GuiError;
use crate::grpc::client;
use crate::ipc::{BootstrapPhase, VendorKind};
use crate::model_backend::{to_serve_env, GRPC_ADDR};
use crate::state::AppState;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// 按当前 vendor 配置（含 keyring 中的 Key）拉起 serve。
pub async fn start_with_vendor(app: &AppHandle) -> Result<(), GuiError> {
    let vendor = app.state::<AppState>().vendor.lock().unwrap().clone();
    let api_key = match vendor.kind {
        VendorKind::Online => get_secret("online_api_key").ok().flatten(),
        VendorKind::Local => None,
    };
    let env = to_serve_env(&vendor, api_key.as_deref());
    start_with_env(app, &env).await
}

/// 以给定环境变量拉起 `aidea serve <GRPC_ADDR>`，并等待 health 转绿。
pub async fn start_with_env(app: &AppHandle, env: &[(String, String)]) -> Result<(), GuiError> {
    emit_progress(app, BootstrapPhase::Serve, 0.75, Some("启动 aidea serve"));
    let bin = bin_path(app, "aidea");
    let child = crate::bootstrap::sidecar::spawn_binary(&bin, &["serve", GRPC_ADDR], env)?;
    // 显式块隔离：MutexGuard 不是 Send，而本函数是 async（下方有 .await）。
    // 语句级临时值本就在 `;` 处 drop，此处加块只为把「守卫不跨 await」写死在语法上。
    {
        app.state::<AppState>().bootstrap.lock().unwrap().serve = Some(child);
    }

    if client::wait_until_ready(GRPC_ADDR, Duration::from_secs(30)).await {
        emit_progress(app, BootstrapPhase::Serve, 0.95, Some("aidea serve 就绪"));
        Ok(())
    } else {
        Err(GuiError::bootstrap(
            "aidea serve 启动后超时未就绪（Health.Check 未 SERVING）",
        ))
    }
}

/// 停止当前 serve 子进程。
fn stop_current(app: &AppHandle) {
    // `Manager::state::<T>()` 返回借用 `app` 的临时守卫 `State<'_, T>`。若写成
    // `app.state::<AppState>().bootstrap.lock().unwrap()` 并把 MutexGuard 绑定到
    // 变量，`State` 临时值在该语句结束即被丢弃，而 guard 仍借着它 → E0716。
    // 绑定为具名局部变量后，`state` 活到函数末尾，drop 顺序为 handles → state。
    let state = app.state::<AppState>();
    let mut handles = state.bootstrap.lock().unwrap();
    if let Some(mut child) = handles.serve.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 切换模型后端：停旧 serve → 以新 vendor 配置重启（前端 `set_vendor_config` 调用）。
pub async fn restart_with_vendor(app: &AppHandle) -> Result<(), GuiError> {
    stop_current(app);
    start_with_vendor(app).await
}
