mod broker;
mod config;
mod context;
mod continuation;
mod credentials;
mod doctor;
mod feed;
mod github;
mod handoff;
mod herd;
mod herd_view;
mod herdr;
mod init;
mod install_hook;
mod lead;
mod ledger;
mod line_ending;
mod lock;
mod matching;
mod merge_alert;
mod merge_guard;
mod notify;
mod pane_event;
mod pane_label;
mod pause;
mod pr_link;
mod process;
mod prompt;
mod read;
mod release;
mod report;
mod restart;
mod retire;
mod revive;
mod sandbox;
mod session_hook;
mod setup;
mod shell;
mod shepherd_pane;
mod shepherd_restart;
mod shepherd_resume;
mod skill;
mod spawn;
mod stats;
mod status;
mod stop_hook;
mod subagent_view;
mod tell;
mod template;
#[cfg(test)]
mod test_fakes;
mod tool_hook;
mod tui;
mod usage;
// build.rs includes it; here only for its tests.
#[cfg(test)]
mod version;
mod view_pane;
mod watch;
mod worktree;

use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::herd::{Paths, SwitchValues, herds};
use crate::herd_view::Options;
use crate::herdr::events::SocketSubscriber;
use crate::herdr::{CliHerdr, Herdr, socket_from_env};
use crate::lead::LeadRequest;
use crate::ledger::{FileLedger, Ledger, Process};
use crate::process::Procs;
use crate::report::{Filter, Kind, Reporter, report, reports};
use crate::revive::{ReviveRequest, revive};
use crate::spawn::{Checkout, Prompt, SHELL_TIMEOUT, SpawnRequest, spawn};
use crate::status::{Format, Queues};
use crate::worktree::{REMOTE_TIMEOUT, remote_timeout, repo_root};

#[derive(Parser)]
#[command(
    version = env!("LOOPHERD_VERSION"),
    about = "Herd coding agents across herdr sessions",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Print the shepherd's and lead's guide as a Claude SKILL.md.
    #[arg(long)]
    skill: bool,
    /// With --skill, print only the `## <SECTION>` section, e.g. `lead`.
    #[arg(long, requires = "skill")]
    section: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a named Claude agent in a new herdr tab and record it in the ledger.
    Spawn(SpawnArgs),
    /// Spawn a herd member from a template in the herd's file.
    Add(AddArgs),
    /// Write a herd for the repo around the current directory: its herd file
    /// and lead prompt, plus any missing GitHub issue labels, and, uncommitted,
    /// any missing tracker doc, docs/agents/loopherd.md and AGENTS.md loopherd
    /// section. Infers the repo, its default branch, the gh account matching
    /// origin's owner when the herd has a GitHub repo (only a GitHub tracker
    /// needs one) and checks from Cargo.toml and package.json. On a terminal (stdin and stderr, a herdr
    /// pane included) it asks for each with that as the default, asking again
    /// for a taken herd name or an existing lead prompt, and confirms before
    /// writing; the repo, default branch and confirm questions have no flag.
    /// Without one it refuses, changing nothing, when the herd file exists,
    /// or the lead prompt does without --no-lead. Either way it refuses a
    /// repo another herd runs.
    Init(InitArgs),
    /// List herd files with a usage line per template; exits 1 if any is invalid.
    Herds {
        /// Check for the prompt hook in this file instead of $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Read or edit one value in a herd file, keeping its comments and layout.
    /// Keys are dotted paths such as `queue.max`, `lead.merge` or
    /// `template.card.args`.
    Herd {
        #[command(subcommand)]
        action: HerdAction,
    },
    /// Show config.toml's [lead_defaults], each lead's switches wherever its herd
    /// file's [lead] leaves one unset, as one `<switch> on|off` line each.
    /// Prints nothing when config.toml has no [lead_defaults].
    LeadDefaults {
        /// Write [lead_defaults] with these switches on and the rest off: a comma
        /// list such as `merge,install`, or `none`. Keeps config.toml's other
        /// keys and comments.
        #[arg(long, value_name = "SWITCHES")]
        set: Option<String>,
    },
    /// Read or edit one of config.toml's settings, keeping its comments and
    /// layout: `context_window`, `default_context_limit` or
    /// `shepherd.context_limit`, each a number of tokens,
    /// `lead_defaults.review_rounds`, each lead's review-round limit,
    /// `shepherd.self_restart` (`true` or `false`), or
    /// `view.theme`, `loopherd view`'s palette: `purple-orange`, `phosphor`
    /// or `catppuccin`.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Set config.toml's machine-wide settings: the lead defaults, then the
    /// context window and the handoff limits. On a terminal (/dev/tty, so it
    /// works under `curl … | sh`) it asks each group while config.toml leaves
    /// it unset, current values as defaults, Enter keeping a value. Without
    /// one it asks nothing and names the flags for whatever is unset. Naming
    /// a group's flag handles just that group: set with a value, asked without.
    Setup(SetupArgs),
    /// Set up the split credentials of a herd whose lead has merge off and
    /// `merge_guard = "credentials"` (D83, D95), as your own gh login: makes
    /// the lead's deploy key in the config dir if missing, adds it to the
    /// herd's GitHub repo with write access, creates or updates the rulesets
    /// `loopherd: default branch` (updates and deletions only through a PR
    /// merged by an admin) and `loopherd: no force-push` (deploy keys bypass
    /// it only with `rebase` on), then says how to create and store the
    /// lead's token, which GitHub makes only in the web UI. Safe to re-run.
    MergeGuard { herd: String },
    /// Start the herd's lead member, lead-<herd>, from its herd file's [lead] table,
    /// in a detached worktree of the same name.
    Lead(LeadArgs),
    /// Retire members whose template's done check exits 0, then fill the herd's
    /// free slots with members, one per task its queue's source prints.
    /// Exits 3 when a member is blocked at startup, 4 when the only errors are
    /// malformed source lines, and 1 on any other error.
    Feed {
        herd: String,
        /// Run the source and done checks and print what would happen, with the source's
        /// stderr notes; retire and spawn nothing.
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
    /// Refuses a member herdr shows working or blocked, or shows idle with no
    /// done, question or handoff report since its last start, unless --force.
    Retire {
        name: String,
        /// Also close its tab, and remove its worktree and branch if clean and merged.
        /// Works on an already-retired entry, to finish or retry cleanup.
        #[arg(long)]
        close: bool,
        /// Retire it even if it may still be at work.
        #[arg(long)]
        force: bool,
    },
    /// Let `feed` spawn a retired member's name again, for a task it left
    /// unfinished. Deletes its local branch and worktree so the new member
    /// starts afresh, refusing while the branch has commits not on the base
    /// unless told to keep or drop them, and deletes its branch on origin,
    /// refusing while an open PR is on it unless told to drop it. Marks the
    /// name released only once origin's branch is gone, so feed skips it
    /// until a rerun finishes. Refuses a member not retired or whose tab
    /// may be open.
    Release {
        name: String,
        /// Leave its branch and worktree; the new member starts from them.
        #[arg(long, conflicts_with = "drop_branch")]
        keep_branch: bool,
        /// Delete its branch and worktree even with commits not on the base,
        /// or an open PR on its origin branch.
        #[arg(long)]
        drop_branch: bool,
    },
    /// Resume ledger agents after a herdr restart or a closed tab.
    Revive {
        /// Print what would happen; start nothing and leave the ledger alone.
        #[arg(long)]
        dry_run: bool,
        /// Only this ledger entry.
        #[arg(long)]
        name: Option<String>,
        /// As herdr's startup hook: nothing unless config.toml sets
        /// [herdr_plugin] revive_on_startup = true, then only entries recorded
        /// on this herdr's socket, noting what it did to each lead whose watch isn't running.
        #[arg(long, conflicts_with = "name")]
        startup: bool,
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
    /// How each herd's work flowed: per retired member, time to its first
    /// `done` report, review rounds, time waiting on merge and in total, and
    /// tokens: `in` read (input + cache write + cache read) and `out` written,
    /// over its sessions and their subagents. `-` when retire recorded none and
    /// the transcripts are gone, the mailbox can't be read, or Claude's dir
    /// can't be found. The `outcome` column is how the run ended: `merged`
    /// (feed retired it with its template's `merged` check passing), `closed`
    /// (feed retired it without that), `released` (`loopherd release`),
    /// `retired` (`loopherd retire`, with or without `--close`), or `-` for a
    /// run retired before outcomes were recorded. A summary per herd counts
    /// each outcome and gives medians and maxima over `merged` runs only,
    /// repeated per template within the herd; open members are listed apart,
    /// and a lead overhead line gives the lead's tokens within `--since` and
    /// `--until`; the lead gets no row. To judge a template or prompt change,
    /// run it over the window before and the window after. Read-only, and
    /// marks no report read.
    Stats {
        /// Only this herd.
        #[arg(long)]
        herd: Option<String>,
        /// Only members started on or after this date (2026-11-01, midnight UTC) or RFC 3339
        /// time with `Z` or an offset.
        #[arg(long, value_parser = stats::parse_since)]
        since: Option<SystemTime>,
        /// Only members started before this, in `--since`'s forms, so a date leaves
        /// that day out; with `--since` it bounds a window, to compare one against
        /// another.
        #[arg(long, value_parser = stats::parse_since)]
        until: Option<SystemTime>,
        /// Print JSON, with tokens split per model into input, cache write, cache
        /// read and output.
        #[arg(long)]
        json: bool,
    },
    /// The herd view as a live TUI, refreshed every 2s. Read-only except that
    /// Enter focuses the selected agent's pane in herdr, `t` saves the theme it
    /// switches to as `view.theme` in config.toml, and `S` focuses the
    /// shepherd or, run in a pane, starts one below the view.
    View {
        /// Start with retired entries shown (toggle with `a`).
        #[arg(long)]
        all: bool,
        /// Start with strays listed (toggle with `s`).
        #[arg(long)]
        strays: bool,
        /// Open the view in a herdr pane right of the shepherd's (else of
        /// $HERDR_PANE_ID's, else in the plugin's popup) instead of here.
        #[arg(long, conflicts_with_all = ["all", "strays"])]
        beside_shepherd: bool,
    },
    /// herdr's link handler for a Ctrl-clicked GitHub PR: shows the open member
    /// that owns it in the plugin's popup, found by a report naming the PR or
    /// by the PR's head branch, and opens any other link in the browser.
    Pr {
        /// The clicked link; defaults to $HERDR_PLUGIN_CLICKED_URL.
        url: Option<String>,
        /// Be the popup: print $LOOPHERD_PR_MEMBER's card for the PR at
        /// $LOOPHERD_PR_URL and wait for q or Escape.
        #[arg(long, conflicts_with = "url")]
        popup: bool,
    },
    /// Send a ledger entry's stored prompt to its live agent, e.g. after spawn exited 3.
    Prompt {
        name: String,
        /// Send it even if it was already sent.
        #[arg(long)]
        force: bool,
    },
    /// Message a live member, refusing while its pane shows a subagent's view,
    /// then confirm it went working.
    Tell {
        name: String,
        #[arg(
            required_unless_present = "text_file",
            conflicts_with = "text_file",
            allow_hyphen_values = true
        )]
        text: Option<String>,
        #[arg(long)]
        text_file: Option<PathBuf>,
    },
    /// Print a live member's last lines of output, unwrapped, as `herdr agent
    /// read --source recent-unwrapped` does. Read-only.
    Read {
        name: String,
        /// How many lines, up to 1000.
        #[arg(long, default_value_t = read::DEFAULT_LINES as u64,
              value_parser = clap::value_parser!(u64).range(1..=read::MAX_READ_LINES as u64))]
        lines: u64,
    },
    /// Give a member that wrote its handoff a fresh session in the same pane:
    /// sends it /clear, waits for the ledger to follow the new session, then
    /// prompts it to read its handoff. Refuses unless its last stop is finished.
    Restart {
        #[arg(required_unless_present = "own")]
        name: Option<String>,
        /// Skip the checks that it handed off and is finished since. A /clear sent
        /// mid-turn waits for that turn, then drops whatever the handoff doesn't
        /// record: that turn's work, background tasks and crons.
        #[arg(long)]
        force: bool,
        /// Restart the lead in this pane once its turn ends: starts a detached
        /// waiter that runs the plain restart after its stop since the handoff
        /// reads finished, giving up after 30 minutes with a note. From the
        /// shepherd's recorded pane, with `[shepherd] self_restart` on, it also
        /// waits until the human leaves the pane alone for 5 minutes, and any
        /// prompt there cancels it.
        #[arg(long = "self", id = "own", conflicts_with_all = ["name", "force"])]
        own: bool,
    },
    /// The detached waiter `restart --self` starts.
    #[command(hide = true)]
    RestartWait { name: String },
    /// The detached broker `lead` and `revive` start for a lead (D98).
    #[command(hide = true)]
    Broker { lead: String },
    /// Put lead <LEAD>'s broker on this loopherd binary, as after a merge that
    /// changes the broker: asks the running one to stop once its request in
    /// flight is answered, then starts a new one, which serves the requests
    /// still queued. With none running it just starts one. A broker still
    /// busy after 4 hours is left running and the command fails, naming its pid.
    /// The shepherd's or the human's; refused from a lead.
    BrokerRestart { lead: String },
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
    /// Write the handoff of the member running in this pane from stdin, to
    /// its own `handoffs/<name>/handoff.md` in the state dir.
    Handoff,
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
        /// Only entries at or after this: a date (2026-11-01), an RFC 3339 time
        /// (2026-11-01T12:00:00Z), or a span back from now (30m, 2h, 1d). Needs --all.
        #[arg(long, requires = "all", value_parser = since_now)]
        since: Option<SystemTime>,
        /// Only the last N entries, after the other filters. Needs --all.
        #[arg(long, value_name = "N", requires = "all", value_parser = at_least_one)]
        tail: Option<NonZeroUsize>,
    },
    /// Claude's Stop and StopFailure hook: records the stop, read from the hook
    /// input on stdin, in the mailbox of the member running in this pane. Once per
    /// session past the member's context limit, it blocks the stop instead and asks
    /// for a handoff. Outside a member's pane it records nothing. Set up by install-hook.
    StopHook,
    /// The herdr plugin's hook on pane.agent_status_changed, pane.exited,
    /// pane.closed, tab.closed, workspace.closed and pane.moved: records a
    /// member going blocked, or its pane, tab or workspace going, in its
    /// mailbox for the next watch, and follows a member's pane moved to
    /// another tab or workspace in the ledger. Reads HERDR_PLUGIN_EVENT
    /// and HERDR_PLUGIN_EVENT_JSON; any other pane, tab or workspace records
    /// nothing.
    HerdrEventHook,
    /// Claude's SessionStart hook: when a member's Claude is cleared or resumed,
    /// points its ledger entry at the new session, read from the hook input on
    /// stdin. Outside a member's pane it changes nothing. Set up by install-hook.
    SessionHook,
    /// Claude's UserPromptSubmit hook: clears the label a watch or a lead's
    /// question left on this pane's herdr sidebar row, as the human is answering;
    /// a background task's notification keeps it. Elsewhere it does nothing.
    /// Set up by install-hook.
    PromptHook,
    /// Claude's PreToolUse hook on Bash: in a lead's pane, denies a merge, install,
    /// rebase or force-push its switches leave to the human, and any
    /// `loopherd restart --force`, `pause`, `resume`, `herd set`, `herd unset`,
    /// `herd remove` or
    /// `lead-defaults --set`, `config set`, `config unset` or `setup`, or
    /// `scripts/install --lead-defaults` or `--context`,
    /// read from the hook input on stdin, by printing a
    /// deny decision. Elsewhere it prints nothing.
    /// Always exits 0. Set up by install-hook.
    ToolHook,
    /// Send one notification for `watch`, detached from it: runs the command
    /// within 30s and notes a failure in the member's mailbox.
    #[command(hide = true)]
    NotifyRun {
        #[arg(long)]
        member: String,
        #[arg(long, value_enum)]
        sender: notify::Sender,
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Check this machine's loopherd setup: loopherd's hooks in Claude's
    /// settings, the binary they call against the `loopherd` on PATH, herdr's
    /// server and version, whether loopherd's herdr plugin is installed,
    /// enabled and at loopherd's version (warnings only), config.toml, herd
    /// files and their repos, the ledger, pause files, mailboxes and members'
    /// panes, and with `--trackers` each herd's `source` and `backlog`. Only
    /// reads. Prints each warning and failure with the command that fixes
    /// it, then `doctor: <n> ok, <n> warnings, <n> failures`. Exits 0 when
    /// clean or only warnings, 1 on any failure, 2 when it couldn't run.
    Doctor {
        /// List every check, not only the problems.
        #[arg(long)]
        verbose: bool,
        /// Print every check and the counts as JSON.
        #[arg(long)]
        json: bool,
        /// Check this file instead of $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Also run each herd's `source` and `backlog` from its repo, paused
        /// herds included, 30s each; a nonzero exit or timeout fails. With
        /// `--verbose` or `--json`, shows each exit status and task count, never
        /// task lines.
        #[arg(long)]
        trackers: bool,
        /// Check only this herd's file, repo, state and trackers; the install,
        /// herdr and config checks still run.
        herd: Option<String>,
    },
    /// Add loopherd's Stop, StopFailure, SessionStart, PreToolUse and UserPromptSubmit hooks to Claude's
    /// user settings, calling this binary, and write the `--skill` guide as the loopherd
    /// skill; safe to re-run. Backs the settings file up before changing it.
    InstallHook {
        /// Take loopherd's hooks and skill out instead.
        #[arg(long)]
        remove: bool,
        /// Edit this file instead of $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Write the skill straight to <dir>/loopherd/SKILL.md. Without it, the skill goes
        /// in ~/.agents/skills/loopherd, linked from skills/loopherd beside the settings
        /// file; with --settings, both sit in the settings file's directory.
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
    /// It exits with `<herd> drained` (a lead's) or `all drained` (the
    /// shepherd's) once the herds it reads are paused with nothing in flight.
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

#[derive(Subcommand)]
enum HerdAction {
    /// Print the key's value as TOML; exits 1 if it isn't set.
    Get { herd: String, key: String },
    /// Set the key to a TOML value (`true`, `3`, `"x"`, `["a", "b"]`), creating
    /// missing tables. Refuses, changing nothing, a file `loopherd herds` would
    /// call invalid.
    Set {
        herd: String,
        key: String,
        value: String,
    },
    /// Remove the key; succeeds if it isn't set. Refuses, changing nothing, a
    /// file `loopherd herds` would call invalid.
    Unset { herd: String, key: String },
    /// Delete a finished herd: its herd file, its pause, its `[lead]`
    /// `prompt_file` when that lies in the herds dir and no other herd file
    /// names it, and every run of the herd in the ledger, archived ones
    /// included. A member's mailbox and handoffs go with its entry, unless the
    /// entry still holds another herd's run: then the newest such run becomes
    /// the entry and the name keeps its files.
    /// Refuses, changing nothing, while any member (lead included) is not
    /// retired, ran on another herdr session, or herdr may still show its
    /// agent or pane, or herdr can't be reached. Never touches worktrees, branches or tabs; `retire --close`
    /// does that.
    Remove {
        herd: String,
        /// Print every path and ledger entry it would remove; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Keep the ledger entries and mailboxes, so `stats` still counts them.
        #[arg(long)]
        keep_history: bool,
    },
}

#[derive(Args)]
struct SetupArgs {
    /// Ask every group again, current values as defaults.
    #[arg(long, conflicts_with_all = ["lead_defaults", "context"])]
    again: bool,
    /// Turn these lead switches on and the rest off: a comma list such as
    /// `merge,install`, or `none`. Alone, ask about them again.
    #[arg(long, value_name = "SWITCHES", num_args = 0..=1)]
    lead_defaults: Option<Option<String>>,
    /// Set any of the context window, the default handoff limit and the
    /// shepherd's, in tokens: `window=1000000,limit=500000,shepherd=400000`.
    /// A limit over 80% of the window is refused. Alone, ask about them again.
    #[arg(long, value_name = "window=N,limit=N,shepherd=N", num_args = 0..=1)]
    context: Option<Option<String>>,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the key's value; exits 1 if it isn't set.
    Get { key: String },
    /// Set the key, refusing, changing nothing, a token count under 10000,
    /// a switch that isn't `true` or `false`, or a theme `view` doesn't have.
    Set { key: String, value: String },
    /// Remove the key; succeeds if it isn't set.
    Unset { key: String },
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
    /// Workspace label; defaults to this pane's workspace.
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
struct InitArgs {
    /// Defaults to the repo's directory name.
    #[arg(long)]
    herd: Option<String>,
    /// A command workers run before calling their work done; repeatable.
    /// Suggested from Cargo.toml and package.json when not given.
    #[arg(long = "check", value_name = "CMD", num_args = 1..)]
    checks: Vec<String>,
    /// The gh account every gh call runs as; needed when no logged-in account
    /// matches origin's owner.
    #[arg(long, value_name = "LOGIN")]
    gh_account: Option<String>,
    /// The GitHub repo, for an origin init can't read as one (an ssh host alias).
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,
    /// With a local tracker, write the herd without GitHub even when origin
    /// is GitHub: no gh calls and no merged check.
    #[arg(long, conflicts_with_all = ["repo", "gh_account"])]
    no_github: bool,
    /// Where the repo tracks issues, when it has no docs/agents/issue-tracker.md;
    /// asked on a terminal, else github.
    #[arg(long, value_enum)]
    tracker: Option<TrackerArg>,
    /// How an `other` tracker works, in prose, for the tracker doc.
    #[arg(long, value_name = "TEXT")]
    tracker_desc: Option<String>,
    /// How many workers run at once; default 3.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(i64).range(1..))]
    max: Option<i64>,
    /// Write no [lead] table and no lead prompt.
    #[arg(long)]
    no_lead: bool,
    /// How a lead with merge off is kept from merging; asked on a terminal,
    /// else hook. credentials needs `loopherd merge-guard <herd>` afterwards.
    #[arg(long, value_enum, conflicts_with = "no_lead")]
    merge_guard: Option<herd::MergeGuard>,
    /// The workers' handoff limit in tokens; unset, config.toml's default applies.
    #[arg(long, value_name = "TOKENS")]
    context_limit: Option<u64>,
    /// Print a unified diff from the files init wrote to what it would write
    /// now, inferring again; write nothing. Exits 1 when they differ, 2 on error.
    #[arg(long)]
    diff: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TrackerArg {
    Github,
    Local,
    Other,
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
    /// Leave focus where it is instead of moving to an attended member's pane.
    #[arg(long)]
    no_focus: bool,
    /// Passed to claude after the herd's and the template's args.
    #[arg(last = true)]
    claude_args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.skill {
        let Some(name) = cli.section else {
            print!("{}", skill::SKILL);
            return ExitCode::SUCCESS;
        };
        return match skill::section(&name) {
            Some(section) => {
                print!("{section}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!(
                    "error: the skill has no section `{name}`; try one of: {}",
                    skill::section_names().join(", ")
                );
                ExitCode::from(2)
            }
        };
    }
    let Some(command) = cli.command else {
        Cli::command()
            .error(
                ErrorKind::MissingSubcommand,
                "a subcommand or --skill is required",
            )
            .exit();
    };
    if let Some((name, why)) = refused_to_leads(&command)
        && let Err(e) = refuse_from_lead(why, by_pane(&command))
    {
        eprintln!("loopherd {name}: {e:#}");
        return ExitCode::FAILURE;
    }
    let (command, result) = match command {
        Command::Spawn(args) => ("spawn", run_spawn(args)),
        Command::Add(args) => ("add", run_add(args)),
        Command::Init(args) => ("init", run_init(args)),
        Command::MergeGuard { herd } => ("merge-guard", run_merge_guard(&herd)),
        Command::Herds { settings } => ("herds", run_herds(settings)),
        Command::Herd { action } => ("herd", run_herd(action)),
        Command::LeadDefaults { set } => ("lead-defaults", run_lead_defaults(set)),
        Command::Config { action } => ("config", run_config(action)),
        Command::Setup(args) => ("setup", run_setup(args)),
        Command::Lead(args) => ("lead", run_lead(args)),
        Command::Feed {
            herd,
            dry_run,
            ready_timeout,
        } => ("feed", run_feed(herd, dry_run, ready_timeout)),
        Command::Pause { herds, reason } => ("pause", run_pause(&herds, reason.as_deref())),
        Command::Resume { herds } => ("resume", run_resume(&herds)),
        Command::Retire { name, close, force } => ("retire", run_retire(&name, close, force)),
        Command::Release {
            name,
            keep_branch,
            drop_branch,
        } => ("release", run_release(&name, keep_branch, drop_branch)),
        Command::Revive {
            dry_run,
            name,
            startup,
        } => ("revive", run_revive(dry_run, name, startup)),
        Command::Prompt { name, force } => ("prompt", run_prompt(&name, force)),
        Command::Tell {
            name,
            text,
            text_file,
        } => ("tell", run_tell(&name, text, text_file)),
        Command::Read { name, lines } => (
            "read",
            run_op(
                own_broker().as_ref(),
                &broker::Op::Read {
                    name,
                    lines: lines as usize,
                },
            ),
        ),
        Command::Restart {
            name: Some(name),
            force,
            own: false,
        } => ("restart", run_restart(&name, force)),
        Command::Restart { .. } => ("restart", run_restart_self()),
        Command::RestartWait { name } => ("restart-wait", run_restart_wait(&name)),
        Command::Broker { lead } => ("broker", run_broker(&lead)),
        Command::BrokerRestart { lead } => ("broker-restart", run_broker_restart(&lead)),
        Command::Status { all, strays, json } => ("status", run_status(all, strays, json)),
        Command::Stats {
            herd,
            since,
            until,
            json,
        } => (
            "stats",
            run_stats(herd, usage::Window { since, until }, json),
        ),
        Command::View {
            beside_shepherd: true,
            ..
        } => ("view", run_view_beside_shepherd()),
        Command::View { all, strays, .. } => ("view", run_view(all, strays)),
        Command::Pr { url, popup } => ("pr", run_pr(url, popup)),
        Command::Report {
            kind,
            as_name,
            text,
        } => ("report", run_report(kind, as_name, &text)),
        Command::Handoff => ("handoff", run_handoff()),
        Command::Reports {
            names,
            herd,
            all,
            since,
            tail,
        } => ("reports", run_reports(names, herd, all, since, tail)),
        Command::Watch {
            herds,
            timeout,
            settings,
        } => ("watch", run_watch(&herds, timeout, settings)),
        Command::StopHook => ("stop-hook", run_stop_hook()),
        Command::HerdrEventHook => ("herdr-event-hook", run_herdr_event_hook()),
        Command::SessionHook => ("session-hook", run_session_hook()),
        Command::PromptHook => ("prompt-hook", run_prompt_hook()),
        Command::ToolHook => ("tool-hook", run_tool_hook()),
        Command::NotifyRun {
            member,
            sender,
            args,
        } => ("notify-run", run_notify(member, sender, args)),
        Command::Doctor {
            verbose,
            json,
            settings,
            trackers,
            herd,
        } => (
            "doctor",
            run_doctor(verbose, json, settings, trackers, herd),
        ),
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

/// The commands a lead's pane refuses, named, with why (D110). Commands a
/// lead's broker carries out, or that refuse a lead themselves, aren't here.
fn refused_to_leads(command: &Command) -> Option<(&'static str, &'static str)> {
    const HERD_FILE: &str = "a herd file is the human's to change";
    const CONFIG: &str = "config.toml is the human's to change";
    const PAUSE: &str = "pausing a herd is the human's";
    Some(match command {
        Command::Lead(_) => ("lead", "starting a lead is the shepherd's"),
        Command::Revive { .. } => ("revive", "reviving members is the shepherd's"),
        Command::Pause { .. } => ("pause", PAUSE),
        Command::Resume { .. } => ("resume", PAUSE),
        Command::Herd {
            action: HerdAction::Set { .. },
        } => ("herd set", HERD_FILE),
        Command::Herd {
            action: HerdAction::Unset { .. },
        } => ("herd unset", HERD_FILE),
        Command::Herd {
            action: HerdAction::Remove { .. },
        } => ("herd remove", HERD_FILE),
        Command::Init(_) => ("init", HERD_FILE),
        Command::LeadDefaults { set: Some(_) } => ("lead-defaults", CONFIG),
        Command::Config {
            action: ConfigAction::Set { .. } | ConfigAction::Unset { .. },
        } => ("config", CONFIG),
        // Plain setup only names its flags without a terminal, and the
        // installer a lead may run (D17) runs it so.
        Command::Setup(args)
            if args.again || args.lead_defaults.is_some() || args.context.is_some() =>
        {
            ("setup", CONFIG)
        }
        Command::MergeGuard { .. } => (
            "merge-guard",
            "a herd's credentials are the human's to set up",
        ),
        Command::Broker { .. } => ("broker", "a lead's broker starts outside its sandbox"),
        Command::RestartWait { .. } => {
            ("restart-wait", "a lead's broker starts its restart waiter")
        }
        _ => return None,
    })
}

/// Refuses the running command, `why` saying whose it is, in a lead's pane
/// or sandbox. The shepherd's pane may carry a dead lead's old id after a
/// herdr restart, so it is never a lead's.
fn refuse_from_lead(why: &str, by_pane: bool) -> Result<()> {
    let Ok(path) = FileLedger::default_path() else {
        return Ok(());
    };
    let ledger = FileLedger::new(path);
    let shepherd = ledger
        .dir()
        .is_ok_and(|state| shepherd_here(&state, own_pane().as_deref()).is_some());
    let lead = match by_pane {
        true => own_broker(),
        false => Paths::from_env().ok().and_then(|paths| {
            broker::route(&paths, &ledger, &process::ProcFs, std::process::id(), None)
        }),
    };
    let lead = lead.filter(|_| !shepherd);
    broker::refuse_from_lead(lead.as_ref(), &ledger, why)
}

/// herdr runs `revive --startup` with its focused pane's id, which may be a
/// lead's, so only its lineage or a sandbox can make it a lead's.
fn by_pane(command: &Command) -> bool {
    !matches!(command, Command::Revive { startup: true, .. })
}

fn run_herd(action: HerdAction) -> Result<i32> {
    let paths = Paths::from_env()?;
    match action {
        HerdAction::Get { herd, key } => println!("{}", herd::get(&paths, &herd, &key)?),
        HerdAction::Set { herd, key, value } => {
            herd::set(&paths, &herd, &key, &value)?;
            println!("set {key} = {value} in {herd}");
        }
        HerdAction::Unset { herd, key } => match herd::unset(&paths, &herd, &key)? {
            true => println!("unset {key} in {herd}"),
            false => println!("{key} was not set in {herd}"),
        },
        HerdAction::Remove {
            herd,
            dry_run,
            keep_history,
        } => {
            let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
            let state = ledger.dir()?;
            let how = herd::Removal {
                dry_run,
                keep_history,
            };
            let mut out = vec![];
            let result = herd::remove(&paths, &CliHerdr, &mut ledger, &state, &herd, how, &mut out);
            for line in out {
                println!("{line}");
            }
            result?;
        }
    }
    Ok(0)
}

fn config_path() -> Result<PathBuf> {
    Ok(Paths::from_env()?.herds.with_file_name("config.toml"))
}

fn run_config(action: ConfigAction) -> Result<i32> {
    let config = config_path()?;
    match action {
        ConfigAction::Get { key } => match config::get(&config, &key)? {
            Some(value) => println!("{value}"),
            None => bail!("{key} is not set"),
        },
        ConfigAction::Set { key, value } => {
            config::set(&config, &key, &value)?;
            println!(
                "set {key} = {}",
                config::get(&config, &key)?.unwrap_or_default()
            );
        }
        ConfigAction::Unset { key } => match config::unset(&config, &key)? {
            true => println!("unset {key}"),
            false => println!("{key} was not set"),
        },
    }
    Ok(0)
}

fn run_setup(args: SetupArgs) -> Result<i32> {
    let config = config_path()?;
    let tty = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok();
    let mut terminal = tty
        .map(|tty| -> Result<_> { Ok((std::io::BufReader::new(tty.try_clone()?), tty)) })
        .transpose()?;
    let lines = setup::setup(
        setup::Request {
            config: &config,
            again: args.again,
            lead_defaults: args.lead_defaults,
            context: args.context,
        },
        terminal
            .as_mut()
            .map(|(input, output)| setup::Tty { input, output }),
    )?;
    lines.iter().for_each(|line| println!("{line}"));
    if let Ok(sandbox::Sockets::Open(sandbox::Unavailable { why, fix })) = sandbox::probe() {
        println!(
            "note: Claude Code's sandbox can't block Unix sockets here ({why}), so a sandboxed \
             lead can reach herdr, your ssh agent and WSL2's Windows interop; to close them: \
             {fix}"
        );
    }
    Ok(0)
}

fn run_merge_guard(herd: &str) -> Result<i32> {
    let paths = Paths::from_env()?;
    merge_guard::setup(&paths, herd)?
        .iter()
        .for_each(|line| println!("{line}"));
    Ok(0)
}

fn run_lead_defaults(set: Option<String>) -> Result<i32> {
    let config = config_path()?;
    if let Some(list) = set {
        config::set_lead_defaults(&config, &list)?;
    }
    if let Some(defaults) = config::lead_defaults(&config)?.filter(SwitchValues::sets_a_switch) {
        println!("{}", defaults.over(&Default::default()).lines());
    }
    Ok(0)
}

fn run_pause(herds: &HerdsArg, reason: Option<&str>) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let done = pause::pause(&paths, herds.herds(), reason, SystemTime::now())?;
    done.lines.iter().for_each(|line| println!("{line}"));
    Ok(done.code)
}

fn run_resume(herds: &HerdsArg) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let reports = ledger.reports_dir()?;
    let done = pause::resume(&paths, &ledger, &reports, herds.herds())?;
    done.lines.iter().for_each(|line| println!("{line}"));
    Ok(done.code)
}

fn run_retire(name: &str, close: bool, force: bool) -> Result<i32> {
    run_op(
        own_broker().as_ref(),
        &broker::Op::Retire {
            name: name.into(),
            close,
            force,
        },
    )
}

fn run_release(name: &str, keep_branch: bool, drop_branch: bool) -> Result<i32> {
    run_op(
        own_broker().as_ref(),
        &broker::Op::Release {
            name: name.into(),
            keep_branch,
            drop_branch,
        },
    )
}

fn run_revive(dry_run: bool, name: Option<String>, startup: bool) -> Result<i32> {
    let refusal = "refused: revive is the shepherd's or the human's, and a lead runs this loopherd";
    let paths = Paths::from_env()?;
    let caller = sandbox::calling_lead(&paths, &process::ProcFs, std::process::id());
    if !dry_run && !matches!(caller, Ok(None)) {
        bail!(refusal);
    }
    let startup = match startup {
        false => None,
        true => {
            let config = herd::config_file(&Paths::from_env().context("config")?);
            if !config::revive_on_startup(&config)? {
                println!(
                    "revive on startup is off; set [herdr_plugin] revive_on_startup = true in {} to turn it on",
                    config.display()
                );
                return Ok(0);
            }
            // Never herdr's own answer: outside a hook that is whichever server the CLI reaches.
            Some(socket_from_env().context(
                "refused: HERDR_SOCKET_PATH is unset; --startup revives only the members of the herdr whose startup hook runs it",
            )?)
        }
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let req = ReviveRequest {
        name,
        dry_run,
        settle_timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(250),
        start_timeout: Duration::from_secs(60),
        reports_dir: ledger.reports_dir()?,
        lock_timeout: Duration::from_secs(600),
        server: startup.is_some().then(std::os::unix::process::parent_id),
        startup,
        paths: Paths::from_env()?,
        sandbox: sandbox::probe,
        broker: broker::start,
        restart_broker: |paths, lead| {
            broker::restart_broker(paths, lead, &runnable_exe()?, &mut Printed)
        },
    };
    let (message, code) = revive(
        &CliHerdr,
        &process::ProcFs,
        &mut ledger,
        &req,
        &live_session,
    )?
    .report();
    if !message.is_empty() {
        println!("{message}");
    }
    Ok(code)
}

fn run_status(all: bool, strays: bool, json: bool) -> Result<i32> {
    use std::io::IsTerminal;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    if let Some(client) = own_broker() {
        let op = broker::Op::Status {
            all,
            strays,
            json,
            color: std::io::stdout().is_terminal(),
        };
        return run_op(Some(&client), &op);
    }
    broker::refuse_unrouted(&ledger, "status")?;
    let format = match json {
        true => Format::Json,
        false => Format::Text {
            color: std::io::stdout().is_terminal(),
        },
    };
    let pauses = pause::Pauses::from_env()?;
    // Paths needs HOME; without it status counts no herd's tasks.
    let paths = Paths::from_env().ok();
    let timeout = remote_timeout().unwrap_or_else(|e| {
        let default = humantime::format_duration(REMOTE_TIMEOUT);
        eprintln!("loopherd status: warning: {e:#}; counting tasks with the default {default}");
        REMOTE_TIMEOUT
    });
    let queues = paths.as_ref().map(|paths| Queues { paths, timeout });
    let out = status::status(
        &CliHerdr,
        &ledger,
        &pauses,
        queues.as_ref(),
        Options { all, strays },
        format,
    )?;
    print!("{}", out.stdout);
    for line in out.stderr.lines() {
        eprintln!("loopherd status: {line}");
    }
    Ok(out.code)
}

fn run_stats(herd: Option<String>, window: usage::Window, json: bool) -> Result<i32> {
    if let (Some(since), Some(until)) = (window.since, window.until)
        && until <= since
    {
        let mut cli = Cli::command();
        // Building fills in each subcommand's usage line.
        cli.build();
        cli.find_subcommand_mut("stats")
            .expect("stats is a subcommand")
            .error(ErrorKind::ValueValidation, "--until must be after --since")
            .exit();
    }
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let claude_dir = install_hook::claude_dir().ok();
    let query = stats::Query {
        herd: herd.as_deref(),
        window,
        now: SystemTime::now(),
        claude_dir: claude_dir.as_deref(),
    };
    require_known(&ledger, herd.as_deref())?;
    let (herds, warnings) = stats::stats(&ledger, &ledger.reports_dir()?, &query)?;
    match json {
        true => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "herds": herds }))?
        ),
        false => print!("{}", stats::render(&herds)),
    }
    for warning in warnings {
        eprintln!("loopherd stats: warning: {warning}");
    }
    Ok(0)
}

fn run_view(all: bool, strays: bool) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let subscriber = SocketSubscriber::new(CliHerdr.socket());
    let (herds, state, config) = (Paths::from_env()?, ledger.dir()?, config_path()?);
    let below = match own_pane() {
        // The split pane gets the herdr server's environment, so pin the
        // roots this view resolved, set in its own environment or not.
        Some(pane) => Some(shepherd_pane::Below {
            pane,
            cwd: workspace_cwd()?,
            env: vec![
                ("XDG_STATE_HOME".into(), env_path(state.parent())?),
                ("LOOPHERD_CONFIG_DIR".into(), env_path(config.parent())?),
            ],
        }),
        None => None,
    };
    let dirs = tui::Dirs {
        pauses: pause::Pauses::from_env()?,
        herds,
        mailboxes: ledger.reports_dir()?,
        config,
        state,
        below,
    };
    tui::run(
        CliHerdr,
        subscriber,
        ledger,
        dirs,
        Options { all, strays },
        tui::theme::Theme::from_env(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
        ),
        std::env::var(view_pane::NOTE)
            .ok()
            .filter(|n| !n.is_empty()),
    )
}

fn env_path(dir: Option<&Path>) -> Result<String> {
    let dir = dir.context("a state or config path has no parent")?;
    dir.to_str()
        .map(String::from)
        .with_context(|| format!("{} is not UTF-8", dir.display()))
}

/// Where the human works: a plugin pane's workspace, whose own cwd is the
/// plugin's directory, else the current directory.
fn workspace_cwd() -> Result<PathBuf> {
    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok();
    let workspace = context
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|c| c["workspace_cwd"].as_str().map(PathBuf::from));
    match workspace {
        Some(cwd) => Ok(cwd),
        None => std::env::current_dir().context("current directory"),
    }
}

fn run_view_beside_shepherd() -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let current = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty());
    match view_pane::open(
        &CliHerdr,
        &process::ProcFs,
        &ledger.dir()?,
        current.as_deref(),
    )? {
        view_pane::Opened::BesideShepherd { pane, how } => {
            println!("{} beside the shepherd's pane {pane}", view_outcome(&how))
        }
        view_pane::Opened::Beside { pane, why, how } => {
            println!("{} beside pane {pane}: {why}", view_outcome(&how))
        }
        view_pane::Opened::Popup { why } => println!("opened the view in the popup: {why}"),
    }
    Ok(0)
}

fn view_outcome(how: &view_pane::How) -> String {
    match how {
        view_pane::How::Opened => "opened the view".into(),
        view_pane::How::OpenedFocused => "opened and focused the view".into(),
        view_pane::How::Focused { view } => format!("focused the view already open in {view}"),
    }
}

fn run_pr(url: Option<String>, popup: bool) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let reports = ledger.reports_dir()?;
    if popup {
        use std::io::IsTerminal;
        let width = match std::io::stdout().is_terminal() {
            true => ratatui::crossterm::terminal::size().map_or(0, |(w, _)| usize::from(w)),
            false => 0,
        };
        // A zero width is a terminal that can't say; cut nothing.
        let width = if width == 0 { usize::MAX } else { width };
        let var = |name: &str| std::env::var(name).map_err(|_| anyhow!("{name} is not set"));
        let card = (|| {
            pr_link::card(
                &var("LOOPHERD_PR_MEMBER")?,
                &var("LOOPHERD_PR_URL")?,
                &ledger.entries()?,
                &reports,
                &CliHerdr,
                width,
            )
        })();
        match &card {
            Ok(card) => print!("{card}"),
            Err(e) => print!("{}", pr_link::failed_card(e, width)),
        }
        std::io::stdout().flush()?;
        // Without a terminal on stdin crossterm would take /dev/tty, the caller's.
        if std::io::stdin().is_terminal() {
            ratatui::crossterm::terminal::enable_raw_mode()?;
            let closed = pr_link::wait_to_close(&mut ratatui::crossterm::event::read);
            ratatui::crossterm::terminal::disable_raw_mode()?;
            closed?;
        }
        return Ok(i32::from(card.is_err()));
    }
    let url = match url {
        Some(url) => url,
        None => std::env::var("HERDR_PLUGIN_CLICKED_URL")
            .context("no link given and HERDR_PLUGIN_CLICKED_URL is not set")?,
    };
    let entries = ledger.entries().unwrap_or_else(|e| {
        eprintln!("loopherd pr: can't read the ledger, so no member owns {url}: {e:#}");
        Default::default()
    });
    let shown = pr_link::click(
        &url,
        &entries,
        &reports,
        &CliHerdr,
        &github::GhCli::default(),
        &mut open_in_browser,
    )?;
    match shown {
        pr_link::Shown::Popup { member } => println!("showed {member} for {url}"),
        pr_link::Shown::Browser => println!("opened {url} in the browser"),
    }
    Ok(0)
}

/// Starts the opener without waiting: a browser it launches would hold this
/// action's output pipes, and herdr its log entry, open until it quits.
fn open_in_browser(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("run {opener}"))?;
    Ok(())
}

fn run_prompt(name: &str, force: bool) -> Result<i32> {
    run_op(
        own_broker().as_ref(),
        &broker::Op::Prompt {
            name: name.into(),
            force,
        },
    )
}

fn run_tell(name: &str, text: Option<String>, text_file: Option<PathBuf>) -> Result<i32> {
    let text = match (text, text_file) {
        (Some(text), _) => text,
        (None, Some(path)) => {
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        (None, None) => unreachable!("clap requires text or --text-file"),
    };
    run_op(
        own_broker().as_ref(),
        &broker::Op::Tell {
            name: name.into(),
            text,
        },
    )
}

/// Runs `op` through `client`, the lead's broker, or in this process when there is none.
fn run_op(client: Option<&broker::Client>, op: &broker::Op) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let mut ctx = broker::Ctx {
        herdr: &CliHerdr,
        ledger: &mut ledger,
        paths: &paths,
        procs: &process::ProcFs,
        claude_dir: install_hook::claude_dir().ok(),
        caller: broker::Caller {
            pane: own_pane(),
            socket: socket_from_env(),
            lineage: own_lineage(),
            workspace: std::env::var("HERDR_WORKSPACE_ID")
                .ok()
                .filter(|v| !v.is_empty()),
            process: own_process()?,
            lead: None,
        },
        live: live_session,
        spawn_waiter,
        prs: &github::GhCli::default(),
    };
    broker::run(&mut ctx, client, op, &mut Printed)
}

/// Prints an operation's output as it comes.
struct Printed;

impl broker::Sink for Printed {
    fn out(&mut self, text: &str) {
        println!("{text}");
    }

    fn err(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// The broker of the lead running this loopherd, if a lead runs it. The
/// shepherd's pane may carry a dead lead's old id after a herdr restart, so
/// it is never a lead's.
fn own_broker() -> Option<broker::Client> {
    let paths = Paths::from_env().ok()?;
    let ledger = FileLedger::new(FileLedger::default_path().ok()?);
    let id = own_pane();
    let shepherd = ledger
        .dir()
        .is_ok_and(|state| shepherd_here(&state, id.as_deref()).is_some());
    if shepherd {
        return None;
    }
    let lineage = own_lineage();
    let socket = socket_from_env();
    let pane = id.as_deref().map(|id| tool_hook::Pane {
        id,
        socket: socket.as_deref(),
        lineage: &lineage,
    });
    broker::route(
        &paths,
        &ledger,
        &process::ProcFs,
        std::process::id(),
        pane.as_ref(),
    )
}

fn run_restart(name: &str, force: bool) -> Result<i32> {
    run_op(
        own_broker().as_ref(),
        &broker::Op::Restart {
            name: name.into(),
            force,
        },
    )
}

const WAITER_FD: &str = "LOOPHERD_RESTART_WAITER_FD";

fn run_restart_self() -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let state = ledger.dir()?;
    if let Some(place) = shepherd_here(&state, own_pane().as_deref()) {
        let armed = arm_shepherd(&ledger, &state, &place)?;
        spawn_waiter(context::SHEPHERD, armed.waiter)?;
        println!("{}", armed.message);
        return Ok(0);
    }
    run_op(own_broker().as_ref(), &broker::Op::RestartSelf)
}

fn arm_shepherd(
    ledger: &FileLedger,
    state: &Path,
    place: &shepherd_resume::Place,
) -> Result<shepherd_restart::Armed> {
    let config = Paths::from_env()?.herds.with_file_name("config.toml");
    let switches = shepherd_restart::Switches {
        enabled: config::self_restart(&config)?,
        prompt_hook: install_hook::prompt_hook_installed(&install_hook::default_settings()?)?,
    };
    let mailboxes = ledger.reports_dir()?;
    let mut notify = |text: &str| notify_self_restart(text, &config, &mailboxes);
    shepherd_restart::arm(
        state,
        place,
        &switches,
        &mut watch::RealClock::start(),
        &mut notify,
    )
}

/// Starts `restart-wait <name>` in a session of its own, handing it `waiter`.
/// Closes this process's copy, since a broker lives on: the claim lasts as
/// long as the waiter runs, and no longer.
fn spawn_waiter(name: &str, waiter: lock::Lock) -> Result<()> {
    let file = waiter.into_file();
    let fd = file.as_raw_fd();
    // The waiter inherits the lock's open file, so the claim lasts as long as it runs.
    // SAFETY: fcntl only clears close-on-exec on a descriptor `file` keeps open.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pass the waiter its lock");
    }
    let exe = runnable_exe()?;
    let mut command = std::process::Command::new(&exe);
    command
        .args(["restart-wait", name])
        .env(WAITER_FD, fd.to_string())
        .env_remove("HERDR_PANE_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe. A session of its own outlives the
    // calling Claude's shells and their hangups.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {} restart-wait", exe.display()))?;
    drop(file);
    // A broker outlives its waiters; reaping each leaves no zombie behind.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// This binary, or `loopherd` on PATH once a reinstall has moved a new one over it.
fn runnable_exe() -> Result<PathBuf> {
    Ok(runnable(
        std::env::current_exe().context("find the loopherd binary")?,
    ))
}

fn runnable(exe: PathBuf) -> PathBuf {
    match exe.to_string_lossy().ends_with(" (deleted)") {
        true => PathBuf::from("loopherd"),
        false => exe,
    }
}

fn run_broker(lead: &str) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let mut env =
        broker::lead_env(&paths, lead).with_context(|| format!("{lead}'s broker can't start"))?;
    env.push((broker::BROKER_ENV.into(), lead.into()));
    for (name, value) in env {
        // SAFETY: the broker has started no threads yet.
        unsafe { std::env::set_var(name, value) };
    }
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let host = broker::Host {
        herdr: &CliHerdr,
        procs: &process::ProcFs,
        claude_dir: install_hook::claude_dir().ok(),
        process: own_process()?,
        live: live_session,
        spawn_waiter,
        prs: &github::GhCli::default(),
    };
    let served = broker::serve(&host, &ledger, &paths, lead, Duration::from_millis(100))?;
    if served == broker::Served::AlreadyRunning {
        eprintln!("loopherd broker: a broker already serves {lead}");
    }
    Ok(0)
}

fn run_broker_restart(lead: &str) -> Result<i32> {
    // Before anything else: a reinstall after this still leaves a path to start the new broker from.
    let exe = runnable_exe()?;
    let paths = Paths::from_env()?;
    let caller = sandbox::calling_lead(&paths, &process::ProcFs, std::process::id());
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    // A broker, or a command its request runs, would wait on itself.
    let in_a_broker = std::env::var_os(broker::BROKER_ENV).is_some();
    if in_a_broker || !matches!(caller, Ok(None)) || own_broker().is_some() || ledger.read_only()? {
        bail!(
            "refused: broker-restart is the shepherd's or the human's, and a lead runs this loopherd"
        );
    }
    broker::restart_broker(&paths, lead, &exe, &mut Printed)?;
    Ok(0)
}

fn run_restart_wait(name: &str) -> Result<i32> {
    let fd = std::env::var(WAITER_FD)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .context("restart-wait runs only from `loopherd restart --self`, which holds its claim")?;
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let state = ledger.dir()?;
    // No member may be named `shepherd`.
    if name == context::SHEPHERD {
        return run_shepherd_wait(&state, fd);
    }
    let _claim = restart::adopt_claim(&state, name, fd)?;
    let paths = Paths::from_env().inspect_err(|e| {
        let text = format!("self-restart failed: {e:#}");
        let note = report::Entry::new(SystemTime::now(), report::Body::Note { text });
        let _ = report::append(&state.join("reports"), name, &note);
    })?;
    let req = restart::RestartRequest {
        name,
        force: false,
        own_pane: None,
        own_lineage: &[],
        pane_shell: &pane_shell,
        state: &state,
        paths: &paths,
        caller: None,
        settle_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(250),
        live: &live_session,
    };
    restart::wait_then_restart(
        &CliHerdr,
        &mut ledger,
        &req,
        restart::SELF_RESTART_TIMEOUT,
        Duration::from_secs(2),
    )?;
    Ok(0)
}

fn run_shepherd_wait(state: &Path, fd: std::os::fd::RawFd) -> Result<i32> {
    let mailboxes = state.join("reports");
    let config_file = Paths::from_env().map(|p| p.herds.with_file_name("config.toml"));
    let mut notify = |text: &str| match &config_file {
        Ok(config_file) => notify_self_restart(text, config_file, &mailboxes),
        Err(e) => eprintln!("loopherd restart-wait: warning: sent no notification: {e:#}"),
    };
    let Some(place) = shepherd_resume::recorded_place(state) else {
        let why = "the shepherd's handoff record is gone";
        notify(&format!("self-restart gave up: {why}"));
        bail!(why);
    };
    let alive = |p: &ledger::Process| process::live(&process::ProcFs, Some(*p)).is_some();
    let enabled = || match &config_file {
        Ok(config_file) => config::self_restart(config_file),
        Err(e) => Err(anyhow!("{e:#}")),
    };
    let waiter = shepherd_restart::Waiter {
        herdr: &CliHerdr,
        state,
        mailboxes: &mailboxes,
        place: &place,
        alive: &alive,
        enabled: &enabled,
    };
    let _claim = match restart::adopt_claim(state, context::SHEPHERD, fd) {
        Ok(claim) => claim,
        Err(e) => {
            if let Err(unsaid) = shepherd_restart::refused(&waiter, &format!("{e:#}"), &mut notify)
            {
                eprintln!("loopherd restart-wait: warning: {unsaid:#}");
            }
            return Err(e);
        }
    };
    shepherd_restart::wait(&waiter, &mut watch::RealClock::start(), &mut notify)?;
    Ok(0)
}

/// Sends `text` as the shepherd's `self_restart` notification.
fn notify_self_restart(text: &str, config: &Path, mailboxes: &Path) {
    let waiting = notify::Waiting {
        herd: None,
        member: context::SHEPHERD.into(),
        kind: notify::Kind::SelfRestart,
        text: text.into(),
        at: Some(humantime::format_rfc3339_nanos(SystemTime::now()).to_string()),
    };
    notify_waiting(&[waiting], config, mailboxes);
}

fn run_report(kind: Kind, as_name: Option<String>, text: &str) -> Result<i32> {
    if let Some(broker) = own_broker() {
        if let Some(other) = as_name.filter(|other| other != broker.lead()) {
            let lead = broker.lead();
            bail!("{lead} reports only as itself; {other}'s reports are {other}'s own");
        }
        let op = broker::Op::Report {
            kind,
            text: text.into(),
        };
        match run_op(Some(&broker), &op) {
            // A lead still on settings from before its broker wrote reports may write its own.
            Err(e) if e.downcast_ref::<broker::TooOld>().is_some() => {
                eprintln!("loopherd report: warning: {e:#}; writing the report directly");
                let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
                let entry = report::Entry::new(SystemTime::now(), report::Body::report(kind, text));
                report::append(&ledger.reports_dir()?, broker.lead(), &entry)?;
                return Ok(0);
            }
            done => return done,
        }
    }
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let own_pane = own_pane();
    let own_socket = socket_from_env();
    let who = Reporter {
        own_pane: own_pane.as_deref(),
        own_lineage: &own_lineage(),
        own_socket: own_socket.as_deref(),
        as_name: as_name.as_deref(),
    };
    let mailboxes = ledger.reports_dir()?;
    let name = report(&ledger, &mailboxes, &who, kind, text, SystemTime::now())?;
    let config = Paths::from_env().map(|p| p.herds.with_file_name("config.toml"));
    let labels = config.is_ok_and(|config| notify::labels_panes(&config));
    if kind != Kind::Question || !labels {
        return Ok(0);
    }
    if let Some(pane) = &own_pane
        && let Err(e) = pane_label::lead_asked(&CliHerdr, &ledger, &mailboxes, pane, &name, text)
    {
        eprintln!("loopherd report: warning: label pane {pane}: {e:#}");
    }
    Ok(0)
}

fn run_handoff() -> Result<i32> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("read the handoff from stdin")?;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let name = match own_broker() {
        Some(broker) => broker.lead().to_string(),
        None => {
            let own_pane = own_pane();
            let own_socket = socket_from_env();
            let who = Reporter {
                own_pane: own_pane.as_deref(),
                own_lineage: &own_lineage(),
                own_socket: own_socket.as_deref(),
                as_name: None,
            };
            report::reporter(&ledger, &who)?
        }
    };
    let reports = ledger.reports_dir()?;
    let state = reports.parent().context("the state dir")?;
    println!("wrote {}", handoff::write(state, &name, &text)?.display());
    Ok(0)
}

fn since_now(value: &str) -> Result<SystemTime> {
    report::parse_since(value, SystemTime::now())
}

fn at_least_one(value: &str) -> Result<NonZeroUsize> {
    value
        .parse::<usize>()
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| anyhow!("{value:?} is not a count; it must be at least 1"))
}

fn run_reports(
    names: Vec<String>,
    herd: Option<String>,
    all: bool,
    since: Option<SystemTime>,
    tail: Option<NonZeroUsize>,
) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    require_known(&ledger, herd.as_deref())?;
    let filter = Filter {
        names: &names,
        herd: herd.as_deref(),
        all,
        since,
        tail,
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

/// Refuses a `--herd` naming no herd, so a typo isn't read as a quiet herd.
fn require_known(ledger: &FileLedger, herd: Option<&str>) -> Result<()> {
    match herd {
        Some(herd) if !herd::known(&Paths::from_env()?.herds, ledger, herd)? => {
            bail!("no herd {herd}")
        }
        _ => Ok(()),
    }
}

fn run_watch(herds: &[String], timeout: Option<u64>, settings: Option<PathBuf>) -> Result<i32> {
    let herd_paths = Paths::from_env().context("herd")?;
    let caller = sandbox::calling_lead(&herd_paths, &process::ProcFs, std::process::id())?;
    let settings = match (settings, &caller) {
        // Outside the sandbox, reading it would reach files the lead's sandbox denies.
        (Some(_), Some(lead)) => bail!(
            "refused: {lead}'s Claude runs this watch, which checks Claude's own settings; \
             drop --settings"
        ),
        (Some(settings), None) => settings,
        (None, _) => install_hook::default_settings()?,
    };
    if !install_hook::installed(&settings)? {
        bail!(
            "loopherd's hooks are not all in {}, so stops could go unseen; run scripts/install",
            settings.display()
        );
    }
    for herd in herds {
        if !herd::is_name(herd) {
            bail!("{herd:?} is not a herd name");
        }
        if !herd_paths.herds.join(format!("{herd}.toml")).is_file() {
            eprintln!("loopherd watch: warning: no herd file for {herd}");
        }
    }
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let own_pane = own_pane();
    let own_socket = socket_from_env();
    // Its checks and merge lookups run outside the sandbox, so as the lead, not the human.
    let as_lead = |lead: &str| -> Result<()> {
        let env = broker::lead_env(&herd_paths, lead)
            .with_context(|| format!("{lead}'s watch can't start"))?;
        for (name, value) in env {
            // SAFETY: the watch has started no threads yet.
            unsafe { std::env::set_var(name, value) };
        }
        Ok(())
    };
    let req = watch::Request {
        herds,
        timeout: timeout.map(Duration::from_secs),
        poll: Duration::from_secs(2),
        own_pane: own_pane.as_deref(),
        own_lineage: &own_lineage(),
        own_socket: own_socket.as_deref(),
        caller: caller.as_deref(),
        as_lead: Some(&as_lead),
        herd_files: Some(&herd_paths),
        check_timeout: remote_timeout,
        shepherd_handoff: None,
        procs: &process::ProcFs,
        notify_own: None,
        merges: None,
        pane_label: false,
    };
    let state = ledger.dir()?;
    let handoff = || shepherd_handoff(&herd_paths, &state);
    let config = herd_paths.herds.with_file_name("config.toml");
    let mailboxes = ledger.reports_dir()?;
    let notify_own = |events: &[notify::Waiting]| notify_waiting(events, &config, &mailboxes);
    let gh = github::GhCli::default();
    let req = watch::Request {
        shepherd_handoff: Some(&handoff),
        notify_own: Some(&notify_own),
        merges: merge_lookups(&config, &gh),
        pane_label: notify::labels_panes(&config),
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
    notify_waiting(&wake.waiting, &config, &mailboxes);
    wake.mark_read(&mut |warning| eprintln!("loopherd watch: warning: {warning}"))?;
    Ok(0)
}

/// `gh` while `config`'s `[notify]` would send a merge alert, else `None`,
/// so a herd that sends none never asks GitHub.
fn merge_lookups<'a>(config: &Path, gh: &'a dyn github::Merges) -> Option<&'a dyn github::Merges> {
    let sends =
        notify::load(config).is_ok_and(|c| c.is_some_and(|c| c.sends(notify::Kind::MergeAlert)));
    sends.then_some(gh)
}

/// Launches each notification `config` asks for as its own `notify-run`,
/// so neither a slow command nor its failure holds up the watch.
fn notify_waiting(waiting: &[notify::Waiting], config: &Path, mailboxes: &Path) {
    if waiting.is_empty() {
        return;
    }
    let config = match notify::load(config) {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(e) => {
            eprintln!("loopherd watch: warning: sent no notification: {e:#}");
            return;
        }
    };
    let exe = runnable_exe().unwrap_or_else(|_| PathBuf::from("loopherd"));
    let sent = notify::send(&config, mailboxes, waiting, &mut |launch| {
        let sender = match launch.sender {
            notify::Sender::Herdr => "herdr",
            notify::Sender::Command => "command",
        };
        let spawned = std::process::Command::new(&exe)
            .args(["notify-run", "--member", &launch.member, "--sender", sender])
            .arg("--")
            .args(&launch.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Its own group, so ending the watch's task leaves it running.
            .process_group(0)
            .spawn();
        match spawned {
            Ok(_) => Ok(()),
            Err(e) => {
                let problem = format!("could not start {}: {e}", exe.display());
                notify::failed(&launch, mailboxes, &problem, SystemTime::now())
                    .and(Err(e).context("start notify-run"))
            }
        }
    });
    if let Err(e) = sent {
        eprintln!("loopherd watch: warning: notify: {e:#}");
    }
}

fn run_notify(member: String, sender: notify::Sender, args: Vec<String>) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let launch = notify::Launch {
        sender,
        member,
        args,
    };
    notify::run(
        &launch,
        &ledger.reports_dir()?,
        notify::TIMEOUT,
        SystemTime::now(),
    )?;
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
        pane: own_pane().as_deref(),
        socket: socket_from_env().as_deref(),
        shell_pid: &|| match own_pane() {
            Some(pane) => CliHerdr.shell_pid(&pane),
            None => Ok(None),
        },
    })
}

/// The session `session` lives on in, unchanged when Claude's dir is unknown.
fn live_session(session: &str) -> String {
    match install_hook::claude_dir() {
        Ok(dir) => continuation::live_session(&dir, session),
        Err(_) => session.to_string(),
    }
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
        lineage: &own_lineage(),
        socket: socket.as_deref(),
        claude_pid: process::claude_pid_from_env(&process::ProcFs),
    };
    let reports = ledger.reports_dir()?;
    let state = ledger.dir()?;
    if let Some(place) = shepherd_here(&state, Some(pane.id))
        && let Err(e) = stop_hook::shepherd_stopped(&state, &place, &input, SystemTime::now())
    {
        eprintln!("loopherd stop-hook: warning: record the shepherd's stop: {e:#}");
    }
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
        &live_session,
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

/// Exits 0 on every error, after printing it: a running watch's poll
/// still covers a missed `blocked` or `gone`, though not a missed move.
fn run_herdr_event_hook() -> Result<i32> {
    let record = || -> Result<()> {
        let event = std::env::var("HERDR_PLUGIN_EVENT").context("no HERDR_PLUGIN_EVENT")?;
        let json =
            std::env::var("HERDR_PLUGIN_EVENT_JSON").context("no HERDR_PLUGIN_EVENT_JSON")?;
        let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
        let reports = ledger.reports_dir()?;
        pane_event::event_hook(
            &mut ledger,
            &reports,
            &event,
            &json,
            socket_from_env().as_deref(),
            SystemTime::now(),
            &pane_shell,
        )
    };
    if let Err(e) = record() {
        eprintln!("loopherd herdr-event-hook: {e:#}");
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
        lineage: &own_lineage(),
        socket: socket.as_deref(),
        claude_pid: process::claude_pid_from_env(&process::ProcFs),
    };
    match shepherd_resume(&ledger, &pane, &input) {
        Ok(None) => {}
        Ok(Some(context)) => println!(
            "{}",
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }})
        ),
        Err(e) => eprintln!("loopherd session-hook: warning: shepherd resume: {e:#}"),
    }
    if let Err(e) = pane_label::session_started(&CliHerdr, &ledger.reports_dir()?, pane.id, &input)
    {
        eprintln!(
            "loopherd session-hook: warning: clear pane {}'s label: {e:#}",
            pane.id
        );
    }
    session_hook::session_hook(
        &mut ledger,
        &process::ProcFs,
        &pane,
        &input,
        &live_session,
        &mut |warning| eprintln!("loopherd session-hook: {warning}"),
    )?;
    Ok(0)
}

/// Follows the Claude where the shepherd's last handoff was recorded across
/// its session starts, and returns its resume context on the `/clear` that resumes it.
fn shepherd_resume(
    ledger: &FileLedger,
    pane: &session_hook::Pane,
    input: &[u8],
) -> Result<Option<String>> {
    let Some(place) = own_place(Some(pane.id)) else {
        return Ok(None);
    };
    shepherd_resume::session_started(&ledger.dir()?, &place, input, &live_session)
}

/// [`own_place`] when the shepherd's handoff record names it.
fn shepherd_here(state: &Path, pane: Option<&str>) -> Option<shepherd_resume::Place> {
    let recorded = shepherd_resume::recorded_place(state)?;
    own_place(pane).filter(|place| *place == recorded)
}

/// This pane, herdr and the Claude this process runs under, as a shepherd
/// record names them; `None` when any is unknown.
fn own_place(pane: Option<&str>) -> Option<shepherd_resume::Place> {
    let socket = socket_from_env()?;
    let pid = process::claude_pid_from_env(&process::ProcFs)?;
    let start = process::ProcFs.start(pid)?;
    Some(shepherd_resume::Place {
        pane: pane?.to_string(),
        socket,
        process: ledger::Process { pid, start },
    })
}

fn run_prompt_hook() -> Result<i32> {
    let Some(pane) = own_pane() else {
        return Ok(0);
    };
    let mut input = vec![];
    std::io::stdin()
        .read_to_end(&mut input)
        .context("read hook input")?;
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let state = ledger.dir()?;
    if let Some(place) = shepherd_here(&state, Some(&pane)) {
        let mailboxes = ledger.reports_dir()?;
        let mut notify = |text: &str| match Paths::from_env() {
            Ok(paths) => {
                notify_self_restart(text, &paths.herds.with_file_name("config.toml"), &mailboxes)
            }
            Err(e) => eprintln!("loopherd prompt-hook: warning: sent no notification: {e:#}"),
        };
        match shepherd_restart::prompted(&state, &place, &input, &mut notify) {
            Ok(None) => {}
            Ok(Some(context)) => println!(
                "{}",
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": context,
                }})
            ),
            Err(e) => eprintln!("loopherd prompt-hook: warning: shepherd self-restart: {e:#}"),
        }
    }
    pane_label::prompted(&CliHerdr, &ledger.reports_dir()?, &pane, &input)?;
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
        lineage: &own_lineage(),
        socket: Some(&socket),
    };
    let switches = |herd: &str| herd::lead_switches(&Paths::from_env()?, herd);
    if let Some(reason) = tool_hook::tool_hook(&ledger, &pane, &input, &switches)? {
        println!("{}", tool_hook::deny(&reason));
    }
    Ok(0)
}

fn run_doctor(
    verbose: bool,
    json: bool,
    settings: Option<PathBuf>,
    trackers: bool,
    herd: Option<String>,
) -> Result<i32> {
    let setup = || -> Result<doctor::Setup> {
        Ok(doctor::Setup {
            paths: herd::Paths::from_env()?,
            ledger: FileLedger::new(FileLedger::default_path()?),
        })
    };
    let checks = doctor_install(settings).and_then(|install| {
        let mut checks = doctor::checks(&install, &setup(), herd.as_deref(), &CliHerdr)?;
        if let Ok(paths) = Paths::from_env() {
            let rules = github::GhCli::default();
            checks.extend(doctor::ruleset_checks(&paths, herd.as_deref(), &rules));
        }
        if trackers {
            let paths = Paths::from_env()?;
            let only = herd.as_deref();
            checks.extend(doctor::tracker_checks(
                &paths,
                only,
                doctor::TRACKER_TIMEOUT,
            ));
        }
        Ok(checks)
    });
    let checks = match checks {
        Ok(checks) => checks,
        Err(e) => {
            eprintln!("loopherd doctor: {e:#}");
            return Ok(2);
        }
    };
    match json {
        true => println!("{}", doctor::json(&checks)),
        false => print!("{}", doctor::text(&checks, verbose)),
    }
    Ok(doctor::exit_code(&checks))
}

fn doctor_install(settings: Option<PathBuf>) -> Result<doctor::Install> {
    let settings_given = settings.is_some();
    let skill = skill_layout(None, settings.as_deref())?;
    let settings = match settings {
        Some(path) => path,
        None => install_hook::default_settings()?,
    };
    // No config dir, or a [notify] that doesn't load, leaves pane labels off.
    let pane_label = herd::herds_dir()
        .is_some_and(|herds| notify::labels_panes(&herds.with_file_name("config.toml")));
    Ok(doctor::Install {
        settings,
        settings_given,
        skill,
        pane_label,
        on_path: on_path("loopherd"),
        running: std::env::current_exe().context("find this binary")?,
        sandbox: sandbox::probe(),
        wsl_interop: sandbox::wsl_interop(),
    })
}

/// The `name` a shell would run: the first executable file of that name on PATH.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The skill layout `install-hook` and `doctor` use for these flags.
fn skill_layout(skills_dir: Option<PathBuf>, settings: Option<&Path>) -> Result<skill::Layout> {
    let home = || {
        Ok(PathBuf::from(
            std::env::var_os("HOME").context("HOME is not set")?,
        ))
    };
    skill::Layout::resolve(skills_dir, settings, home, install_hook::claude_dir)
}

fn install_hook_command(settings_given: bool, settings: &Path) -> String {
    match settings_given {
        true => format!(
            "loopherd install-hook --settings {}",
            shell::quote(&settings.to_string_lossy())
        ),
        false => "loopherd install-hook".into(),
    }
}

fn run_install_hook(
    remove: bool,
    settings: Option<PathBuf>,
    skills_dir: Option<PathBuf>,
) -> Result<i32> {
    let settings_given = settings.is_some();
    let layout = skill_layout(skills_dir, settings.as_deref())?;
    let settings = match settings {
        Some(path) => path,
        None => install_hook::default_settings()?,
    };
    // First, so a skill that can't be written leaves the settings untouched.
    let skill = layout.skill();
    let outcome = match remove {
        true => skill::uninstall(&layout)?,
        false => skill::install(&layout)?,
    };
    match (remove, outcome.skill) {
        (true, true) => println!("removed {}", skill.display()),
        (true, false) => println!("no skill at {}", skill.display()),
        (false, true) => println!("installed {}", skill.display()),
        (false, false) => println!("{} already up to date", skill.display()),
    }
    if let skill::Layout::Linked { link, .. } = &layout {
        let link = link.join("loopherd");
        match (outcome.link, &outcome.left_alone, remove) {
            (true, _, false) => println!(
                "linked {} to {}",
                link.display(),
                skill.parent().unwrap().display()
            ),
            (true, _, true) => println!("removed {}", link.display()),
            (false, Some(why), true) => println!("left {} alone: it {why}", link.display()),
            (false, Some(why), false) => println!(
                "left {} alone: it {why}; move it aside, then rerun `{}`",
                link.display(),
                install_hook_command(settings_given, &settings),
            ),
            (false, None, _) => {}
        }
    }
    let outcome = match remove {
        true => install_hook::remove(&settings, SystemTime::now())?,
        false => {
            let exe = std::env::current_exe().context("find this binary")?;
            install_hook::install(&settings, &exe, SystemTime::now())?
        }
    };
    let done = match remove {
        true => {
            "removed loopherd's Stop, StopFailure, SessionStart, PreToolUse and UserPromptSubmit hooks from"
        }
        false => {
            "installed loopherd's Stop, StopFailure, SessionStart, PreToolUse and UserPromptSubmit hooks in"
        }
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

/// The shell herdr started in `pane`, which a moved member's old pane id is kept for.
fn pane_shell(pane: &str) -> Result<Option<Process>> {
    let pid = CliHerdr.shell_pid(pane)?;
    Ok(pid.and_then(|pid| {
        let start = process::ProcFs.start(pid)?;
        Some(Process { pid, start })
    }))
}

/// loopherd and its ancestors, which carry a moved member's old pane id.
fn own_lineage() -> Vec<ledger::Process> {
    process::lineage(&process::ProcFs, std::process::id())
}

fn own_pane() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty())
}

fn run_spawn(args: SpawnArgs) -> Result<i32> {
    let ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    broker::refuse_spawn(own_broker().as_ref(), &ledger)?;
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
    let inherited = inherited_workspace(&ledger)?;
    let spawned = spawn(&CliHerdr, &mut ledger, &req, inherited.as_deref())?;
    let (message, code) = spawned.report(&req.name);
    println!("{message}");
    Ok(code)
}

fn run_add(args: AddArgs) -> Result<i32> {
    let op = broker::Op::Add {
        herd: args.herd,
        template: args.template,
        params: args.params,
        claude_args: args.claude_args,
        dry_run: args.dry_run,
        ready_timeout_secs: args.ready_timeout,
        no_focus: args.no_focus,
    };
    run_op(own_broker().as_ref(), &op)
}

fn run_lead(args: LeadArgs) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let req = LeadRequest {
        herd: args.herd,
        claude_args: args.claude_args,
        ready_timeout: Duration::from_secs(args.ready_timeout),
        sandbox: sandbox::probe,
        broker: broker::start,
    };
    let mut ledger = FileLedger::new(FileLedger::default_path().context("ledger")?);
    let inherited = inherited_workspace(&ledger)?;
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
    let op = broker::Op::Feed {
        herd,
        dry_run,
        ready_timeout_secs: ready_timeout,
    };
    run_op(own_broker().as_ref(), &op)
}

fn own_process() -> Result<Process> {
    let pid = std::process::id();
    let start = process::ProcFs
        .start(pid)
        .context("read this process's start time")?;
    Ok(Process { pid, start })
}

fn run_init(args: InitArgs) -> Result<i32> {
    let diff = args.diff;
    match init_or_diff(args) {
        // diff(1)'s codes, so trouble can't read as drift.
        Err(e) if diff => {
            eprintln!("loopherd init: {e:#}");
            Ok(2)
        }
        result => result,
    }
}

fn init_or_diff(args: InitArgs) -> Result<i32> {
    let paths = Paths::from_env().context("herd")?;
    let dir = std::env::current_dir().context("current directory")?;
    let req = init::Request {
        dir: &dir,
        herd: args.herd.as_deref(),
        checks: &args.checks,
        gh_account: args.gh_account.as_deref(),
        github: args.repo.as_deref(),
        no_github: args.no_github,
        tracker: match (args.tracker, args.tracker_desc) {
            (Some(TrackerArg::Other), desc) => Some(init::Tracker::Other(desc)),
            (_, Some(_)) => bail!("--tracker-desc goes with --tracker other"),
            (Some(TrackerArg::Github), None) => Some(init::Tracker::GitHub),
            (Some(TrackerArg::Local), None) => Some(init::Tracker::Local),
            (None, None) => None,
        },
        max: args.max.map(|max| max as u64),
        lead: args.no_lead.then_some(false),
        merge_guard: args.merge_guard,
        context_limit: args.context_limit,
    };
    if args.diff {
        return Ok(match init::diff(&paths, req)? {
            None => 0,
            Some(diff) => {
                print!("{diff}");
                1
            }
        });
    }
    for line in init::init(&paths, req)? {
        println!("{line}");
    }
    Ok(0)
}

fn run_herds(settings: Option<PathBuf>) -> Result<i32> {
    let settings = match settings {
        Some(settings) => settings,
        None => install_hook::default_settings()?,
    };
    let (report, code) = herds(&Paths::from_env().context("herd")?, &settings)?;
    println!("{report}");
    Ok(code)
}

fn inherited_workspace(ledger: &dyn Ledger) -> Result<Option<String>> {
    let env = std::env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|v| !v.is_empty());
    Ok(spawn::inherited_workspace(
        &ledger.entries().context("ledger")?,
        own_pane().as_deref(),
        &own_lineage(),
        socket_from_env().as_deref(),
        env,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_a_reinstall_replaced_runs_as_loopherd_on_path() {
        let gone = PathBuf::from("/home/u/.local/bin/loopherd (deleted)");
        let here = PathBuf::from("/home/u/.local/bin/loopherd");

        assert_eq!(runnable(gone), PathBuf::from("loopherd"));
        assert_eq!(runnable(here.clone()), here);
    }

    #[test]
    fn merges_are_looked_up_only_while_notify_sends_merge_alerts() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let gh = github::fake::FakeMerges::new(dir.path(), vec![]);
        for (notify, looks) in [
            ("", false),
            ("[notify]\nenabled = false\n", false),
            (
                "[notify]\nenabled = true\nkinds = ['question', 'blocked']\n",
                false,
            ),
            ("[notify]\nenabled = true\nkinds = ['merge_alert']\n", true),
            ("[notify]\nenabled = true\n", true),
        ] {
            std::fs::write(&config, notify).unwrap();

            assert_eq!(merge_lookups(&config, &gh).is_some(), looks, "{notify}");
        }
    }

    #[test]
    fn tell_takes_text_that_starts_with_a_hyphen() {
        for text in ["- fix the test", "--text"] {
            let cli = Cli::try_parse_from(["loopherd", "tell", "lh-1", text]).unwrap();
            let Some(Command::Tell {
                name, text: sent, ..
            }) = cli.command
            else {
                panic!("not a tell: {text}");
            };
            assert_eq!((name.as_str(), sent.as_deref()), ("lh-1", Some(text)));
        }
        let cli = Cli::try_parse_from(["loopherd", "tell", "lh-1", "--text-file", "m.md"]).unwrap();
        let Some(Command::Tell {
            text, text_file, ..
        }) = cli.command
        else {
            panic!("not a tell");
        };
        assert_eq!((text, text_file), (None, Some(PathBuf::from("m.md"))));
    }

    #[test]
    fn tell_sends_a_flag_name_as_text_after_a_double_dash() {
        for flag in ["--help", "-h", "--text-file"] {
            let cli = Cli::try_parse_from(["loopherd", "tell", "lh-1", "--", flag]).unwrap();
            let Some(Command::Tell { text, .. }) = cli.command else {
                panic!("not a tell: {flag}");
            };
            assert_eq!(text.as_deref(), Some(flag));
        }
    }

    #[test]
    fn a_feeds_own_process_is_live_by_its_pid_and_start_time() {
        let owner = own_process().unwrap();
        assert_eq!(owner.pid, std::process::id());
        assert_eq!(process::live(&process::ProcFs, Some(owner)), Some(owner));
    }
}
