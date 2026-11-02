mod feed;
mod herd;
mod herd_view;
mod herdr;
mod ledger;
mod matching;
mod prompt;
mod retire;
mod revive;
mod shell;
mod spawn;
mod status;
mod template;
mod tui;
mod worktree;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::feed::{FeedRequest, feed, lock};
use crate::herd::{AddRequest, Paths, add, dry_run, herds};
use crate::herd_view::Options;
use crate::herdr::CliHerdr;
use crate::ledger::FileLedger;
use crate::prompt::prompt;
use crate::retire::{retire, retire_and_close};
use crate::revive::{ReviveRequest, revive};
use crate::spawn::{Checkout, Prompt, SpawnRequest, spawn};
use crate::status::{Format, status};
use crate::worktree::{remote_timeout, repo_root};

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
    /// Spawn a herd member from a template in the herd's file.
    Add(AddArgs),
    /// List herd files with a usage line per template; exits 1 if any is invalid.
    Herds,
    /// Retire members whose template's done check exits 0, then fill the herd's
    /// free slots with members, one per task its queue's source prints.
    /// Exits 3 when a member is blocked at startup, 4 when the only errors are
    /// malformed source lines, and 1 on any other error.
    Feed {
        herd: String,
        /// Run the source and done checks and print what would happen; retire and spawn nothing.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait for each agent to become idle.
        #[arg(long, default_value_t = 60)]
        ready_timeout: u64,
    },
    /// Mark a ledger entry finished so revive leaves it alone. Without --close, changes only the ledger.
    Retire {
        name: String,
        /// Also close its tab, and remove its worktree and branch if clean and merged.
        /// Works on an already-retired entry, to finish or retry cleanup.
        #[arg(long)]
        close: bool,
    },
    /// Resume ledger agents after a herdr restart or a closed tab.
    Revive {
        /// Print what would happen; start nothing and leave the ledger alone.
        #[arg(long)]
        dry_run: bool,
        /// Only this ledger entry.
        #[arg(long)]
        name: Option<String>,
    },
    /// List ledger members by herd with herdr's live status; read-only.
    /// Exits 2 when herdr is unreachable, or 3 when its `agent list` reply is
    /// malformed, showing every status as unknown.
    Status {
        /// Also show retired entries and herds with only retired entries.
        #[arg(long)]
        all: bool,
        /// List live herdr agents missing from the ledger, not just count them.
        #[arg(long)]
        strays: bool,
        /// Print JSON. Its shape is unstable.
        #[arg(long)]
        json: bool,
    },
    /// The herd view as a live TUI, refreshed every 2s. Read-only except that
    /// Enter focuses the selected agent's pane in herdr.
    View {
        /// Start with retired entries shown (toggle with `a`).
        #[arg(long)]
        all: bool,
        /// Start with strays listed (toggle with `s`).
        #[arg(long)]
        strays: bool,
    },
    /// Send a ledger entry's stored prompt to its live agent, e.g. after spawn exited 3.
    Prompt {
        name: String,
        /// Send it even if it was already sent.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args)]
struct SpawnArgs {
    /// Defaults to the main checkout of the repo containing the current directory.
    #[arg(long)]
    repo: Option<PathBuf>,
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

#[derive(Args)]
struct AddArgs {
    herd: String,
    template: String,
    /// The template's params, in order.
    params: Vec<String>,
    /// Print the resolved member; create nothing.
    #[arg(long)]
    dry_run: bool,
    /// Seconds to wait for the agent to become idle.
    #[arg(long, default_value_t = 60)]
    ready_timeout: u64,
    /// Passed to claude after the herd's and the template's args.
    #[arg(last = true)]
    claude_args: Vec<String>,
}

fn main() -> ExitCode {
    let (command, result) = match Cli::parse().command {
        Command::Spawn(args) => ("spawn", run_spawn(args)),
        Command::Add(args) => ("add", run_add(args)),
        Command::Herds => ("herds", run_herds()),
        Command::Feed {
            herd,
            dry_run,
            ready_timeout,
        } => ("feed", run_feed(herd, dry_run, ready_timeout)),
        Command::Retire { name, close } => ("retire", run_retire(&name, close)),
        Command::Revive { dry_run, name } => ("revive", run_revive(dry_run, name)),
        Command::Prompt { name, force } => ("prompt", run_prompt(&name, force)),
        Command::Status { all, strays, json } => ("status", run_status(all, strays, json)),
        Command::View { all, strays } => ("view", run_view(all, strays)),
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("loopherd {command}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_retire(name: &str, close: bool) -> Result<i32> {
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    if !close {
        retire(&mut ledger, name, SystemTime::now())?;
        println!("retired {name}");
        return Ok(0);
    }
    let own_pane = std::env::var("HERDR_PANE_ID").ok();
    let mut lines = vec![];
    let result = retire_and_close(
        &CliHerdr,
        &mut ledger,
        name,
        SystemTime::now(),
        own_pane.as_deref(),
        &mut lines,
    );
    for line in lines {
        println!("{line}");
    }
    result.map(|()| 0)
}

fn run_revive(dry_run: bool, name: Option<String>) -> Result<i32> {
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let req = ReviveRequest {
        name,
        dry_run,
        settle_timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(250),
        start_timeout: Duration::from_secs(60),
    };
    let (message, code) = revive(&CliHerdr, &mut ledger, &req)?.report();
    if !message.is_empty() {
        println!("{message}");
    }
    Ok(code)
}

fn run_status(all: bool, strays: bool, json: bool) -> Result<i32> {
    use std::io::IsTerminal;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let format = match json {
        true => Format::Json,
        false => Format::Text {
            color: std::io::stdout().is_terminal(),
        },
    };
    let out = status(&CliHerdr, &ledger, Options { all, strays }, format)?;
    print!("{}", out.stdout);
    for line in out.stderr.lines() {
        eprintln!("loopherd status: {line}");
    }
    Ok(out.code)
}

fn run_view(all: bool, strays: bool) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    tui::run(CliHerdr, ledger, Options { all, strays })
}

fn run_prompt(name: &str, force: bool) -> Result<i32> {
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    prompt(&CliHerdr, &mut ledger, name, force)?;
    println!("prompted {name}");
    Ok(0)
}

fn run_spawn(args: SpawnArgs) -> Result<i32> {
    let prompt = match &args.prompt_file {
        Some(path) => Some(Prompt {
            source: format!("--prompt-file {}", path.display()),
            text: std::fs::read_to_string(path)
                .with_context(|| format!("prompt: read {}", path.display()))?,
        }),
        None => args.prompt.map(|text| Prompt {
            source: "--prompt".into(),
            text,
        }),
    };
    let repo = match &args.repo {
        Some(repo) => repo
            .canonicalize()
            .with_context(|| format!("repo: {}", repo.display()))?,
        None => repo_root(&std::env::current_dir().context("repo: current directory")?)
            .context("repo")?,
    };
    let req = SpawnRequest {
        repo,
        name: args.name,
        worktree: args.worktree.map(Checkout::Branch),
        workspace: args.workspace,
        prompt,
        ready_timeout: Duration::from_secs(args.ready_timeout),
        claude_args: args.claude_args,
        member_of: None,
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let inherited = inherited_workspace();
    let spawned = spawn(&CliHerdr, &mut ledger, &req, inherited.as_deref())?;
    let (message, code) = spawned.report(&req.name);
    println!("{message}");
    Ok(code)
}

fn run_add(args: AddArgs) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let req = AddRequest {
        herd: args.herd,
        template: args.template,
        params: args.params,
        claude_args: args.claude_args,
        ready_timeout: Duration::from_secs(args.ready_timeout),
    };
    let inherited = inherited_workspace();
    if args.dry_run {
        println!(
            "{}",
            dry_run(&CliHerdr, &paths, &req, inherited.as_deref())?
        );
        return Ok(0);
    }
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let (name, spawned) = add(&CliHerdr, &mut ledger, &paths, &req, inherited.as_deref())?;
    let (message, code) = spawned.report(&name);
    println!("{message}");
    Ok(code)
}

fn run_feed(herd: String, dry_run: bool, ready_timeout: u64) -> Result<i32> {
    let ledger_path = FileLedger::default_path().context("ledger")?;
    let _lock = lock(&ledger_path.with_file_name("feed.lock"))?;
    let paths = Paths::from_env().context("herd")?;
    let mut ledger = FileLedger::new(ledger_path);
    let req = FeedRequest {
        herd,
        dry_run,
        ready_timeout: Duration::from_secs(ready_timeout),
        command_timeout: remote_timeout()?,
        own_pane: std::env::var("HERDR_PANE_ID").ok(),
    };
    let inherited = inherited_workspace();
    let fed = feed(&CliHerdr, &mut ledger, &paths, &req, inherited.as_deref())?;
    for line in fed.lines {
        println!("{line}");
    }
    Ok(fed.code)
}

fn run_herds() -> Result<i32> {
    let (report, code) = herds(&Paths::from_env().context("herd")?)?;
    println!("{report}");
    Ok(code)
}

fn inherited_workspace() -> Option<String> {
    std::env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|v| !v.is_empty())
}
