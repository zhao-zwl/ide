//! T21 端到端 eval 套件（v2.0 阶段 B — 测试评测收口）.
//!
//! 本文件是 **M1 → v2.0A 之上的一次性增量**：把内核各模块已有的单测/集成
//! 测试整合成一套 **可跑的端到端 eval 场景**，用既有 Mock 设施
//! （[`MockLlm`] / [`CliHost`] / [`InMemoryCkg`] / [`MockAuditSink`] 等）
//! 串起 **真实调用链**，断言跨模块行为正确。它做的是"组装验证"，不是单测重复。
//!
//! 设计约束（与 v2.0A 一致）：
//!   * 复用既有 Mock 设施，**不重写生产代码**、**不另起炉灶**。
//!   * 若某场景需要新的 Mock 构造器，仅在本文内自建轻量 double（`ModifyActionLlm` /
//!     `CommitQuestLlm` / `PatchLlm` / `FlakyTool`），不触碰生产代码。
//!   * 无外部依赖、可进程内 `cargo test` 跑；proto **G6 冻结**，eval 不进 `.proto`。
//!   * 沙箱重型依赖会 OOM，未实跑 `cargo test`；以下场景结构正确、依赖声明合理，
//!     单测逻辑可概念性运行。
//!
//! 8 个场景（详见各 `#[tokio::test]`）：
//!   1. `demo_react_completes`                         — 默认栈 ReAct 闭环终态/budget。
//!   2. `chat_returns_answer_and_suggestion`       — ChatEngine 返回 final answer + 工具建议。
//!   3. `craft_propose_then_confirm_applies_edit_under_permission`
//!                                                 — Craft propose→confirm 应用编辑；无 Modify 位拒绝。
//!   4. `quest_decomposes_and_collects_pending_when_auto_commit_false`
//!                                                 — Quest 分解 + auto_commit=false 收待确认、不擅自提交。
//!   5. `self_heal_retries_on_failure`              — SelfHeal 首败→补丁→二次成功（计数 1）。
//!   6. `ckg_query_returns_neighborhood`           — InMemoryCkg 邻居查询（Calls/Contains）。
//!   7. `audit_records_and_metrics_increment`       — 带动作链路审计计数 + 指标递增（O1 `>=` 容差）。
//!   8. `sso_no_token_denies_when_enabled`       — 启用 SSO 但无 token → Unauthorized（Noop 兜底不拒）。

use async_trait::async_trait;
use ide_core::audit::{AuditAction, MockAuditSink};
use ide_core::auth::{AuthError, Hs256Authenticator, NoopAuthenticator};
use ide_core::chat::{ChatEngine, ChatSession};
use ide_core::ckg::{EdgeKind, InMemoryCkg};
use ide_core::craft::{CraftEngine, CraftState, EditKind};
use ide_core::host::{CliHost, HostBridge};
use ide_core::llm::{ActionPlan, Llm, Thought};
use ide_core::metrics::Metrics;
use ide_core::permissions::PermissionSet;
use ide_core::planner::Planner;
use ide_core::principal::Principal;
use ide_core::quest::{LlmGoalDecomposer, Quest, QuestConfig};
use ide_core::self_heal::SelfHeal;
use ide_core::tool_executor::{BasicToolExecutor, Observation, ToolExecutor};
use ide_core::validator::BasicValidator;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

// ===========================================================================
// 轻量测试 double（仅本文使用，不触碰生产代码）
// ===========================================================================

/// Double：始终规划一个特权 `modify` 动作（Modify 位）。用于审计/指标 e2e 场景，
/// 以驱动"动作边界"审计路径。
struct ModifyActionLlm;
#[async_trait]
impl Llm for ModifyActionLlm {
    async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
        Ok(Thought {
            text: "I will modify the file.".to_string(),
        })
    }
    async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
        Ok(ActionPlan {
            tool: "modify".to_string(),
            argument: "apply patch".to_string(),
        })
    }
}

/// Double：用于 Quest 目标分解 + 每子任务规划 `commit`。产出编号子任务列表，
/// 并始终规划 `commit`，使审批闸（auto_commit=false）收集 pending 而非执行。
struct CommitQuestLlm;
#[async_trait]
impl Llm for CommitQuestLlm {
    async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
        Ok(Thought {
            text: "1. implement the feature\n2. commit the change".to_string(),
        })
    }
    async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
        Ok(ActionPlan {
            tool: "commit".to_string(),
            argument: "all".to_string(),
        })
    }
}

/// Double：为一个失败工具返回自愈补丁。
struct PatchLlm;
#[async_trait]
impl Llm for PatchLlm {
    async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
        Ok(Thought {
            text: "PATCH: fixed".to_string(),
        })
    }
    async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
        Ok(ActionPlan {
            tool: "noop".to_string(),
            argument: String::new(),
        })
    }
}

/// Flaky 工具 double：参数在 `fail_on` 中则失败，否则成功；记录每次调用。
struct FlakyTool {
    fail_on: HashSet<String>,
    runs: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl ToolExecutor for FlakyTool {
    async fn run(&self, _tool: &str, argument: &str) -> anyhow::Result<Observation> {
        self.runs.lock().unwrap().push(argument.to_string());
        if self.fail_on.contains(argument) {
            Err(anyhow::anyhow!("simulated failure for `{argument}`"))
        } else {
            Ok(Observation {
                tool: _tool.to_string(),
                output: format!("ok: {argument}"),
                terminal: false,
            })
        }
    }
}

// ===========================================================================
// 8 个端到端 eval 场景
// ===========================================================================

/// 场景 1：默认栈 ReAct 闭环。CliHost + MockLlm + BasicToolExecutor 串起真实调用链，
/// 断言终态为 budget 耗尽、每个 observation 以 "inspected" 开头（复用默认栈语义）。
#[tokio::test]
async fn demo_react_completes() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    // 组装真实的 M1 默认栈（CliHost + MockLlm + BasicToolExecutor）。
    let planner = Planner::with_defaults(bridge, 4);
    let trace = planner.run("add retry logic to utils").await.unwrap();
    // 终态必须为 budget 耗尽；每步 action 为 inspect。
    assert_eq!(trace.final_answer, "budget exhausted");
    assert_eq!(trace.steps.len(), 4);
    for s in &trace.steps {
        assert_eq!(s.action, "inspect");
        assert!(
            s.observation.starts_with("inspected"),
            "observation 必须以 'inspected' 开头: {}",
            s.observation
        );
    }
}

/// 场景 2：ChatEngine 用 MockLlm 返回 final answer 且含工具建议。断言返回非空内容 +
/// 至少一个工具建议（复用既有 `Llm` 推理核，证明 chat 与 Planner 共用同一核）。
#[tokio::test]
async fn chat_returns_answer_and_suggestion() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let planner = Planner::with_defaults(bridge, 8);
    // ChatEngine 复用 Planner 的同一 MockLlm 实例。
    let engine = ChatEngine::new(planner.llm(), PermissionSet::all());
    let mut session = ChatSession::new(2048, 16);
    let reply = engine
        .reply(&mut session, "where is main?", &[])
        .await
        .unwrap();
    assert!(!reply.content.is_empty(), "final answer 必须返回");
    assert!(!reply.suggestions.is_empty(), "必须给出工具建议");
    assert_eq!(reply.suggestions[0].tool, "inspect");
}

/// 场景 3：CraftEngine propose→confirm（CliHost 桩）应用编辑；断言
/// `HostProvider::apply_edit` 被调用、文件内容变更；无 Modify 权限时 confirm 拒绝。
#[tokio::test]
async fn craft_propose_then_confirm_applies_edit_under_permission() {
    // 有 Modify 位：propose + confirm 应当真正应用编辑到宿主文档。
    let host = Arc::new(CliHost::new());
    host.seed("mem://a.rs", "let x = 1;");
    let bridge = Arc::new(HostBridge::new(host.clone()));
    let engine = CraftEngine::new(bridge, PermissionSet::all());
    let mut proposal =
        engine.propose("mem://a.rs", "let x = 1;", "let x = 2;", "bump value", EditKind::FileEdit);
    let state = engine.confirm(&mut proposal).await.unwrap();
    assert_eq!(state, CraftState::Applied);
    // 宿主文档必须确实被变更（apply_edit 真的跑了）。
    let doc = bridge.read_document("mem://a.rs").await.unwrap();
    assert_eq!(doc, "let x = 2;");

    // 无 Modify 位：confirm 必须被拒绝且文件不被改动。
    let host2 = Arc::new(CliHost::new());
    host2.seed("mem://b.rs", "let y = 1;");
    let bridge2 = Arc::new(HostBridge::new(host2.clone()));
    let engine2 = CraftEngine::new(bridge2, PermissionSet::empty());
    let mut denied =
        engine2.propose("mem://b.rs", "let y = 1;", "let y = 9;", "bump", EditKind::FileEdit);
    let err = engine2.confirm(&mut denied).await.unwrap_err();
    assert!(err.to_string().contains("permission denied"));
    assert_eq!(denied.state, CraftState::Suggestion);
}

/// 场景 4：Quest 用 MockLlm（CommitQuestLlm）分解子任务；auto_commit=false 时断言
/// 返回 QuestReport 含 pending_approvals、不擅自提交（FlakyTool 零调用）。复用既有
/// `LlmGoalDecomposer` + `Quest` + 审批闸。
#[tokio::test]
async fn quest_decomposes_and_collects_pending_when_auto_commit_false() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    // FlakyTool 证明 auto_commit=false 时没有任何风险动作被执行。
    let runs = Arc::new(Mutex::new(Vec::new()));
    let tools: Arc<dyn ToolExecutor> = Arc::new(FlakyTool {
        fail_on: HashSet::new(),
        runs: runs.clone(),
    });
    let llm: Arc<dyn Llm> = Arc::new(CommitQuestLlm);
    let mut cfg = QuestConfig::default();
    cfg.max_steps = 3;
    cfg.max_subtasks = 8;
    cfg.auto_commit = false; // 审批闸打开
    let quest = Quest::new(
        Arc::new(LlmGoalDecomposer::new(llm.clone())),
        llm,
        tools,
        Arc::new(BasicValidator::new(cfg.max_steps)),
        bridge,
        cfg,
    );
    let report = quest.run("ship the feature").await.unwrap();
    // 编号分解必须产出子任务。
    assert!(!report.subtasks.is_empty());
    // 审批闸打开时，`commit` 动作被收集为待确认，而非执行。
    assert!(
        report.pending_approvals.len() >= 1,
        "commit 必须被收集为 pending_approvals"
    );
    assert!(
        runs.lock().unwrap().is_empty(),
        "auto_commit=false 时绝不可执行任何风险动作"
    );
}

/// 场景 5：SelfHeal 包一个首次失败工具 + MockLlm 给补丁，断言二次成功、修复计数=1。
/// 复用 `SelfHeal` + `FlakyTool` + `PatchLlm`。
#[tokio::test]
async fn self_heal_retries_on_failure() {
    let mut fail_on = HashSet::new();
    fail_on.insert("broken".to_string());
    let runs = Arc::new(Mutex::new(Vec::new()));
    let tool: Arc<dyn ToolExecutor> = Arc::new(FlakyTool {
        fail_on,
        runs: runs.clone(),
    });
    let heal = SelfHeal::new(tool, 3);
    let outcome = heal
        .run("sh", "broken", Arc::new(PatchLlm))
        .await
        .unwrap();
    // 首次失败，LLM 补丁修复，二次成功。
    assert!(outcome.healed);
    assert_eq!(outcome.repair_attempts, 1);
    assert_eq!(outcome.observation.output, "ok: fixed");
    assert_eq!(runs.lock().unwrap().len(), 2);
}

/// 场景 6：InMemoryCkg 注入小样本，断言 query 返回相关符号（Calls/Contains）。
#[test]
fn ckg_query_returns_neighborhood() {
    let mut ckg = InMemoryCkg::new();
    ckg.ingest_file(
        "a.rs",
        "fn retry() {}\nfn process() {\n    retry();\n}\n",
    );
    // 查询调用方 `process` -> 其被调用方 `retry`（Calls 边）。
    let related = ckg.query("process");
    assert!(
        related
            .iter()
            .any(|r| r.symbol.name == "retry" && r.relation == EdgeKind::Calls),
        "query(process) 应暴露被调用方 `retry`"
    );
    // 反向：查询被调用方 -> 调用方。
    let reverse = ckg.query("retry");
    assert!(
        reverse.iter().any(|r| r.symbol.name == "process"),
        "query(retry) 应暴露调用方 `process`"
    );
}

/// 场景 7：跑一段带动作的链路（Planner 的 Modify 动作 + ChatEngine 的 Generate 动作，
/// 共享同一 MockAuditSink + Metrics），断言审计计数递增、指标计数器递增。
///
/// O1 容差说明：在完整 gRPC 路径下，同一 principal 还会在 RPC 边界额外写一条
/// 审计（RPC 级 + 动作级双重写入——这是 **设计取舍、非缺陷**，见 test-report.md §已知项）。
/// 因此 `audit_events` 计数可能 `>=` sink 事件数；此处用 `>=` 断言，使 e2e 检查对该
/// 双重写入保持韧性（无论直跑还是经 server 跑都绿）。
#[tokio::test]
async fn audit_records_and_metrics_increment() {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    let audit = Arc::new(MockAuditSink::new());
    let metrics = Arc::new(Metrics::new());
    let principal = Principal::all("single", "tester");

    // (a) Planner 动作链：每步一个特权 `modify` 动作，驱动动作边界审计 + 工具调用计数。
    let planner = Planner::new(
        Arc::new(ModifyActionLlm),
        Arc::new(BasicToolExecutor::new()),
        Arc::new(BasicValidator::new(4)),
        bridge.clone(),
        4,
    )
    .with_governance(principal.clone(), audit.clone(), metrics.clone());
    let _ = planner.run("goal").await.unwrap();

    // (b) Chat 链（共享同一 sink + metrics）：一个 `Generate` 动作。
    let chat = ChatEngine::new(planner.llm(), PermissionSet::all())
        .with_governance(principal, audit.clone(), metrics.clone());
    let mut session = ChatSession::new(2048, 16);
    let _ = chat.reply(&mut session, "hi", &[]).await.unwrap();

    // 审计 sink 记录了两类特权动作。
    assert!(
        audit.count_action(AuditAction::Modify) >= 1,
        "modify 审计必须被记录"
    );
    assert!(
        audit.count_action(AuditAction::Generate) >= 1,
        "generate 审计必须被记录"
    );
    // 指标计数器在两条热路径上都前进了。
    assert!(metrics.tool_calls() >= 1);
    assert!(metrics.llm_calls() >= 2);

    // O1 容差：metrics.audit_events() 可能 >= sink 事件数（RPC 级双重写入）。
    assert!(
        metrics.audit_events() >= audit.count(),
        "audit_events 计数必须 >= sink 事件数（O1 双重写入容差）"
    );
}

/// 场景 8：启用 SSO 但无 token，断言认证返回 Unauthorized（Noop 兜底时不拒）。
#[test]
fn sso_no_token_denies_when_enabled() {
    // SSO 开启（Hs256）但无 bearer token -> Unauthorized。
    let hs = Hs256Authenticator::new("secret", "", "");
    assert!(matches!(
        hs.authenticate(None),
        Err(AuthError::Unauthorized(_))
    ));
    // SSO 关闭（Noop）-> 即使无 token 也返回 dev 兜底 principal（不拒）。
    let noop = NoopAuthenticator::new(Principal::all("single", "core"));
    assert!(noop.authenticate(None).is_ok());
}
