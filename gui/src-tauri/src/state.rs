//! 全局应用状态 [`AppState`]。
//!
//! 聚合：gRPC 客户端、当前模型后端配置、后端栈 sidecar 句柄、流式取消令牌、
//! 连接状态、数据目录与数据库连接串。comment/lock/secret 存储按需连接 bundled
//! PostgreSQL（见 `commands/collab.rs`），不常驻于状态以减少生命周期复杂度。

use crate::grpc::GrpcClients;
use crate::ipc::{BootstrapPhase, BootstrapState, ConnStatus, VendorConfig, VendorKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 后端栈各 sidecar 的子进程句柄 + 当前阶段。
///
/// 外层已由 `AppState.bootstrap: Mutex<BootstrapHandles>` 加锁，因此各字段本身
/// 无需再各自套一层 Mutex（避免 [`pg.rs`] / [`ollama.rs`] / [`serve.rs`] 写句柄时
/// 多层锁嵌套）。
pub struct BootstrapHandles {
    pub pg: Option<Child>,
    pub ollama: Option<Child>,
    pub serve: Option<Child>,
    pub phase: BootstrapPhase,
}

impl BootstrapHandles {
    pub fn new() -> Self {
        Self {
            pg: None,
            ollama: None,
            serve: None,
            phase: BootstrapPhase::Idle,
        }
    }
}

/// 全局状态，注册到 Tauri 的 `manage()`。
pub struct AppState {
    /// 已建立的 gRPC 客户端（serve 就绪后填充）。
    pub grpc: Mutex<Option<GrpcClients>>,
    /// 当前模型后端配置（非密钥，可经 store 持久化）。
    pub vendor: Mutex<VendorConfig>,
    /// 后端栈 sidecar 句柄。
    pub bootstrap: Mutex<BootstrapHandles>,
    /// 最近一次 bootstrap 进度快照（供 `bootstrap_status` 即时返回）。
    pub last_bootstrap: Mutex<Option<BootstrapState>>,
    /// 按 session_id 维护的流式取消令牌（chat_stop / agent 停止用）。
    pub aborts: Mutex<HashMap<String, CancellationToken>>,
    /// gRPC 连接状态。
    pub conn_status: Mutex<ConnStatus>,
    /// App 数据目录（PG 数据目录、Ollama 模型库置于其下）。
    pub app_data_dir: Mutex<Option<PathBuf>>,
    /// bundled PostgreSQL 连接串（由 bootstrap 写入）。
    pub database_url: Mutex<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            grpc: Mutex::new(None),
            vendor: Mutex::new(VendorConfig {
                kind: VendorKind::Local,
                base_url: None,
                local_model: "nes-tab:latest".to_string(),
                model: None,
            }),
            bootstrap: Mutex::new(BootstrapHandles::new()),
            last_bootstrap: Mutex::new(None),
            aborts: Mutex::new(HashMap::new()),
            conn_status: Mutex::new(ConnStatus::Booting),
            app_data_dir: Mutex::new(None),
            database_url: Mutex::new(
                "postgres://aidea:aidea@127.0.0.1:5432/aidea".to_string(),
            ),
        }
    }

    /// 注册一个 session 的取消令牌，返回克隆句柄。
    pub fn register_abort(&self, session_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        self.aborts
            .lock()
            .unwrap()
            .insert(session_id.to_string(), token.clone());
        token
    }

    /// 取消并移除某 session 的令牌。
    pub fn cancel_abort(&self, session_id: &str) {
        if let Some(token) = self.aborts.lock().unwrap().remove(session_id) {
            token.cancel();
        }
    }

    /// 取出某 session 的令牌（不移除），用于流式循环检测。
    pub fn abort_for(&self, session_id: &str) -> Option<CancellationToken> {
        self.aborts
            .lock()
            .unwrap()
            .get(session_id)
            .map(|t| t.clone())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
