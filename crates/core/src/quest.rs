//! Quest — autonomous agent (T15, F4).
//!
//! Relative to Craft (which proposes and waits for per-edit human confirmation),
//! Quest runs **autonomously** toward a high-level `goal`:
//!
//!   1. **Goal decomposition** — an [`Llm`]-based [`GoalDecomposer`] splits the
//!      goal into an ordered list of [`SubTask`]s.
//!   2. **Autonomous ReAct per subtask** — each subtask is executed by the
//!      *existing* [`Planner`] (reused, not reimplemented) with its
//!      `Llm`/`ToolExecutor`/`ContextManager`/`Validator` injected via the
//!      governance triple (`Principal`/`AuditSink`/`Metrics`), so the six-bit
//!      mask is still enforced on every action.
//!   3. **Self-healing** — the per-subtask tool executor is wrapped in
//!      [`SelfHealingExecutor`] (T16) so a failed action is auto-repaired and
//!      re-run, forming the "execute → fail → self-heal → re-execute" loop.
//!   4. **Approval gate** — when `auto_commit` is `false`, `Execute`/`Commit`
//!      class actions are *not* run; instead they are collected as
//!      [`PendingApproval`]s in the report. When `true`, they run autonomously.
//!
//! Safety budgets (`quest_max_steps` per subtask, `quest_max_subtasks` total)
//! reuse the same idea as [`Verdict::BudgetExhausted`]: the Planner's validator
//! caps steps, and Quest caps the subtask count. A failed subtask is retried
//! (`subtask_max_retries`) then skipped and marked.
//!
//! Quest is a **Core-internal** capability; it is NOT added to `.proto` (G6 is
//! frozen). The gRPC surface reuses the existing `AgentService` — see
//! [`crate::agent::AgentServer::run_quest`].

use crate::audit::{AuditSink, MockAuditSink};
use crate::host::HostBridge;
use crate::llm::Llm;
use crate::metrics::Metrics;
use crate::permissions::Permission;
use crate::planner::Planner;
use crate::principal::{Principal, action_permission};
use crate::self_heal::{SelfHeal, SelfHealingExecutor};
use crate::tool_executor::{Observation, ToolExecutor};
use crate::validator::Validator;
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::Instrument;
use std::sync::{Arc, Mutex};

/// Lifecycle status of a single [`SubTask`] within a Quest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubTaskStatus {
    /// Not yet started.
    Pending,
    /// Currently being executed.
    Running,
    /// The Planner reached a terminal `finish` for this subtask.
    Success,
    /// All retries exhausted without a terminal success.
    Failed,
    /// Excluded by the `quest_max_subtasks` budget.
    Skipped,
}

impl SubTaskStatus {
    /// Stable string form (used by the CLI report).
    pub fn as_str(&self) -> &'static str {
        match self {
            SubTaskStatus::Pending => "pending",
            SubTaskStatus::Running => "running",
            SubTaskStatus::Success => "success",
            SubTaskStatus::Failed => "failed",
            SubTaskStatus::Skipped => "skipped",
        }
    }
}

/// One ordered unit of work produced by goal decomposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubTask {
    pub id: String,
    pub description: String,
    pub status: SubTaskStatus,
}

impl SubTask {
    /// Create a subtask in the `Pending` state.
    pub fn new(id: &str, description: &str) -> Self {
        Self {
            id: id.to_string(),
            description: description.to_string(),
            status: SubTaskStatus::Pending,
        }
    }
}

/// Splits a high-level goal into an ordered list of [`SubTask`]s.
///
/// Implemented by [`LlmGoalDecomposer`] (the default, LLM-driven decomposition).
/// Kept as a trait so tests can inject a deterministic decomposer and the Quest
/// never reaches into the `Llm` interface directly for parsing.
#[async_trait]
pub trait GoalDecomposer: Send + Sync {
    /// Decompose `goal` into an ordered list of subtasks.
    async fn decompose(&self, goal: &str) -> Vec<SubTask>;
}

/// Default decomposer: drives an [`Llm`] with a decomposition prompt and parses
/// the numbered lines from the returned `Thought` into subtasks.
pub struct LlmGoalDecomposer {
    llm: Arc<dyn Llm>,
}

impl LlmGoalDecomposer {
    /// Build a decomposer over `llm`.
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        Self { llm }
    }
}

/// Prompt handed to the LLM to decompose a goal into an ordered subtask list.
fn decomposition_prompt(goal: &str) -> String {
    format!(
        "Decompose the following high-level goal into an ordered list of small, \
         independently-executable subtasks.\nGoal: {goal}\n\
         Reply with one subtask per line, prefixed with its index, e.g.\n\
         1. first step\n2. second step\n3. third step"
    )
}

#[async_trait]
impl GoalDecomposer for LlmGoalDecomposer {
    async fn decompose(&self, goal: &str) -> Vec<SubTask> {
        match self.llm.think(&decomposition_prompt(goal)).await {
            Ok(t) => parse_subtasks(&t.text, goal),
            // On LLM failure, treat the whole goal as a single subtask rather
            // than aborting the entire Quest.
            Err(e) => {
                tracing::warn!("goal decomposition failed: {e}; using goal as one subtask");
                vec![SubTask::new("st-1", goal)]
            }
        }
    }
}

/// Pure parser: extract numbered subtasks from an LLM decomposition response.
///
/// Accepts lines of the form `N. desc`, `N) desc` or `N - desc`. When no numbered
/// line is found, falls back to a single subtask equal to `goal` (so a Quest
/// always has at least one unit of work). Unit-tested (T15 acceptance).
pub fn parse_subtasks(text: &str, goal: &str) -> Vec<SubTask> {
    let mut out: Vec<SubTask> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Split on the first separator ('.' | ')' | '-') and validate the head
        // is a non-empty run of digits.
        if let Some((head, rest)) = line.split_once(|c| c == '.' || c == ')' || c == '-') {
            let head = head.trim();
            if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
                let desc = rest.trim();
                if !desc.is_empty() {
                    out.push(SubTask::new(&format!("st-{}", out.len() + 1), desc));
                }
            }
        }
    }
    if out.is_empty() {
        out.push(SubTask::new("st-1", goal));
    }
    out
}

/// An `Execute`/`Commit` class action that was *not* auto-run because the Quest
/// approval gate (`auto_commit = false`) held it for human confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: String,
    pub tool: String,
    pub argument: String,
    /// The subtask this pending action belongs to.
    pub subtask_id: String,
}

/// The outcome of running a Quest (T15 acceptance: 产出 `QuestReport`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuestReport {
    pub goal: String,
    pub subtasks: Vec<SubTask>,
    pub successes: usize,
    pub failures: usize,
    pub pending_approvals: Vec<PendingApproval>,
}

/// Configuration for a Quest run (derived from [`CoreConfig`] via
/// `CoreConfig::quest_config`).
#[derive(Debug, Clone)]
pub struct QuestConfig {
    /// Max ReAct steps per subtask loop (reuses the `BudgetExhausted` idea).
    pub max_steps: usize,
    /// Cap on the number of decomposed subtasks processed in one run.
    pub max_subtasks: usize,
    /// When `false` (default), `Execute`/`Commit` class actions are collected as
    /// pending approvals instead of auto-running. When `true`, they run.
    pub auto_commit: bool,
    /// Self-heal circuit breaker: max repair/retry attempts (T16).
    pub max_repair_attempts: usize,
    /// Retry a failed subtask this many times before skipping + marking Failed.
    pub subtask_max_retries: usize,
}

impl Default for QuestConfig {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_subtasks: 8,
            auto_commit: false,
            max_repair_attempts: 3,
            subtask_max_retries: 1,
        }
    }
}

/// Autonomous Quest agent (T15).
pub struct Quest {
    decomposer: Arc<dyn GoalDecomposer>,
    llm: Arc<dyn Llm>,
    tools: Arc<dyn ToolExecutor>,
    validator: Arc<dyn Validator>,
    bridge: Arc<HostBridge>,
    config: QuestConfig,
    /// v1.0 — security identity authorizing every subtask action (T11).
    principal: Principal,
    /// v1.0 — append-only audit sink (T11) — threaded into each subtask Planner.
    audit: Arc<dyn AuditSink>,
    /// v1.0 — observability counters (T14).
    metrics: Arc<Metrics>,
    /// Shared collection of pending approvals gathered across all subtasks.
    pending: Arc<Mutex<Vec<PendingApproval>>>,
}

impl Quest {
    /// Assemble a Quest from its collaborators.
    pub fn new(
        decomposer: Arc<dyn GoalDecomposer>,
        llm: Arc<dyn Llm>,
        tools: Arc<dyn ToolExecutor>,
        validator: Arc<dyn Validator>,
        bridge: Arc<HostBridge>,
        config: QuestConfig,
    ) -> Self {
        Self {
            decomposer,
            llm,
            tools,
            validator,
            bridge,
            config,
            // Defaults: a fully-privileged "quest" principal with an in-memory
            // audit sink + empty metrics. Override with `with_governance`.
            principal: Principal::all("single", "quest"),
            audit: Arc::new(MockAuditSink::new()),
            metrics: Arc::new(Metrics::new()),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Override the governance triple (principal / audit sink / metrics), matching
    /// the `with_governance` builder used across the Core (T11 / T12).
    pub fn with_governance(
        mut self,
        principal: Principal,
        audit: Arc<dyn AuditSink>,
        metrics: Arc<Metrics>,
    ) -> Self {
        self.principal = principal;
        self.audit = audit;
        self.metrics = metrics;
        self
    }

    /// Run an autonomous Quest toward `goal`, returning a [`QuestReport`].
    pub async fn run(&self, goal: &str) -> anyhow::Result<QuestReport> {
        let span = tracing::info_span!("quest.run", goal = goal);
        async move {
            // 1) Goal decomposition.
            let mut subtasks = self.decomposer.decompose(goal).await;

            // 2) Enforce the total-subtask budget (reuses the BudgetExhausted idea).
            //    Over-budget subtasks are marked Skipped and kept in the report for
            //    observability, but never executed.
            if subtasks.len() > self.config.max_subtasks {
                for (i, st) in subtasks.iter_mut().enumerate() {
                    if i >= self.config.max_subtasks {
                        st.status = SubTaskStatus::Skipped;
                    }
                }
            }

            let mut successes = 0usize;
            let mut failures = 0usize;

            // 3) Execute each subtask through the *existing* Planner (reused).
            for st in subtasks.iter_mut() {
                if st.status == SubTaskStatus::Skipped {
                    continue;
                }
                st.status = SubTaskStatus::Running;

                let mut attempt = 0usize;
                let mut ok = false;
                // Retry a failed subtask up to `subtask_max_retries` times, then skip.
                while attempt <= self.config.subtask_max_retries && !ok {
                    match self.execute_subtask(st).await {
                        // Terminal success reached (`finish`).
                        Ok(true) => {
                            st.status = SubTaskStatus::Success;
                            successes += 1;
                            ok = true;
                        }
                        // Completed but not a terminal success (e.g. budget exhausted).
                        Ok(false) => attempt += 1,
                        // Fatal execution error (e.g. self-heal circuit breaker tripped).
                        Err(e) => {
                            tracing::warn!(subtask = %st.id, "subtask execution failed: {e}");
                            attempt += 1;
                        }
                    }
                }
                if !ok {
                    st.status = SubTaskStatus::Failed;
                    failures += 1;
                }
            }

            // 4) Collect the pending approvals gathered across all subtasks.
            let pending = self.pending.lock().expect("pending lock poisoned").clone();
            Ok(QuestReport {
                goal: goal.to_string(),
                subtasks: subtasks.clone(),
                successes,
                failures,
                pending_approvals: pending,
            })
        }
        .instrument(span)
        .await
    }

    /// Execute one subtask through the existing [`Planner`] (reused, not
    /// reimplemented), with a self-healing, approval-gating tool executor.
    ///
    /// Returns `Ok(true)` when the Planner reached a terminal `finish` (success),
    /// `Ok(false)` otherwise (e.g. budget exhausted), and `Err` on a fatal
    /// execution error (e.g. the self-heal circuit breaker tripped).
    async fn execute_subtask(&self, st: &SubTask) -> anyhow::Result<bool> {
        // Self-healing inner executor (T16): wraps the base tool executor so a
        // failed action is auto-repaired and re-run.
        let heal = SelfHeal::new(self.tools.clone(), self.config.max_repair_attempts);
        let inner: Arc<dyn ToolExecutor> =
            Arc::new(SelfHealingExecutor::new(heal, self.llm.clone()));
        // Approval gate: collects `Execute`/`Commit` class actions as pending when
        // `auto_commit` is false.
        let quest_exec: Arc<dyn ToolExecutor> = Arc::new(QuestToolExecutor {
            inner,
            auto_commit: self.config.auto_commit,
            subtask_id: st.id.clone(),
            pending: self.pending.clone(),
        });

        let planner = Planner::new(
            self.llm.clone(),
            quest_exec,
            self.validator.clone(),
            self.bridge.clone(),
            self.config.max_steps,
        )
        .with_governance(
            self.principal.clone(),
            self.audit.clone(),
            self.metrics.clone(),
        );

        let trace = planner.run(&st.description).await?;
        // A subtask succeeds when the Planner reaches a terminal `finish` — the
        // same terminal semantics as the M1 ReAct loop (the `finish` tool yields
        // the "task finished" observation).
        Ok(trace.final_answer == "task finished")
    }
}

/// Tool executor used by Quest: applies the approval gate (`auto_commit`) and
/// collects `Execute`/`Commit` class actions as pending approvals when
/// `auto_commit` is `false`. All other actions (and all actions when
/// `auto_commit` is `true`) delegate to the inner (self-healing) executor.
///
/// Note: the six-bit mask is still enforced *inside* the Planner's
/// `authorize_and_record`; this executor only decides whether an already
/// authorized risky action is executed now or deferred for human confirmation.
pub struct QuestToolExecutor {
    inner: Arc<dyn ToolExecutor>,
    auto_commit: bool,
    subtask_id: String,
    pending: Arc<Mutex<Vec<PendingApproval>>>,
}

#[async_trait]
impl ToolExecutor for QuestToolExecutor {
    async fn run(&self, tool: &str, argument: &str) -> anyhow::Result<Observation> {
        match action_permission(tool) {
            Some(Permission::Execute) | Some(Permission::Commit) if !self.auto_commit => {
                // Gate the risky action: collect a pending approval instead of
                // executing. Returns a neutral (non-terminal) observation so the
                // Planner loop continues; the action is neither fatal nor terminal.
                let pa = PendingApproval {
                    id: fast_id(),
                    tool: tool.to_string(),
                    argument: argument.to_string(),
                    subtask_id: self.subtask_id.clone(),
                };
                self.pending.lock().expect("pending lock poisoned").push(pa);
                Ok(Observation {
                    tool: tool.to_string(),
                    output: format!("[pending approval] {tool} {argument}"),
                    terminal: false,
                })
            }
            _ => {
                // Run the (self-healing) tool. Six-bit enforcement already happened
                // upstream in the Planner's `authorize_and_record`.
                self.inner.run(tool, argument).await
            }
        }
    }
}

/// Cheap, deterministic id for pending approvals (no external `uuid` dependency;
/// mirrors the `fast_id` helper in `craft.rs`).
fn fast_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{CliHost, HostBridge};
    use crate::llm::{ActionPlan, Llm, Thought};
    use crate::permissions::PermissionSet;
    use crate::principal::AuditAction;
    use crate::tool_executor::BasicToolExecutor;
    use crate::validator::BasicValidator;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    fn bridge() -> Arc<HostBridge> {
        Arc::new(HostBridge::new(Arc::new(CliHost::new())))
    }

    // --- LLM doubles -------------------------------------------------------

    /// Returns a fixed numbered subtask list (used for decomposition tests).
    struct DecomposingLlm;
    #[async_trait]
    impl Llm for DecomposingLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "1. explore the repo\n2. write the feature\n3. run the tests".into(),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "finish".into(),
                argument: String::new(),
            })
        }
    }

    /// Immediately finishes (terminal success).
    struct FinishLlm;
    #[async_trait]
    impl Llm for FinishLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought { text: "done".into() })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "finish".into(),
                argument: String::new(),
            })
        }
    }

    /// Always inspects (never reaches a terminal).
    struct InspectLlm;
    #[async_trait]
    impl Llm for InspectLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought { text: "inspecting".into() })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "inspect".into(),
                argument: "state".into(),
            })
        }
    }

    /// Always plans a `commit` action (gated by the approval gate).
    struct CommitLlm;
    #[async_trait]
    impl Llm for CommitLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought { text: "committing".into() })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "commit".into(),
                argument: "all".into(),
            })
        }
    }

    // --- Decomposer doubles -----------------------------------------------

    struct OneSub;
    #[async_trait]
    impl GoalDecomposer for OneSub {
        async fn decompose(&self, _goal: &str) -> Vec<SubTask> {
            vec![SubTask::new("st-1", "do it")]
        }
    }

    struct OneCommit;
    #[async_trait]
    impl GoalDecomposer for OneCommit {
        async fn decompose(&self, _goal: &str) -> Vec<SubTask> {
            vec![SubTask::new("st-1", "commit the change")]
        }
    }

    struct FiveSub;
    #[async_trait]
    impl GoalDecomposer for FiveSub {
        async fn decompose(&self, _goal: &str) -> Vec<SubTask> {
            (1..=5)
                .map(|i| SubTask::new(&format!("st-{i}"), &format!("step {i}")))
                .collect()
        }
    }

    /// Tool double that records every run (used to assert gating behavior).
    struct RecordingTool {
        runs: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl ToolExecutor for RecordingTool {
        async fn run(&self, _tool: &str, argument: &str) -> anyhow::Result<Observation> {
            self.runs.lock().unwrap().push(argument.to_string());
            Ok(Observation {
                tool: _tool.into(),
                output: "done".into(),
                terminal: false,
            })
        }
    }

    fn base_quest(
        decomposer: Arc<dyn GoalDecomposer>,
        llm: Arc<dyn Llm>,
        config: QuestConfig,
    ) -> Quest {
        Quest::new(
            decomposer,
            llm,
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(config.max_steps)),
            bridge(),
            config,
        )
    }

    #[test]
    fn parse_subtasks_extracts_numbered() {
        let st = parse_subtasks("1. alpha\n2. beta\n3. gamma", "goal");
        assert_eq!(st.len(), 3);
        assert_eq!(st[0].description, "alpha");
        assert_eq!(st[2].description, "gamma");
        // No numbered lines -> single subtask equal to the goal.
        let single = parse_subtasks("just text", "do it");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].description, "do it");
    }

    #[tokio::test]
    async fn decomposition_returns_fixed_list() {
        let dec = LlmGoalDecomposer::new(Arc::new(DecomposingLlm));
        let subs = dec.decompose("build a feature").await;
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0].description, "explore the repo");
        assert_eq!(subs[1].description, "write the feature");
        assert_eq!(subs[2].description, "run the tests");
    }

    #[tokio::test]
    async fn reuses_planner_for_subtask_execution() {
        // One subtask; InspectLlm never finishes, so the Planner loop runs to its
        // step budget and the subtask is marked Failed. The fact that the Planner
        // loop executed (and reached budget) proves Quest reused the existing
        // Planner rather than reimplementing the ReAct loop.
        let mut cfg = QuestConfig::default();
        cfg.max_steps = 3;
        cfg.max_subtasks = 4;
        cfg.auto_commit = true;
        let quest = base_quest(Arc::new(OneSub), Arc::new(InspectLlm), cfg);
        let report = quest.run("goal").await.unwrap();
        assert_eq!(report.subtasks.len(), 1);
        assert_eq!(report.failures, 1);
        assert_eq!(report.subtasks[0].status, SubTaskStatus::Failed);
    }

    #[tokio::test]
    async fn auto_commit_false_collects_pending_instead_of_running() {
        let runs = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = QuestConfig::default();
        cfg.max_steps = 1;
        cfg.auto_commit = false;
        let quest = Quest::new(
            Arc::new(OneCommit),
            Arc::new(CommitLlm),
            Arc::new(RecordingTool {
                runs: runs.clone(),
            }),
            Arc::new(BasicValidator::new(cfg.max_steps)),
            bridge(),
            cfg,
        );
        let report = quest.run("goal").await.unwrap();
        // The risky `commit` action was NOT executed (gated).
        assert_eq!(runs.lock().unwrap().len(), 0);
        // It was collected as a pending approval instead.
        assert!(report.pending_approvals.len() >= 1);
        assert_eq!(report.pending_approvals[0].tool, "commit");
        assert_eq!(report.pending_approvals[0].subtask_id, "st-1");
    }

    #[tokio::test]
    async fn auto_commit_true_runs_autonomously() {
        let runs = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = QuestConfig::default();
        cfg.max_steps = 1;
        cfg.auto_commit = true;
        let quest = Quest::new(
            Arc::new(OneCommit),
            Arc::new(CommitLlm),
            Arc::new(RecordingTool {
                runs: runs.clone(),
            }),
            Arc::new(BasicValidator::new(cfg.max_steps)),
            bridge(),
            cfg,
        );
        let report = quest.run("goal").await.unwrap();
        // The risky `commit` action executed autonomously.
        assert!(runs.lock().unwrap().len() >= 1);
        // No pending approvals were collected.
        assert_eq!(report.pending_approvals.len(), 0);
    }

    #[tokio::test]
    async fn budget_caps_subtasks_and_skips_overflow() {
        let mut cfg = QuestConfig::default();
        cfg.max_subtasks = 2;
        cfg.max_steps = 1;
        cfg.auto_commit = true;
        // Decomposer returns 5 subtasks; only 2 should run, the other 3 Skipped.
        let quest = base_quest(Arc::new(FiveSub), Arc::new(FinishLlm), cfg);
        let report = quest.run("goal").await.unwrap();
        assert_eq!(report.subtasks.len(), 5);
        let skipped = report
            .subtasks
            .iter()
            .filter(|s| s.status == SubTaskStatus::Skipped)
            .count();
        assert_eq!(skipped, 3);
        // The 2 executed subtasks reached a terminal `finish` -> success.
        assert_eq!(report.successes, 2);
        assert_eq!(report.failures, 0);
    }

    // --- v1.5 (T15) QA gap: the six-bit mask must STILL be enforced inside the
    // autonomous Quest loop. The engineer's gate tests cover `auto_commit`
    // (collect-pending vs run), but never assert that a principal LACKING a bit
    // is denied at the Planner action boundary within a Quest subtask. This is
    // the single most safety-critical property of T15 ("autonomous != bypass").

    /// LLM double that always plans `modify` (a privileged Modify-bit action).
    struct ModifyLlm;
    #[async_trait]
    impl Llm for ModifyLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought { text: "modifying".into() })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "modify".into(),
                argument: "apply patch".into(),
            })
        }
    }

    struct OneModify;
    #[async_trait]
    impl GoalDecomposer for OneModify {
        async fn decompose(&self, _goal: &str) -> Vec<SubTask> {
            vec![SubTask::new("st-1", "apply the edit")]
        }
    }

    #[tokio::test]
    async fn six_bit_mask_enforced_in_quest_subtask() {
        // Principal with ONLY the Read bit: a `modify` action planned inside a
        // Quest subtask must be DENIED at the Planner action boundary (the same
        // six-bit check v1.0A enforces), never executed, and the subtask must be
        // reported Failed. This proves autonomy does not bypass permission.
        let principal = Principal::new(
            "single",
            "tester",
            PermissionSet::empty().grant(Permission::Read),
        );
        let audit = Arc::new(MockAuditSink::new());
        let metrics = Arc::new(Metrics::new());
        let runs = Arc::new(Mutex::new(Vec::new()));

        let mut cfg = QuestConfig::default();
        cfg.max_steps = 2;
        cfg.max_subtasks = 2;
        cfg.auto_commit = true; // even fully autonomous, the bit still gates

        let quest = Quest::new(
            Arc::new(OneModify),
            Arc::new(ModifyLlm),
            Arc::new(RecordingTool {
                runs: runs.clone(),
            }),
            Arc::new(BasicValidator::new(cfg.max_steps)),
            bridge(),
            cfg,
        )
        .with_governance(principal, audit.clone(), metrics.clone());

        let report = quest.run("goal").await.unwrap();
        // The privileged `modify` action was DENIED at the boundary, so the tool
        // was never reached.
        assert_eq!(
            runs.lock().unwrap().len(),
            0,
            "denied action must not be executed inside a Quest subtask"
        );
        // A denied Modify audit must have been recorded by the Planner.
        assert!(
            audit.count_denied(AuditAction::Modify) >= 1,
            "six-bit mask must deny Modify inside Quest subtask"
        );
        // No GRANTED Modify audit should exist.
        assert_eq!(
            audit.count_action(AuditAction::Modify),
            audit.count_denied(AuditAction::Modify)
        );
        // auto_commit=true => no pending approvals collected.
        assert_eq!(report.pending_approvals.len(), 0);
        // Subtask never reached a terminal `finish` -> reported Failed.
        assert_eq!(report.failures, 1);
        assert_eq!(report.subtasks[0].status, SubTaskStatus::Failed);
    }

    // --- v1.5 (T16) QA gap: a tripped self-heal circuit breaker must surface as
    // a Failed subtask in the Quest report (no runaway / Doom loop). The
    // `SelfHeal` unit tests cover the breaker in isolation, but never that the
    // breaker's `Err` propagates up through QuestToolExecutor -> Planner ->
    // Quest and is reported as a failure rather than silently retried forever.

    /// Tool double that fails whenever the argument is in `fail_on`.
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
                    terminal: true,
                })
            }
        }
    }

    /// LLM double: always plans `sh always` (which fails), and for every
    /// self-heal repair prompt returns `PATCH: always` (which also fails), so
    /// the SelfHeal circuit breaker is guaranteed to trip.
    struct PatchAlwaysLlm;
    #[async_trait]
    impl Llm for PatchAlwaysLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "PATCH: always".into(),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "sh".into(),
                argument: "always".into(),
            })
        }
    }

    #[tokio::test]
    async fn self_heal_circuit_breaker_reported_as_failed_in_quest() {
        let mut fail_on = HashSet::new();
        fail_on.insert("always".into());
        let runs = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(FlakyTool {
            fail_on,
            runs: runs.clone(),
        });

        let mut cfg = QuestConfig::default();
        cfg.max_steps = 1;
        cfg.max_subtasks = 1;
        cfg.auto_commit = true;
        cfg.max_repair_attempts = 2; // 1 first try + 2 repairs = 3 tool runs
        cfg.subtask_max_retries = 0; // exactly one subtask attempt

        let quest = Quest::new(
            Arc::new(OneSub),
            Arc::new(PatchAlwaysLlm),
            tools,
            Arc::new(BasicValidator::new(cfg.max_steps)),
            bridge(),
            cfg,
        );
        let report = quest.run("goal").await.unwrap();
        // The breaker tripped; the subtask must be reported Failed (not silently
        // retried forever — the Doom-loop guard).
        assert_eq!(report.subtasks.len(), 1);
        assert_eq!(report.subtasks[0].status, SubTaskStatus::Failed);
        assert_eq!(report.failures, 1);
        // 1 (first attempt) + 2 (repair attempts) = 3 tool runs before giving up.
        assert_eq!(runs.lock().unwrap().len(), 3);
        // No approvals collected (auto_commit true; action failed pre-result).
        assert_eq!(report.pending_approvals.len(), 0);
    }
}
