//! gRPC 流式 → 前端事件桥接。
//!
//! `chat` / `run_agent` 的流式响应被转换为前端 `listen` 的事件：
//! `chat-token` / `chat-done` / `chat-error` 与 `agent-event` / `agent-done` /
//! `agent-error`。流式过程受 [`CancellationToken`] 控制（对应 `chat_stop`）。

use crate::error::GuiError;
use crate::grpc::GrpcClients;
use crate::ipc::{AgentEventDto, ChatDoneEvent, ChatErrorEvent, ChatTokenEvent};
use futures_util::StreamExt;
use ide_core::v1::{AgentRequest, ChatRequest};
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

/// 驱动一次 Chat 流式交互，逐 token 向前端 emit。
pub async fn run_chat(
    app: AppHandle,
    session_id: String,
    message: String,
    attachments: Vec<String>,
    clients: GrpcClients,
    abort: CancellationToken,
) -> Result<(), GuiError> {
    let req = ChatRequest {
        session_id: session_id.clone(),
        message,
        attachments,
    };
    let mut stream = clients
        .agent
        .clone()
        .chat(req)
        .await
        .map_err(|e| GuiError::grpc(format!("chat failed: {e}")))?
        .into_inner();

    while let Some(item) = stream.next().await {
        if abort.is_cancelled() {
            break;
        }
        match item {
            Ok(msg) => {
                let _ = app.emit(
                    "chat-token",
                    ChatTokenEvent {
                        session_id: session_id.clone(),
                        delta: msg.content,
                    },
                );
            }
            Err(e) => {
                let _ = app.emit(
                    "chat-error",
                    ChatErrorEvent {
                        session_id: session_id.clone(),
                        message: e.to_string(),
                    },
                );
                return Err(GuiError::grpc(e.to_string()));
            }
        }
    }
    let _ = app.emit("chat-done", ChatDoneEvent { session_id });
    Ok(())
}

/// 驱动一次 RunAgent（ReAct）流式交互，逐事件向前端 emit。
pub async fn run_agent(
    app: AppHandle,
    session_id: String,
    goal: String,
    project_id: Option<String>,
    clients: GrpcClients,
    abort: CancellationToken,
) -> Result<(), GuiError> {
    let req = AgentRequest {
        session_id: session_id.clone(),
        goal,
        project_id: project_id.unwrap_or_default(),
    };
    let mut stream = clients
        .agent
        .clone()
        .run_agent(req)
        .await
        .map_err(|e| GuiError::grpc(format!("run_agent failed: {e}")))?
        .into_inner();

    while let Some(item) = stream.next().await {
        if abort.is_cancelled() {
            break;
        }
        match item {
            Ok(ev) => {
                let _ = app.emit(
                    "agent-event",
                    AgentEventDto {
                        kind: ev.kind,
                        payload: ev.payload,
                        ts_ms: ev.ts_ms,
                    },
                );
            }
            Err(e) => {
                let _ = app.emit(
                    "agent-error",
                    ChatErrorEvent {
                        session_id: session_id.clone(),
                        message: e.to_string(),
                    },
                );
                return Err(GuiError::grpc(e.to_string()));
            }
        }
    }
    let _ = app.emit("agent-done", ChatDoneEvent { session_id });
    Ok(())
}
