//! 进程内复用 ide-core 领域逻辑（quest / craft / collab / security）。
//!
//! 与 CLI 同构：GUI 直接在进程内调用 `ide_core` 的 `Quest` / `CraftEngine` /
//! `CommentStore` / `PgSecretStore` 等，**不依赖 gRPC**（gRPC 仅用于 Chat /
//! RunAgent / NesComplete / Health）。这是设计文档「Quest/Craft/Comment/Lock/
//! Secret 无 gRPC RPC」的落地方式。

pub mod collab;
pub mod core_config;
pub mod craft;
pub mod fs_host;
pub mod quest;

use crate::ipc::VendorConfig;
use crate::state::AppState;
use tauri::{AppHandle, Manager};

/// 读取当前模型后端配置（用于构造进程内 `CoreConfig`）。
pub fn current_vendor(app: &AppHandle) -> VendorConfig {
    app.state::<AppState>().vendor.lock().unwrap().clone()
}
