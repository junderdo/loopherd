mod herdr;
mod ledger;
mod spawn;
mod worktree;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::herdr::CliHerdr;
use crate::ledger::FileLedger;
use crate::spawn::{SpawnRequest, spawn};

#[derive(Parser)]
#[command(version, about = "Herd coding agents across herdr sessions")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a named Claude agent in a new herdr tab and record it in the ledger.
    Spawn(SpawnArgs),
}

#[derive(Args)]
struct SpawnArgs {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    name: String,
    /// Run in a worktree for this branch, created if needed.
    #[arg(long)]
    worktree: Option<String>,
    /// Workspace label; defaults to $HERDR_WORKSPACE_ID.
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long, conflicts_with = "prompt_file")]
    prompt: Option<String>,
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Seconds to wait for the agent to become idle.
    #[arg(long, default_value_t = 60)]
    ready_timeout: u64,
    /// Passed through to claude.
    #[arg(last = true)]
    claude_args: Vec<String>,
}

fn main() -> ExitCode {
    let Command::Spawn(args) = Cli::parse().command;
    match run_spawn(args) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("loopherd spawn: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_spawn(args: SpawnArgs) -> Result<i32> {
    let prompt = match &args.prompt_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("prompt: read {}", path.display()))?,
        ),
        None => args.prompt,
    };
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("repo: {}", args.repo.display()))?;
    let req = SpawnRequest {
        repo,
        name: args.name,
        worktree: args.worktree,
        workspace: args.workspace,
        prompt,
        ready_timeout: Duration::from_secs(args.ready_timeout),
        claude_args: args.claude_args,
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let inherited = std::env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|v| !v.is_empty());
    let spawned = spawn(&CliHerdr, &mut ledger, &req, inherited.as_deref())?;
    let (message, code) = spawned.report();
    println!("{message}");
    Ok(code)
}
