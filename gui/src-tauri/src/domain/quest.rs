//! 进程内 Quest（决策 #A 真实 LLM 驱动目标分解 + ReAct 循环）。

use crate::domain::core_config::build_core_config;
use crate::domain::current_vendor;
use crate::error::{GuiError, GuiResult};
use crate::ipc::QuestReportDto;
use crate::ipc::QuestRunRequest;
use ide_core::agent::build_llm;
use ide_core::host::{CliHost, HostBridge};
use ide_core::llm::Llm;
use ide_core::quest::{LlmGoalDecomposer, Quest, QuestConfig};
use ide_core::tool_executor::{BasicToolExecutor, ToolExecutor, Validator};
use ide_core::validator::BasicValidator;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// 运行一次 Quest（进程内复用 ide_core::quest::Quest）。
pub async fn run_quest(app: &AppHandle, req: QuestRunRequest) -> GuiResult<QuestReportDto> {
    let vendor = current_vendor(app);
    let config = build_core_config(&vendor);
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let llm: Arc<dyn Llm> = build_llm(&config);
    let tools: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new());
    let validator: Arc<dyn Validator> = Arc::new(BasicValidator::new(config.quest_max_steps));
    let mut quest_config: QuestConfig = config.quest_config();
    // 前端可在请求中覆盖 auto_commit。
    if let Some(ac) = req.auto_commit {
        quest_config.auto_commit = ac;
    }
    let quest = Quest::new(
        Arc::new(LlmGoalDecomposer::new(llm.clone())),
        llm,
        tools,
        validator,
        bridge,
        quest_config,
    );
    let report = quest.run(&req.goal).await.map_err(GuiError::from)?;
    Ok(QuestReportDto::from(report))
}
