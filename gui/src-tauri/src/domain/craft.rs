//! 进程内 Craft（决策 #B：真实落盘到磁盘，走 `FsHost`）。

use crate::domain::fs_host::FsHost;
use crate::error::{GuiError, GuiResult};
use crate::ipc::{CraftActionRequest, CraftProposeRequest, CraftProposalDto, CraftStateDto};
use ide_core::craft::{CraftEngine, CraftProposal, CraftState, EditKind};
use ide_core::host::HostBridge;
use ide_core::permissions::PermissionSet;
use std::sync::Arc;
use tauri::Manager;

/// 把前端 kind 字符串映射到 ide_core 的 [`EditKind`]。
fn map_kind(kind: &str) -> EditKind {
    match kind.to_ascii_lowercase().as_str() {
        "runcommand" => EditKind::RunCommand,
        "commit" => EditKind::Commit,
        _ => EditKind::FileEdit,
    }
}

/// 把 ide_core 的 [`CraftState`] 映射到 IPC 的 [`CraftStateDto`]。
fn map_state(s: CraftState) -> CraftStateDto {
    match s {
        CraftState::Suggestion => CraftStateDto::Suggestion,
        CraftState::PendingConfirm => CraftStateDto::PendingConfirm,
        CraftState::Applied => CraftStateDto::Applied,
        CraftState::Rejected => CraftStateDto::Rejected,
    }
}

/// 构造一个带真实 `FsHost` 的 Craft 引擎（confirm 即落盘）。
fn engine_with_fs() -> CraftEngine {
    let host = Arc::new(FsHost::new());
    let bridge = Arc::new(HostBridge::new(host));
    CraftEngine::new(bridge, PermissionSet::all())
}

/// 提出一个 craft 编辑提案（不落盘，仅展示 diff）。
pub async fn craft_propose(_app: &tauri::AppHandle, req: CraftProposeRequest) -> GuiResult<CraftProposalDto> {
    let engine = engine_with_fs();
    let proposal = engine.propose(
        &req.document_uri,
        &req.old_text,
        &req.new_text,
        &req.rationale,
        map_kind(&req.kind),
    );
    Ok(CraftProposalDto {
        id: proposal.id,
        document_uri: proposal.document_uri,
        old_text: proposal.old_text,
        new_text: proposal.new_text,
        rationale: proposal.rationale,
        kind: req.kind,
        state: map_state(proposal.state),
    })
}

/// 确认并应用提案（真实写盘）。
pub async fn craft_confirm(_app: &tauri::AppHandle, req: CraftActionRequest) -> GuiResult<CraftProposalDto> {
    let engine = engine_with_fs();
    let mut proposal = CraftProposal::new(
        &req.id,
        &req.document_uri,
        &req.old_text,
        &req.new_text,
        &req.rationale,
        map_kind(&req.kind),
    );
    let state = engine.confirm(&mut proposal).await.map_err(GuiError::from)?;
    Ok(CraftProposalDto {
        id: proposal.id,
        document_uri: proposal.document_uri,
        old_text: proposal.old_text,
        new_text: proposal.new_text,
        rationale: proposal.rationale,
        kind: req.kind,
        state: map_state(state),
    })
}

/// 拒绝提案（不落盘）。
pub async fn craft_reject(_app: &tauri::AppHandle, req: CraftActionRequest) -> GuiResult<CraftProposalDto> {
    Ok(CraftProposalDto {
        id: req.id,
        document_uri: req.document_uri,
        old_text: req.old_text,
        new_text: req.new_text,
        rationale: req.rationale,
        kind: req.kind,
        state: CraftStateDto::Rejected,
    })
}
