//! `aidea` CLI — the dual-form Agentic IDE command line (T09, F8).
//!
//! Subcommands share the same decision Core as the desktop IDE (via the
//! `ide-core` library). `serve` starts the ProtoBus gRPC server; `chat`,
//! `craft`, `nes` drive the Core directly (no running server required); `version`
//! prints build metadata. Argument structs and the dispatch routine live here so
//! they are unit-testable ([`parse_args`], [`dispatch_with`]).

use clap::{Parser, Subcommand};
use ide_core::admin::{render_console, ConsoleStatus};
use ide_core::agent::{build_llm, default_nes_backend};
use ide_core::chat::{parse_attachments, ChatEngine, ChatSession};
use ide_core::config::{CoreConfig, NesBackend};
use ide_core::craft::{CraftEngine, EditKind};
use ide_core::host::{CliHost, HostBridge, HostProvider};
use ide_core::metrics::Metrics;
use ide_core::permissions::PermissionSet;
use ide_core::planner::Planner;
use ide_core::quest::{LlmGoalDecomposer, Quest, QuestConfig};
use ide_core::server;
use ide_core::collab::{
    Comment, CommentStore, InMemoryCommentStore, InMemoryLockStore, Lock, LockStore, PgCommentStore,
};
use ide_core::security::PgSecretStore;
use ide_core::speed_test;
use ide_core::llm::Llm;
use ide_core::tool_executor::{BasicToolExecutor, ToolExecutor};
use ide_core::validator::{BasicValidator, Validator};
use std::io::Write;
use std::sync::Arc;

/// Top-level `aidea` CLI.
#[derive(Parser)]
#[command(name = "aidea", version, about = "Agentic IDE CLI (v0.5 MVP)", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// `aidea` subcommands.
#[derive(Subcommand)]
pub enum Command {
    /// Ask the Core a question (drives the ChatEngine directly).
    Chat {
        /// The message to send.
        message: String,
        /// Session id (kept in-memory for this invocation).
        #[arg(long, default_value = "cli-session")]
        session_id: String,
        /// Attachments, e.g. `@file:src/main.rs` `@symbol:retry`.
        #[arg(long, value_delimiter = ' ')]
        attachments: Vec<String>,
    },
    /// Propose an edit and confirm it interactively (human-led Craft).
    Craft {
        /// Document uri the edit applies to.
        #[arg(long)]
        file: String,
        /// Text to be replaced.
        #[arg(long)]
        old: String,
        /// Replacement text.
        #[arg(long)]
        new: String,
        /// Kind of edit: `file` | `command` | `commit`.
        #[arg(long, default_value = "file")]
        kind: String,
        /// Auto-confirm without prompting (non-interactive / tests).
        #[arg(long, default_value = "false")]
        yes: bool,
        /// Rationale shown to the user.
        #[arg(long, default_value = "proposed by aidea")]
        rationale: String,
    },
    /// Run the NES latency probe and assert the L0 p95 < 300ms budget.
    Nes {
        /// Number of samples for the speed test.
        #[arg(long, default_value = "50")]
        samples: usize,
        /// Override the NES backend (`mock` | `ollama`).
        #[arg(long)]
        backend: Option<String>,
    },
    /// Start the Core gRPC (ProtoBus) server.
    Serve {
        /// Listen address.
        #[arg(default_value = "[::1]:50051")]
        addr: String,
    },
    /// Print version and loaded configuration summary.
    Version,
    /// Print the read-only enterprise console status (tenant/identity/metrics).
    Console,
    /// Run an autonomous Quest toward a high-level goal (T15).
    Quest {
        /// High-level goal for the autonomous agent.
        #[arg(long, default_value = "add retry logic to utils")]
        goal: String,
    },
    /// Code review comments / annotations on a `(file, line)` (T19 collaboration).
    Comment {
        #[command(subcommand)]
        cmd: CommentCmd,
    },
    /// Lightweight editing-presence lock hints — "who is editing this file" (T19).
    Lock {
        #[command(subcommand)]
        cmd: LockCmd,
    },
    /// Tenant-scoped secrets stored encrypted at rest via pgcrypto (T20).
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
}

/// `aidea comment` subcommands.
#[derive(Subcommand)]
pub enum CommentCmd {
    /// List comments on a file (for the current tenant).
    List {
        /// File the comments are anchored to.
        file: String,
    },
    /// Add a comment on a single line.
    Add {
        /// File the comment is anchored to.
        file: String,
        /// Line number (1-based).
        line: usize,
        /// Comment body.
        #[arg(long)]
        text: String,
    },
    /// Mark a comment resolved.
    Resolve {
        /// Comment id (from `list`).
        id: String,
    },
}

/// `aidea lock` subcommands (T19 presence hints).
#[derive(Subcommand)]
pub enum LockCmd {
    /// Acquire the editing lock on a file (as the current user).
    Acquire {
        /// File to lock.
        file: String,
    },
    /// Release the editing lock on a file.
    Release {
        /// File to unlock.
        file: String,
    },
    /// Show who (if anyone) is editing a file.
    Show {
        /// File to inspect.
        file: String,
    },
}

/// `aidea secret` subcommands (T20 pgcrypto).
#[derive(Subcommand)]
pub enum SecretCmd {
    /// Store a secret encrypted at rest.
    Set {
        /// Secret name.
        name: String,
        /// Secret value (encrypted before it leaves the Core).
        value: String,
    },
    /// Read a secret back (decrypted with `AIDEA_ENC_KEY`).
    Get {
        /// Secret name.
        name: String,
    },
}

/// Parse CLI args (testable entry point — does not touch the environment).
pub fn parse_args<I, T>(itr: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(itr)
}

/// `main` entry: parse and dispatch against the process environment config.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    dispatch(cli.command).await
}

/// Parse-and-dispatch convenience used by `main`.
pub async fn dispatch(command: Command) -> anyhow::Result<()> {
    let config = CoreConfig::from_env();
    dispatch_with(command, &config).await
}

/// Dispatch a parsed command against an explicit config (testable).
pub async fn dispatch_with(command: Command, config: &CoreConfig) -> anyhow::Result<()> {
    match command {
        Command::Chat {
            message,
            session_id,
            attachments,
        } => cmd_chat(&message, &session_id, &attachments, config).await,
        Command::Craft {
            file,
            old,
            new,
            kind,
            yes,
            rationale,
        } => cmd_craft(&file, &old, &new, &kind, yes, &rationale).await,
        Command::Nes { samples, backend } => cmd_nes(samples, backend.as_deref(), config).await,
        Command::Serve { addr } => cmd_serve(&addr, config).await,
        Command::Version => cmd_version(config),
        Command::Console => cmd_console(config),
        Command::Quest { goal } => cmd_quest(&goal, config).await,
        Command::Comment { cmd } => cmd_comment(cmd, config).await,
        Command::Lock { cmd } => cmd_lock(cmd, config).await,
        Command::Secret { cmd } => cmd_secret(cmd, config).await,
    }
}

// ------------------------------- handlers ----------------------------------

async fn cmd_chat(
    message: &str,
    session_id: &str,
    attachments: &[String],
    config: &CoreConfig,
) -> anyhow::Result<()> {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    // 决策 #A：使用配置选择的真实 LLM backend（Ollama / OpenAi / Mock）。
    let llm: Arc<dyn Llm> = build_llm(config);
    let planner = Planner::new(
        llm.clone(),
        Arc::new(BasicToolExecutor::new()),
        Arc::new(BasicValidator::new(8)),
        bridge,
        8,
    );
    let engine = ChatEngine::new(llm, PermissionSet::all());
    let mut session = ChatSession::new(2048, 16);
    let atts = parse_attachments(
        &attachments
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );
    let reply = engine.reply(&mut session, message, &atts).await?;
    println!("[chat:{session_id}] {}\n", reply.content);
    if !reply.suggestions.is_empty() {
        println!("suggested tools:");
        for s in &reply.suggestions {
            println!("  - {} ({}) — {}", s.tool, s.argument, s.rationale);
        }
    }
    Ok(())
}

async fn cmd_craft(
    file: &str,
    old: &str,
    new: &str,
    kind: &str,
    yes: bool,
    rationale: &str,
) -> anyhow::Result<()> {
    let host = Arc::new(CliHost::new());
    // Seed so the in-memory host can apply the edit (demo behaviour).
    host.seed(file, old);
    let bridge = Arc::new(HostBridge::new(host.clone()));
    let engine = CraftEngine::new(bridge, PermissionSet::all());
    let edit_kind = match kind.to_ascii_lowercase().as_str() {
        "command" => EditKind::RunCommand,
        "commit" => EditKind::Commit,
        _ => EditKind::FileEdit,
    };
    let mut proposal = engine.propose(file, old, new, rationale, edit_kind);
    println!("Proposed edit ({:?}):", proposal.state);
    println!("  file:    {file}");
    println!("  - {old}");
    println!("  + {new}");
    println!("  rationale: {rationale}");

    let confirm = if yes {
        true
    } else {
        print!("Apply this edit? [y/N] ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };

    if confirm {
        let state = engine.confirm(&mut proposal).await?;
        println!("Applied. state = {state:?}");
        if edit_kind == EditKind::FileEdit {
            println!(
                "document now: {}",
                host.read_document(file).await.unwrap_or_default()
            );
        }
    } else {
        println!("Aborted; no file changed.");
    }
    Ok(())
}

async fn cmd_nes(samples: usize, backend_override: Option<&str>, config: &CoreConfig) -> anyhow::Result<()> {
    let mut cfg = config.clone();
    if let Some(b) = backend_override {
        cfg.nes_backend = NesBackend::parse(b);
    }
    let backend = default_nes_backend(&cfg);
    println!(
        "[nes] backend = {:?}, samples = {samples}",
        cfg.nes_backend
    );
    let (p50, p95) = speed_test(backend.as_ref(), samples).await?;
    println!("L0 Tab latency: p50={p50:.1}ms p95={p95:.1}ms");
    if p95 < 300.0 {
        println!("[nes] OK — within L0 budget (<300ms).");
    } else {
        println!("[nes] WARN — p95 {p95:.1}ms exceeds L0 budget 300ms.");
    }
    Ok(())
}

async fn cmd_serve(addr: &str, config: &CoreConfig) -> anyhow::Result<()> {
    // All gRPC wiring (AgentService + HealthService) lives in `ide-core` so the
    // CLI crate stays free of a direct `tonic` dependency; we just delegate.
    println!(
        "[aidea] starting Core gRPC on {addr} (single_tenant={}, tenant={})",
        config.single_tenant, config.tenant_id
    );
    server::serve_configured(addr, config).await
}

fn cmd_version(config: &CoreConfig) -> anyhow::Result<()> {
    println!("aidea {}", env!("CARGO_PKG_VERSION"));
    println!(
        "config: grpc_addr={} nes_backend={:?} llm_backend={:?} model={}@{} single_tenant={} tenant={} sso_enabled={}",
        config.grpc_addr,
        config.nes_backend,
        config.llm_backend,
        config.model_name,
        config.model_endpoint,
        config.single_tenant,
        config.tenant_id,
        config.sso_enabled,
    );
    Ok(())
}

/// Print the read-only enterprise console status (T13).
///
/// Uses the **direct library** path (no gRPC / no new heavy dependency): it
/// constructs a [`ConsoleStatus`] from the loaded config principal plus a fresh
/// metrics snapshot. For *live* metrics and audit counts from a running server,
/// query `GET /console` on the admin port (served by `ide_core::admin`).
fn cmd_console(config: &CoreConfig) -> anyhow::Result<()> {
    let principal = config.principal();
    let metrics = Arc::new(Metrics::new());
    let status = ConsoleStatus {
        tenant_id: principal.tenant_id.clone(),
        user_id: principal.user_id.clone(),
        perm_mask: principal.perm_mask(),
        audit_events: metrics.audit_events(),
        requests: metrics.requests(),
        tool_calls: metrics.tool_calls(),
        llm_calls: metrics.llm_calls(),
        completions: metrics.completions(),
        denials: metrics.denials(),
        request_p95_ms: metrics.request_latency.p95_ms(),
    };
    println!("{}", render_console(&status));
    if config.sso_enabled {
        println!(
            "[console] SSO enabled — principals are derived per-request from HS256 bearer tokens."
        );
    } else {
        println!("[console] SSO disabled (Noop) — using the configured principal.");
    }
    println!(
        "[console] For live metrics/audit counts, query: GET http://{}/console",
        config.admin_addr
    );
    Ok(())
}

/// Run an autonomous Quest (T15) headless and print the [`QuestReport`].
///
/// Reuses the decision Core directly (no running gRPC server required): it builds
/// the default M1 stack (CLI host + `MockLlm` + `BasicToolExecutor` +
/// `BasicValidator`), derives the Quest config from `CoreConfig`, and runs the
/// autonomous loop. `auto_commit` is taken from config, so
/// `AIDEA_QUEST_AUTO_COMMIT=true` flips the approval gate to fully autonomous
/// execution.
async fn cmd_quest(goal: &str, config: &CoreConfig) -> anyhow::Result<()> {
    let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
    // 决策 #A：使用配置选择的真实 LLM backend（Ollama / OpenAi / Mock）。
    let llm: Arc<dyn Llm> = build_llm(config);
    let tools: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new());
    let validator: Arc<dyn Validator> = Arc::new(BasicValidator::new(config.quest_max_steps));
    let quest_config: QuestConfig = config.quest_config();
    let quest = Quest::new(
        Arc::new(LlmGoalDecomposer::new(llm.clone())),
        llm,
        tools,
        validator,
        bridge,
        quest_config,
    );
    println!(
        "[quest] goal = {goal}  (auto_commit = {})",
        config.quest_auto_commit
    );
    let report = quest.run(goal).await?;
    println!("[quest] subtasks = {}", report.subtasks.len());
    for st in &report.subtasks {
        println!("  - [{:>8}] {}", st.status.as_str(), st.description);
    }
    println!(
        "[quest] successes = {}  failures = {}  pending_approvals = {}",
        report.successes, report.failures, report.pending_approvals.len()
    );
    Ok(())
}

/// Pick a [`CommentStore`] for the CLI: prefer Postgres (tenant-scoped), fall
/// back to an in-memory store if the DB is unreachable (offline / tests).
async fn comment_store(config: &CoreConfig) -> Arc<dyn CommentStore> {
    match PgCommentStore::connect(&config.database_url, &config.tenant_id).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[comment] pg unavailable ({e}); using in-memory store (not durable)");
            Arc::new(InMemoryCommentStore::new())
        }
    }
}

/// Handle `aidea comment <list|add|resolve>` (T19 collaboration).
async fn cmd_comment(cmd: CommentCmd, config: &CoreConfig) -> anyhow::Result<()> {
    let store: Arc<dyn CommentStore> = comment_store(config).await;
    match cmd {
        CommentCmd::List { file } => {
            let comments = store.list_for_file(&config.tenant_id, &file).await?;
            if comments.is_empty() {
                println!("[comment] no comments on {file} (tenant={})", config.tenant_id);
            }
            for c in &comments {
                let state = if c.resolved { "resolved" } else { "open" };
                println!(
                    "#{} [{}] {}:{} {} — {}",
                    c.id, state, c.file, c.line_start, c.author, c.body
                );
            }
        }
        CommentCmd::Add { file, line, text } => {
            let author = config.user_id.clone();
            let comment = Comment::new(&config.tenant_id, &file, line, line, &author, &text);
            let id = store.add(&comment).await?;
            println!("[comment] added #{} on {file}:{line} (tenant={})", id, config.tenant_id);
        }
        CommentCmd::Resolve { id } => {
            let ok = store.resolve(&config.tenant_id, &id).await?;
            println!(
                "[comment] resolve #{} -> {}",
                id,
                if ok { "ok" } else { "not found" }
            );
        }
    }
    Ok(())
}

/// Handle `aidea lock <acquire|release|show>` (T19 presence hints, in-memory).
async fn cmd_lock(cmd: LockCmd, config: &CoreConfig) -> anyhow::Result<()> {
    let store: Arc<dyn LockStore> = Arc::new(InMemoryLockStore::new());
    match cmd {
        LockCmd::Acquire { file } => {
            let owner = config.user_id.clone();
            let lock = Lock::new(&config.tenant_id, &file, &owner);
            store.acquire(&lock).await?;
            println!("[lock] {} is now editing {file} (tenant={})", owner, config.tenant_id);
        }
        LockCmd::Release { file } => {
            store.release(&config.tenant_id, &file).await?;
            println!("[lock] released {file} (tenant={})", config.tenant_id);
        }
        LockCmd::Show { file } => match store.get(&config.tenant_id, &file).await? {
            Some(l) => println!(
                "[lock] {file} is being edited by {} (since {})",
                l.owner, l.acquired_at
            ),
            None => println!("[lock] {file} is free (tenant={})", config.tenant_id),
        },
    }
    Ok(())
}

/// Handle `aidea secret <set|get>` (T20 pgcrypto). Requires a reachable Postgres;
/// the value is encrypted at rest by `pgcrypto` using `AIDEA_ENC_KEY`.
async fn cmd_secret(cmd: SecretCmd, config: &CoreConfig) -> anyhow::Result<()> {
    let store = PgSecretStore::connect(&config.database_url, &config.tenant_id).await?;
    match cmd {
        SecretCmd::Set { name, value } => {
            store
                .set(&config.tenant_id, &name, &value, &config.enc_key)
                .await?;
            println!(
                "[secret] stored '{name}' encrypted at rest (tenant={})",
                config.tenant_id
            );
        }
        SecretCmd::Get { name } => match store.get(&config.tenant_id, &name, &config.enc_key).await? {
            Some(v) => println!("[secret] {name} = {v}"),
            None => println!("[secret] '{name}' not found (tenant={})", config.tenant_id),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg() -> CoreConfig {
        CoreConfig::from_map(&HashMap::new())
    }

    #[test]
    fn parses_all_subcommands() {
        match parse_args(["aidea", "chat", "hello world"]).unwrap().command {
            Command::Chat { message, .. } => assert_eq!(message, "hello world"),
            _ => panic!("expected chat"),
        }
        match parse_args(["aidea", "nes", "--samples", "10"]).unwrap().command {
            Command::Nes { samples, .. } => assert_eq!(samples, 10),
            _ => panic!("expected nes"),
        }
        match parse_args([
            "aidea", "craft", "--file", "a.rs", "--old", "x", "--new", "y", "--kind", "file",
        ])
        .unwrap()
        .command
        {
            Command::Craft {
                file, old, new, kind, ..
            } => {
                assert_eq!(file, "a.rs");
                assert_eq!(old, "x");
                assert_eq!(new, "y");
                assert_eq!(kind, "file");
            }
            _ => panic!("expected craft"),
        }
        match parse_args(["aidea", "serve", "[::1]:60051"]).unwrap().command {
            Command::Serve { addr } => assert_eq!(addr, "[::1]:60051"),
            _ => panic!("expected serve"),
        }
        assert!(matches!(
            parse_args(["aidea", "version"]).unwrap().command,
            Command::Version
        ));
    }

    #[test]
    fn unknown_subcommand_fails() {
        assert!(parse_args(["aidea", "bogus"]).is_err());
    }

    #[tokio::test]
    async fn routes_chat_offline() {
        dispatch_with(
            Command::Chat {
                message: "hi".into(),
                session_id: "s".into(),
                attachments: vec![],
            },
            &cfg(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_nes_mock_within_budget() {
        dispatch_with(
            Command::Nes {
                samples: 20,
                backend: Some("mock".to_string()),
            },
            &cfg(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_craft_auto_confirm() {
        dispatch_with(
            Command::Craft {
                file: "mem://a.rs".into(),
                old: "let x = 1;".into(),
                new: "let x = 2;".into(),
                kind: "file".into(),
                yes: true,
                rationale: "bump".into(),
            },
            &cfg(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_version() {
        dispatch_with(Command::Version, &cfg()).await.unwrap();
    }

    #[test]
    fn parses_console_subcommand() {
        assert!(matches!(
            parse_args(["aidea", "console"]).unwrap().command,
            Command::Console
        ));
    }

    #[test]
    fn parses_quest_subcommand() {
        match parse_args(["aidea", "quest", "--goal", "ship the feature"])
            .unwrap()
            .command
        {
            Command::Quest { goal } => assert_eq!(goal, "ship the feature"),
            _ => panic!("expected quest"),
        }
    }

    #[tokio::test]
    async fn routes_quest_offline() {
        dispatch_with(
            Command::Quest {
                goal: "add retry logic".into(),
            },
            &cfg(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_console() {
        dispatch_with(Command::Console, &cfg()).await.unwrap();
    }

    #[test]
    fn parses_comment_subcommands() {
        match parse_args(["aidea", "comment", "list", "src/a.rs"])
            .unwrap()
            .command
        {
            Command::Comment {
                cmd: CommentCmd::List { file },
            } => assert_eq!(file, "src/a.rs"),
            _ => panic!("expected comment list"),
        }
        match parse_args(["aidea", "comment", "add", "src/a.rs", "10", "--text", "guard here"])
            .unwrap()
            .command
        {
            Command::Comment {
                cmd: CommentCmd::Add { file, line, text },
            } => {
                assert_eq!(file, "src/a.rs");
                assert_eq!(line, 10);
                assert_eq!(text, "guard here");
            }
            _ => panic!("expected comment add"),
        }
        match parse_args(["aidea", "comment", "resolve", "c1"]).unwrap().command {
            Command::Comment {
                cmd: CommentCmd::Resolve { id },
            } => assert_eq!(id, "c1"),
            _ => panic!("expected comment resolve"),
        }
    }

    #[test]
    fn parses_lock_subcommands() {
        assert!(matches!(
            parse_args(["aidea", "lock", "acquire", "src/a.rs"]).unwrap().command,
            Command::Lock { cmd: LockCmd::Acquire { .. } }
        ));
        assert!(matches!(
            parse_args(["aidea", "lock", "release", "src/a.rs"]).unwrap().command,
            Command::Lock { cmd: LockCmd::Release { .. } }
        ));
        assert!(matches!(
            parse_args(["aidea", "lock", "show", "src/a.rs"]).unwrap().command,
            Command::Lock { cmd: LockCmd::Show { .. } }
        ));
    }

    #[test]
    fn parses_secret_subcommands() {
        match parse_args(["aidea", "secret", "set", "api_key", "s3cr3t"])
            .unwrap()
            .command
        {
            Command::Secret {
                cmd: SecretCmd::Set { name, value },
            } => {
                assert_eq!(name, "api_key");
                assert_eq!(value, "s3cr3t");
            }
            _ => panic!("expected secret set"),
        }
        match parse_args(["aidea", "secret", "get", "api_key"]).unwrap().command {
            Command::Secret {
                cmd: SecretCmd::Get { name },
            } => assert_eq!(name, "api_key"),
            _ => panic!("expected secret get"),
        }
    }

    #[tokio::test]
    async fn routes_comment_offline() {
        dispatch_with(
            Command::Comment {
                cmd: CommentCmd::List { file: "src/a.rs".into() },
            },
            &cfg(),
        )
        .await
        .unwrap();
        dispatch_with(
            Command::Comment {
                cmd: CommentCmd::Add {
                    file: "src/a.rs".into(),
                    line: 10,
                    text: "guard here".into(),
                },
            },
            &cfg(),
        )
        .await
        .unwrap();
        dispatch_with(
            Command::Comment {
                cmd: CommentCmd::Resolve { id: "c1".into() },
            },
            &cfg(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_lock_offline() {
        dispatch_with(
            Command::Lock {
                cmd: LockCmd::Acquire { file: "src/a.rs".into() },
            },
            &cfg(),
        )
        .await
        .unwrap();
        dispatch_with(
            Command::Lock {
                cmd: LockCmd::Release { file: "src/a.rs".into() },
            },
            &cfg(),
        )
        .await
        .unwrap();
    }
}
