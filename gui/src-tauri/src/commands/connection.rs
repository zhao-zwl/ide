//! 启动状态 / 模型后端配置 / 在线厂商连通测试命令。

use crate::config::{load_auto_bootstrap, load_vendor_config, save_auto_bootstrap, save_vendor_config};
use crate::error::GuiResult;
use crate::grpc::client;
use crate::ipc::{BootstrapState, TestVendorRequest, VendorConfig};
use crate::state::AppState;
use crate::model_backend;
use tauri::{AppHandle, Manager};

/// 当前 bootstrap 进度快照（即时返回，无需等待事件）。
#[tauri::command]
pub async fn bootstrap_status(app: AppHandle) -> GuiResult<BootstrapState> {
    // `state` 是借用 `app` 的守卫，而 `app` 在 async fn 中被移入 future；把它的
    // 借用（经 `if let` 的 scrutinee 临时延长到整个 if/else，且该 if/else 又是
    // 函数尾表达式）留到函数末尾会推过 `app` 的存活期 → E0597。
    // 用块把两把锁的 **owned 快照** 取出来，块结束时守卫与 `State` 全部归还。
    // 两把锁在块内顺序获取、互不嵌套（emit_progress 中是 bootstrap→last_bootstrap
    // 嵌套持有），因此不引入任何锁序问题。
    let (last, phase) = {
        let state = app.state::<AppState>();
        // BootstrapState: Clone → owned；guard 是语句级临时，`;` 处即 drop。
        let last = state.last_bootstrap.lock().unwrap().clone();
        // BootstrapPhase: Copy → owned。
        let phase = state.bootstrap.lock().unwrap().phase;
        (last, phase)
    };
    if let Some(s) = last {
        return Ok(s);
    }
    Ok(BootstrapState {
        phase,
        progress: 0.0,
        detail: None,
    })
}

/// 读取当前模型后端配置（非密钥；密钥走 keyring / secret_set）。
#[tauri::command]
pub async fn get_vendor_config(app: AppHandle) -> GuiResult<VendorConfig> {
    load_vendor_config(&app)
}

/// 写入模型后端配置并重启 serve 以切换后端（在线 Key 由前端经 secret_set 落 keyring）。
#[tauri::command]
pub async fn set_vendor_config(app: AppHandle, config: VendorConfig) -> GuiResult<()> {
    save_vendor_config(&app, &config)?;
    // `state` 必须先绑定为具名局部变量，否则 `State<'_, AppState>` 临时值在本条
    // `let` 语句结束就被丢弃，而 MutexGuard 还借着它 → E0716。
    // 块结束时 v → state 依次 drop，均早于下面的 .await（MutexGuard 非 Send）。
    {
        let state = app.state::<AppState>();
        let mut v = state.vendor.lock().unwrap();
        *v = config.clone();
    }
    // 重启 serve（serve 按新 vendor 注入环境变量；在线 Key 从 keyring 读取）。
    crate::bootstrap::serve::restart_with_vendor(&app).await?;
    client::ensure_connected(&app, model_backend::GRPC_ADDR).await;
    Ok(())
}

/// 读取自动拉起开关。
#[tauri::command]
pub async fn get_auto_bootstrap(app: AppHandle) -> GuiResult<bool> {
    Ok(load_auto_bootstrap(&app))
}

/// 写入自动拉起开关。
#[tauri::command]
pub async fn set_auto_bootstrap(app: AppHandle, enabled: bool) -> GuiResult<()> {
    save_auto_bootstrap(&app, enabled)
}

/// 在线厂商连通测试（临时探活 OpenAI 兼容 /v1/chat/completions，Key 不持久化）。
#[tauri::command]
pub async fn test_vendor(req: TestVendorRequest) -> GuiResult<bool> {
    model_backend::test_vendor(&req.base_url, req.api_key.as_deref()).await
}
