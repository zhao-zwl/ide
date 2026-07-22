//! Self-heal executor — autonomous failure recovery (T16, F3 P1).
//!
//! Wraps a [`ToolExecutor`] (or command run). When the wrapped tool fails, the
//! error text is handed to an [`Llm`] to synthesize a corrected input (a patch /
//! revised command). The corrected input is re-run; this forms the
//! "execute → fail → self-heal → re-execute" loop. A `max_repair_attempts`
//! circuit breaker stops runaway retries: once exceeded, the executor reports
//! the failure instead of looping forever.
//!
//! The same executor is reused by the Quest autonomous agent (T15) so every
//! subtask execution is self-healing. It can also be used standalone:
//! `SelfHeal::run(tool, input, llm)`.

use crate::llm::{Llm, Thought};
use crate::tool_executor::{Observation, ToolExecutor};
use async_trait::async_trait;
use std::sync::Arc;

/// Outcome of a self-healing execution attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    /// The final observation (from the first success, or the last failure's
    /// observation when all attempts failed).
    pub observation: Observation,
    /// Number of repair/retry attempts performed (0 = first try succeeded).
    pub repair_attempts: usize,
    /// Whether a retry succeeded after an initial failure.
    pub healed: bool,
}

/// Self-healing executor.
pub struct SelfHeal {
    executor: Arc<dyn ToolExecutor>,
    max_repair_attempts: usize,
}

impl SelfHeal {
    /// Build a self-healer over `executor`, allowing at most
    /// `max_repair_attempts` repair retries before the circuit breaker trips.
    pub fn new(executor: Arc<dyn ToolExecutor>, max_repair_attempts: usize) -> Self {
        Self {
            executor,
            max_repair_attempts,
        }
    }

    /// Run `tool` with `input`, self-healing on failure using `llm`.
    ///
    /// Matches the spec signature `SelfHeal::run(tool, input, llm)`. The first
    /// attempt runs `input` as-is; on error, `llm` proposes a corrected input
    /// (see [`suggest_patch`]) which is re-run, up to `max_repair_attempts`
    /// times. Returns the successful [`RepairOutcome`], or `Err` once the breaker
    /// trips.
    pub async fn run(
        &self,
        tool: &str,
        input: &str,
        llm: Arc<dyn Llm>,
    ) -> anyhow::Result<RepairOutcome> {
        // First attempt with the original input.
        match self.executor.run(tool, input).await {
            Ok(obs) => Ok(RepairOutcome {
                observation: obs,
                repair_attempts: 0,
                healed: false,
            }),
            Err(first_err) => {
                let mut attempts = 0usize;
                let mut current = input.to_string();
                let mut last_err = first_err;
                while attempts < self.max_repair_attempts {
                    let patch = self
                        .suggest_patch(tool, &current, &last_err, llm.clone())
                        .await?;
                    current = patch;
                    attempts += 1;
                    match self.executor.run(tool, &current).await {
                        Ok(obs) => {
                            return Ok(RepairOutcome {
                                observation: obs,
                                repair_attempts: attempts,
                                healed: true,
                            });
                        }
                        Err(e) => {
                            last_err = e;
                        }
                    }
                }
                // Circuit breaker tripped: give up and surface the last error.
                Err(anyhow::anyhow!(
                    "self-heal exhausted after {attempts} repair attempts for tool `{tool}`: {last_err}"
                ))
            }
        }
    }

    /// Ask the LLM for a corrected input given the failure. Returns the parsed
    /// patch (see [`parse_patch`]).
    async fn suggest_patch(
        &self,
        tool: &str,
        input: &str,
        err: &anyhow::Error,
        llm: Arc<dyn Llm>,
    ) -> anyhow::Result<String> {
        let prompt = format!(
            "The tool `{tool}` failed when run with input `{input}`.\n\
             Error: {err}\n\
             Propose a corrected input that would make the tool succeed. \
             Reply with a single line beginning with PATCH: followed by the corrected input."
        );
        let thought: Thought = llm.think(&prompt).await?;
        Ok(parse_patch(&thought.text))
    }
}

/// Extract the corrected input from an LLM repair thought.
///
/// Accepts either a line prefixed with `PATCH:` (recommended) or a bare
/// corrected command/argument. Leading/trailing whitespace is trimmed. Pure and
/// unit-tested (T16 acceptance: 修复补丁构造逻辑).
pub fn parse_patch(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("PATCH:") {
        return rest.trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("PATCH ") {
        return rest.trim().to_string();
    }
    trimmed.to_string()
}

/// Adapter that turns a [`SelfHeal`] into a [`ToolExecutor`] so it can be slotted
/// into the existing Planner / Quest execution chain. The LLM is captured at
/// construction time (the Planner only knows about `ToolExecutor`).
pub struct SelfHealingExecutor {
    heal: SelfHeal,
    llm: Arc<dyn Llm>,
}

impl SelfHealingExecutor {
    /// Wrap `heal` with a fixed `llm` into a `ToolExecutor`.
    pub fn new(heal: SelfHeal, llm: Arc<dyn Llm>) -> Self {
        Self { heal, llm }
    }
}

#[async_trait]
impl ToolExecutor for SelfHealingExecutor {
    async fn run(&self, tool: &str, argument: &str) -> anyhow::Result<Observation> {
        let outcome = self.heal.run(tool, argument, self.llm.clone()).await?;
        Ok(outcome.observation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ActionPlan, Thought};
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// Tool double: fails (Err) when `input` is in `fail_on`, else succeeds.
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

    /// LLM double: returns `PATCH: <corrected>` for repair prompts.
    struct PatchLlm {
        patch: String,
    }
    #[async_trait]
    impl Llm for PatchLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: format!("PATCH: {}", self.patch),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "noop".into(),
                argument: String::new(),
            })
        }
    }

    #[test]
    fn parse_patch_strips_prefix() {
        assert_eq!(parse_patch("PATCH: rm -rf build"), "rm -rf build");
        assert_eq!(parse_patch("PATCH:  fixed"), "fixed");
        assert_eq!(parse_patch("just the command"), "just the command");
        assert_eq!(parse_patch("  leading/trailing  "), "leading/trailing");
    }

    #[tokio::test]
    async fn first_success_no_repair() {
        let tool = Arc::new(FlakyTool {
            fail_on: HashSet::new(),
            runs: Arc::new(Mutex::new(Vec::new())),
        });
        let heal = SelfHeal::new(tool.clone(), 3);
        let out = heal
            .run("sh", "good", Arc::new(PatchLlm { patch: "good".into() }))
            .await
            .unwrap();
        assert_eq!(out.repair_attempts, 0);
        assert!(!out.healed);
        assert_eq!(tool.runs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fail_then_patch_then_success() {
        let mut fail_on = HashSet::new();
        fail_on.insert("broken".into());
        let tool = Arc::new(FlakyTool {
            fail_on,
            runs: Arc::new(Mutex::new(Vec::new())),
        });
        let heal = SelfHeal::new(tool.clone(), 3);
        let out = heal
            .run("sh", "broken", Arc::new(PatchLlm { patch: "fixed".into() }))
            .await
            .unwrap();
        assert_eq!(out.repair_attempts, 1);
        assert!(out.healed);
        assert_eq!(out.observation.output, "ok: fixed");
        // First attempt (broken) + one healed attempt (fixed) = 2 runs.
        assert_eq!(tool.runs.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn circuit_breaker_trips() {
        let mut fail_on = HashSet::new();
        fail_on.insert("always".into());
        let tool = Arc::new(FlakyTool {
            fail_on,
            runs: Arc::new(Mutex::new(Vec::new())),
        });
        let heal = SelfHeal::new(tool.clone(), 2);
        let res = heal
            .run("sh", "always", Arc::new(PatchLlm { patch: "always".into() }))
            .await;
        assert!(res.is_err());
        // First attempt + 2 repair attempts = 3 runs before the breaker trips.
        assert_eq!(tool.runs.lock().unwrap().len(), 3);
    }
}
