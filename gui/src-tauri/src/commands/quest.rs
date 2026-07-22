//! Quest（进程内自治代理）命令。

use crate::domain;
use crate::error::GuiResult;
use crate::ipc::{QuestReportDto, QuestRunRequest};
use tauri::AppHandle;

/// 运行一次 Quest（决策 #A：真实 LLM 驱动目标分解 + ReAct 循环）。
#[tauri::command]
pub async fn quest_run(app: AppHandle, req: QuestRunRequest) -> GuiResult<QuestReportDto> {
    domain::quest::run_quest(&app, req).await
}
