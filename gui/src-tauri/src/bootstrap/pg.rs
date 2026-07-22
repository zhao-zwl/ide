//! PostgreSQL sidecar 编排：initdb → 启动 → 建库/角色 → 幂等应用 migrations。
//!
//! 与系统 PG（`/usr/local/var/postgres-17`）隔离，使用 AppData 内独立数据目录
//! `pgdata`。若 127.0.0.1:5432 已有可达的 PG（dev 场景使用外部 PG），则跳过
//! 拉起，仅补建库/角色/迁移，保证两种形态都能工作。

use crate::bootstrap::emit_progress;
use crate::bootstrap::sidecar::{app_data_dir, bin_path, resource_path, spawn_binary};
use crate::error::GuiError;
use crate::ipc::BootstrapPhase;
use crate::state::AppState;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Manager};

const PG_PORT: u16 = 5432;
const PG_USER: &str = "aidea";
const PG_DB: &str = "aidea";

/// 初始化并启动 bundled PostgreSQL（或被外部 PG 复用）。
pub async fn init_and_start(app: &AppHandle) -> Result<(), GuiError> {
    emit_progress(app, BootstrapPhase::Pg, 0.04, Some("检查 PostgreSQL 状态"));

    // 若外部 PG 已可达，直接复用（dev 友好）。
    if pg_ready(app) {
        emit_progress(app, BootstrapPhase::Pg, 0.2, Some("复用外部 PostgreSQL"));
        ensure_db_and_role(app)?;
        apply_migrations(app)?;
        emit_progress(app, BootstrapPhase::Pg, 0.35, Some("PostgreSQL 就绪"));
        return Ok(());
    }

    let postgres_bin = bin_path(app, "postgres");
    let initdb_bin = bin_path(app, "initdb");
    let pg_ctl_bin = bin_path(app, "pg_ctl");
    let pgdata = app_data_dir(app).join("pgdata");
    let _ = std::fs::create_dir_all(&pgdata);

    if !pgdata.join("PG_VERSION").exists() {
        emit_progress(app, BootstrapPhase::Pg, 0.08, Some("initdb 初始化数据目录"));
        run_initdb(&initdb_bin, &pgdata)?;
    }

    emit_progress(app, BootstrapPhase::Pg, 0.18, Some("启动 PostgreSQL"));
    // 用 pg_ctl 拉起常驻 postgres（日志写到 pgdata/postgres.log）。
    let log = pgdata.join("postgres.log");
    let pg_ctl = spawn_pg_ctl(&pg_ctl_bin, &postgres_bin, &pgdata, &log)?;
    // pg_ctl 会 fork 出 postgres 并退出；此处把 pg_ctl 句柄存下（监管用）。
    app.state::<AppState>().bootstrap.lock().unwrap().pg = Some(pg_ctl);

    wait_pg_ready(app).await?;
    ensure_db_and_role(app)?;
    apply_migrations(app)?;
    emit_progress(app, BootstrapPhase::Pg, 0.35, Some("PostgreSQL 就绪"));
    Ok(())
}

/// 运行 initdb（trust 鉴权，单租户本地部署）。
fn run_initdb(initdb_bin: &PathBuf, pgdata: &PathBuf) -> Result<(), GuiError> {
    let status = Command::new(initdb_bin)
        .args([
            "-D",
            pgdata.to_str().unwrap(),
            "-U",
            PG_USER,
            "-E",
            "UTF8",
            "--auth=trust",
        ])
        .stdout(std::process::Stdio::null)
        .stderr(std::process::Stdio::piped)
        .status()
        .map_err(|e| GuiError::bootstrap(format!("initdb 执行失败: {e}")))?;
    if !status.success() {
        return Err(GuiError::bootstrap("initdb 返回非零退出码"));
    }
    Ok(())
}

/// 用 pg_ctl 启动 postgres 常驻进程。
fn spawn_pg_ctl(
    pg_ctl_bin: &PathBuf,
    _postgres_bin: &PathBuf,
    pgdata: &PathBuf,
    log: &PathBuf,
) -> Result<std::process::Child, GuiError> {
    let child = Command::new(pg_ctl_bin)
        .args([
            "-D",
            pgdata.to_str().unwrap(),
            "-l",
            log.to_str().unwrap(),
            "-o",
            &format!("-p {PG_PORT} -c listen_addresses=127.0.0.1 -c unix_socket_directories={}", pgdata.to_str().unwrap()),
            "start",
        ])
        .stdout(std::process::Stdio::null)
        .stderr(std::process::Stdio::piped)
        .spawn()
        .map_err(|e| GuiError::bootstrap(format!("pg_ctl 拉起失败: {e}")))?;
    Ok(child)
}

/// 探测 PG 是否可达。
fn pg_ready(app: &AppHandle) -> bool {
    let psql = bin_path(app, "psql");
    if !psql.exists() {
        return false;
    }
    let out = Command::new(&psql)
        .args([
            "-h", "127.0.0.1",
            "-p", &PG_PORT.to_string(),
            "-U", PG_USER,
            "-d", "postgres",
            "-tAc", "SELECT 1",
        ])
        .env("PGPASSWORD", "aidea")
        .stdout(std::process::Stdio::null)
        .stderr(std::process::Stdio::null)
        .status();
    matches!(out, Ok(s) if s.success())
}

/// 等待 PG 就绪（轮询 SELECT 1，非阻塞）。
async fn wait_pg_ready(app: &AppHandle) -> Result<(), GuiError> {
    let psql = bin_path(app, "psql");
    for _ in 0..60 {
        if pg_ready(app) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = psql;
    Err(GuiError::bootstrap("PostgreSQL 启动后超时未就绪"))
}

/// 建角色 aidea 与库 aidea（已存在则忽略）。
fn ensure_db_and_role(app: &AppHandle) -> Result<(), GuiError> {
    let psql = bin_path(app, "psql");
    // 角色已在 initdb -U aidea 中创建；此处确保库存在。
    let _ = Command::new(&psql)
        .args([
            "-h", "127.0.0.1",
            "-p", &PG_PORT.to_string(),
            "-U", PG_USER,
            "-d", "postgres",
            "-c", "CREATE DATABASE aidea;",
        ])
        .env("PGPASSWORD", "aidea")
        .stdout(std::process::Stdio::null)
        .stderr(std::process::Stdio::null)
        .status();
    Ok(())
}

/// 按序幂等应用 `resources/migrations/*.sql`（ON_ERROR_STOP=0，容忍「已存在」）。
fn apply_migrations(app: &AppHandle) -> Result<(), GuiError> {
    let psql = bin_path(app, "psql");
    let migrations = resource_path(app, "resources/migrations");
    if !migrations.exists() {
        return Err(GuiError::bootstrap(format!(
            "migrations 目录缺失: {}",
            migrations.display()
        )));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&migrations)
        .map_err(|e| GuiError::bootstrap(format!("读取 migrations 失败: {e}")))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();

    for f in files {
        let status = Command::new(&psql)
            .args([
                "-h", "127.0.0.1",
                "-p", &PG_PORT.to_string(),
                "-U", PG_USER,
                "-d", PG_DB,
                "-v", "ON_ERROR_STOP=0",
                "-f",
                f.to_str().unwrap(),
            ])
            .env("PGPASSWORD", "aidea")
            .stdout(std::process::Stdio::null)
            .stderr(std::process::Stdio::piped)
            .status();
        match status {
            Ok(s) if s.success() => {
                tracing::info!("applied migration: {}", f.file_name().unwrap().to_string_lossy());
            }
            _ => {
                tracing::warn!(
                    "migration 部分应用（容忍幂等错误）: {}",
                    f.file_name().unwrap().to_string_lossy()
                );
            }
        }
    }
    Ok(())
}
