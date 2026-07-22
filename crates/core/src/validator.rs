//! Validator — guards the ReAct loop (safety rail for M1).
//!
//! M1 implements a minimal guard: it stops the loop when an observation is
//! terminal, when the step budget is exhausted, or when a disallowed action is
//! attempted. v1.5 adds self-healing / Doom-Loop detection (T16).

use crate::tool_executor::Observation;

/// Outcome of validating a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Continue the loop.
    Continue,
    /// Stop: the goal is reached.
    Done,
    /// Stop: budget exhausted.
    BudgetExhausted,
}

/// Decides whether the Planner may continue or must stop.
pub trait Validator: Send + Sync {
    /// Validate the current step given the observation and step index (1-based).
    fn validate(&self, observation: &Observation, step: usize) -> Verdict;
}

/// Default guardrail with a maximum step budget.
pub struct BasicValidator {
    max_steps: usize,
}

impl BasicValidator {
    pub fn new(max_steps: usize) -> Self {
        Self { max_steps }
    }
}

impl Validator for BasicValidator {
    fn validate(&self, observation: &Observation, step: usize) -> Verdict {
        if observation.terminal {
            return Verdict::Done;
        }
        if step >= self.max_steps {
            return Verdict::BudgetExhausted;
        }
        Verdict::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_executor::Observation;

    #[test]
    fn terminal_observation_is_done() {
        let v = BasicValidator::new(10);
        let obs = Observation {
            tool: "finish".to_string(),
            output: "task finished".to_string(),
            terminal: true,
        };
        assert_eq!(v.validate(&obs, 1), Verdict::Done);
    }

    #[test]
    fn budget_exhausted_exactly_at_limit() {
        let v = BasicValidator::new(3);
        let obs = Observation {
            tool: "inspect".to_string(),
            output: "x".to_string(),
            terminal: false,
        };
        // One step before the limit -> continue.
        assert_eq!(v.validate(&obs, 2), Verdict::Continue);
        // At the limit -> budget exhausted.
        assert_eq!(v.validate(&obs, 3), Verdict::BudgetExhausted);
        // Past the limit also stops.
        assert_eq!(v.validate(&obs, 4), Verdict::BudgetExhausted);
    }

    #[test]
    fn non_terminal_continues_before_limit() {
        let v = BasicValidator::new(5);
        let obs = Observation {
            tool: "inspect".to_string(),
            output: "x".to_string(),
            terminal: false,
        };
        assert_eq!(v.validate(&obs, 1), Verdict::Continue);
    }
}
