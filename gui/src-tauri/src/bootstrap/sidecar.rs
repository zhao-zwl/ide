//! Sidecar 进程工具：资源路径解析 + 子进程拉起/停止。
//!
//! 自包含形态下，PG / Ollama / aidea serve 三个二进制随 .app 置于
//! `Contents/Resources/bin/`，资源（migrations / 模型 Modelfile）置于
//! `Contents/Resources/resources/`。Rust 用 `std::process::Command` 拉起并监管，
//! 不依赖 tauri-plugin-shell 的 JS 侧 sidecar（进程生命周期由 GUI 持有）。

use crate::error::GuiError;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tauri::{AppHandle, Manager};

/// 解析 .app 内的 Resources 目录（dev 下指向 `src-tauri`）。
pub fn resource_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .resource_dir()
        .expect("resource_dir 不可用")
}

/// 解析 Resources 下的某个相对路径。
pub fn resource_path(app: &AppHandle, rel: &str) -> PathBuf {
    resource_dir(app).join(rel)
}

/// App 数据目录（PG 数据目录、Ollama 模型库置于其下）。
pub fn app_data_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .expect("app_data_dir 不可用")
}

/// 拼接 sidecar 二进制路径：`<resources>/bin/<name>`。
///
/// Tauri 在打包时会给 externalBin 追加 target triple 后缀（如
/// `aidea-x86_64-apple-darwin`），`fetch-binaries.sh` 负责按此命名拷贝。
pub fn bin_path(app: &AppHandle, name: &str) -> PathBuf {
    resource_path(app, &format!("bin/{name}"))
}

/// 拉起一个子进程（stdin 丢弃，stdout/stderr 重定向管道，便于监管）。
pub fn spawn_binary(
    bin: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<Child, GuiError> {
    if !bin.exists() {
        return Err(GuiError::bootstrap(format!(
            "sidecar 二进制缺失: {}（请运行 scripts/fetch-binaries.sh）",
            bin.display()
        )));
    }
    let mut cmd = Command::new(bin);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null)
        .stdout(Stdio::piped)
        .stderr(Stdio::piped);
    cmd.spawn()
        .map_err(|e| GuiError::bootstrap(format!("拉起 {} 失败: {e}", bin.display())))
}

/// 终止一个子进程（best-effort）。
pub fn kill_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}
