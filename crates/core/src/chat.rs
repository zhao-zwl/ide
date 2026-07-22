//! Chat capability (T06, F2): multi-turn conversation with tool-call suggestions.
//!
//! Reuses the existing [`Llm`] trait — the very same one the ReAct
//! [`Planner`](crate::planner::Planner) drives — so chat and the agent share a
//! single reasoning core. Supports `@file`/`@symbol` attachment parsing, a
//! rolling multi-turn history bound to a token budget, and both a non-streaming
//! [`ChatEngine::reply`] and a streaming skeleton [`ChatEngine::reply_stream`].
//!
//! The gRPC surface is the already-frozen `AgentService.Chat`
//! (proto `ChatRequest` -> stream `ChatMessage`); this module is the Core-side
//! implementation wired into `agent.rs`.

use crate::audit::{AuditEvent, AuditSink, MockAuditSink};
use crate::context_manager::ContextManager;
use crate::llm::{ActionPlan, Llm, Thought};
use crate::metrics::Metrics;
use crate::permissions::{Permission, PermissionSet};
use crate::principal::{AuditAction, Principal, check_permission};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::Instrument;

/// A parsed attachment reference carried in a `ChatRequest.attachments` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// `@file:<path>` — inject the referenced file as context.
    File(String),
    /// `@symbol:<name>` — inject a symbol's definition as context.
    Symbol(String),
    /// Anything else is treated as a raw literal reference.
    Raw(String),
}

/// Parse raw attachment strings into typed [`Attachment`]s. Pure; no I/O.
///
/// Mirrors the `@file`/`@symbol` reference convention from the v0.5 plan (T05/
/// T06 acceptance: "`@file`/`@symbol` 解析").
pub fn parse_attachments(raw: &[String]) -> Vec<Attachment> {
    raw.iter()
        .map(|s| {
            if let Some(p) = s.strip_prefix("@file:") {
                Attachment::File(p.to_string())
            } else if let Some(p) = s.strip_prefix("@symbol:") {
                Attachment::Symbol(p.to_string())
            } else {
                Attachment::Raw(s.clone())
            }
        })
        .collect()
}

/// A tool the Agent suggests the user (or itself) run next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSuggestion {
    pub tool: String,
    pub argument: String,
    pub rationale: String,
}

/// One exchanged chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

/// Rolling multi-turn chat state, bound to a token budget.
///
/// Reuses [`ContextManager`] for the working memory so chat inherits the same
/// token-budget trimming used by the ReAct loop (T05 wiring).
#[derive(Debug, Default)]
pub struct ChatSession {
    history: VecDeque<ChatTurn>,
    ctx: ContextManager,
    max_turns: usize,
    budget_tokens: usize,
}

impl ChatSession {
    /// Create an empty session. `budget_tokens` caps the rolling working memory;
    /// `max_turns` caps the in-history turn count (oldest turns are evicted).
    pub fn new(budget_tokens: usize, max_turns: usize) -> Self {
        Self {
            history: VecDeque::new(),
            ctx: ContextManager::with_budget(max_turns * 4, budget_tokens),
            max_turns: max_turns.max(1),
            budget_tokens: budget_tokens.max(1),
        }
    }

    /// Read-only view of the conversation history.
    pub fn history(&self) -> &VecDeque<ChatTurn> {
        &self.history
    }

    /// Push a user turn into the session.
    pub fn push_user(&mut self, content: &str) {
        self.push("user", content);
    }

    /// Push an assistant turn into the session.
    pub fn push_assistant(&mut self, content: &str) {
        self.push("assistant", content);
    }

    fn push(&mut self, role: &str, content: &str) {
        self.history.push_back(ChatTurn {
            role: role.to_string(),
            content: content.to_string(),
        });
        self.ctx.push(content);
        if self.history.len() > self.max_turns {
            self.history.pop_front();
        }
        self.ctx.budget_trim();
    }

    /// Compose the prompt the LLM sees: latest attachments + user message +
    /// rolling working memory (history + retrieved context from the manager).
    pub fn compose_prompt(&self, user_msg: &str, attachments: &[Attachment]) -> String {
        let mut p = String::from("=== Chat ===\n");
        for a in attachments {
            match a {
                Attachment::File(f) => p.push_str(&format!("@file:{f}\n")),
                Attachment::Symbol(s) => p.push_str(&format!("@symbol:{s}\n")),
                Attachment::Raw(r) => p.push_str(&format!("@raw:{r}\n")),
            }
        }
        p.push_str(&format!("user: {user_msg}\n"));
        p.push_str("=== History + context ===\n");
        p.push_str(&self.ctx.build_prompt());
        p
    }
}

/// Chat engine: turns a user message into a reply plus tool suggestions.
pub struct ChatEngine {
    llm: Arc<dyn Llm>,
    perms: PermissionSet,
    /// v1.0 — security identity authorizing the `Generate` action (T11).
    principal: Principal,
    /// v1.0 — append-only audit sink (T11).
    audit: Arc<dyn AuditSink>,
    /// v1.0 — observability counters (T14).
    metrics: Arc<Metrics>,
}

// `Clone` is cheap (all collaborators are `Arc`/`Copy`); it lets the gRPC server
// build a per-request chat engine with a freshly authenticated [`Principal`] via
// `with_governance` (T12 SSO).
impl Clone for ChatEngine {
    fn clone(&self) -> Self {
        Self {
            llm: self.llm.clone(),
            perms: self.perms,
            principal: self.principal.clone(),
            audit: self.audit.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl ChatEngine {
    /// Build a chat engine around an LLM and a six-bit permission set.
    pub fn new(llm: Arc<dyn Llm>, perms: PermissionSet) -> Self {
        Self {
            llm,
            perms,
            principal: Principal::from_mask("single", "chat", perms.mask()),
            audit: Arc::new(MockAuditSink::new()),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Override the governance triple (principal / audit sink / metrics).
    pub fn with_governance(
        mut self,
        principal: Principal,
        audit: Arc<dyn AuditSink>,
        metrics: Arc<Metrics>,
    ) -> Self {
        self.perms = principal.perms;
        self.principal = principal;
        self.audit = audit;
        self.metrics = metrics;
        self
    }

    /// Expose the active permission set (used by the server to gate replies).
    pub fn permissions(&self) -> PermissionSet {
        self.perms
    }

    /// Non-streaming reply.
    ///
    /// Enforces the `Generate` permission bit (reused from the six-bit mask),
    /// then asks the shared LLM for a thought and a planned action. The planned
    /// action becomes a single [`ToolSuggestion`], demonstrating the
    /// request -> thought -> action -> tool-suggestion chain reused from the
    /// ReAct Planner. The user message and assistant reply are recorded in the
    /// session for multi-turn continuity.
    pub async fn reply(
        &self,
        session: &mut ChatSession,
        user_msg: &str,
        attachments: &[Attachment],
    ) -> anyhow::Result<ChatReply> {
        // T11: chat is a `Generate` action — enforce the bit at the boundary.
        if let Err(e) = check_permission(&self.principal, Permission::Generate) {
            self.metrics.inc_denials();
            self.audit(
                AuditAction::Generate,
                false,
                json!({ "reason": e.to_string(), "user_msg": user_msg }),
            )
            .await;
            return Err(e.into());
        }
        self.audit(
            AuditAction::Generate,
            true,
            json!({ "user_msg": user_msg, "attachments": attachments.len() }),
        )
        .await;

        let span = tracing::info_span!("chat.reply");
        let prompt = session.compose_prompt(user_msg, attachments);
        let thought: Thought = self.llm.think(&prompt).instrument(span.clone()).await?;
        let plan: ActionPlan = self.llm.plan_action(&thought).instrument(span).await?;
        self.metrics.inc_llm_calls();

        let content = format!("(assistant) {}", thought.text);
        // 真实 LLM（OllamaLlm / OpenAiLlm）在未给出 `ACTION:` 指令时返回空 tool，
        // 此时不展示工具建议，避免在前端渲染出无意义的空建议行。MockLlm 等仍正常给出。
        let suggestions = if plan.tool.is_empty() {
            Vec::new()
        } else {
            vec![ToolSuggestion {
                tool: plan.tool.clone(),
                argument: plan.argument.clone(),
                rationale: format!("based on: {}", thought.text),
            }]
        };

        session.push_user(user_msg);
        session.push_assistant(&content);
        Ok(ChatReply {
            content,
            suggestions,
        })
    }

    /// Append an audit event (fire-and-forget; errors are logged, never fatal).
    async fn audit(&self, action: AuditAction, granted: bool, payload: serde_json::Value) {
        let ev = AuditEvent::new(&self.principal, action, granted, payload);
        self.metrics.inc_audit_events();
        if let Err(e) = self.audit.record(&ev).await {
            tracing::warn!("chat audit record failed: {e}");
        }
    }

    /// Streaming skeleton: split a finished reply into incremental chunks on
    /// sentence-ish boundaries so the host can render progressively. Pure with
    /// respect to the model — a real implementation would yield token-by-token
    /// deltas straight from the LLM stream instead of post-splitting.
    pub fn reply_stream(&self, full: &str) -> impl Iterator<Item = String> {
        let bytes = full.as_bytes();
        let mut chunks: Vec<String> = Vec::new();
        let mut start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'.' || b == b'!' || b == b'?' {
                let piece = full[start..=i].to_string();
                if !piece.trim().is_empty() {
                    chunks.push(piece);
                }
                start = i + 1;
            }
        }
        if start < full.len() {
            let tail = full[start..].trim().to_string();
            if !tail.is_empty() {
                chunks.push(tail);
            }
        }
        chunks.into_iter()
    }
}

/// The product of [`ChatEngine::reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub content: String,
    pub suggestions: Vec<ToolSuggestion>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Deterministic LLM double: fixed thought, always plans `read_file`.
    struct ScriptLlm;
    #[async_trait]
    impl Llm for ScriptLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "I will read the entrypoint".to_string(),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "read_file".to_string(),
                argument: "src/main.rs".to_string(),
            })
        }
    }

    #[test]
    fn parses_attachments() {
        let a = parse_attachments(&[
            "@file:src/main.rs".to_string(),
            "@symbol:retry".to_string(),
            "loose reference".to_string(),
        ]);
        assert_eq!(
            a,
            vec![
                Attachment::File("src/main.rs".to_string()),
                Attachment::Symbol("retry".to_string()),
                Attachment::Raw("loose reference".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn reply_yields_content_and_suggestion() {
        let engine = ChatEngine::new(Arc::new(ScriptLlm), PermissionSet::all());
        let mut session = ChatSession::new(2048, 16);
        let reply = engine
            .reply(
                &mut session,
                "where is main?",
                &[Attachment::File("src/main.rs".to_string())],
            )
            .await
            .unwrap();
        assert!(reply.content.contains("entrypoint"));
        assert_eq!(reply.suggestions.len(), 1);
        assert_eq!(reply.suggestions[0].tool, "read_file");
        assert_eq!(reply.suggestions[0].argument, "src/main.rs");
        // Multi-turn: the user message is recorded in history.
        assert!(session
            .history()
            .iter()
            .any(|t| t.role == "user" && t.content == "where is main?"));
    }

    #[tokio::test]
    async fn reply_requires_generate_permission() {
        let engine = ChatEngine::new(Arc::new(ScriptLlm), PermissionSet::empty());
        let mut session = ChatSession::new(2048, 16);
        let err = engine.reply(&mut session, "hi", &[]).await.unwrap_err();
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn stream_splits_on_sentence_boundaries() {
        let engine = ChatEngine::new(Arc::new(ScriptLlm), PermissionSet::all());
        let chunks: Vec<String> =
            engine.reply_stream("First sentence. Second one! Third?").collect();
        assert_eq!(
            chunks,
            vec![
                "First sentence.".to_string(),
                " Second one!".to_string(),
                " Third?".to_string(),
            ]
        );
    }
}
