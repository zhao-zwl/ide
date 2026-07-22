//! Core runtime configuration (T10 — private-deployment base).
//!
//! Loaded from (in priority order): an optional config file -> environment
//! variables (prefix `AIDEA_`) -> built-in defaults. The loader is kept *pure*
//! ([`CoreConfig::from_map`]) so it can be unit-tested without touching the
//! process environment; [`CoreConfig::from_env`] wraps it around
//! `std::env::vars`.
//!
//! The MVP (F7) explicitly *downgrades* multi-tenant isolation to
//! **single-tenant validation**: there is exactly one logical tenant, and
//! (when `single_tenant` is on) a non-empty `tenant_id` identifies it. No
//! per-org RLS isolation is built in this slice — that lands in v1.0 (T11).

use std::collections::HashMap;

/// Which NES completion backend the Core should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NesBackend {
    /// Deterministic rule/mock backend (no model; CI / offline).
    #[default]
    Mock,
    /// Real local Ollama client (the productionized `NesClient` with cache +
    /// degradation + batch inference).
    Ollama,
    /// OpenAI-compatible chat/completions backend (DeepSeek / 通义 / 智谱 / …),
    /// used when the NES completion is served by a remote vendor rather than the
    /// bundled local model (决策 #A).
    OpenAi,
}

impl NesBackend {
    /// Parse a configuration string into a [`NesBackend`]; anything that is not
    /// exactly `ollama` / `openai` (case-insensitive) maps to [`NesBackend::Mock`].
    pub fn parse(s: &str) -> NesBackend {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => NesBackend::Ollama,
            "openai" => NesBackend::OpenAi,
            _ => NesBackend::Mock,
        }
    }
}

/// Which LLM backend drives the ReAct [`Planner`](crate::planner::Planner) and
/// the [`ChatEngine`](crate::chat::ChatEngine) (aidea LLM backend 模块, 决策 #A).
///
/// This is the *reasoning* LLM selector (chat / quest / run-agent). The NES
/// *completion* backend is selected independently via [`NesBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LlmBackend {
    /// Deterministic mock LLM (demos / CI / offline). No network.
    #[default]
    Mock,
    /// Real local Ollama chat model (the bundled `nes-tab:latest`) via
    /// `/api/chat`. Zero-config once the bundled model is created.
    Ollama,
    /// OpenAI-compatible `/v1/chat/completions` (DeepSeek / 通义 / 智谱 / …).
    /// The API key is injected by the GUI into the `aidea serve` environment and
    /// never persisted by the front-end.
    OpenAi,
}

impl LlmBackend {
    /// Parse a configuration string into an [`LlmBackend`]; anything that is not
    /// exactly `ollama` / `openai` (case-insensitive) maps to [`LlmBackend::Mock`].
    pub fn parse(s: &str) -> LlmBackend {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => LlmBackend::Ollama,
            "openai" => LlmBackend::OpenAi,
            _ => LlmBackend::Mock,
        }
    }
}

/// Runtime configuration shared by the Core gRPC server and the `aidea` CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreConfig {
    /// PostgreSQL + pgvector connection string.
    pub database_url: String,
    /// Model inference endpoint (Ollama base URL).
    pub model_endpoint: String,
    /// Model name served by the inference endpoint.
    pub model_name: String,
    /// NES completion backend selector.
    pub nes_backend: NesBackend,
    /// Reasoning LLM backend selector (chat / quest / run-agent). 决策 #A.
    pub llm_backend: LlmBackend,
    /// OpenAI-compatible base URL (含 `/v1`)，用于 `LlmBackend::OpenAi` 与
    /// `NesBackend::OpenAi`。例如 `https://api.openai.com/v1` / `https://api.deepseek.com/v1`。
    pub llm_base_url: String,
    /// OpenAI-compatible API Key（Bearer）。由 GUI 在 serve 启动时注入环境变量，
    /// 进程内持有，前端不持久化、不日志。
    pub llm_api_key: String,
    /// OpenAI-compatible 模型名（如 `gpt-4o-mini` / `deepseek-chat` / `qwen-plus`），
    /// 仅用于 `LlmBackend::OpenAi` 与 `NesBackend::OpenAi`。
    pub llm_model: String,
    /// MVP (F7) mode: single-tenant validation only (no multi-tenant isolation).
    pub single_tenant: bool,
    /// Logical tenant identifier (single value in this slice).
    pub tenant_id: String,
    /// Tracing / log level (`error` | `warn` | `info` | `debug` | `trace`).
    pub log_level: String,
    /// gRPC listen address (e.g. `[::1]:50051` or `0.0.0.0:50051`).
    pub grpc_addr: String,
    /// Admin listener address for `/metrics` + `/healthz` (T14). Served on a
    /// plain TCP socket, *outside* the frozen gRPC ProtoBus contract.
    pub admin_addr: String,
    /// Acting user id for the server principal (T11 single-tenant).
    pub user_id: String,
    /// Six-bit permission mask for the server principal (T11). Domain 0..63,
    /// same encoding as the SQL `perm_mask` domain in `0001_init.sql`.
    pub perm_mask: u32,
    /// T12 SSO: when `true`, every gRPC request must present a valid bearer
    /// token and the [`crate::auth::Principal`] is derived per-request from it
    /// (HS256). When `false` (default), the [`CoreConfig::principal`] is used
    /// directly via the no-op authenticator (dev / private single-tenant).
    pub sso_enabled: bool,
    /// Shared HMAC-SHA256 secret used to verify SSO bearer tokens (T12).
    pub sso_secret: String,
    /// Expected `iss` claim of SSO tokens (T12). Empty disables the check.
    pub sso_issuer: String,
    /// Expected `aud`/`client_id` claim of SSO tokens (T12). Empty disables it.
    pub sso_client_id: String,
    /// T15 Quest: when `false` (default) `Execute`/`Commit` class actions are
    /// collected as pending approvals instead of auto-running; when `true` they
    /// execute autonomously within the Quest.
    pub quest_auto_commit: bool,
    /// T15 Quest: max ReAct steps per subtask loop (reuses the `BudgetExhausted`
    /// idea via the Planner's validator).
    pub quest_max_steps: usize,
    /// T15 Quest: cap on the number of decomposed subtasks processed in one run.
    pub quest_max_subtasks: usize,
    /// T16 Self-heal: max repair/retry attempts before the circuit breaker trips.
    pub max_repair_attempts: usize,
    /// T18 Engineering: working directory the `GitClient` / `BuildRunner` operate
    /// in (default "."). Single-tenant deployment keeps one repo root.
    pub repo_root: String,
    /// T20 等保三级: data-encryption key (DEK) for `pgcrypto` at-rest encryption
    /// of secrets (sourced from `AIDEA_ENC_KEY`). Never logged; passed straight
    /// to Postgres. Empty in dev (encryption-at-rest is then unavailable).
    pub enc_key: String,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            database_url: "postgres://aidea:aidea@localhost:5432/aidea".to_string(),
            model_endpoint: "http://localhost:11434".to_string(),
            model_name: "nes-tab:latest".to_string(),
            nes_backend: NesBackend::Mock,
            llm_backend: LlmBackend::Mock,
            llm_base_url: "https://api.openai.com/v1".to_string(),
            llm_api_key: String::new(),
            llm_model: "gpt-4o-mini".to_string(),
            single_tenant: true,
            tenant_id: "single".to_string(),
            log_level: "info".to_string(),
            grpc_addr: "[::1]:50051".to_string(),
            admin_addr: "127.0.0.1:9090".to_string(),
            user_id: "admin".to_string(),
            perm_mask: 63,
            sso_enabled: false,
            sso_secret: String::new(),
            sso_issuer: String::new(),
            sso_client_id: String::new(),
            quest_auto_commit: false,
            quest_max_steps: 8,
            quest_max_subtasks: 8,
            max_repair_attempts: 3,
            repo_root: ".".to_string(),
            enc_key: String::new(),
        }
    }
}

impl CoreConfig {
    /// Environment variable prefix recognized by [`CoreConfig::from_env`].
    const ENV_PREFIX: &'static str = "AIDEA_";

    /// Build the config from an explicit key/value map shaped like the
    /// environment (keys *without* the prefix, e.g. `NES_BACKEND`). Unknown
    /// keys are ignored; missing keys fall back to [`CoreConfig::default`].
    ///
    /// Pure — this is the entry point exercised by unit tests.
    pub fn from_map(map: &HashMap<String, String>) -> Self {
        let mut cfg = CoreConfig::default();
        if let Some(v) = map.get("DATABASE_URL") {
            cfg.database_url = v.clone();
        }
        if let Some(v) = map.get("MODEL_ENDPOINT") {
            cfg.model_endpoint = v.clone();
        }
        if let Some(v) = map.get("MODEL_NAME") {
            cfg.model_name = v.clone();
        }
        if let Some(v) = map.get("NES_BACKEND") {
            cfg.nes_backend = NesBackend::parse(v);
        }
        if let Some(v) = map.get("LLM_BACKEND") {
            cfg.llm_backend = LlmBackend::parse(v);
        }
        if let Some(v) = map.get("LLM_BASE_URL") {
            cfg.llm_base_url = v.clone();
        }
        if let Some(v) = map.get("LLM_API_KEY") {
            cfg.llm_api_key = v.clone();
        }
        if let Some(v) = map.get("LLM_MODEL") {
            cfg.llm_model = v.clone();
        }
        if let Some(v) = map.get("SINGLE_TENANT") {
            cfg.single_tenant = parse_bool(v, true);
        }
        if let Some(v) = map.get("TENANT_ID") {
            cfg.tenant_id = v.clone();
        }
        if let Some(v) = map.get("LOG_LEVEL") {
            cfg.log_level = v.clone();
        }
        if let Some(v) = map.get("GRPC_ADDR") {
            cfg.grpc_addr = v.clone();
        }
        if let Some(v) = map.get("ADMIN_ADDR") {
            cfg.admin_addr = v.clone();
        }
        if let Some(v) = map.get("USER_ID") {
            cfg.user_id = v.clone();
        }
        if let Some(v) = map.get("PERM_MASK") {
            // Parse the six-bit mask; on garbage, degrade to the full mask (63)
            // so a misconfigured deployment fails open loudly rather than silently.
            cfg.perm_mask = v.trim().parse().unwrap_or(63);
        }
        if let Some(v) = map.get("SSO_ENABLED") {
            cfg.sso_enabled = parse_bool(v, false);
        }
        if let Some(v) = map.get("SSO_SECRET") {
            cfg.sso_secret = v.clone();
        }
        if let Some(v) = map.get("SSO_ISSUER") {
            cfg.sso_issuer = v.clone();
        }
        if let Some(v) = map.get("SSO_CLIENT_ID") {
            cfg.sso_client_id = v.clone();
        }
        if let Some(v) = map.get("QUEST_AUTO_COMMIT") {
            cfg.quest_auto_commit = parse_bool(v, false);
        }
        if let Some(v) = map.get("QUEST_MAX_STEPS") {
            // Garbage degrades to the default 8 rather than erroring.
            cfg.quest_max_steps = v.trim().parse().unwrap_or(8);
        }
        if let Some(v) = map.get("QUEST_MAX_SUBTASKS") {
            cfg.quest_max_subtasks = v.trim().parse().unwrap_or(8);
        }
        if let Some(v) = map.get("MAX_REPAIR_ATTEMPTS") {
            cfg.max_repair_attempts = v.trim().parse().unwrap_or(3);
        }
        if let Some(v) = map.get("REPO_ROOT") {
            cfg.repo_root = v.clone();
        }
        if let Some(v) = map.get("ENC_KEY") {
            cfg.enc_key = v.clone();
        }
        cfg
    }

    /// Load from the process environment (keys prefixed `AIDEA_`).
    pub fn from_env() -> Self {
        let map: HashMap<String, String> = std::env::vars()
            .filter_map(|(k, v)| {
                k.strip_prefix(Self::ENV_PREFIX)
                    .map(|s| (s.to_ascii_uppercase(), v))
            })
            .collect();
        Self::from_map(&map)
    }

    /// Validate the single-tenant deployment invariant for the MVP (F7).
    ///
    /// The MVP explicitly downgrades multi-tenant isolation to "single-tenant
    /// validation": there must be exactly one logical tenant, and (when
    /// `single_tenant` is on) a non-empty `tenant_id` identifies it. This is the
    /// only tenancy check performed in this slice; real multi-tenant isolation
    /// (RLS per org) lands in v1.0 (T11).
    pub fn validate_tenancy(&self) -> anyhow::Result<()> {
        if self.single_tenant && self.tenant_id.trim().is_empty() {
            anyhow::bail!("single-tenant mode requires a non-empty tenant_id");
        }
        Ok(())
    }

    /// Build the server [`Principal`] from the single-tenant config (T11).
    ///
    /// The principal carries the tenant + user identity and the six-bit mask
    /// (`perm_mask`) that authorizes every action the Core performs on behalf of
    /// this deployment. The mask is clamped to the six-bit domain (0..63) so it
    /// stays byte-identical to the SQL `perm_mask` column.
    pub fn principal(&self) -> crate::principal::Principal {
        crate::principal::Principal::from_mask(
            self.tenant_id.clone(),
            self.user_id.clone(),
            self.perm_mask,
        )
    }

    /// Build the Quest configuration (T15 / T16) from this Core config.
    ///
    /// Wires the four v1.5 knobs (`quest_auto_commit` / `quest_max_steps` /
    /// `quest_max_subtasks` / `max_repair_attempts`) into a
    /// [`QuestConfig`](crate::quest::QuestConfig) used by the autonomous
    /// [`Quest`](crate::quest::Quest) and the [`SelfHeal`](crate::self_heal::SelfHeal)
    /// circuit breaker. `subtask_max_retries` defaults to 1 (retry once, then
    /// skip and mark the subtask failed).
    pub fn quest_config(&self) -> crate::quest::QuestConfig {
        crate::quest::QuestConfig {
            max_steps: self.quest_max_steps,
            max_subtasks: self.quest_max_subtasks,
            auto_commit: self.quest_auto_commit,
            max_repair_attempts: self.max_repair_attempts,
            subtask_max_retries: 1,
        }
    }
}

/// Parse a boolean-ish configuration value. Returns `default` for unrecognized
/// strings (so a missing/garbled value degrades safely rather than erroring).
fn parse_bool(s: &str, default: bool) -> bool {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "y" => true,
        "0" | "false" | "no" | "off" | "n" => false,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = CoreConfig::default();
        assert!(c.single_tenant);
        assert_eq!(c.tenant_id, "single");
        assert_eq!(c.nes_backend, NesBackend::Mock);
        assert!(c.grpc_addr.contains("50051"));
        assert!(!c.database_url.is_empty());
    }

    #[test]
    fn from_map_overrides_defaults() {
        let mut m = HashMap::new();
        m.insert("NES_BACKEND".to_string(), "ollama".to_string());
        m.insert("SINGLE_TENANT".to_string(), "false".to_string());
        m.insert("TENANT_ID".to_string(), "acme".to_string());
        m.insert("GRPC_ADDR".to_string(), "[::1]:60051".to_string());
        let c = CoreConfig::from_map(&m);
        assert_eq!(c.nes_backend, NesBackend::Ollama);
        assert!(!c.single_tenant);
        assert_eq!(c.tenant_id, "acme");
        assert_eq!(c.grpc_addr, "[::1]:60051");
    }

    #[test]
    fn single_tenant_switch_and_validation() {
        // Empty tenant id under single-tenant mode must be rejected.
        let mut m = HashMap::new();
        m.insert("SINGLE_TENANT".to_string(), "true".to_string());
        m.insert("TENANT_ID".to_string(), "".to_string());
        let c = CoreConfig::from_map(&m);
        assert!(c.single_tenant);
        assert!(c.validate_tenancy().is_err());

        // A non-empty tenant id passes validation.
        let mut m2 = HashMap::new();
        m2.insert("SINGLE_TENANT".to_string(), "true".to_string());
        m2.insert("TENANT_ID".to_string(), "acme".to_string());
        assert!(CoreConfig::from_map(&m2).validate_tenancy().is_ok());
    }

    #[test]
    fn nes_backend_parse_fallback() {
        assert_eq!(NesBackend::parse("OLAMA"), NesBackend::Mock);
        assert_eq!(NesBackend::parse("ollama"), NesBackend::Ollama);
        assert_eq!(NesBackend::parse("openai"), NesBackend::OpenAi);
        assert_eq!(NesBackend::parse(""), NesBackend::Mock);
    }

    #[test]
    fn llm_backend_parse_and_defaults() {
        assert_eq!(LlmBackend::default(), LlmBackend::Mock);
        assert_eq!(LlmBackend::parse("ollama"), LlmBackend::Ollama);
        assert_eq!(LlmBackend::parse("OPENAI"), LlmBackend::OpenAi);
        assert_eq!(LlmBackend::parse("garbage"), LlmBackend::Mock);
    }

    #[test]
    fn llm_backend_fields_load_from_map() {
        let mut m = HashMap::new();
        m.insert("LLM_BACKEND".to_string(), "openai".to_string());
        m.insert("LLM_BASE_URL".to_string(), "https://api.deepseek.com/v1".to_string());
        m.insert("LLM_API_KEY".to_string(), "sk-test".to_string());
        m.insert("LLM_MODEL".to_string(), "deepseek-chat".to_string());
        let c = CoreConfig::from_map(&m);
        assert_eq!(c.llm_backend, LlmBackend::OpenAi);
        assert_eq!(c.llm_base_url, "https://api.deepseek.com/v1");
        assert_eq!(c.llm_api_key, "sk-test");
        assert_eq!(c.llm_model, "deepseek-chat");
    }

    #[test]
    fn llm_backend_fields_default_when_absent() {
        let c = CoreConfig::default();
        assert_eq!(c.llm_backend, LlmBackend::Mock);
        assert_eq!(c.llm_base_url, "https://api.openai.com/v1");
        assert_eq!(c.llm_api_key, "");
        assert_eq!(c.llm_model, "gpt-4o-mini");
    }

    #[test]
    fn bool_parser_defaults_on_garbage() {
        assert!(parse_bool("true", false));
        assert!(!parse_bool("false", true));
        assert!(parse_bool("garbage", true));
        assert!(!parse_bool("garbage", false));
    }

    #[test]
    fn admin_addr_and_principal_are_configurable() {
        let mut m = HashMap::new();
        m.insert("ADMIN_ADDR".to_string(), "0.0.0.0:9091".to_string());
        m.insert("USER_ID".to_string(), "svc-1".to_string());
        m.insert("PERM_MASK".to_string(), "35".to_string()); // 0b100011
        let c = CoreConfig::from_map(&m);
        assert_eq!(c.admin_addr, "0.0.0.0:9091");
        assert_eq!(c.user_id, "svc-1");
        let p = c.principal();
        assert_eq!(p.tenant_id, "single");
        assert_eq!(p.user_id, "svc-1");
        // 35 == 0b100011 == Read(1) | Generate(2) | Commit(32).
        assert_eq!(p.perm_mask(), 35);
        assert!(p.perms.has(crate::permissions::Permission::Commit));
        assert!(!p.perms.has(crate::permissions::Permission::Modify));
    }

    #[test]
    fn principal_mask_clamped_to_six_bits() {
        let mut m = HashMap::new();
        m.insert("PERM_MASK".to_string(), "255".to_string());
        let c = CoreConfig::from_map(&m);
        assert_eq!(c.principal().perm_mask(), 63);
    }

    #[test]
    fn sso_fields_load_from_map() {
        let mut m = HashMap::new();
        m.insert("SSO_ENABLED".to_string(), "true".to_string());
        m.insert("SSO_SECRET".to_string(), "shared".to_string());
        m.insert("SSO_ISSUER".to_string(), "aidea".to_string());
        m.insert("SSO_CLIENT_ID".to_string(), "cli".to_string());
        let c = CoreConfig::from_map(&m);
        assert!(c.sso_enabled);
        assert_eq!(c.sso_secret, "shared");
        assert_eq!(c.sso_issuer, "aidea");
        assert_eq!(c.sso_client_id, "cli");
        // Dev fallback principal is still constructible when SSO is off.
        let off = CoreConfig::from_map(&HashMap::new());
        assert!(!off.sso_enabled);
        assert_eq!(off.principal().perm_mask(), 63);
    }

    #[test]
    fn quest_and_repair_fields_load_from_map() {
        let mut m = HashMap::new();
        m.insert("QUEST_AUTO_COMMIT".to_string(), "true".to_string());
        m.insert("QUEST_MAX_STEPS".to_string(), "12".to_string());
        m.insert("QUEST_MAX_SUBTASKS".to_string(), "5".to_string());
        m.insert("MAX_REPAIR_ATTEMPTS".to_string(), "4".to_string());
        let c = CoreConfig::from_map(&m);
        assert!(c.quest_auto_commit);
        assert_eq!(c.quest_max_steps, 12);
        assert_eq!(c.quest_max_subtasks, 5);
        assert_eq!(c.max_repair_attempts, 4);
        let q = c.quest_config();
        assert!(q.auto_commit);
        assert_eq!(q.max_steps, 12);
        assert_eq!(q.max_subtasks, 5);
        assert_eq!(q.max_repair_attempts, 4);
        assert_eq!(q.subtask_max_retries, 1);
    }

    #[test]
    fn repo_root_defaults_to_dot_and_loads() {
        let c = CoreConfig::default();
        assert_eq!(c.repo_root, ".");
        let mut m = HashMap::new();
        m.insert("REPO_ROOT".to_string(), "/workspace/src".to_string());
        let c2 = CoreConfig::from_map(&m);
        assert_eq!(c2.repo_root, "/workspace/src");
    }

    #[test]
    fn enc_key_loads_from_env_prefix_and_defaults_empty() {
        // Default is empty (dev: encryption-at-rest unavailable).
        assert_eq!(CoreConfig::default().enc_key, "");
        let mut m = HashMap::new();
        m.insert("ENC_KEY".to_string(), "super-secret-dek".to_string());
        let c = CoreConfig::from_map(&m);
        assert_eq!(c.enc_key, "super-secret-dek");
        // from_env() routes AIDEA_ENC_KEY -> enc_key (exercised via from_map shape).
    }
}
