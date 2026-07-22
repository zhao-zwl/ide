//! Tool executor — runs the actions the Planner selects.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

/// Result of running a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub tool: String,
    pub output: String,
    /// Marks this observation as the terminal one (goal reached).
    pub terminal: bool,
}

/// Executes named tools and returns observations.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn run(&self, tool: &str, argument: &str) -> anyhow::Result<Observation>;
}

/// Built-in executor covering the minimal ReAct tool set.
pub struct BasicToolExecutor;

impl Default for BasicToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl BasicToolExecutor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolExecutor for BasicToolExecutor {
    async fn run(&self, tool: &str, argument: &str) -> anyhow::Result<Observation> {
        let span = tracing::info_span!("tool_executor.run", tool = tool);
        async move {
            match tool {
                "inspect" => Ok(Observation {
                    tool: tool.to_string(),
                    output: format!("inspected {argument}"),
                    terminal: false,
                }),
                "finish" => Ok(Observation {
                    tool: tool.to_string(),
                    output: "task finished".to_string(),
                    terminal: true,
                }),
                other => Ok(Observation {
                    tool: other.to_string(),
                    output: format!("no-op tool: {other}"),
                    terminal: false,
                }),
            }
        }
        .instrument(span)
        .await
    }
}
