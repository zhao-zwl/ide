//! Planner — the ReAct closed-loop skeleton (T01 minimal) + v1.0 hardening.
//!
//! Sequence (matches the four-layer decision Core):
//! ```text
//!   Thought (LLM) -> ActionPlan (LLM) -> Observation (ToolExecutor)
//!   -> ContextManager.update -> Validator -> repeat until Done / Budget
//! ```
//!
//! The Planner is host-agnostic: it only talks to injected traits, so it can be
//! driven by the CLI host stub, a gRPC host, or a unit test alike.
//!
//! v1.0 Stage A (T11 / T14): a [`Principal`] + [`AuditSink`] + [`Metrics`] are
//! threaded into the loop. Before every tool runs, the six-bit right is enforced
//! at the **action boundary** ([`check_permission`]); granted and denied actions
//! are both written to the append-only audit log. LLM calls and tool runs emit
//! `tracing` spans, and counter/histogram metrics are updated on the hot path.

use crate::audit::{AuditEvent, AuditSink, MockAuditSink};
use crate::context_manager::{ContextChunk, ContextManager, ContextSource, Priority};
use crate::host::provider::{HostEvent, TextEdit};
use crate::host::HostBridge;
use crate::llm::{Llm, MockLlm};
use crate::metrics::Metrics;
use crate::permissions::Permission;
use crate::principal::{AuditAction, Principal, action_permission, check_permission};
use crate::tool_executor::{BasicToolExecutor, Observation, ToolExecutor};
use crate::validator::{BasicValidator, Validator, Verdict};
use serde_json::json;
use std::sync::Arc;
use tracing::Instrument;

/// A single recorded step of the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub thought: String,
    pub action: String,
    pub observation: String,
}

/// The full trace of a run, returned to the caller (and serialized over the
/// AgentService stream).
#[derive(Debug, Default)]
pub struct RunTrace {
    pub steps: Vec<Step>,
    pub final_answer: String,
}

/// ReAct planner wiring together the five decision-Core components.
pub struct Planner {
    llm: Arc<dyn Llm>,
    tools: Arc<dyn ToolExecutor>,
    validator: Arc<dyn Validator>,
    bridge: Arc<HostBridge>,
    max_steps: usize,
    /// v1.0 — security identity authorizing every action (T11).
    principal: Principal,
    /// v1.0 — append-only audit sink (T11).
    audit: Arc<dyn AuditSink>,
    /// v1.0 — observability counters / histograms (T14).
    metrics: Arc<Metrics>,
}

// `Clone` is cheap (all heavy collaborators are `Arc`); it lets the gRPC server
// build a per-request planner with a freshly authenticated [`Principal`] via
// `with_governance` without reconstructing the reasoning core (T12 SSO).
impl Clone for Planner {
    fn clone(&self) -> Self {
        Self {
            llm: self.llm.clone(),
            tools: self.tools.clone(),
            validator: self.validator.clone(),
            bridge: self.bridge.clone(),
            max_steps: self.max_steps,
            principal: self.principal.clone(),
            audit: self.audit.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl Planner {
    /// Assemble a planner from its collaborators.
    pub fn new(
        llm: Arc<dyn Llm>,
        tools: Arc<dyn ToolExecutor>,
        validator: Arc<dyn Validator>,
        bridge: Arc<HostBridge>,
        max_steps: usize,
    ) -> Self {
        Self {
            llm,
            tools,
            validator,
            bridge,
            max_steps,
            // Defaults: a fully-privileged "core" principal with an in-memory
            // audit sink + empty metrics. Override with `with_governance`.
            principal: Principal::all("single", "core"),
            audit: Arc::new(MockAuditSink::new()),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Override the governance triple (principal / audit sink / metrics). Used by
    /// the server to inject the configured principal and a Postgres audit sink.
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

    /// Convenience constructor: build the whole M1 default stack around a host
    /// bridge. Demonstrates the ReAct wiring end-to-end.
    pub fn with_defaults(bridge: Arc<HostBridge>, max_steps: usize) -> Self {
        Self::new(
            Arc::new(MockLlm::new()),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(max_steps)),
            bridge,
            max_steps,
        )
    }

    /// Access the LLM collaborator (shared with the ChatEngine for T06 reuse).
    pub fn llm(&self) -> Arc<dyn Llm> {
        self.llm.clone()
    }

    /// Access the tool executor collaborator. Reused by the autonomous
    /// [`Quest`](crate::quest::Quest) (T15) as the base (non-self-healing)
    /// executor that the [`SelfHeal`](crate::self_heal::SelfHeal) wrapper wraps.
    pub fn tools(&self) -> Arc<dyn ToolExecutor> {
        self.tools.clone()
    }

    /// Access the validator collaborator. Reused by the Quest (T15) so each
    /// subtask loop is capped by the same `BudgetExhausted` semantics.
    pub fn validator(&self) -> Arc<dyn Validator> {
        self.validator.clone()
    }

    /// Access the host bridge collaborator. Reused by the Quest (T15) so subtask
    /// edits/observations drive the same host as the main ReAct loop.
    pub fn bridge(&self) -> Arc<HostBridge> {
        self.bridge.clone()
    }

    /// Enforce the six-bit right for `tool` at the action boundary, recording an
    /// audit entry (granted or denied). Returns `Err` when the action is
    /// privileged and the principal lacks the bit; `Ok(())` otherwise (including
    /// for non-privileged tools such as `inspect`/`finish`).
    async fn authorize_and_record(&self, tool: &str) -> Result<(), crate::permissions::PermissionDenied> {
        match action_permission(tool) {
            None => Ok(()),
            Some(required) => match check_permission(&self.principal, required) {
                Ok(()) => {
                    self.record_audit(
                        AuditAction::from_permission(required),
                        true,
                        json!({ "tool": tool }),
                    )
                    .await;
                    Ok(())
                }
                Err(e) => {
                    self.metrics.inc_denials();
                    self.record_audit(
                        AuditAction::from_permission(required),
                        false,
                        json!({ "tool": tool, "reason": e.to_string() }),
                    )
                    .await;
                    Err(e)
                }
            },
        }
    }

    /// Append an audit event (fire-and-forget; errors are logged, never fatal).
    async fn record_audit(&self, action: AuditAction, granted: bool, payload: serde_json::Value) {
        let ev = AuditEvent::new(&self.principal, action, granted, payload);
        self.metrics.inc_audit_events();
        if let Err(e) = self.audit.record(&ev).await {
            tracing::warn!("planner audit record failed: {e}");
        }
    }

    /// Run the ReAct loop toward `goal`. Returns the traced steps.
    pub async fn run(&self, goal: &str) -> anyhow::Result<RunTrace> {
        let span = tracing::info_span!("planner.run", goal = goal);
        async move {
            let mut ctx = ContextManager::new(16);
            ctx.push(&format!("goal: {goal}"));

            let mut trace = RunTrace::default();
            for step in 0..self.max_steps {
                self.bridge
                    .emit_event(HostEvent::Status(format!("step {step}: thinking")));

                let context = ctx.snapshot();
                let thought = {
                    let span = tracing::info_span!("planner.think", step = step);
                    self.metrics.inc_llm_calls();
                    self.llm.think(&context).instrument(span).await?
                };
                let plan = {
                    let span = tracing::info_span!("planner.plan", step = step);
                    self.llm.plan_action(&thought).instrument(span).await?
                };

                self.bridge.emit_event(HostEvent::Status(format!(
                    "action={} arg={}",
                    plan.tool, plan.argument
                )));

                // T11: enforce the six-bit right at the action boundary, then run.
                let (obs, authorized) = match self.authorize_and_record(&plan.tool).await {
                    Ok(()) => {
                        self.metrics.inc_tool_calls();
                        let o = self.tools.run(&plan.tool, &plan.argument).await?;
                        (o, true)
                    }
                    Err(_denied) => {
                        self.bridge
                            .emit_event(HostEvent::Status(format!("permission denied: {}", plan.tool)));
                        tracing::warn!(tool = %plan.tool, "action denied by six-bit mask");
                        (
                            Observation {
                                tool: plan.tool.clone(),
                                output: format!("[denied] action `{}` not permitted", plan.tool),
                                terminal: false,
                            },
                            false,
                        )
                    }
                };
                ctx.push(&obs.output);

                // Only apply the edit when the `modify` action was authorized.
                if authorized && plan.tool == "modify" {
                    let _ = self
                        .bridge
                        .apply_edit(TextEdit {
                            document_uri: "in-memory://plan".to_string(),
                            old_text: String::new(),
                            new_text: obs.output.clone(),
                        })
                        .await;
                }

                trace.steps.push(Step {
                    thought: thought.text.clone(),
                    action: plan.tool.clone(),
                    observation: obs.output.clone(),
                });

                match self.validator.validate(&obs, step + 1) {
                    Verdict::Continue => continue,
                    Verdict::Done => {
                        trace.final_answer = obs.output;
                        break;
                    }
                    Verdict::BudgetExhausted => {
                        trace.final_answer = "budget exhausted".to_string();
                        break;
                    }
                }
            }
            Ok(trace)
        }
        .instrument(span)
        .await
    }

    /// Run the ReAct loop toward `goal`, seeding the context manager with
    /// explicit multi-source [`ContextChunk`]s (T05 wiring). Demonstrates the
    /// context-engineering subsystem feeding the Planner: chunks are added, the
    /// window is budget-trimmed, and `build_prompt` composes the LLM context.
    pub async fn run_with_context(
        &self,
        goal: &str,
        sources: Vec<ContextChunk>,
    ) -> anyhow::Result<RunTrace> {
        let span = tracing::info_span!("planner.run_with_context", goal = goal);
        async move {
            let mut ctx = ContextManager::with_budget(self.max_steps, 4096);
            for s in sources {
                ctx.add_chunk(s);
            }
            ctx.push(&format!("goal: {goal}"));

            let mut trace = RunTrace::default();
            for step in 0..self.max_steps {
                ctx.budget_trim();
                let context = ctx.build_prompt();
                let thought = {
                    let span = tracing::info_span!("planner.think", step = step);
                    self.metrics.inc_llm_calls();
                    self.llm.think(&context).instrument(span).await?
                };
                let plan = {
                    let span = tracing::info_span!("planner.plan", step = step);
                    self.llm.plan_action(&thought).instrument(span).await?
                };

                self.bridge.emit_event(HostEvent::Status(format!(
                    "action={} arg={}",
                    plan.tool, plan.argument
                )));

                // T11: enforce the six-bit right at the action boundary, then run.
                let (obs, authorized) = match self.authorize_and_record(&plan.tool).await {
                    Ok(()) => {
                        self.metrics.inc_tool_calls();
                        let o = self.tools.run(&plan.tool, &plan.argument).await?;
                        (o, true)
                    }
                    Err(_denied) => {
                        self.bridge
                            .emit_event(HostEvent::Status(format!("permission denied: {}", plan.tool)));
                        tracing::warn!(tool = %plan.tool, "action denied by six-bit mask");
                        (
                            Observation {
                                tool: plan.tool.clone(),
                                output: format!("[denied] action `{}` not permitted", plan.tool),
                                terminal: false,
                            },
                            false,
                        )
                    }
                };
                ctx.push(&obs.output);

                // Only apply the edit when the `modify` action was authorized.
                if authorized && plan.tool == "modify" {
                    let _ = self
                        .bridge
                        .apply_edit(TextEdit {
                            document_uri: "in-memory://plan".to_string(),
                            old_text: String::new(),
                            new_text: obs.output.clone(),
                        })
                        .await;
                }

                trace.steps.push(Step {
                    thought: thought.text.clone(),
                    action: plan.tool.clone(),
                    observation: obs.output.clone(),
                });

                match self.validator.validate(&obs, step + 1) {
                    Verdict::Continue => continue,
                    Verdict::Done => {
                        trace.final_answer = obs.output;
                        break;
                    }
                    Verdict::BudgetExhausted => {
                        trace.final_answer = "budget exhausted".to_string();
                        break;
                    }
                }
            }
            Ok(trace)
        }
        .instrument(span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_manager::{ContextChunk, ContextSource, Priority};
    use crate::host::{CliHost, HostBridge};
    use crate::llm::{ActionPlan, Llm, Thought};
    use crate::permissions::{Permission, PermissionSet};
    use crate::tool_executor::BasicToolExecutor;
    use crate::validator::BasicValidator;
    use crate::audit::MockAuditSink;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Test double: decides the goal is already satisfied and finishes on the
    /// first action. Exercises the `Verdict::Done` termination path.
    struct FinishLlm;
    #[async_trait]
    impl Llm for FinishLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "Goal satisfied; finalizing.".to_string(),
            })
        }
        async fn plan_action(&self, _thought: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "finish".to_string(),
                argument: String::new(),
            })
        }
    }

    /// Test double: always inspects, never finishes. Exercises the
    /// `Verdict::BudgetExhausted` termination path.
    struct InspectLlm;
    #[async_trait]
    impl Llm for InspectLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "Inspecting context to progress toward goal.".to_string(),
            })
        }
        async fn plan_action(&self, _thought: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "inspect".to_string(),
                argument: "current_state".to_string(),
            })
        }
    }

    /// Test double: always plans a `modify` action (a privileged Modify bit).
    struct ModifyLlm;
    #[async_trait]
    impl Llm for ModifyLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "I will modify the file.".to_string(),
            })
        }
        async fn plan_action(&self, _thought: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "modify".to_string(),
                argument: "apply patch".to_string(),
            })
        }
    }

    fn bridge() -> Arc<HostBridge> {
        Arc::new(HostBridge::new(Arc::new(CliHost::new())))
    }

    #[tokio::test]
    async fn finishes_when_tool_is_terminal() {
        let planner = Planner::new(
            Arc::new(FinishLlm),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(8)),
            bridge(),
            8,
        );
        let trace = planner.run("goal").await.unwrap();
        assert_eq!(trace.final_answer, "task finished");
        assert_eq!(trace.steps.len(), 1);
        assert_eq!(trace.steps[0].action, "finish");
    }

    #[tokio::test]
    async fn exhausts_budget_without_progress() {
        let planner = Planner::new(
            Arc::new(InspectLlm),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(3)),
            bridge(),
            3,
        );
        let trace = planner.run("goal").await.unwrap();
        assert_eq!(trace.final_answer, "budget exhausted");
        assert_eq!(trace.steps.len(), 3);
        // Every step must be the inspect action (no premature finish).
        for s in &trace.steps {
            assert_eq!(s.action, "inspect");
        }
    }

    #[tokio::test]
    async fn default_stack_runs_headless_and_terminates() {
        // The default stack (CliHost + MockLlm + BasicToolExecutor) must run the
        // full ReAct loop in-process with no UI / no model, ending on budget.
        let planner = Planner::with_defaults(bridge(), 4);
        let trace = planner.run("add retry logic to utils").await.unwrap();
        assert_eq!(trace.final_answer, "budget exhausted");
        assert_eq!(trace.steps.len(), 4);
        // MockLlm emits an inspect action each step.
        for s in &trace.steps {
            assert_eq!(s.action, "inspect");
            assert!(s.observation.starts_with("inspected"));
        }
    }

    #[tokio::test]
    async fn context_sources_feed_the_prompt() {
        // CaptureLlm records the context string it was given so we can assert the
        // multi-source context manager actually injected the source chunk (T05).
        struct CaptureLlm {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait]
        impl Llm for CaptureLlm {
            async fn think(&self, ctx: &str) -> anyhow::Result<Thought> {
                *self.captured.lock().unwrap() = ctx.to_string();
                Ok(Thought {
                    text: "captured".to_string(),
                })
            }
            async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
                Ok(ActionPlan {
                    tool: "finish".to_string(),
                    argument: String::new(),
                })
            }
        }
        let captured = Arc::new(Mutex::new(String::new()));
        let planner = Planner::new(
            Arc::new(CaptureLlm {
                captured: captured.clone(),
            }),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(1)),
            bridge(),
            1,
        );
        let sources = vec![ContextChunk::new(
            ContextSource::OpenFile,
            "src/main.rs",
            "fn main() { retry_on_timeout(); }",
            Priority::High,
        )];
        let _ = planner.run_with_context("goal", sources).await.unwrap();
        let ctx = captured.lock().unwrap();
        assert!(
            ctx.contains("src/main.rs"),
            "open-file source must appear in prompt: {ctx}"
        );
        assert!(
            ctx.contains("retry_on_timeout"),
            "source content must appear in prompt"
        );
    }

    // --- v1.0 Stage A (T11): runtime enforcement + audit at the action boundary.

    #[tokio::test]
    async fn modify_action_denied_when_mask_lacks_modify() {
        // A principal with only Read: a `modify` action must be denied, the edit
        // must NOT be applied, an audit denial must be recorded, and the metrics
        // denial counter must increment.
        let principal = Principal::new(
            "single",
            "tester",
            PermissionSet::empty().grant(Permission::Read),
        );
        let audit = Arc::new(MockAuditSink::new());
        let metrics = Arc::new(Metrics::new());
        let planner = Planner::new(
            Arc::new(ModifyLlm),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(4)),
            bridge(),
            4,
        )
        .with_governance(principal, audit.clone(), metrics.clone());

        let trace = planner.run("goal").await.unwrap();
        // The denied modify action still produces a (denial) observation each
        // step, so the loop runs to budget.
        assert_eq!(trace.final_answer, "budget exhausted");
        // Exactly one denied Modify audit entry per attempted step.
        assert!(audit.count_denied(AuditAction::Modify) >= 1);
        assert_eq!(metrics.denials(), audit.count_denied(AuditAction::Modify) as u64);
        // No granted Modify entry should exist.
        assert_eq!(
            audit.count_action(AuditAction::Modify),
            audit.count_denied(AuditAction::Modify)
        );
    }

    #[tokio::test]
    async fn modify_action_allowed_and_audited_when_mask_has_modify() {
        let principal = Principal::all("single", "tester"); // all six bits
        let audit = Arc::new(MockAuditSink::new());
        let metrics = Arc::new(Metrics::new());
        let planner = Planner::new(
            Arc::new(ModifyLlm),
            Arc::new(BasicToolExecutor::new()),
            Arc::new(BasicValidator::new(2)),
            bridge(),
            2,
        )
        .with_governance(principal, audit.clone(), metrics.clone());

        let _ = planner.run("goal").await.unwrap();
        // Granted Modify audits recorded (the ModifyLlm plans `modify`).
        assert!(audit.count_action(AuditAction::Modify) >= 1);
        assert_eq!(audit.count_denied(AuditAction::Modify), 0);
        // Tool calls counted.
        assert!(metrics.tool_calls() >= 1);
    }
}
