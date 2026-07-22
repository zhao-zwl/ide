//! Craft（决策 #B：真实落盘编辑）命令。

use crate::domain;
use crate::error::GuiResult;
use crate::ipc::{CraftActionRequest, CraftProposeRequest, CraftProposalDto};
use tauri::AppHandle;

/// 提出一个 craft 编辑提案（不落盘，仅展示 diff）。
#[tauri::command]
pub async fn craft_propose(app: AppHandle, req: CraftProposeRequest) -> GuiResult<CraftProposalDto> {
    domain::craft::craft_propose(&app, req).await
}

/// 确认并应用提案（真实写盘）。
#[tauri::command]
pub async fn craft_confirm(app: AppHandle, req: CraftActionRequest) -> GuiResult<CraftProposalDto> {
    domain::craft::craft_confirm(&app, req).await
}

/// 拒绝提案（不落盘）。
#[tauri::command]
pub async fn craft_reject(app: AppHandle, req: CraftActionRequest) -> GuiResult<CraftProposalDto> {
    domain::craft::craft_reject(&app, req).await
}
