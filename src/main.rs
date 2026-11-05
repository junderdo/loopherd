mod context;
mod feed;
mod herd;
mod herd_view;
mod herdr;
mod install_hook;
mod lead;
mod ledger;
mod lock;
mod matching;
mod pause;
mod process;
mod prompt;
mod report;
mod restart;
mod retire;
mod revive;
mod session_hook;
mod shell;
mod skill;
mod spawn;
mod status;
mod stop_hook;
mod template;
mod tool_hook;
mod tui;
mod watch;
mod worktree;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::feed::{FeedRequest, feed};
use crate::herd::{AddRequest, Paths, add, dry_run, herds};
use crate::herd_view::Options;
use crate::herdr::{CliHerdr, socket_from_env};
use crate::lead::LeadRequest;
use crate::ledger::FileLedger;
use crate::prompt::prompt;
use crate::report::{Filter, Kind, Reporter, report, reports};
use crate::retire::{retire, retire_and_close};
use crate::revive::{ReviveRequest, revive};
use crate::spawn::{Checkout, Prompt, SHELL_TIMEOUT, SpawnRequest, spawn};
use crate::status::{Format, status};
use crate::worktree::{remote_timeout, repo_root};

#[derive(Parser)]
#[command(
    version,
    about = "Herd coding agents across herdr sessions",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Print the shepherd's and lead's guide as a Claude SKILL.md.
    #[arg(long, exclusive = true)]
    skill: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a named Claude agent in a new herdr tab and record it in the ledger.
    Spawn(SpawnArgs),
    /// Spawn a herd member from a template in the herd's file.
    Add(AddArgs),
    /// List herd files with a usage line per template; exits 1 if any is invalid.
    Herds,
    /// Start the herd's lead member, lead-<herd>, from its herd file's [lead] table,
    /// in a detached worktree of the same name.
    Lead(LeadArgs),
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
    /// Pause a herd: its `feed` retires finished members but spawns none. Prompts nobody.
    Pause {
        #[command(flatten)]
        herds: HerdsArg,
        /// Why, shown by status and view; replaces an existing pause's reason.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Lift a herd's pause and print the step that gets it workers again. Prompts nobody.
    Resume {
        #[command(flatten)]
        herds: HerdsArg,
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
    /// Give a member that wrote its handoff a fresh session in the same pane:
    /// sends it /clear, waits for the ledger to follow the new session, then
    /// prompts it to read its handoff. Refuses unless its last stop is finished.
    Restart {
        name: String,
        /// Skip the checks that it handed off and is finished since. A /clear sent
        /// mid-turn waits for that turn, then drops whatever the handoff doesn't
        /// record: that turn's work, background tasks and crons.
        #[arg(long)]
        force: bool,
    },
    /// Leave a report for the shepherd from the member running in this pane.
    /// The one command members run against the real state dir.
    Report {
        #[arg(long, value_enum, default_value_t = Kind::Note)]
        kind: Kind,
        /// Report as this member instead of the one whose pane is $HERDR_PANE_ID.
        #[arg(long = "as")]
        as_name: Option<String>,
        text: String,
    },
    /// Print members' unread reports, questions first, and mark them read.
    Reports {
        /// Only these members, retired ones included.
        names: Vec<String>,
        /// Only members of this herd.
        #[arg(long)]
        herd: Option<String>,
        /// Print every report, read or not, and mark none read.
        #[arg(long)]
        all: bool,
    },
    /// Claude's Stop and StopFailure hook: records the stop, read from the hook
    /// input on stdin, in the mailbox of the member running in this pane. Once per
    /// session past the member's context limit, it blocks the stop instead and asks
    /// for a handoff. Outside a member's pane it records nothing. Set up by install-hook.
    StopHook,
    /// Claude's SessionStart hook: when a member's Claude is cleared or resumed,
    /// points its ledger entry at the new session, read from the hook input on
    /// stdin. Outside a member's pane it changes nothing. Set up by install-hook.
    SessionHook,
    /// Claude's PreToolUse hook on Bash: in a lead's pane, denies a merge, install,
    /// rebase or force-push its herd file's switch leaves to the human, and any
    /// `loopherd restart --force`, read from the hook input on stdin, by printing a
    /// deny decision. Elsewhere it prints nothing.
    /// Always exits 0. Set up by install-hook.
    ToolHook,
    /// Add loopherd's Stop, StopFailure, SessionStart and PreToolUse hooks to Claude's
    /// user settings, calling this binary, and write the `--skill` guide as the loopherd
    /// skill; safe to re-run. Backs the settings file up before changing it.
    InstallHook {
        /// Take loopherd's hooks and skill out instead.
        #[arg(long)]
        remove: bool,
        /// Edit this file instead of $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Write the skill to <dir>/loopherd/SKILL.md; defaults to the skills directory
        /// beside the settings file.
        #[arg(long, value_name = "dir")]
        skills_dir: Option<PathBuf>,
    },
    /// Block until a member needs the shepherd, print what happened, and exit:
    /// an unread report (marked read), a stop from the Stop hook that leaves a
    /// member paused, finished or failed, or one going blocked or gone in
    /// herdr. Polls every 2s, read-only toward herdr. Run from a lead's pane it
    /// reads that lead's herd's workers; otherwise it is the shepherd's and
    /// reads leads and unled herds. One watch per reader at a time, and only
    /// with loopherd's Stop, StopFailure and SessionStart hooks installed. Run
    /// from no member's pane, it adds `shepherd handoff <n>k over <limit>k`
    /// once per session when its caller's Claude is past the shepherd's limit.
    Watch {
        /// Only members of these herds; a led herd narrows the shepherd to its lead.
        herds: Vec<String>,
        /// Give up after this many seconds, exiting 2 with no output unless a
        /// shepherd handoff line is due.
        #[arg(long)]
        timeout: Option<u64>,
        /// Check for the hook in this file instead of $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct HerdsArg {
    herd: Option<String>,
    /// Every herd with a herd file now; a herd added later starts unpaused.
    #[arg(long)]
    all: bool,
}

impl HerdsArg {
    fn herds(&self) -> pause::Herds<'_> {
        match &self.herd {
            Some(herd) => pause::Herds::One(herd),
            None => pause::Herds::All,
        }
    }
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
struct LeadArgs {
    herd: String,
    /// Print the resolved lead; create nothing.
    #[arg(long)]
    dry_run: bool,
    /// Seconds to wait for the agent to become idle.
    #[arg(long, default_value_t = 60)]
    ready_timeout: u64,
    /// Passed to claude after [lead].args, else the herd's args.
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
    let cli = Cli::parse();
    if cli.skill {
        print!("{}", skill::SKILL);
        return ExitCode::SUCCESS;
    }
    let Some(command) = cli.command else {
        Cli::command()
            .error(
                ErrorKind::MissingSubcommand,
                "a subcommand or --skill is required",
            )
            .exit();
    };
    let (command, result) = match command {
        Command::Spawn(args) => ("spawn", run_spawn(args)),
        Command::Add(args) => ("add", run_add(args)),
        Command::Herds => ("herds", run_herds()),
        Command::Lead(args) => ("lead", run_lead(args)),
        Command::Feed {
            herd,
            dry_run,
            ready_timeout,
        } => ("feed", run_feed(herd, dry_run, ready_timeout)),
        Command::Pause { herds, reason } => ("pause", run_pause(&herds, reason.as_deref())),
        Command::Resume { herds } => ("resume", run_resume(&herds)),
        Command::Retire { name, close } => ("retire", run_retire(&name, close)),
        Command::Revive { dry_run, name } => ("revive", run_revive(dry_run, name)),
        Command::Prompt { name, force } => ("prompt", run_prompt(&name, force)),
        Command::Restart { name, force } => ("restart", run_restart(&name, force)),
        Command::Status { all, strays, json } => ("status", run_status(all, strays, json)),
        Command::View { all, strays } => ("view", run_view(all, strays)),
        Command::Report {
            kind,
            as_name,
            text,
        } => ("report", run_report(kind, as_name, &text)),
        Command::Reports { names, herd, all } => ("reports", run_reports(names, herd, all)),
        Command::Watch {
            herds,
            timeout,
            settings,
        } => ("watch", run_watch(&herds, timeout, settings)),
        Command::StopHook => ("stop-hook", run_stop_hook()),
        Command::SessionHook => ("session-hook", run_session_hook()),
        Command::ToolHook => ("tool-hook", run_tool_hook()),
        Command::InstallHook {
            remove,
            settings,
            skills_dir,
        } => (
            "install-hook",
            run_install_hook(remove, settings, skills_dir),
        ),
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("loopherd {command}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_pause(herds: &HerdsArg, reason: Option<&str>) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let lines = pause::pause(&paths, herds.herds(), reason, SystemTime::now())?;
    lines.iter().for_each(|line| println!("{line}"));
    Ok(0)
}

fn run_resume(herds: &HerdsArg) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let lines = pause::resume(&paths, &ledger, herds.herds())?;
    lines.iter().for_each(|line| println!("{line}"));
    Ok(0)
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
        watch_dir: ledger.reports_dir()?,
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
    let paused = pause::dir()?;
    let out = status(&CliHerdr, &ledger, &paused, Options { all, strays }, format)?;
    print!("{}", out.stdout);
    for line in out.stderr.lines() {
        eprintln!("loopherd status: {line}");
    }
    Ok(out.code)
}

fn run_view(all: bool, strays: bool) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    tui::run(CliHerdr, ledger, pause::dir()?, Options { all, strays })
}

fn run_prompt(name: &str, force: bool) -> Result<i32> {
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    prompt(&CliHerdr, &mut ledger, name, force)?;
    println!("prompted {name}");
    Ok(0)
}

fn run_restart(name: &str, force: bool) -> Result<i32> {
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let state = ledger.dir()?;
    let own_pane = own_pane();
    let req = restart::RestartRequest {
        name,
        force,
        own_pane: own_pane.as_deref(),
        state: &state,
        settle_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(250),
    };
    println!(
        "{}",
        restart::restart(&CliHerdr, &mut ledger, &req, SystemTime::now())?
    );
    Ok(0)
}

fn run_report(kind: Kind, as_name: Option<String>, text: &str) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let own_pane = own_pane();
    let own_socket = socket_from_env();
    let who = Reporter {
        own_pane: own_pane.as_deref(),
        own_socket: own_socket.as_deref(),
        as_name: as_name.as_deref(),
    };
    report(
        &ledger,
        &ledger.reports_dir()?,
        &who,
        kind,
        text,
        SystemTime::now(),
    )?;
    Ok(0)
}

fn run_reports(names: Vec<String>, herd: Option<String>, all: bool) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let filter = Filter {
        names: &names,
        herd: herd.as_deref(),
        all,
    };
    let read = reports(&ledger, &ledger.reports_dir()?, &filter)?;
    for line in read.lines {
        println!("{line}");
    }
    for warning in read.warnings {
        eprintln!("loopherd reports: warning: {warning}");
    }
    Ok(0)
}

fn run_watch(herds: &[String], timeout: Option<u64>, settings: Option<PathBuf>) -> Result<i32> {
    let settings = match settings {
        Some(settings) => settings,
        None => install_hook::default_settings()?,
    };
    if !install_hook::installed(&settings)? {
        bail!(
            "loopherd's hooks are not all in {}, so stops could go unseen; run scripts/install",
            settings.display()
        );
    }
    let herd_paths = Paths::from_env().context("herd")?;
    for herd in herds {
        if !herd_paths.herds.join(format!("{herd}.toml")).is_file() {
            eprintln!("loopherd watch: warning: no herd file for {herd}");
        }
    }
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let own_pane = own_pane();
    let own_socket = socket_from_env();
    let req = watch::Request {
        herds,
        timeout: timeout.map(Duration::from_secs),
        poll: Duration::from_secs(2),
        own_pane: own_pane.as_deref(),
        own_socket: own_socket.as_deref(),
        herd_files: Some(&herd_paths),
        check_timeout: remote_timeout,
        shepherd_handoff: None,
    };
    let state = ledger.dir()?;
    let handoff = || shepherd_handoff(&herd_paths, &state);
    let req = watch::Request {
        shepherd_handoff: Some(&handoff),
        ..req
    };
    let woke = watch::watch(
        &CliHerdr,
        &ledger,
        &ledger.reports_dir()?,
        &req,
        &mut watch::RealClock::start(),
        &mut |warning| eprintln!("loopherd watch: {warning}"),
    )?;
    let Some(wake) = woke else {
        return Ok(2);
    };
    let mut out = std::io::stdout().lock();
    wake.lines
        .iter()
        .try_for_each(|line| writeln!(out, "{line}"))
        .and_then(|()| out.flush())
        .context("print the wake; left it unread")?;
    wake.mark_read()?;
    Ok(0)
}

/// The shepherd's handoff line when its Claude is past its limit; a failing
/// check warns and adds no line.
fn shepherd_handoff(paths: &Paths, state: &Path) -> Option<context::ShepherdHandoff> {
    let limit = context::shepherd_limit(&paths.herds.with_file_name("config.toml"));
    let limit = limit
        .inspect_err(|e| eprintln!("loopherd watch: warning: {e:#}"))
        .ok()?;
    let claude_dir = install_hook::claude_dir().ok()?;
    context::shepherd_handoff(&context::ShepherdCheck {
        procs: &process::ProcFs,
        claude_pid: std::env::var("CLAUDE_PID").ok().as_deref(),
        me: std::process::id(),
        claude_dir: &claude_dir,
        state,
        limit,
    })
}

fn run_stop_hook() -> Result<i32> {
    // Outside herdr there is no member to find; skip even the ledger read.
    let Some(pane) = own_pane() else {
        return Ok(0);
    };
    let mut input = vec![];
    std::io::stdin()
        .read_to_end(&mut input)
        .context("read hook input")?;
    let socket = socket_from_env();
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let pane = stop_hook::Pane {
        id: &pane,
        socket: socket.as_deref(),
        claude_pid: process::claude_pid_from_env(&process::ProcFs),
    };
    let reports = ledger.reports_dir()?;
    let state = ledger.dir()?;
    let handoff = |member: &ledger::AgentRecord| {
        let limit = Paths::from_env().and_then(|paths| herd::context_limit(&paths, member));
        let note = |text| {
            let entry = report::Entry::new(SystemTime::now(), report::Body::Note { text });
            report::append(&reports, &member.name, &entry)
        };
        context::handoff_block(member, &input, limit, &state, &note)
    };
    let stopped = stop_hook::stop_hook(
        &mut ledger,
        &process::ProcFs,
        &reports,
        &pane,
        &input,
        SystemTime::now(),
        &handoff,
        &mut |warning| eprintln!("loopherd stop-hook: {warning}"),
    )?;
    if let Some(stop_hook::Stopped::Blocked { reason, .. }) = stopped {
        println!(
            "{}",
            serde_json::json!({"decision": "block", "reason": reason})
        );
    }
    Ok(0)
}

fn run_session_hook() -> Result<i32> {
    let Some(pane) = own_pane() else {
        return Ok(0);
    };
    let mut input = vec![];
    std::io::stdin()
        .read_to_end(&mut input)
        .context("read hook input")?;
    let socket = socket_from_env();
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let pane = session_hook::Pane {
        id: &pane,
        socket: socket.as_deref(),
        claude_pid: process::claude_pid_from_env(&process::ProcFs),
    };
    session_hook::session_hook(
        &mut ledger,
        &process::ProcFs,
        &pane,
        &input,
        &mut |warning| eprintln!("loopherd session-hook: {warning}"),
    )?;
    Ok(0)
}

fn run_tool_hook() -> Result<i32> {
    let Some(pane) = own_pane() else {
        return Ok(0);
    };
    let Some(socket) = socket_from_env() else {
        return Ok(0);
    };
    let mut input = vec![];
    std::io::stdin()
        .read_to_end(&mut input)
        .context("read hook input")?;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let pane = tool_hook::Pane {
        id: &pane,
        socket: Some(&socket),
    };
    let switches = |herd: &str| herd::lead_switches(&Paths::from_env()?, herd);
    if let Some(reason) = tool_hook::tool_hook(&ledger, &pane, &input, &switches)? {
        println!("{}", tool_hook::deny(&reason));
    }
    Ok(0)
}

fn run_install_hook(
    remove: bool,
    settings: Option<PathBuf>,
    skills_dir: Option<PathBuf>,
) -> Result<i32> {
    let settings = match settings {
        Some(path) => path,
        None => install_hook::default_settings()?,
    };
    let skills_dir = match skills_dir {
        Some(dir) => dir,
        None => settings.with_file_name("skills"),
    };
    // First, so a skill that can't be written leaves the settings untouched.
    let skill = skill::path(&skills_dir);
    match remove {
        true if skill::remove(&skills_dir)? => println!("removed {}", skill.display()),
        true => println!("no skill at {}", skill.display()),
        false if skill::install(&skills_dir)? => println!("installed {}", skill.display()),
        false => println!("{} already up to date", skill.display()),
    }
    let outcome = match remove {
        true => install_hook::remove(&settings, SystemTime::now())?,
        false => {
            let exe = std::env::current_exe().context("find this binary")?;
            install_hook::install(&settings, &exe, SystemTime::now())?
        }
    };
    let done = match remove {
        true => "removed loopherd's Stop, StopFailure, SessionStart and PreToolUse hooks from",
        false => "installed loopherd's Stop, StopFailure, SessionStart and PreToolUse hooks in",
    };
    match outcome {
        install_hook::Outcome::Unchanged => println!("{} already up to date", settings.display()),
        install_hook::Outcome::Changed { backup } => {
            println!("{done} {}", settings.display());
            if let Some(backup) = backup {
                println!("backup {}", backup.display());
            }
        }
    }
    Ok(0)
}

fn own_pane() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty())
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
        shell_timeout: SHELL_TIMEOUT,
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
    let added = add(&CliHerdr, &mut ledger, &paths, &req, inherited.as_deref())?;
    let (message, code) = added.spawned.report(&added.name);
    println!("{message}");
    if let Some(warning) = added.warning {
        eprintln!("loopherd add: warning: {warning}");
    }
    Ok(code)
}

fn run_lead(args: LeadArgs) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let req = LeadRequest {
        herd: args.herd,
        claude_args: args.claude_args,
        ready_timeout: Duration::from_secs(args.ready_timeout),
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let inherited = inherited_workspace();
    let (message, code) = lead::run(
        &CliHerdr,
        &mut ledger,
        &paths,
        &req,
        args.dry_run,
        inherited.as_deref(),
    )?;
    println!("{message}");
    Ok(code)
}

fn run_feed(herd: String, dry_run: bool, ready_timeout: u64) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    // A dry run changes nothing, so it must not block a real one.
    let _lock = match dry_run {
        true => None,
        false => Some(paths.lock_feed()?),
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
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
