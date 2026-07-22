//! Chat 流式交互命令（逐 token 向前端 emit `chat-token` / `chat-done` / `chat-error`）。

use crate::error::GuiError;
use crate::grpc::stream;
use crate::ipc::{ChatSendRequest, ChatStopRequest};
use crate::state::AppState;
use tauri::{AppHandle, Manager};

/// 发起一次 Chat 流式请求（后台 spawn，受 CancellationToken 控制）。
#[tauri::command]
pub async fn chat_send(app: AppHandle, req: ChatSendRequest) -> GuiResult<()> {
    let state = app.state::<AppState>();
    let clients = state
        .grpc
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| GuiError::grpc("serve 尚未就绪，请等待 bootstrap 完成"))?;
    let token = state.register_abort(&req.session_id);

    let app2 = app.clone();
    let session_id = req.session_id.clone();
    let message = req.message.clone();
    let attachments = req.attachments.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = stream::run_chat(app2, session_id, message, attachments, clients, token).await {
            tracing::warn!("chat stream error: {e}");
        }
    });
    Ok(())
}

/// 停止某 session 的 Chat 流式（触发 CancellationToken）。
#[tauri::command]
pub async fn chat_stop(app: AppHandle, req: ChatStopRequest) -> GuiResult<()> {
    app.state::<AppState>().cancel_abort(&req.session_id);
    Ok(())
}
