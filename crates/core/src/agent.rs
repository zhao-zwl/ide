//! AgentService — ProtoBus entry point implemented by the Core (T02 / T06 / T11 / T12 / T14).
//!
//! Bridges gRPC requests into the decision Core:
//!   * `NesComplete` streams ghost text produced by the NES probe (T04).
//!   * `RunAgent` streams the ReAct trace (T01).
//!   * `Chat` (T06) streams assistant replies + tool suggestions via the
//!     [`ChatEngine`], reusing the same `Llm` trait the Planner drives.
//!
//! v1.0 Stage A (T11 / T14): every privileged RPC enforces the six-bit mask at
//! the action boundary via [`check_permission`], records an [`AuditEvent`]
//! through the injected [`AuditSink`], bumps [`Metrics`], and emits `tracing`
//! spans/events on the hot path. The admin `/metrics` + `/healthz` endpoints are
//! served on a *separate* TCP port (see [`crate::admin`]) and never touch the
//! frozen gRPC contract.
//!
//! v1.0 Stage B (T12 SSO): each privileged RPC first authenticates the caller
//! from the `Authorization: Bearer <jwt>` header via the configured
//! [`Authenticator`]. The derived [`Principal`] is injected per-request into the
//! Planner / ChatEngine through the existing `with_governance` builder, so the
//! governance chain is identical to v1.0 Stage A. When SSO is disabled the
//! [`NoopAuthenticator`] returns the configured principal (dev / private
//! single-tenant), preserving full backward compatibility (the `demo` path and
//! `default_stack` keep working unchanged).

use crate::audit::{AuditEvent, AuditSink, MockAuditSink, PgAuditSink};
use crate::auth::{Authenticator, Hs256Authenticator, NoopAuthenticator};
use crate::chat::{Attachment, ChatEngine, ChatSession};
use crate::config::{CoreConfig, LlmBackend};
use crate::engineering::ShellTool;
use crate::host::HostBridge;
use crate::llm::{Llm, MockLlm, OllamaLlm, OpenAiLlm};
use crate::metrics::Metrics;
use crate::permissions::PermissionSet;
use crate::planner::Planner;
use crate::principal::{AuditAction, Principal, check_permission};
use crate::quest::{GoalDecomposer, LlmGoalDecomposer, Quest};
use crate::tool_executor::BasicToolExecutor;
use crate::validator::BasicValidator;
use async_trait::async_trait;
use ide_core::v1::agent_service_server::AgentService;
use ide_core::v1::{
    AgentEvent, AgentRequest, ChatMessage, ChatRequest, Empty, NesRequest, NesSuggestion, Pong,
};
use ide_probe::{
    rank_completions, CompletionBackend, MockOllamaClient, NesClient, OllamaConfig,
    OpenAiCompletionBackend, OpenAiConfig,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{info_span, Instrument};
use tonic::{Code, Request, Response, Status};

/// Concrete implementation of `AgentService`.
pub struct AgentServer {
    planner: Arc<Planner>,
    /// NES probe backend (defaults to a mock; swap for `OllamaClient`).
    nes_backend: Arc<dyn CompletionBackend>,
    /// Chat engine (T06) reusing the same `Llm` as the Planner.
    chat_engine: Arc<ChatEngine>,
    /// Per-session rolling chat state (multi-turn).
    sessions: Arc<Mutex<HashMap<String, ChatSession>>>,
    /// T12 SSO: derives the per-request [`Principal`] from the bearer token.
    /// `NoopAuthenticator` when SSO is off, `Hs256Authenticator` when on.
    authenticator: Arc<dyn Authenticator>,
    /// The server (configured) principal — used as the Noop fallback identity
    /// and for the `/console` snapshot; per-request RPCs replace it with the
    /// authenticated principal.
    principal: Principal,
    /// v1.0 — append-only audit sink (T11).
    audit: Arc<dyn AuditSink>,
    /// v1.0 — observability counters / histograms (T14).
    metrics: Arc<Metrics>,
}

impl AgentServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        planner: Arc<Planner>,
        nes_backend: Arc<dyn CompletionBackend>,
        chat_engine: Arc<ChatEngine>,
        authenticator: Arc<dyn Authenticator>,
        principal: Principal,
        audit: Arc<dyn AuditSink>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            planner,
            nes_backend,
            chat_engine,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            authenticator,
            principal,
            audit,
            metrics,
        }
    }

    /// Shared metrics handle (used by the admin listener spawned in `server`).
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// The server (configured) principal — used for the `/console` snapshot and
    /// as the Noop fallback identity. Per-request RPCs derive their own.
    pub fn principal(&self) -> Principal {
        self.principal.clone()
    }

    /// Derive the per-request [`Principal`] from the bearer token in `request`.
    ///
    /// When SSO is off the [`NoopAuthenticator`] returns the configured
    /// principal regardless of token. When SSO is on, a missing/invalid token
    /// yields `tonic::Status::unauthenticated`. The transport-layer bearer
    /// extraction (`Authorization: Bearer <jwt>`) is delegated to
    /// [`crate::auth::extract_bearer`].
    fn principal_from_request<T>(&self, request: &Request<T>) -> Result<Principal, Status> {
        let bearer = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let token = crate::auth::extract_bearer(bearer.as_deref());
        match self.authenticator.authenticate(token.as_deref()) {
            Ok(p) => Ok(p),
            Err(e) => {
                self.metrics.inc_denials();
                tracing::warn!("auth failed: {e}");
                Err(Status::new(Code::Unauthenticated, e.to_string()))
            }
        }
    }

    /// Build a server wired to the default M1+v0.5+v1.0 stack (CLI host + mock
    /// probe + chat engine). SSO is off here, so the [`NoopAuthenticator`]
    /// returns the fully-privileged "core" principal (dev / demo path).
    pub fn default_stack(bridge: Arc<HostBridge>) -> Self {
        let principal = Principal::all("single", "core");
        let authn: Arc<dyn Authenticator> = Arc::new(NoopAuthenticator::new(principal.clone()));
        let audit: Arc<dyn AuditSink> = Arc::new(MockAuditSink::new());
        let metrics = Arc::new(Metrics::new());
        let planner = Arc::new(
            Planner::with_defaults(bridge.clone(), 8)
                .with_governance(principal.clone(), audit.clone(), metrics.clone()),
        );
        let nes_backend: Arc<dyn CompletionBackend> = Arc::new(MockOllamaClient::new());
        let chat_engine = Arc::new(
            ChatEngine::new(planner.llm(), PermissionSet::all())
                .with_governance(principal.clone(), audit.clone(), metrics.clone()),
        );
        Self::new(
            planner,
            nes_backend,
            chat_engine,
            authn,
            principal,
            audit,
            metrics,
        )
    }

    /// Build a server from [`CoreConfig`] (T08/T10/T11/T12): selects the NES
    /// backend per `config.nes_backend`, builds the server principal from the
    /// single-tenant config, attaches a Postgres audit sink (falling back to an
    /// in-memory sink if the DB is unreachable), and chooses the authenticator:
    /// [`Hs256Authenticator`] when `sso_enabled`, else [`NoopAuthenticator`].
    pub async fn from_config(bridge: Arc<HostBridge>, config: &CoreConfig) -> Self {
        let principal = config.principal();
        let authn: Arc<dyn Authenticator> = if config.sso_enabled {
            Arc::new(Hs256Authenticator::new(
                config.sso_secret.clone(),
                config.sso_issuer.clone(),
                config.sso_client_id.clone(),
            ))
        } else {
            Arc::new(NoopAuthenticator::new(principal.clone()))
        };
        let audit: Arc<dyn AuditSink> =
            match PgAuditSink::connect(&config.database_url, &config.tenant_id).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    "audit DB unavailable ({e}); using in-memory audit sink (not durable)"
                );
                Arc::new(MockAuditSink::new())
            }
        };
        let metrics = Arc::new(Metrics::new());
        // 决策 #A：按配置选择真实 LLM backend（Ollama / OpenAi / Mock）。
        let llm = build_llm(config);
        let planner = Arc::new(
            Planner::new(
                llm.clone(),
                Arc::new(BasicToolExecutor::new()),
                Arc::new(BasicValidator::new(config.quest_max_steps)),
                bridge.clone(),
                8,
            )
            .with_governance(principal.clone(), audit.clone(), metrics.clone()),
        );
        let nes_backend = default_nes_backend(config);
        let chat_engine = Arc::new(
            ChatEngine::new(llm, PermissionSet::all())
                .with_governance(principal.clone(), audit.clone(), metrics.clone()),
        );
        Self::new(
            planner,
            nes_backend,
            chat_engine,
            authn,
            principal,
            audit,
            metrics,
        )
    }

    /// Fire-and-forget audit recording (does not block the RPC handler).
    /// `principal` is the (per-request) authenticated identity.
    fn audit(
        &self,
        principal: &Principal,
        action: AuditAction,
        granted: bool,
        payload: serde_json::Value,
    ) {
        let ev = AuditEvent::new(principal, action, granted, payload);
        self.metrics.inc_audit_events();
        let sink = self.audit.clone();
        let ev = ev.clone();
        tokio::spawn(async move {
            if let Err(e) = sink.record(&ev).await {
                tracing::warn!("audit record dropped: {e}");
            }
        });
    }

    /// Run an autonomous [`Quest`](crate::quest::Quest) (T15) toward `goal` and
    /// return a [`QuestReport`](crate::quest::QuestReport).
    ///
    /// This is a **Core-internal capability hook** — it does NOT add a new RPC to
    /// the frozen `.proto` (G6 contract freeze). If a gRPC surface is desired,
    /// Quest would be exposed by *reusing* the existing `RunAgent`/`Chat` entry
    /// points, never by defining new messages. The Quest reuses this server's
    /// Planner collaborators (`llm` / `tools` / `validator` / `bridge`) and the
    /// configured governance triple (`principal` / `audit` / `metrics`), so the
    /// six-bit mask is still enforced on every subtask action.
    pub async fn run_quest(
        &self,
        goal: &str,
        config: &crate::quest::QuestConfig,
    ) -> anyhow::Result<crate::quest::QuestReport> {
        let quest = Quest::new(
            Arc::new(LlmGoalDecomposer::new(self.planner.llm())),
            self.planner.llm(),
            self.planner.tools(),
            self.planner.validator(),
            self.planner.bridge(),
            config.clone(),
        )
        .with_governance(self.principal.clone(), self.audit.clone(), self.metrics.clone());
        quest.run(goal).await
    }

    /// Run an autonomous [`Quest`] whose subtasks may invoke real build/test/git
    /// tools (T18 engineering). The base tool executor is replaced with
    /// [`ShellTool`], which maps `cargo_test` / `cargo_build` / `git` / `sh` tool
    /// names to real `cargo`/`git` invocations and — because it implements
    /// [`ToolExecutor`](crate::tool_executor::ToolExecutor) — slots straight into
    /// the v1.5A [`SelfHeal`] executor so a failing build yields real stderr that
    /// an LLM patch can repair. Governance (six-bit mask, audit, metrics) is
    /// identical to [`AgentServer::run_quest`].
    ///
    /// This is the T18 ⇄ v1.5A wiring point: a Quest subtask can *run the tests*
    /// and self-heal them autonomously (subject to the `auto_commit` gate for any
    /// `commit`/`Execute` actions it plans).
    pub async fn run_quest_with_engineering(
        &self,
        goal: &str,
        config: &crate::quest::QuestConfig,
    ) -> anyhow::Result<crate::quest::QuestReport> {
        let tools: Arc<dyn crate::tool_executor::ToolExecutor> = Arc::new(ShellTool::new());
        let quest = Quest::new(
            Arc::new(LlmGoalDecomposer::new(self.planner.llm())),
            self.planner.llm(),
            tools,
            self.planner.validator(),
            self.planner.bridge(),
            config.clone(),
        )
        .with_governance(self.principal.clone(), self.audit.clone(), self.metrics.clone());
        quest.run(goal).await
    }
}

/// Build the reasoning LLM for the given config (决策 #A).
///
/// 选择逻辑：
///   * `LlmBackend::Ollama` → [`OllamaLlm`]，调用本地 Ollama 的 `nes-tab:latest`
///     （端点 `model_endpoint`，模型 `model_name`）。
///   * `LlmBackend::OpenAi` → [`OpenAiLlm`]，调用 OpenAI 兼容 `/v1/chat/completions`
///     （base `llm_base_url`，key `llm_api_key`，模型 `llm_model`）。
///   * 其它（含 `Mock`）→ [`MockLlm`]，离线/CI 确定性行为。
///
/// 该函数在 `from_config`、`aidea chat`、`aidea quest` 间共享，保证服务端 gRPC 与
/// CLI 走同一套 backend 选择（决策 #A：先让 aidea chat 走真实 Ollama 推理）。
pub fn build_llm(config: &CoreConfig) -> Arc<dyn Llm> {
    match config.llm_backend {
        LlmBackend::Ollama => Arc::new(OllamaLlm::new(
            &config.model_endpoint,
            &config.model_name,
        )),
        LlmBackend::OpenAi => Arc::new(OpenAiLlm::new(
            &config.llm_base_url,
            &config.llm_api_key,
            &config.llm_model,
        )),
        LlmBackend::Mock => Arc::new(MockLlm::new()),
    }
}

/// Build the default NES completion backend for the given config (T08/T09 + #A).
///
/// `ollama` selects the productionized [`NesClient`] (in-memory cache +
/// rule-based degradation + bounded-concurrency batch inference); `openai`
/// selects the OpenAI-compatible [`OpenAiCompletionBackend`]; everything else
/// falls back to the deterministic [`MockOllamaClient`] (offline / CI).
/// Shared by the gRPC server and the `aidea` CLI so both pick the same backend.
pub fn default_nes_backend(config: &CoreConfig) -> Arc<dyn CompletionBackend> {
    match config.nes_backend {
        crate::config::NesBackend::Ollama => Arc::new(NesClient::from_config(&OllamaConfig {
            endpoint: config.model_endpoint.clone(),
            model: config.model_name.clone(),
            ..OllamaConfig::default()
        })),
        crate::config::NesBackend::OpenAi => Arc::new(OpenAiCompletionBackend::new(OpenAiConfig {
            base_url: config.llm_base_url.clone(),
            api_key: config.llm_api_key.clone(),
            model: config.llm_model.clone(),
            ..OpenAiConfig::default()
        })),
        crate::config::NesBackend::Mock => Arc::new(MockOllamaClient::new()),
    }
}

#[async_trait]
impl AgentService for AgentServer {
    async fn ping(&self, _req: Request<Empty>) -> Result<Response<Pong>, Status> {
        self.metrics.inc_requests();
        Ok(Response::new(Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_ms: 0,
        }))
    }

    type NesCompleteStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<NesSuggestion, Status>> + Send + 'static>,
    >;

    async fn nes_complete(
        &self,
        request: Request<NesRequest>,
    ) -> Result<Response<Self::NesCompleteStream>, Status> {
        self.metrics.inc_requests();
        // T12: authenticate the caller (Noop when SSO off) and derive the
        // per-request principal that authorizes this action.
        let principal = self.principal_from_request(&request)?;
        let req = request.into_inner();

        // T11: the NES completion is a `Generate` action — enforce the bit.
        if let Err(e) = check_permission(&principal, crate::permissions::Permission::Generate) {
            self.metrics.inc_denials();
            self.audit(
                &principal,
                AuditAction::Generate,
                false,
                json!({ "reason": e.to_string() }),
            );
            return Err(Status::new(Code::PermissionDenied, e.to_string()));
        }

        let span = info_span!("agent.nes_complete", session = %req.session_id);
        let t0 = Instant::now();
        let candidates = self
            .nes_backend
            .complete(&req.prefix, &req.suffix, &req.language)
            .instrument(span)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let ranked = rank_completions(candidates, 5);
        let latency = t0.elapsed().as_secs_f64() * 1000.0;

        self.metrics.inc_completions();
        self.metrics.observe_completion(latency);
        self.audit(
            &principal,
            AuditAction::Generate,
            true,
            json!({ "candidates": ranked.len(), "latency_ms": latency }),
        );
        tracing::debug!(latency_ms = latency, "nes_complete finished");

        let stream = futures_util::stream::iter(ranked.into_iter().map(|c| {
            Ok(NesSuggestion {
                text: c.text,
                confidence: c.score,
                latency_ms: 0,
            })
        }));
        Ok(Response::new(Box::pin(stream)))
    }

    type RunAgentStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<AgentEvent, Status>> + Send + 'static>,
    >;

    async fn run_agent(
        &self,
        request: Request<AgentRequest>,
    ) -> Result<Response<Self::RunAgentStream>, Status> {
        self.metrics.inc_requests();
        // T12: authenticate and derive the per-request principal.
        let principal = self.principal_from_request(&request)?;
        let req = request.into_inner();

        // T11: running the agent is an `Execute` action — enforce the bit.
        if let Err(e) = check_permission(&principal, crate::permissions::Permission::Execute) {
            self.metrics.inc_denials();
            self.audit(
                &principal,
                AuditAction::Execute,
                false,
                json!({ "reason": e.to_string() }),
            );
            return Err(Status::new(Code::PermissionDenied, e.to_string()));
        }

        let span = info_span!("agent.run_agent", session = %req.session_id);
        // Re-govern the planner with the authenticated principal (v1.0A link:
        // the same `with_governance` builder, now per-request). Cloning the
        // planner is cheap — all collaborators are `Arc`.
        let per_request = (*self.planner).clone().with_governance(
            principal.clone(),
            self.audit.clone(),
            self.metrics.clone(),
        );
        let trace = per_request
            .run(&req.goal)
            .instrument(span)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        self.audit(
            &principal,
            AuditAction::Execute,
            true,
            json!({ "goal": req.goal, "steps": trace.steps.len() }),
        );

        let events: Vec<Result<AgentEvent, Status>> = trace
            .steps
            .into_iter()
            .map(|s| {
                Ok(AgentEvent {
                    kind: "step".to_string(),
                    payload: format!(
                        "thought={} action={} obs={}",
                        s.thought, s.action, s.observation
                    ),
                    ts_ms: 0,
                })
            })
            .collect();
        Ok(Response::new(Box::pin(futures_util::stream::iter(events))))
    }

    type ChatStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<ChatMessage, Status>> + Send + 'static>,
    >;

    async fn chat(
        &self,
        request: Request<ChatRequest>,
    ) -> Result<Response<Self::ChatStream>, Status> {
        self.metrics.inc_requests();
        // T12: authenticate and derive the per-request principal.
        let principal = self.principal_from_request(&request)?;
        let req = request.into_inner();

        // T11: chat is a `Generate` action — enforce the bit.
        if let Err(e) = check_permission(&principal, crate::permissions::Permission::Generate) {
            self.metrics.inc_denials();
            self.audit(
                &principal,
                AuditAction::Generate,
                false,
                json!({ "reason": e.to_string() }),
            );
            return Err(Status::new(Code::PermissionDenied, e.to_string()));
        }

        let attachments: Vec<Attachment> = crate::chat::parse_attachments(&req.attachments);
        let span = info_span!("agent.chat", session = %req.session_id);

        // Acquire/reuse the session, then build the reply while holding the guard.
        // The chat engine is re-governed per-request with the authenticated
        // principal so its `Generate` check uses the caller's rights.
        let mut sessions = self.sessions.lock().instrument(span.clone()).await;
        let session = sessions
            .entry(req.session_id.clone())
            .or_insert_with(|| ChatSession::new(2048, 64));
        let engine = (*self.chat_engine).clone().with_governance(
            principal.clone(),
            self.audit.clone(),
            self.metrics.clone(),
        );
        let reply = engine
            .reply(session, &req.message, &attachments)
            .instrument(span)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        drop(sessions);

        self.audit(
            &principal,
            AuditAction::Generate,
            true,
            json!({ "attachments": req.attachments.len() }),
        );

        // Streaming skeleton: emit the assistant content in sentence-ish chunks,
        // then a system message enumerating any tool suggestions.
        let mut messages: Vec<ChatMessage> = Vec::new();
        for chunk in engine.reply_stream(&reply.content) {
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: chunk,
            });
        }
        if !reply.suggestions.is_empty() {
            let mut body = String::from("suggested tools:\n");
            for s in &reply.suggestions {
                body.push_str(&format!("- {} ({}): {}\n", s.tool, s.argument, s.rationale));
            }
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: body,
            });
        }

        Ok(Response::new(Box::pin(futures_util::stream::iter(
            messages.into_iter().map(Ok),
        ))))
    }
}
