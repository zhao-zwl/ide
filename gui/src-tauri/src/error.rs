//! GUI 统一错误类型。
//!
//! Tauri v2 命令返回 `Result<T, GuiError>`，框架会把 `GuiError` 序列化为
//! `{ error: { code, message } }` 供前端 `try/catch` 消费。错误码与 gRPC 状态
//! 透传的语义一致（`code` 字段）。

use serde::Serialize;
use serde_json::json;

/// GUI 层错误：结构化 `{ code, message }`。
#[derive(Debug, Clone, Serialize)]
pub struct GuiError {
    /// 错误码（如 `grpc`, `bootstrap`, `vendor`, `internal`, `permission`）。
    pub code: String,
    /// 人类可读信息（不在此记录密钥）。
    pub message: String,
}

impl GuiError {
    /// 构造错误。
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for GuiError {}

impl From<anyhow::Error> for GuiError {
    fn from(e: anyhow::Error) -> Self {
        GuiError::new("internal", e.to_string())
    }
}

impl From<GuiError> for String {
    fn from(e: GuiError) -> Self {
        e.to_string()
    }
}

/// Tauri v2 命令错误必须可转为 `InvokeError`；优先以结构化 JSON 返回
/// `{ code, message }`，便于前端程序化读取错误类别。
impl From<GuiError> for tauri::ipc::InvokeError {
    fn from(e: GuiError) -> Self {
        tauri::ipc::InvokeError::from_serde(json!({ "code": e.code, "message": e.message }))
            .unwrap_or_else(|_| tauri::ipc::InvokeError::from(e.to_string()))
    }
}

/// GUI 命令统一返回类型。
pub type GuiResult<T> = Result<T, GuiError>;
