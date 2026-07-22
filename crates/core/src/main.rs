//! `ide-core` binary: drive the M1 stack from the CLI.
//!
//! Subcommands:
//!   * `demo`  — run the ReAct loop in-process against the CLI host stub (T01).
//!   * `serve` — start the gRPC ProtoBus server (T02).

use clap::{Parser, Subcommand};
use ide_core::host::{CliHost, HostBridge};
use ide_core::planner::Planner;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "ide-core", version, about = "Agentic IDE Core (M1 slice)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the ReAct loop against the in-process CLI host stub.
    Demo {
        /// Goal passed to the planner.
        #[arg(default_value = "add retry logic to utils")]
        goal: String,
    },
    /// Start the gRPC ProtoBus server.
    Serve {
        /// Listen address.
        #[arg(default_value = "[::1]:50051")]
        addr: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Demo { goal } => {
            let bridge = Arc::new(HostBridge::new(Arc::new(CliHost::new())));
            let planner = Planner::with_defaults(bridge, 8);
            println!("== Running ReAct loop for goal: {goal} ==");
            let trace = planner.run(&goal).await?;
            println!("== Trace ==");
            for (i, s) in trace.steps.iter().enumerate() {
                println!(
                    "  [{i}] thought={} action={} obs={}",
                    s.thought, s.action, s.observation
                );
            }
            println!("== Final: {} ==", trace.final_answer);
        }
        Command::Serve { addr } => {
            ide_core::server::serve(&addr).await?;
        }
    }
    Ok(())
}
