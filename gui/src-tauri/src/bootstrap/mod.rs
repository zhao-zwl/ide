//! 后端栈一键拉起编排（sidecar 的生命周期总控）。
//!
//! 按序启动：PostgreSQL → Ollama → 端模型 nes-tab → aidea serve，并在 serve
//! 就绪后建立 gRPC 连接。每一阶段通过 [`emit_progress`] 向前端广播进度；失败按
//! 阶段语义决定是否可容忍（Ollama 离线可容忍，PG / serve 不可容忍）。

pub mod ollama;
pub mod pg;
pub mod serve;
pub mod sidecar;

use crate::error::GuiError;
use crate::grpc::client;
use crate::ipc::{BootstrapPhase, BootstrapState};
use crate::state::AppState;
use tauri::{AppHandle, Emitter, Manager};

/// 广播一次 bootstrap 进度。
///
/// 同时更新 [`AppState`] 中的当前阶段与最近快照，供 `bootstrap_status` 即时返回。
pub fn emit_progress(app: &AppHandle, phase: BootstrapPhase, progress: f32, detail: Option<&str>) {
    // 更新当前阶段 + 缓存最近一次快照（status 查询可直接返回）。
    {
        let state = app.state::<AppState>();
        let mut handles = state.bootstrap.lock().unwrap();
        handles.phase = phase;
        *state.last_bootstrap.lock().unwrap() = Some(BootstrapState {
            phase,
            progress,
            detail: detail.map(|s| s.to_string()),
        });
    }
    // 向前端广播（前端 BootstrapPage 监听 `bootstrap` 事件）。
    let _ = app.emit(
        "bootstrap",
        BootstrapState {
            phase,
            progress,
            detail: detail.map(|s| s.to_string()),
        },
    );
}

/// 拉起完整后端栈（PG → Ollama → 模型 → serve → 连接）。
///
/// * PG 失败 → 返回错误（数据库不可用，后续都无意义）。
/// * Ollama / 模型创建失败 → 容忍（离线或缺少基础权重，serve 仍可启动）。
/// * serve 失败 → 返回错误（核心服务不可达）。
pub async fn bootstrap_stack(app: &AppHandle) -> Result<(), GuiError> {
    // 记录 App 数据目录（PG 数据目录、Ollama 模型库置于其下）。
    {
        let dir = sidecar::app_data_dir(app);
        *app.state::<AppState>().app_data_dir.lock().unwrap() = Some(dir);
    }

    emit_progress(
        app,
        BootstrapPhase::Pg,
        0.02,
        Some("准备启动后端栈"),
    );

    // ① PostgreSQL
    if let Err(e) = pg::init_and_start(app).await {
        emit_progress(
            app,
            BootstrapPhase::Error,
            1.0,
            Some(&format!("PostgreSQL 启动失败: {e}")),
        );
        return Err(e);
    }

    // ② Ollama + 端模型 nes-tab（离线可容忍）
    if let Err(e) = ollama::start(app).await {
        tracing::warn!("ollama / 端模型启动失败（离线或缺少基础权重）: {e}");
        emit_progress(
            app,
            BootstrapPhase::Ollama,
            0.7,
            Some("Ollama / 模型不可用（离线），serve 将继续启动"),
        );
    }

    // ③ aidea serve（按当前 vendor 注入环境变量）
    if let Err(e) = serve::start_with_vendor(app).await {
        emit_progress(
            app,
            BootstrapPhase::Error,
            1.0,
            Some(&format!("aidea serve 启动失败: {e}")),
        );
        return Err(e);
    }

    // ④ 建立 gRPC 连接
    emit_progress(app, BootstrapPhase::Ready, 1.0, Some("后端栈就绪"));
    client::ensure_connected(app, crate::model_backend::GRPC_ADDR).await;
    Ok(())
}

/// 带重试的拉起（bootstrap 阶段允许短暂失败重试，默认 2 次）。
pub async fn bootstrap_stack_with_retry(app: &AppHandle, attempts: usize) {
    let mut last: Option<GuiError> = None;
    for i in 1..=attempts.max(1) {
        match bootstrap_stack(app).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("bootstrap 第 {i} 次尝试失败: {e}；准备重试");
                last = Some(e);
                // 间隔 1s 再试，避免紧耦合重试打满资源。
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    if let Some(e) = last {
        tracing::error!("bootstrap 多次重试后仍失败: {e}");
    }
}
