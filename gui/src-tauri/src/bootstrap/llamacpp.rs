//! 本地推理 sidecar 编排（决策 B）：拉起 llama.cpp 的 `llama-server`
//! （OpenAI 兼容 `/v1/chat/completions`），监听 `127.0.0.1:8080`，对齐
//! `aidea serve` 在本地模式下注入的 `LlmBackend::Local` / `NesBackend::Local`
//! （`AIDEA_LLM_BASE_URL=http://127.0.0.1:8080/v1`）。
//!
//! 与旧 Ollama 路径相比：不再需要 `ollama create` 把 GGUF 注册成模型名，
//! 而是直接 `llama-server --model <gguf>` 加载权重；就绪判定改为轮询 8080 端口。

use crate::bootstrap::emit_progress;
use crate::bootstrap::sidecar::{bin_path, resource_path, spawn_binary};
use crate::error::GuiError;
use crate::ipc::BootstrapPhase;
use crate::state::AppState;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// llama-server 监听端口（与 `AIDEA_LLM_BASE_URL=http://127.0.0.1:8080/v1` 对齐）。
const LLAMACPP_PORT: u16 = 8080;
/// 内置端模型权重（GGUF）在 .app `Resources` 下的相对路径。
///
/// 权重不一定随包烘焙（设计允许首启联网/手动放置），缺失时 serve 仍可启动，
/// 但本地 chat 不可用——与原 Ollama 缺权重降级路径一致。
const BUNDLED_MODEL_RESOURCE: &str = "models/nes-tab/nes-tab.gguf";

/// 拉起本地 llama.cpp 推理服务（llama-server）。
///
/// * 若 8080 已被外部 llama-server 占用则复用；
/// * 否则以解析出的 GGUF 路径为 `--model` 启动；
/// * 权重缺失或启动失败 → 容忍（warn），serve 仍可启动，本地 chat 不可用。
pub async fn start(app: &AppHandle) -> Result<(), GuiError> {
    emit_progress(
        app,
        BootstrapPhase::Ollama,
        0.4,
        Some("启动本地推理服务 llama-server"),
    );
    let bin = bin_path(app, "llama-server");

    if llamacpp_ready() {
        emit_progress(
            app,
            BootstrapPhase::Ollama,
            0.5,
            Some("复用外部 llama-server"),
        );
        return Ok(());
    }

    let model_path = resolve_model_path(app);
    if !model_path.exists() {
        tracing::warn!(
            "本地 GGUF 权重缺失（{}），跳过 llama-server 启动；serve 仍可启动，但本地 chat 不可用",
            model_path.display()
        );
        emit_progress(
            app,
            BootstrapPhase::Ollama,
            0.5,
            Some("未找到本地 GGUF 权重，跳过本地推理"),
        );
        return Ok(());
    }

    let child = spawn_binary(
        &bin,
        &[
            "--model",
            model_path.to_str().unwrap_or_default(),
            "--host",
            "127.0.0.1",
            "--port",
            &LLAMACPP_PORT.to_string(),
        ],
        &[],
    )?;
    // 显式块隔离：MutexGuard 不是 Send，紧接着就是 .await。
    {
        app.state::<AppState>().bootstrap.lock().unwrap().llamacpp = Some(child);
    }
    wait_llamacpp_ready().await?;
    emit_progress(app, BootstrapPhase::Ollama, 0.5, Some("llama-server 就绪"));
    Ok(())
}

/// 解析本地 GGUF 权重路径。
///
/// 优先使用 vendor 配置的 `local_model`；若其为绝对/相对已存在路径则直接用，
/// 否则按 `Resources` 相对路径解析，最终回退到内置 GGUF 路径。这样即使前端仍
/// 传入旧式 `nes-tab:latest`，也能稳妥落到内置权重，避免启动即崩。
fn resolve_model_path(app: &AppHandle) -> PathBuf {
    let configured = app
        .state::<AppState>()
        .vendor
        .lock()
        .unwrap()
        .local_model
        .clone();
    let direct = PathBuf::from(&configured);
    if direct.is_absolute() && direct.exists() {
        return direct;
    }
    if direct.exists() {
        return direct;
    }
    let as_resource = resource_path(app, &configured);
    if as_resource.exists() {
        return as_resource;
    }
    resource_path(app, BUNDLED_MODEL_RESOURCE)
}

/// 探测 llama-server 端口是否可达。
fn llamacpp_ready() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], LLAMACPP_PORT));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 等待 llama-server 就绪（轮询端口，非阻塞）。
async fn wait_llamacpp_ready() -> Result<(), GuiError> {
    for _ in 0..60 {
        if llamacpp_ready() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(GuiError::bootstrap("llama-server 启动后超时未就绪"))
}
