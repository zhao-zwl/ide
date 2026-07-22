//! Ollama sidecar 编排：启动 `ollama serve` → 创建端模型 `nes-tab`
//! （FROM 随包内置的 qwen2.5:0.5b.gguf 权重，对齐 `model_name` 默认值 nes-tab:latest）。

use crate::bootstrap::emit_progress;
use crate::bootstrap::sidecar::{app_data_dir, bin_path, resource_path, spawn_binary};
use crate::error::GuiError;
use crate::ipc::BootstrapPhase;
use crate::state::AppState;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Manager};

const OLLAMA_PORT: u16 = 11434;

/// 启动 Ollama 并创建端模型 nes-tab。
pub async fn start(app: &AppHandle) -> Result<(), GuiError> {
    emit_progress(app, BootstrapPhase::Ollama, 0.4, Some("启动 Ollama"));
    let bin = bin_path(app, "ollama");
    let models_dir = app_data_dir(app).join("ollama");
    let _ = std::fs::create_dir_all(&models_dir);
    let models_env = models_dir.to_string_lossy().to_string();

    if ollama_ready() {
        emit_progress(app, BootstrapPhase::Ollama, 0.5, Some("复用外部 Ollama"));
    } else {
        let child = spawn_binary(
            &bin,
            &["serve"],
            &[("OLLAMA_MODELS".to_string(), models_env.clone())],
        )?;
        app.state::<AppState>().bootstrap.lock().unwrap().ollama = Some(child);
        wait_ollama_ready().await?;
        emit_progress(app, BootstrapPhase::Ollama, 0.5, Some("Ollama 就绪"));
    }

    emit_progress(app, BootstrapPhase::Model, 0.6, Some("创建端模型 nes-tab"));
    create_nes_tab(app, &models_env)?;
    emit_progress(app, BootstrapPhase::Model, 0.7, Some("端模型 nes-tab 就绪"));
    Ok(())
}

/// 探测 Ollama 端口是否可达。
fn ollama_ready() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], OLLAMA_PORT));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 等待 Ollama 就绪（轮询端口，非阻塞）。
async fn wait_ollama_ready() -> Result<(), GuiError> {
    for _ in 0..60 {
        if ollama_ready() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(GuiError::bootstrap("Ollama 启动后超时未就绪"))
}

/// `ollama create nes-tab -f Modelfile`（FROM 随包内置的 qwen2.5:0.5b.gguf）。
/// 权重已随 .dmg 内置，首次启动离线即可创建；若内置缺失则容忍失败（serve 仍可启动，chat 不可用）。
fn create_nes_tab(app: &AppHandle, models_env: &str) -> Result<(), GuiError> {
    let bin = bin_path(app, "ollama");
    let modelfile = resource_path(app, "resources/models/nes-tab/Modelfile");
    if !modelfile.exists() {
        return Ok(());
    }
    let status = Command::new(&bin)
        .args(["create", "nes-tab", "-f", modelfile.to_str().unwrap()])
        .env("OLLAMA_MODELS", models_env)
        .stdout(std::process::Stdio::null)
        .stderr(std::process::Stdio::piped)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            tracing::warn!(
                "ollama create nes-tab 失败（可能缺少基础权重 qwen2.5:0.5b，离线时正常）；serve 将继续启动，但 chat 可能不可用"
            );
            Ok(())
        }
    }
}
