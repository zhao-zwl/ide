//! RunAgent 流式交互命令（逐事件向前端 emit `agent-event` / `agent-done` / `agent-error`）。

use crate::error::GuiError;
use crate::grpc::stream;
use crate::ipc::{AgentRunRequest, ChatStopRequest};
use crate::state::AppState;
use tauri::{AppHandle, Manager};

/// 发起一次 RunAgent（ReAct）流式请求（后台 spawn，受 CancellationToken 控制）。
#[tauri::command]
pub async fn agent_run(app: AppHandle, req: AgentRunRequest) -> GuiResult<()> {
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
    let goal = req.goal.clone();
    let project_id = req.project_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = stream::run_agent(app2, session_id, goal, project_id, clients, token).await {
            tracing::warn!("agent stream error: {e}");
        }
    });
    Ok(())
}

/// 停止某 session 的 RunAgent 流式（复用 chat_stop 的取消令牌机制）。
#[tauri::command]
pub async fn agent_stop(app: AppHandle, req: ChatStopRequest) -> GuiResult<()> {
    app.state::<AppState>().cancel_abort(&req.session_id);
    Ok(())
}
