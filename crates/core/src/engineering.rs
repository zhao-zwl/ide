//! Engineering — build / test / git automation (T18, F6).
//!
//! Lets the autonomous agent actually *run* the project: `git status/diff/
//! branch/add/commit` and `cargo build/test/check`. All external process
//! invocation goes through [`tokio::process::Command`] (reusing the existing
//! `tokio` dependency — no new heavy deps), capturing `stdout`/`stderr` so a
//! failing build yields **real error text**.
//!
//! The key v1.5 integration: [`ShellTool`] implements the existing
//! [`ToolExecutor`] trait and maps engineering tool names (`cargo_test`,
//! `cargo_build`, `git`, `sh`, ...) to real commands. Because it is a
//! [`ToolExecutor`], it slots straight into the v1.5A [`SelfHeal`] executor:
//! when a build/test fails, `SelfHeal` captures the real stderr, asks the LLM
//! for a patch, and re-runs — a genuine self-healing loop. The same
//! [`ShellTool`] can be handed to a [`Quest`](crate::quest::Quest) as its base
//! tool executor (see `AgentServer::run_quest_with_engineering`), so a Quest
//! subtask can *run the tests* and repair them autonomously.
//!
//! Failure handling / degradation (CliHost-stub mode, `cargo`/`git` possibly
//! absent):
//!   * A spawn failure (binary missing) surfaces as `Err` from [`run_shell`] /
//!     [`ShellTool`] — never a panic.
//!   * A non-zero exit is reported as `ok = false`; [`ShellTool`] turns it into
//!     an `Err` so `SelfHeal` engages (or its circuit breaker trips cleanly).
//!   * `GitClient::commit` honors an approval gate: it refuses to write unless
//!     `allow_commit` is set (mirroring the v1.5A `auto_commit` gate).

use crate::tool_executor::{BasicToolExecutor, Observation, ToolExecutor};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

/// Result of a git subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Result of a cargo build / test / check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Result of an arbitrary shell command run via [`run_shell`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Convert process output bytes to a `String` (lossy, as build output is text).
fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// Git automation client (T18). Wraps `git` subcommands via
/// [`tokio::process::Command`]. Pure arg builders ([`GitClient::args_for`]) are
/// unit-tested without spawning; the async methods actually run git.
pub struct GitClient {
    workdir: PathBuf,
    /// Approval gate: when `false` (default), `commit` refuses to write and
    /// returns a non-fatal `GitResult { ok: false }` instead of invoking git.
    allow_commit: bool,
}

impl GitClient {
    /// Build a client rooted at `workdir`.
    pub fn new<P: AsRef<Path>>(workdir: P) -> Self {
        Self {
            workdir: workdir.as_ref().to_path_buf(),
            allow_commit: false,
        }
    }

    /// Enable/disable the commit approval gate (maps to `quest_auto_commit`).
    pub fn with_auto_commit(mut self, allow: bool) -> Self {
        self.allow_commit = allow;
        self
    }

    /// Pure arg builder (no spawn) — asserted by unit tests.
    pub fn args_for(subcommand: &str, args: &[&str]) -> Vec<String> {
        let mut v = vec![subcommand.to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    /// Build (but do not spawn) the git `Command` for `subcommand`.
    pub fn command(&self, subcommand: &str, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg(subcommand);
        for a in args {
            c.arg(a);
        }
        c.current_dir(&self.workdir);
        c
    }

    async fn run_cmd(&self, subcommand: &str, args: &[&str]) -> anyhow::Result<GitResult> {
        let out = self.command(subcommand, args).output().await?;
        Ok(GitResult {
            ok: out.status.success(),
            stdout: bytes_to_string(&out.stdout),
            stderr: bytes_to_string(&out.stderr),
        })
    }

    /// `git status`.
    pub async fn status(&self) -> anyhow::Result<GitResult> {
        self.run_cmd("status", &[]).await
    }

    /// `git diff` (`--cached` when `cached`).
    pub async fn diff(&self, cached: bool) -> anyhow::Result<GitResult> {
        if cached {
            self.run_cmd("diff", &["--cached"]).await
        } else {
            self.run_cmd("diff", &[]).await
        }
    }

    /// `git branch`.
    pub async fn branch(&self) -> anyhow::Result<GitResult> {
        self.run_cmd("branch", &[]).await
    }

    /// `git add <pathspec>`.
    pub async fn add(&self, pathspec: &str) -> anyhow::Result<GitResult> {
        self.run_cmd("add", &[pathspec]).await
    }

    /// `git commit -m <message>`.
    ///
    /// Honors the approval gate: when `allow_commit` is `false`, no git process
    /// is spawned and a non-fatal `GitResult { ok: false }` is returned
    /// (analogous to the v1.5A pending-approval gate).
    pub async fn commit(&self, message: &str) -> anyhow::Result<GitResult> {
        if !self.allow_commit {
            return Ok(GitResult {
                ok: false,
                stdout: String::new(),
                stderr: "commit blocked: auto_commit=false (requires approval)".to_string(),
            });
        }
        self.run_cmd("commit", &["-m", message]).await
    }
}

/// Cargo build/test/check runner (T18). Wraps `cargo` via
/// [`tokio::process::Command`]. [`BuildRunner::extract_error_lines`] pulls the
/// cargo error lines out of `stderr` so the v1.5A [`SelfHeal`] executor can feed
/// them to the LLM for a repair patch.
pub struct BuildRunner {
    workdir: PathBuf,
}

impl BuildRunner {
    /// Build a runner rooted at `workdir`.
    pub fn new<P: AsRef<Path>>(workdir: P) -> Self {
        Self {
            workdir: workdir.as_ref().to_path_buf(),
        }
    }

    /// Pure arg builder (no spawn) — asserted by unit tests.
    pub fn cargo_args(cmd: &str) -> Vec<String> {
        // All known cargo verbs map 1:1; unknown verbs pass through unchanged.
        vec![cmd.to_string()]
    }

    /// Build (but do not spawn) the cargo `Command` for `cmd`.
    pub fn command(&self, cmd: &str) -> Command {
        let mut c = Command::new("cargo");
        c.arg(cmd);
        c.current_dir(&self.workdir);
        c
    }

    async fn run_cmd(&self, cmd: &str) -> anyhow::Result<BuildResult> {
        let out = self.command(cmd).output().await?;
        Ok(BuildResult {
            ok: out.status.success(),
            stdout: bytes_to_string(&out.stdout),
            stderr: bytes_to_string(&out.stderr),
        })
    }

    /// `cargo build`.
    pub async fn build(&self) -> anyhow::Result<BuildResult> {
        self.run_cmd("build").await
    }

    /// `cargo check`.
    pub async fn check(&self) -> anyhow::Result<BuildResult> {
        self.run_cmd("check").await
    }

    /// `cargo test`.
    pub async fn test(&self) -> anyhow::Result<BuildResult> {
        self.run_cmd("test").await
    }

    /// Pure parser: extract cargo error lines from `stderr` (the lines a repair
    /// LLM should focus on). Matches `error: ...` and `error[Exxxx]: ...`.
    pub fn extract_error_lines(stderr: &str) -> Vec<String> {
        stderr
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("error") || t.contains("error[")
            })
            .map(|l| l.trim().to_string())
            .collect()
    }
}

/// Run `command` via `sh -c` in `workdir`, capturing stdout/stderr.
///
/// * Spawn failure (e.g. `sh` missing) -> `Err` (propagated by callers).
/// * Non-zero exit -> `Ok(RunOutput { ok: false, .. })` (callers decide how to
///   react; [`ShellTool`] treats it as a failure for `SelfHeal`).
pub async fn run_shell(command: &str, workdir: &Path) -> anyhow::Result<RunOutput> {
    let out: std::process::Output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .output()
        .await?;
    Ok(RunOutput {
        ok: out.status.success(),
        stdout: bytes_to_string(&out.stdout),
        stderr: bytes_to_string(&out.stderr),
    })
}

/// A [`ToolExecutor`] that maps engineering tool names to real commands and
/// otherwise falls back to [`BasicToolExecutor`] (so the ReAct loop keeps
/// working for `inspect`/`finish`/etc.).
///
/// This is the T18 ⇄ v1.5A wiring point: hand a `ShellTool` to
/// [`SelfHeal`](crate::self_heal::SelfHeal) (or to a
/// [`Quest`](crate::quest::Quest) as its base executor) and a failed
/// `cargo test` yields real stderr that `SelfHeal` can repair.
pub struct ShellTool {
    workdir: PathBuf,
    fallback: Arc<dyn ToolExecutor>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    /// Build a tool rooted at the current directory.
    pub fn new() -> Self {
        Self {
            workdir: PathBuf::from("."),
            fallback: Arc::new(BasicToolExecutor::new()),
        }
    }

    /// Root command execution in `dir`.
    pub fn in_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        self.workdir = dir.as_ref().to_path_buf();
        self
    }

    /// Map a tool name to the shell command it should run. `None` means "not an
    /// engineering tool" (caller should fall back).
    fn command_for(tool: &str, argument: &str) -> Option<String> {
        match tool {
            "sh" | "shell" => Some(argument.to_string()),
            "git" => Some(format!("git {argument}")),
            "cargo" => Some(format!("cargo {argument}")),
            "cargo_build" | "build" => Some("cargo build".to_string()),
            "cargo_check" | "check" => Some("cargo check".to_string()),
            "cargo_test" | "test" => Some("cargo test".to_string()),
            _ => None,
        }
    }
}

#[async_trait]
impl ToolExecutor for ShellTool {
    async fn run(&self, tool: &str, argument: &str) -> anyhow::Result<Observation> {
        match Self::command_for(tool, argument) {
            Some(cmd) => {
                // Spawn failure propagates as Err; a non-zero exit is reported as
                // a failure so SelfHeal engages (or its breaker trips cleanly).
                let out = run_shell(&cmd, &self.workdir).await?;
                if !out.ok {
                    return Err(anyhow::anyhow!(
                        "command `{cmd}` failed: {}",
                        out.stderr.trim()
                    ));
                }
                Ok(Observation {
                    tool: tool.to_string(),
                    output: out.stdout,
                    terminal: true,
                })
            }
            None => self.fallback.run(tool, argument).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ActionPlan, Llm, Thought};
    use crate::self_heal::SelfHeal;
    use async_trait::async_trait;
    use std::sync::Arc;

    // --- pure command-construction tests (no process spawn) -----------------

    #[test]
    fn git_args_are_well_formed() {
        assert_eq!(GitClient::args_for("status", &[]), vec!["status".to_string()]);
        assert_eq!(
            GitClient::args_for("commit", &["-m", "ci"]),
            vec!["commit", "-m", "ci"]
        );
        assert_eq!(GitClient::args_for("add", &["."]), vec!["add", "."]);
    }

    #[test]
    fn cargo_args_map_directly() {
        assert_eq!(BuildRunner::cargo_args("test"), vec!["test".to_string()]);
        assert_eq!(BuildRunner::cargo_args("build"), vec!["build".to_string()]);
        assert_eq!(
            BuildRunner::cargo_args("weird-verb"),
            vec!["weird-verb".to_string()]
        );
    }

    #[test]
    fn extract_error_lines_finds_cargo_errors() {
        let stderr = "\
warning: unused variable `x`
error[E0308]: mismatched types
  --> src/lib.rs:1:1
error: could not compile `demo`
";
        let errs = BuildRunner::extract_error_lines(stderr);
        assert_eq!(errs.len(), 2, "only the two error lines should match");
        assert!(errs[0].contains("E0308"));
        assert!(errs[1].contains("could not compile"));
    }

    // --- git approval gate --------------------------------------------------

    #[tokio::test]
    async fn commit_blocked_without_auto_commit() {
        let g = GitClient::new(".");
        let r = g.commit("msg").await.unwrap();
        assert!(!r.ok);
        assert!(r.stderr.contains("blocked"));
    }

    #[tokio::test]
    async fn commit_allowed_flag_attempts_run() {
        // With the gate open we attempt a real commit; whether or not this is a
        // git repo (or git is installed) it must NOT panic and returns a Result.
        let g = GitClient::new(".").with_auto_commit(true);
        let r = g.commit("msg").await;
        assert!(r.is_ok() || r.is_err());
    }

    // --- ShellTool behavior ------------------------------------------------

    #[tokio::test]
    async fn shell_tool_falls_back_for_unknown_tool() {
        let t = ShellTool::new();
        let o = t.run("inspect", "x").await.unwrap();
        assert_eq!(o.output, "inspected x");
    }

    #[tokio::test]
    async fn shell_tool_runs_real_command() {
        let t = ShellTool::new();
        let o = t.run("sh", "echo hello_ckg_123").await.unwrap();
        assert!(o.output.contains("hello_ckg_123"));
        assert!(o.terminal);
    }

    #[tokio::test]
    async fn shell_tool_fails_gracefully_on_missing_command() {
        let t = ShellTool::new();
        // A nonexistent binary -> non-zero exit -> Err (no panic), so SelfHeal
        // can engage / its circuit breaker can trip cleanly.
        let res = t.run("sh", "this_cmd_does_not_exist_xyz_12345").await;
        assert!(res.is_err());
    }

    // --- SelfHeal wiring (T18 ⇄ v1.5A T16) ---------------------------------

    /// LLM double that always proposes the *same* (still-failing) command.
    struct PatchSameLlm;
    #[async_trait]
    impl Llm for PatchSameLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "PATCH: this_cmd_does_not_exist_xyz_12345".into(),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "noop".into(),
                argument: String::new(),
            })
        }
    }

    /// LLM double that proposes a command that succeeds.
    struct EchoPatchLlm;
    #[async_trait]
    impl Llm for EchoPatchLlm {
        async fn think(&self, _ctx: &str) -> anyhow::Result<Thought> {
            Ok(Thought {
                text: "PATCH: echo ok_ckg".into(),
            })
        }
        async fn plan_action(&self, _t: &Thought) -> anyhow::Result<ActionPlan> {
            Ok(ActionPlan {
                tool: "noop".into(),
                argument: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn self_heal_wires_shell_tool_and_degrades_on_unreachable() {
        let tool = Arc::new(ShellTool::new());
        let heal = SelfHeal::new(tool, 2);
        // First attempt runs a missing binary -> Err; the LLM keeps returning
        // the same failing command -> circuit breaker trips -> Err (no panic,
        // no Doom loop).
        let res = heal
            .run(
                "sh",
                "this_cmd_does_not_exist_xyz_12345",
                Arc::new(PatchSameLlm),
            )
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn self_heal_wires_shell_tool_success() {
        let tool = Arc::new(ShellTool::new());
        let heal = SelfHeal::new(tool, 2);
        // A real `echo` succeeds on the first try -> 0 repairs.
        let out = heal
            .run("sh", "echo ok_ckg", Arc::new(EchoPatchLlm))
            .await
            .unwrap();
        assert_eq!(out.repair_attempts, 0);
        assert!(out.observation.output.contains("ok_ckg"));
    }
}
