//! aidea 自包含 macOS GUI 客户端入口（Tauri v2 + React/Vite 前端）。
//!
//! 启动流程：注册全局状态与插件 → `setup` 中按 store 的 `auto_bootstrap` 开关
//! 自动拉起后端栈（PG → Ollama → 模型 → serve）→ 暴露命令层供前端调用。

mod bootstrap;
mod commands;
mod config;
mod domain;
mod error;
mod grpc;
mod ipc;
mod model_backend;
mod state;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState::new())
        .setup(|app| {
            let handle = app.handle().clone();
            // 自动拉起后端栈（按 store 的 auto_bootstrap 开关；默认开启）。
            tauri::async_runtime::spawn(async move {
                let auto = crate::config::load_auto_bootstrap(&handle);
                if auto {
                    crate::bootstrap::bootstrap_stack_with_retry(&handle, 2).await;
                } else {
                    tracing::info!("auto_bootstrap 关闭，跳过自动拉起后端栈");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // 连接 / 模型后端配置
            commands::connection::bootstrap_status,
            commands::connection::get_vendor_config,
            commands::connection::set_vendor_config,
            commands::connection::get_auto_bootstrap,
            commands::connection::set_auto_bootstrap,
            commands::connection::test_vendor,
            // Chat / Agent 流式
            commands::chat::chat_send,
            commands::chat::chat_stop,
            commands::agent::agent_run,
            commands::agent::agent_stop,
            // Quest / Craft
            commands::quest::quest_run,
            commands::craft::craft_propose,
            commands::craft::craft_confirm,
            commands::craft::craft_reject,
            // 协作
            commands::collab::comment_add,
            commands::collab::comment_list,
            commands::collab::comment_resolve,
            commands::collab::lock_acquire,
            commands::collab::lock_release,
            commands::collab::lock_show,
            commands::collab::secret_set,
            commands::collab::secret_get,
            // 健康概览 / 模型
            commands::health::health_overview,
            commands::model::model_list_local,
            commands::model::set_local_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running aidea gui");
}
