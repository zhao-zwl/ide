//! 前后端共享的 IPC 类型（Rust 侧）。
//!
//! 这些结构体是 Tauri Command 的入参/返回值，以及 `emit` 事件的载荷。字段命名
//! 与前端 `src/types/models.ts` 一一对应（snake_case ↔ camelCase 由 serde 自动
//! 转换；枚举用 `snake_case` 以便前端直接 `kind === "local"` 判断）。

use crate::error::GuiError;
use ide_core::admin::ConsoleStatus as CoreConsoleStatus;
use ide_core::collab::{Comment as CoreComment, Lock as CoreLock};
use ide_core::quest::{PendingApproval as CorePendingApproval, QuestReport as CoreQuestReport, SubTask as CoreSubTask, SubTaskStatus};
use serde::{Deserialize, Serialize};

// ----------------------------- 启动 / 连接 --------------------------------

/// 后端栈拉起阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapPhase {
    Idle,
    Pg,
    Ollama,
    Model,
    Serve,
    Ready,
    Error,
}

/// 后端栈拉起进度（前端 `BootstrapPage` 消费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapState {
    pub phase: BootstrapPhase,
    /// 0.0 ~ 1.0
    pub progress: f32,
    pub detail: Option<String>,
}

/// gRPC 连接状态（固定 127.0.0.1:50051，由 GUI 拉起 serve）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnStatus {
    Booting,
    Connected,
    Error,
}

// ----------------------------- 模型后端（vendor） --------------------------

/// 模型后端种类：本地 Ollama / 在线 OpenAI 兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendorKind {
    Local,
    Online,
}

/// 当前模型后端配置（密钥不出现在此结构中）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorConfig {
    pub kind: VendorKind,
    /// 在线厂商 base url（含 /v1）；本地为 None。
    pub base_url: Option<String>,
    /// 本地模型名（默认 nes-tab:latest）。
    pub local_model: String,
    /// 在线厂商模型名（如 deepseek-chat / qwen-plus）；本地为 None。
    /// 非密钥，可与 base_url 一并持久化。
    pub model: Option<String>,
}

/// 在线厂商连通测试请求（Key 仅在内存瞬时使用，不持久化）。
#[derive(Debug, Clone, Deserialize)]
pub struct TestVendorRequest {
    pub base_url: String,
    pub api_key: Option<String>,
}

// ----------------------------- Chat / Agent -------------------------------

/// `chat_send` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct ChatSendRequest {
    pub session_id: String,
    pub message: String,
    /// `@file:` / `@symbol:` 引用。
    pub attachments: Vec<String>,
}

/// `chat_stop` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct ChatStopRequest {
    pub session_id: String,
}

/// `agent_run` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRunRequest {
    pub session_id: String,
    pub goal: String,
    pub project_id: Option<String>,
}

/// 流式事件载荷：token 增量。
#[derive(Debug, Clone, Serialize)]
pub struct ChatTokenEvent {
    pub session_id: String,
    pub delta: String,
}

/// 流式事件载荷：本轮结束。
#[derive(Debug, Clone, Serialize)]
pub struct ChatDoneEvent {
    pub session_id: String,
}

/// 流式事件载荷：错误。
#[derive(Debug, Clone, Serialize)]
pub struct ChatErrorEvent {
    pub session_id: String,
    pub message: String,
}

/// RunAgent 流式事件载荷（prost `AgentEvent` 不可直接序列化，这里抽取字段）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentEventDto {
    pub kind: String,
    pub payload: String,
    pub ts_ms: i64,
}

// ----------------------------- Quest --------------------------------------

/// `quest_run` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct QuestRunRequest {
    pub goal: String,
    pub auto_commit: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubTaskStatus {
    Pending,
    Running,
    Success,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTaskDto {
    pub id: String,
    pub description: String,
    pub status: SubTaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalDto {
    pub id: String,
    pub tool: String,
    pub argument: String,
    pub subtask_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestReportDto {
    pub goal: String,
    pub subtasks: Vec<SubTaskDto>,
    pub successes: usize,
    pub failures: usize,
    pub pending_approvals: Vec<PendingApprovalDto>,
}

impl From<CoreQuestReport> for QuestReportDto {
    fn from(r: CoreQuestReport) -> Self {
        Self {
            goal: r.goal,
            subtasks: r.subtasks.into_iter().map(SubTaskDto::from).collect(),
            successes: r.successes,
            failures: r.failures,
            pending_approvals: r.pending_approvals.into_iter().map(PendingApprovalDto::from).collect(),
        }
    }
}

impl From<CoreSubTask> for SubTaskDto {
    fn from(s: CoreSubTask) -> Self {
        Self {
            id: s.id,
            description: s.description,
            status: match s.status {
                SubTaskStatus::Pending => SubTaskStatus::Pending,
                SubTaskStatus::Running => SubTaskStatus::Running,
                SubTaskStatus::Success => SubTaskStatus::Success,
                SubTaskStatus::Failed => SubTaskStatus::Failed,
                SubTaskStatus::Skipped => SubTaskStatus::Skipped,
            },
        }
    }
}

impl From<CorePendingApproval> for PendingApprovalDto {
    fn from(p: CorePendingApproval) -> Self {
        Self {
            id: p.id,
            tool: p.tool,
            argument: p.argument,
            subtask_id: p.subtask_id,
        }
    }
}

// ----------------------------- Craft --------------------------------------

/// `craft_propose` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct CraftProposeRequest {
    pub document_uri: String,
    pub old_text: String,
    pub new_text: String,
    pub rationale: String,
    /// "FileEdit" | "RunCommand" | "Commit"
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CraftStateDto {
    Suggestion,
    PendingConfirm,
    Applied,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftProposalDto {
    pub id: String,
    pub document_uri: String,
    pub old_text: String,
    pub new_text: String,
    pub rationale: String,
    /// "FileEdit" | "RunCommand" | "Commit"
    pub kind: String,
    pub state: CraftStateDto,
}

/// `craft_confirm` / `craft_reject` 入参（前端回传 proposal 全量）。
#[derive(Debug, Clone, Deserialize)]
pub struct CraftActionRequest {
    pub id: String,
    pub document_uri: String,
    pub old_text: String,
    pub new_text: String,
    pub rationale: String,
    pub kind: String,
}

// ----------------------------- Collab（comment/lock/secret） --------------

/// `comment_add` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentAddRequest {
    pub file: String,
    pub line: usize,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentDto {
    pub id: String,
    pub tenant_id: String,
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub author: String,
    pub body: String,
    pub resolved: bool,
    pub created_at: i64,
}

impl From<CoreComment> for CommentDto {
    fn from(c: CoreComment) -> Self {
        Self {
            id: c.id,
            tenant_id: c.tenant_id,
            file: c.file,
            line_start: c.line_start,
            line_end: c.line_end,
            author: c.author,
            body: c.body,
            resolved: c.resolved,
            created_at: c.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockDto {
    pub tenant_id: String,
    pub file: String,
    pub owner: String,
    pub acquired_at: i64,
}

impl From<CoreLock> for LockDto {
    fn from(l: CoreLock) -> Self {
        Self {
            tenant_id: l.tenant_id,
            file: l.file,
            owner: l.owner,
            acquired_at: l.acquired_at,
        }
    }
}

/// `secret_set` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct SecretSetRequest {
    pub name: String,
    pub value: String,
}

/// `secret_get` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct SecretGetRequest {
    pub name: String,
}

// ----------------------------- 健康概览（9090） ----------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleMetricsDto {
    pub requests: u64,
    pub tool_calls: u64,
    pub llm_calls: u64,
    pub completions: u64,
    pub denials: u64,
    pub request_p95_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleStatusDto {
    pub tenant_id: String,
    pub user_id: String,
    pub perm_mask: u32,
    pub permissions: String,
    pub audit_events: u64,
    pub metrics: ConsoleMetricsDto,
}

impl From<CoreConsoleStatus> for ConsoleStatusDto {
    fn from(c: CoreConsoleStatus) -> Self {
        Self {
            tenant_id: c.tenant_id,
            user_id: c.user_id,
            perm_mask: c.perm_mask,
            permissions: format_permissions(c.perm_mask),
            audit_events: c.audit_events,
            metrics: ConsoleMetricsDto {
                requests: c.requests,
                tool_calls: c.tool_calls,
                llm_calls: c.llm_calls,
                completions: c.completions,
                denials: c.denials,
                request_p95_ms: c.request_p95_ms,
            },
        }
    }
}

/// 把六位权限掩码渲染为逗号分隔名（按 ide_core::permissions 的编码）。
fn format_permissions(mask: u32) -> String {
    use ide_core::permissions::Permission;
    let mut names: Vec<&str> = Vec::new();
    let all = [
        (Permission::Read, "Read"),
        (Permission::Generate, "Generate"),
        (Permission::Modify, "Modify"),
        (Permission::Execute, "Execute"),
        (Permission::Commit, "Commit"),
        (Permission::Audit, "Audit"),
    ];
    for (p, name) in all {
        // `Permission` 自身只有 `bit()`（返回 u8），掩码运算提升为 u32。
        if (p.bit() as u32) & mask != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        "None".to_string()
    } else {
        names.join(",")
    }
}

/// 健康概览（拉 9090 /healthz + /console）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthOverviewDto {
    pub healthz: String,
    pub console: ConsoleStatusDto,
}

impl GuiError {
    /// 便捷构造：连接类错误。
    pub fn grpc(message: impl Into<String>) -> Self {
        GuiError::new("grpc", message)
    }
    /// 便捷构造：bootstrap 类错误。
    pub fn bootstrap(message: impl Into<String>) -> Self {
        GuiError::new("bootstrap", message)
    }
    /// 便捷构造：vendor/配置类错误。
    pub fn vendor(message: impl Into<String>) -> Self {
        GuiError::new("vendor", message)
    }
}
