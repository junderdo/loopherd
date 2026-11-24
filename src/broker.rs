//! A lead's broker (D98): one process per lead, outside its sandbox, that
//! carries out the lead's herdr-touching operations within its herd's reach.
//!
//! The lead writes requests to its own folder in the state dir; the broker
//! answers in the config dir, which the lead can read but not write.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::context::SHEPHERD;
use crate::feed::{FeedRequest, ForLead, run_after_merge};
use crate::github::PullRequests;
use crate::herd::{AddRequest, Paths};
use crate::herdr::Herdr;
use crate::ledger::{Ledger, Process};
use crate::lock::Lock;
use crate::process::{self, Procs};
use crate::report::{Body, Kind};
use crate::restart::RestartRequest;
use crate::sandbox::{self, Guard};
use crate::spawn::validate_name;
use crate::tool_hook;

/// An operation a lead asks its broker for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    Tell {
        name: String,
        text: String,
    },
    Feed {
        herd: String,
        dry_run: bool,
        ready_timeout_secs: u64,
    },
    Add {
        herd: String,
        template: String,
        params: Vec<String>,
        /// Refused from a lead: a worker's command line comes only from its herd file.
        claude_args: Vec<String>,
        dry_run: bool,
        ready_timeout_secs: u64,
        /// Leaves focus where it is after starting an attended member.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_focus: bool,
    },
    Prompt {
        name: String,
        force: bool,
    },
    Restart {
        name: String,
        force: bool,
    },
    /// The lead restarting itself once its turn after its handoff ends.
    RestartSelf,
    Retire {
        name: String,
        close: bool,
        force: bool,
    },
    Release {
        name: String,
        keep_branch: bool,
        drop_branch: bool,
    },
    /// The lead's own report, written to its mailbox; a question also
    /// labels its pane.
    Report {
        kind: Kind,
        text: String,
    },
    /// `loopherd status`; a lead sees its own herd alone.
    Status {
        all: bool,
        /// Refused from a lead: strays are herdr's agents outside every herd.
        strays: bool,
        json: bool,
        color: bool,
    },
    /// A member's last `lines` lines of output.
    Read {
        name: String,
        lines: usize,
    },
    /// The herd file's `[lead] gate` commands, run on a commit pushed to
    /// origin, merged with origin's default branch.
    Gate {
        rev: String,
        /// A branch of origin's to merge instead of its default, as a stacked PR's base.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
}

/// What an operation acts on, for the broker's reach rule.
enum Reach<'a> {
    Member(&'a str),
    /// A member, retired or not: retire --close reruns and release needs one retired.
    AnyMember(&'a str),
    Herd(&'a str),
    /// The lead itself, found by its pane.
    Lead,
}

/// Who runs an operation and what it runs against.
pub struct Ctx<'a> {
    pub herdr: &'a dyn Herdr,
    pub ledger: &'a mut dyn Ledger,
    pub paths: &'a Paths,
    pub procs: &'a dyn Procs,
    pub claude_dir: Option<PathBuf>,
    pub caller: Caller,
    pub live: LiveSession,
    pub spawn_waiter: SpawnWaiter,
    pub prs: &'a dyn PullRequests,
}

/// The session a Claude session lives on in, after any continuations.
pub type LiveSession = fn(&str) -> String;

/// Starts `restart-wait` for a member, handing it the claim it holds.
pub type SpawnWaiter = fn(&str, Lock) -> Result<()>;

/// Where an operation runs: loopherd's own pane and process, or, in a
/// broker, the pane of the lead it serves.
pub struct Caller {
    pub pane: Option<String>,
    pub socket: Option<PathBuf>,
    pub lineage: Vec<Process>,
    /// `$HERDR_WORKSPACE_ID`, or in a broker its lead's.
    pub workspace: Option<String>,
    pub process: Process,
    /// The lead a broker serves.
    pub lead: Option<ForLead>,
}

/// Takes an operation's output as it is made.
pub trait Sink {
    fn out(&mut self, text: &str);
    fn err(&mut self, text: &str);
}

impl Op {
    fn command(&self) -> &'static str {
        match self {
            Op::Tell { .. } => "tell",
            Op::Feed { .. } => "feed",
            Op::Add { .. } => "add",
            Op::Prompt { .. } => "prompt",
            Op::Restart { .. } => "restart",
            Op::RestartSelf => "restart --self",
            Op::Retire { .. } => "retire",
            Op::Release { .. } => "release",
            Op::Report { .. } => "report",
            Op::Status { .. } => "status",
            Op::Read { .. } => "read",
            Op::Gate { .. } => "gate",
        }
    }

    fn reach(&self) -> Reach<'_> {
        match self {
            Op::Tell { name, .. }
            | Op::Prompt { name, .. }
            | Op::Restart { name, .. }
            | Op::Read { name, .. } => Reach::Member(name),
            Op::Feed { herd, .. } | Op::Add { herd, .. } => Reach::Herd(herd),
            // Status reads the herd the launch record names, never one a request does.
            Op::RestartSelf | Op::Report { .. } | Op::Status { .. } | Op::Gate { .. } => {
                Reach::Lead
            }
            Op::Retire { name, .. } | Op::Release { name, .. } => Reach::AnyMember(name),
        }
    }

    /// Herd changes run one at a time, apart from the quick requests and
    /// from status, whose queue source and backlog may each take a git timeout.
    fn lane(&self) -> Lane {
        match self {
            Op::Feed { .. }
            | Op::Add { .. }
            | Op::Restart { .. }
            | Op::Retire { .. }
            | Op::Release { .. } => Lane::Herd,
            Op::Tell { .. }
            | Op::Read { .. }
            | Op::Prompt { .. }
            | Op::RestartSelf
            | Op::Report { .. } => Lane::Quick,
            Op::Status { .. } => Lane::Status,
            Op::Gate { .. } => Lane::Gate,
        }
    }

    /// How long a lead waits for its broker to answer, `remote` being the
    /// broker's git timeout.
    fn answer_within(&self, remote: Duration) -> Duration {
        match self {
            Op::Tell { .. }
            | Op::Prompt { .. }
            | Op::RestartSelf
            | Op::Read { .. }
            | Op::Report { .. } => Duration::from_secs(120),
            // Its queue's source and backlog may each take the git timeout.
            Op::Status { .. } => remote * 2 + Duration::from_secs(120),
            // Its settle timeout, with room for herdr and the member's view.
            Op::Restart { .. } => Duration::from_secs(300),
            Op::Retire { .. } | Op::Release { .. } => origin_wait(remote),
            Op::Gate { .. } => GATE_WITHIN + origin_wait(remote),
            // Each member may take its ready timeout, then after_merge its own.
            Op::Feed { .. } => FEED_WITHIN,
            Op::Add {
                ready_timeout_secs, ..
            } => Duration::from_secs((*ready_timeout_secs).min(MAX_READY_TIMEOUT_SECS) + 300),
        }
    }

    /// Carries the operation out, returning its exit code.
    fn carry_out(&self, ctx: &mut Ctx, sink: &mut dyn Sink) -> Result<i32> {
        match self {
            Op::Tell { name, text } => {
                let told = crate::tell::tell(ctx.herdr, name, text, crate::tell::WORKING_TIMEOUT)?;
                sink.out(&told);
                Ok(0)
            }
            Op::Feed {
                herd,
                dry_run,
                ready_timeout_secs,
            } => feed(ctx, herd, *dry_run, *ready_timeout_secs, sink),
            Op::Add {
                herd,
                template,
                params,
                claude_args,
                dry_run,
                ready_timeout_secs,
                no_focus,
            } => {
                let req = AddRequest {
                    herd: herd.clone(),
                    template: template.clone(),
                    params: params.clone(),
                    claude_args: claude_args.clone(),
                    ready_timeout: Duration::from_secs(*ready_timeout_secs),
                    return_pane: ctx.caller.pane.clone(),
                    focus: !no_focus,
                    sandbox: crate::sandbox::probe,
                };
                add(ctx, &req, *dry_run, sink)
            }
            Op::Prompt { name, force } => {
                let guard = ctx.guard();
                crate::prompt::prompt(ctx.herdr, ctx.ledger, name, *force, &guard)?;
                sink.out(&format!("prompted {name}"));
                Ok(0)
            }
            Op::Restart { name, force } => restart(ctx, name, *force, sink),
            Op::RestartSelf => {
                let caller = &ctx.caller;
                let state = ctx.paths.state_dir();
                let scheduled = match &caller.lead {
                    Some(lead) => crate::restart::schedule_member(
                        &own_record(ctx.herdr, &*ctx.ledger, &lead.name)?,
                        state,
                    )?,
                    None => crate::restart::schedule_self(
                        ctx.herdr,
                        &*ctx.ledger,
                        caller.pane.as_deref(),
                        &caller.lineage,
                        state,
                    )?,
                };
                (ctx.spawn_waiter)(&scheduled.name, scheduled.waiter)?;
                sink.out(&scheduled.message);
                Ok(0)
            }
            Op::Retire { name, close, force } => retire(ctx, name, *close, *force, sink),
            Op::Release {
                name,
                keep_branch,
                drop_branch,
            } => release(ctx, name, *keep_branch, *drop_branch, sink),
            Op::Report { kind, text } => report(ctx, *kind, text, sink),
            // The broker refused strays already.
            Op::Status {
                all, json, color, ..
            } => status(ctx, *all, *json, *color, sink),
            Op::Gate { rev, base } => gate(ctx, rev, base.as_deref(), sink),
            Op::Read { name, lines } => {
                let read = crate::read::read(ctx.herdr, &*ctx.ledger, name, *lines)?;
                // A reply drops empty output, so an empty screen prints nothing either way.
                let read = read.trim_end_matches('\n');
                if !read.is_empty() {
                    sink.out(read);
                }
                Ok(0)
            }
        }
    }
}

/// A lead's `loopherd status`: its own herd's. The shepherd's runs directly.
fn status(ctx: &mut Ctx, all: bool, json: bool, color: bool, sink: &mut dyn Sink) -> Result<i32> {
    use crate::status::{Format, Queues};
    let herd = &ctx
        .caller
        .lead
        .as_ref()
        .context("only a lead's broker runs status")?
        .herd;
    let format = match json {
        true => Format::Json,
        false => Format::Text { color },
    };
    let pauses = crate::pause::Pauses {
        dir: ctx.paths.paused.clone(),
        herds: Some(ctx.paths.herds.clone()),
    };
    let timeout = crate::worktree::remote_timeout().unwrap_or_else(|e| {
        let default = humantime::format_duration(crate::worktree::REMOTE_TIMEOUT);
        sink.err(&format!(
            "loopherd status: warning: {e:#}; counting tasks with the default {default}"
        ));
        crate::worktree::REMOTE_TIMEOUT
    });
    let queues = Queues {
        paths: ctx.paths,
        timeout,
    };
    let out = crate::status::herd_status(
        ctx.herdr,
        &*ctx.ledger,
        &pauses,
        Some(&queues),
        all,
        format,
        herd,
        json.then_some(&crate::status::Subagents {
            mailboxes: &ctx.paths.state_dir().join("reports"),
            now: SystemTime::now(),
        }),
    )?;
    if !out.stdout.is_empty() {
        sink.out(out.stdout.trim_end_matches('\n'));
    }
    for line in out.stderr.lines() {
        sink.err(&format!("loopherd status: {line}"));
    }
    Ok(out.code)
}

/// Writes the lead's report to its own mailbox, then labels its pane for a
/// question, from the lead's own record; a label that fails only warns.
fn report(ctx: &mut Ctx, kind: Kind, text: &str, sink: &mut dyn Sink) -> Result<i32> {
    let lead = ctx
        .caller
        .lead
        .as_ref()
        .context("only a lead's broker writes its report")?;
    let mailboxes = ctx.paths.state_dir().join("reports");
    let entry = crate::report::Entry::new(SystemTime::now(), Body::report(kind, text));
    crate::report::append(&mailboxes, &lead.name, &entry)?;
    if kind != Kind::Question || !crate::notify::labels_panes(&crate::herd::config_file(ctx.paths))
    {
        return Ok(0);
    }
    let labelled = match own_record(ctx.herdr, &*ctx.ledger, &lead.name) {
        Ok(record) => crate::pane_label::lead_asked(
            ctx.herdr,
            &*ctx.ledger,
            &mailboxes,
            &record.pane_id,
            &record.name,
            text,
        )
        .with_context(|| format!("label pane {}", record.pane_id)),
        Err(e) => Err(e.context("the report went out, but its pane gets no label")),
    };
    if let Err(e) = labelled {
        sink.err(&format!("loopherd report: warning: {e:#}"));
    }
    Ok(0)
}

/// How long each of a gate's commands may run.
const GATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How long a lead waits on its gate's commands, past fetching and merging.
const GATE_WITHIN: Duration = Duration::from_secs(4 * 60 * 60);

/// Runs the lead's herd's gate on `rev` merged with origin's `base` branch,
/// else its default branch, in a worktree of the lead's own; exit 1 when a command fails or the merge conflicts.
fn gate(ctx: &mut Ctx, rev: &str, base: Option<&str>, sink: &mut dyn Sink) -> Result<i32> {
    let lead = ctx.caller.lead.as_ref().context(
        "gate runs in a lead's broker, from its pane; elsewhere run the checks directly",
    )?;
    let commands = crate::herd::lead_gate(ctx.paths, &lead.herd)?;
    if commands.is_empty() {
        bail!(
            "refused: herd {} sets no [lead] gate, the commands to check a merge with; \
             ask the human to add them",
            lead.herd
        );
    }
    let is_sha = (7..=40).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_sha {
        bail!("refused: {rev:?} is not a commit sha");
    }
    let repo = crate::herd::repo(ctx.paths, &lead.herd)?;
    let Some(sha) = crate::worktree::pushed_commit(&repo, rev)? else {
        bail!(
            "refused: {rev} is on none of origin's branches in {}; push it, then gate it",
            repo.display()
        );
    };
    let slug = format!("loopherd-gate-{}", lead.name);
    let merged = crate::worktree::merge_with_base(&repo, &slug, &sha, base)?;
    let checked = format!("gate {sha} merged with {} {}", merged.base, merged.base_sha);
    if !merged.conflicts.is_empty() {
        let files = merged.conflicts.join("\n");
        sink.out(&format!(
            "{checked}: failed: it doesn't merge cleanly; conflicts in:\n{files}"
        ));
        return Ok(1);
    }
    match crate::feed::run_commands(&commands, &merged.path, GATE_COMMAND_TIMEOUT) {
        None => {
            sink.out(&format!("{checked}: pass"));
            Ok(0)
        }
        Some((why, tail)) => {
            sink.out(&format!("{checked}: failed: {why}{tail}"));
            Ok(1)
        }
    }
}

/// How long a lead waits on an operation that asks origin a few things in
/// turn, each within `remote`, the git timeout: release usually asks nine,
/// and a fetch may retry once or `remote set-head` join them, so twice that.
fn origin_wait(remote: Duration) -> Duration {
    remote * 18 + Duration::from_secs(300)
}

/// Lead `lead`'s own unretired record, in this herdr; by name, since a stale
/// entry may share its pane id.
fn own_record(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    lead: &str,
) -> Result<crate::ledger::AgentRecord> {
    let record = ledger
        .entries()?
        .remove(lead)
        .filter(|r| !r.retired)
        .ok_or_else(|| anyhow!("{lead} has no unretired record in the ledger"))?;
    if record.socket != herdr.socket() {
        bail!("{lead} runs in another herdr than its broker's");
    }
    Ok(record)
}

/// The longest ready timeout a lead may ask for.
const MAX_READY_TIMEOUT_SECS: u64 = 60 * 60;

/// Long enough for `scripts/install` to build a release binary.
const AFTER_MERGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long a lead waits on a feed through its broker.
const FEED_WITHIN: Duration = Duration::from_secs(4 * 60 * 60);

fn feed_request<'a>(
    ctx: &Ctx<'a>,
    herd: &str,
    dry_run: bool,
    ready_timeout_secs: u64,
) -> Result<FeedRequest<'a>> {
    let caller = &ctx.caller;
    Ok(FeedRequest {
        herd: herd.into(),
        dry_run,
        ready_timeout: Duration::from_secs(ready_timeout_secs),
        command_timeout: crate::worktree::remote_timeout()?,
        own_pane: caller.pane.clone(),
        own_lineage: caller.lineage.clone(),
        own_socket: caller.socket.clone(),
        mailboxes: ctx.paths.state_dir().join("reports"),
        claude_dir: ctx.claude_dir.clone(),
        after_merge_timeout: AFTER_MERGE_TIMEOUT,
        // Another feed holds the lock across all its members' commands.
        after_merge_wait: FEED_WITHIN,
        owner: caller.process,
        procs: ctx.procs,
        lead: caller.lead.clone(),
        sandbox: crate::sandbox::probe,
    })
}

fn feed(
    ctx: &mut Ctx,
    herd: &str,
    dry_run: bool,
    ready_timeout_secs: u64,
    sink: &mut dyn Sink,
) -> Result<i32> {
    let req = feed_request(ctx, herd, dry_run, ready_timeout_secs)?;
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A dry run changes nothing, so it must not block a real one.
        let lock = (!dry_run).then(|| ctx.paths.lock_feed()).transpose()?;
        let inherited = ctx.inherited_workspace()?;
        let mut fed =
            crate::feed::feed(ctx.herdr, ctx.ledger, ctx.paths, &req, inherited.as_deref())?;
        drop(lock);
        for line in &fed.lines {
            sink.out(line);
        }
        let printed = fed.lines.len();
        run_after_merge(ctx.ledger, ctx.paths, &req, &mut fed)?;
        for line in &fed.lines[printed..] {
            sink.out(line);
        }
        Ok(fed.code)
    }));
    let panicked = ran.as_ref().err().map(|payload| {
        let text = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
        format!("the feed panicked: {}", text.unwrap_or("no message"))
    });
    let failed = match (&ran, &panicked) {
        (Ok(Ok(_)), _) => None,
        (Ok(Err(e)), _) => Some(format!("the feed failed before noting an outcome: {e:#}")),
        (Err(_), panicked) => panicked
            .as_ref()
            .map(|p| format!("{p}, before noting an outcome")),
    };
    // A broker outlives its feeds, so no later feed would see these as interrupted.
    if let Some(why) = failed.filter(|_| !dry_run) {
        for line in crate::feed::abandon(ctx.ledger, &req, &why) {
            sink.err(&line);
        }
    }
    // An error, not a panic, so a broker's reply keeps the lines above.
    ran.unwrap_or_else(|_| Err(anyhow!(panicked.unwrap_or_default())))
}

fn add(ctx: &mut Ctx, req: &AddRequest, dry_run: bool, sink: &mut dyn Sink) -> Result<i32> {
    let planned = crate::herd::resolve(ctx.paths, req)?;
    if let Some(why) = ctx.guard().spawn_refusal(&planned, &*ctx.ledger)? {
        bail!("refused: {why}");
    }
    let inherited = ctx.inherited_workspace()?;
    if dry_run {
        let plan = crate::herd::dry_run(ctx.herdr, ctx.paths, req, inherited.as_deref())?;
        sink.out(&plan);
        return Ok(0);
    }
    let added = crate::herd::add(ctx.herdr, ctx.ledger, ctx.paths, req, inherited.as_deref())?;
    let (message, code) = added.spawned.report(&added.name);
    sink.out(&message);
    if let Some(warning) = added.warning {
        sink.err(&format!("loopherd add: warning: {warning}"));
    }
    Ok(code)
}

fn restart(ctx: &mut Ctx, name: &str, force: bool, sink: &mut dyn Sink) -> Result<i32> {
    let (herdr, procs) = (ctx.herdr, ctx.procs);
    let pane_shell = |pane: &str| -> Result<Option<Process>> {
        Ok(herdr.shell_pid(pane)?.and_then(|pid| {
            let start = procs.start(pid)?;
            Some(Process { pid, start })
        }))
    };
    let caller = &ctx.caller;
    let req = RestartRequest {
        name,
        force,
        own_pane: caller.pane.as_deref(),
        own_lineage: &caller.lineage,
        pane_shell: &pane_shell,
        state: ctx.paths.state_dir(),
        paths: ctx.paths,
        caller: caller.lead.as_ref().map(|lead| lead.name.as_str()),
        settle_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(250),
        live: &ctx.live,
    };
    sink.out(&crate::restart::restart(
        herdr,
        ctx.ledger,
        &req,
        SystemTime::now(),
    )?);
    Ok(0)
}

fn retire(ctx: &mut Ctx, name: &str, close: bool, force: bool, sink: &mut dyn Sink) -> Result<i32> {
    use crate::ledger::Outcome;
    use crate::retire::{refuse_own_lead, refuse_unfinished, retire_and_close};
    let caller = &ctx.caller;
    let pane = caller.pane.as_deref().map(|id| tool_hook::Pane {
        id,
        socket: caller.socket.as_deref(),
        lineage: &caller.lineage,
    });
    refuse_own_lead(
        &*ctx.ledger,
        name,
        pane.as_ref(),
        ctx.procs,
        caller.process.pid,
    )?;
    let mailboxes = ctx.paths.state_dir().join("reports");
    if !force {
        refuse_unfinished(
            ctx.herdr,
            &*ctx.ledger,
            name,
            &mailboxes,
            caller.lead.is_some(),
        )?;
    }
    let now = SystemTime::now();
    if !close {
        let record = crate::retire::retire(ctx.ledger, name, now, Outcome::Retired)?;
        sink.out(&format!("retired {name}"));
        // The lead's Claude may still be running on its settings.
        for line in ctx.guard().forget_launch(name, false) {
            sink.out(&line);
        }
        let warnings = crate::notify::forget_marks(&mailboxes, name)
            .into_iter()
            .chain(crate::usage::record_at_retire(
                &mailboxes,
                ctx.claude_dir.as_deref(),
                &record,
                now,
            ));
        for warning in warnings {
            sink.err(&warning);
        }
        return Ok(0);
    }
    let guard = ctx.guard();
    let mut lines = vec![];
    let result = retire_and_close(
        ctx.herdr,
        ctx.procs,
        ctx.ledger,
        &guard,
        name,
        now,
        Outcome::Retired,
        &mailboxes,
        ctx.claude_dir.as_deref(),
        pane.as_ref(),
        &mut lines,
    );
    lines.iter().for_each(|line| sink.out(line));
    result.map(|()| 0)
}

fn release(
    ctx: &mut Ctx,
    name: &str,
    keep_branch: bool,
    drop_branch: bool,
    sink: &mut dyn Sink,
) -> Result<i32> {
    use crate::worktree::Fate;
    // A feed spawning this name owns its fresh worktree before the ledger says so.
    let _feed = ctx.paths.lock_feed()?;
    let fate = match (keep_branch, drop_branch) {
        (true, _) => Fate::Keep,
        (_, true) => Fate::Drop,
        _ => Fate::Clear,
    };
    let guard = ctx.guard();
    let mut lines = vec![];
    let result = crate::release::release(
        ctx.herdr,
        ctx.prs,
        ctx.ledger,
        &guard,
        name,
        fate,
        SystemTime::now(),
        &ctx.paths.state_dir().join("reports"),
        &mut lines,
    );
    lines.iter().for_each(|line| sink.out(line));
    result.map(|()| 0)
}

impl Ctx<'_> {
    /// Refuses, for a broker, entries outside its lead's herd.
    fn guard(&self) -> Guard {
        let lead = self.caller.lead.as_ref().map(|lead| lead.name.clone());
        Guard::new(self.paths.clone(), lead)
    }

    /// The workspace a new member joins when its herd names none: the caller's.
    fn inherited_workspace(&self) -> Result<Option<String>> {
        let caller = &self.caller;
        Ok(crate::spawn::inherited_workspace(
            &self.ledger.entries().context("ledger")?,
            caller.pane.as_deref(),
            &caller.lineage,
            caller.socket.as_deref(),
            caller.workspace.clone(),
        ))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Done { out: String, err: String, code: i32 },
    Refused(String),
    Failed(String),
}

/// Output kept for a reply, one line per `out` or `err`.
#[derive(Default)]
struct Kept {
    out: Vec<String>,
    err: Vec<String>,
}

impl Sink for Kept {
    fn out(&mut self, text: &str) {
        self.out.push(text.into());
    }

    fn err(&mut self, text: &str) {
        self.err.push(text.into());
    }
}

/// How a broker's run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    /// Its lead was retired, or left the ledger.
    Retired,
    /// Another broker already serves the lead.
    AlreadyRunning,
    /// `loopherd broker-restart` asked it to stop.
    Stopped,
}

/// Set in a broker's environment, naming its lead, so the commands its
/// requests run know they run inside it.
pub const BROKER_ENV: &str = "LOOPHERD_BROKER_OF";

/// Requests bigger than this are refused unread.
const MAX_REQUEST: u64 = 1 << 20;
/// A reply nobody read by now belongs to a client that gave up.
const REPLY_KEPT: Duration = Duration::from_secs(600);

/// Runs `op` through the lead's broker when `client` is set, else in
/// `ctx`, returning its exit code.
pub fn run(ctx: &mut Ctx, client: Option<&Client>, op: &Op, sink: &mut dyn Sink) -> Result<i32> {
    let Some(client) = client else {
        refuse_unrouted(&*ctx.ledger, op.command())?;
        return op.carry_out(ctx, sink);
    };
    let (out, err, code) = client.call(op)?;
    if !out.is_empty() {
        sink.out(&out);
    }
    if !err.is_empty() {
        sink.err(&err);
    }
    Ok(code)
}

/// Refuses a lead's `loopherd spawn`, routed by `client`, and a spawn in a
/// sandbox that found no lead to route by.
pub fn refuse_spawn(client: Option<&Client>, ledger: &dyn Ledger) -> Result<()> {
    if client.is_some() {
        bail!(
            "a lead can't run loopherd spawn: add and feed start its herd's workers from its herd file"
        );
    }
    refuse_unrouted(ledger, "spawn")
}

/// Refuses a command that is the shepherd's or the human's, `why` saying so,
/// where `found` places a lead, or from a sandbox, where the ledger is
/// read-only, that found no lead. A pane alone places a lead only while its
/// recorded Claude lives, so the human's shell in a dead lead's pane runs it.
/// `ledger_free` commands run when the ledger's writability can't be told.
pub fn refuse_from_lead(
    found: Option<&Found>,
    ledger: &dyn Ledger,
    procs: &dyn Procs,
    why: &str,
    ledger_free: bool,
) -> Result<()> {
    let read_only = || match ledger.read_only() {
        Err(_) if ledger_free => Ok(false),
        read_only => read_only,
    };
    let lead = match found {
        None => {
            if read_only()? {
                bail!("refused: this loopherd {}, and {why}", cant_write(ledger));
            }
            return Ok(());
        }
        Some(Found::Lineage(lead)) => lead,
        Some(Found::Pane { lead, .. }) if read_only()? => lead,
        Some(Found::Pane {
            lead,
            process: None,
        }) => bail!(
            "refused: this is lead {lead}'s pane, and {why}; the ledger records no process \
             to tell whether {lead} still runs here, so run it from another pane"
        ),
        Some(Found::Pane { lead, process }) => match process::live(procs, *process) {
            None => return Ok(()),
            Some(_) => lead,
        },
    };
    bail!("refused: lead {lead} runs this loopherd, and {why}")
}

/// Refuses `command` run directly where the ledger is read-only: in a lead's
/// sandbox, yet the ledger places no lead in this pane, so no broker runs it.
pub fn refuse_unrouted(ledger: &dyn Ledger, command: &str) -> Result<()> {
    if ledger.read_only()? {
        bail!(
            "refused: this loopherd {}, so it can't run loopherd {command} itself, and the \
             ledger places no lead in this pane (as after the pane moved), so no lead's broker \
             can; if a lead runs here, ask the shepherd to run loopherd revive --name <lead>, \
             since loopherd doctor doesn't see a moved pane",
            cant_write(ledger)
        );
    }
    Ok(())
}

/// Why a read-only ledger is read-only, as far as loopherd can tell.
fn cant_write(ledger: &dyn Ledger) -> String {
    let dir = ledger
        .dir_shown()
        .map_or("its dir".into(), |dir| dir.display().to_string());
    format!("can't write the ledger (a lead's sandbox, or permissions on {dir})")
}

/// What a broker runs operations with, beside the ledger and paths.
pub struct Host<'a> {
    pub herdr: &'a (dyn Herdr + Sync),
    pub procs: &'a (dyn Procs + Sync),
    pub claude_dir: Option<PathBuf>,
    /// The broker's own process, which owns the after_merge its feeds take on.
    pub process: Process,
    pub live: LiveSession,
    pub spawn_waiter: SpawnWaiter,
    pub prs: &'a (dyn PullRequests + Sync),
    /// Moves the broker onto a reinstalled loopherd; `None` keeps it on its own.
    pub follow: Option<Follow<'a>>,
}

/// How a broker follows a reinstall of the loopherd on its PATH.
pub struct Follow<'a> {
    /// The loopherd its PATH resolves to now, canonicalized.
    pub installed: &'a (dyn Fn() -> Option<PathBuf> + Sync),
    /// Replaces this process with `exe` serving `lead` under `lock`, as
    /// [`reexec`] does; returns only when it couldn't.
    pub reexec: &'a (dyn Fn(&Path, &str, &Lock) -> anyhow::Error + Sync),
    /// How long an idle broker goes between looks; it also looks after each
    /// request, but never while one is in flight.
    pub every: Duration,
    /// The install this broker last moved onto, which it counts as running.
    pub moved_onto: Option<FileId>,
}

/// Serves lead `lead`'s requests, polling every `poll`, until it is retired.
/// Its herd comes from the record `loopherd lead` kept in the config dir,
/// never from a request or the ledger.
///
/// Requests run in four lanes, each one at a time: herd changes (feed,
/// add, restart, retire, release), status, gates, and everything else, so a long
/// feed holds up no tell, except while a restart drains the broker. A lane
/// holds at most [`LANE_WAITING`] requests waiting and refuses more as busy.
/// While its lead reads retired it refuses every request it hasn't started,
/// and returns once none is in flight.
///
/// `inherited` is the lock a broker it replaced handed it across `exec`.
/// When `host.follow` finds a new loopherd installed while no request is in
/// flight, the broker re-execs it; one that can't take over is noted in the
/// lead's mailbox once, and the broker serves on.
pub fn serve<L: Ledger + Clone + Send>(
    host: &Host,
    ledger: &L,
    paths: &Paths,
    lead: &str,
    poll: Duration,
    inherited: Option<std::fs::File>,
) -> Result<Served> {
    validate_name(lead)?;
    // Before the lock: a client writes as soon as it sees the lock held.
    let _ = Dir::open(paths.state_dir(), &["broker", lead, "requests"]);
    let lock = match inherited.and_then(|file| adopt_lock(paths, lead, file)) {
        Some(lock) => lock,
        None => match take_lock(paths, lead)? {
            Some(lock) => lock,
            None => return Ok(Served::AlreadyRunning),
        },
    };
    // Its lead sizes waits on origin by the broker's git timeout, not its own, and trusts
    // a missing start mark only from a broker that writes them. A plain write is safe:
    // the lead's sandbox can't write config/broker to plant a link.
    let remote =
        crate::worktree::remote_timeout().map_or(String::new(), |r| r.as_secs().to_string());
    let running = Binary::running();
    let mut installs = host.follow.as_ref().map(|follow| Installs {
        follow,
        running: follow
            .moved_onto
            .or_else(|| running.as_ref().map(|b| b.file)),
        failed: None,
        looked: Instant::now(),
    });
    let binary = running.map_or(String::new(), |b| {
        format!(
            "{VERSION}{}\n{BINARY}{}\n{FILE}{} {}\n",
            b.version,
            b.path.display(),
            b.file.dev,
            b.file.ino
        )
    });
    let mut record = LockRecord {
        path: lock_file(paths, lead),
        text: format!("{remote}\n{MARKS}\n{binary}"),
    };
    record.write();
    write_replacing(&pid_file(paths, lead), &std::process::id().to_string())?;
    let _pid = RemovedOnExit(pid_file(paths, lead));
    let herd = sandbox::Lead::kept(paths, lead)?
        .ok_or_else(|| anyhow!("{lead} has no launch record, so its broker has no herd"))?
        .herd;
    let broker = Broker {
        lead,
        herd: &herd,
        host,
        paths,
    };
    let replies = replies_dir(paths, lead);
    std::fs::create_dir_all(&replies).with_context(|| format!("create {}", replies.display()))?;
    let broker_stopping = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (done_tx, done) = mpsc::channel::<String>();
        let mut senders = vec![];
        for _ in [Lane::Quick, Lane::Herd, Lane::Status, Lane::Gate] {
            let (tx, jobs) = mpsc::channel::<Job>();
            let (mut ledger, done_tx) = (ledger.clone(), done_tx.clone());
            let (broker, replies) = (&broker, &replies);
            let stopping = &broker_stopping;
            scope.spawn(move || {
                for job in jobs {
                    broker.take_up(&mut ledger, &job, replies, stopping);
                    let _ = done_tx.send(job.name);
                }
            });
            senders.push(tx);
        }
        let mut in_flight: HashMap<String, Lane> = HashMap::new();
        loop {
            let mut answered = false;
            for name in done.try_iter() {
                in_flight.remove(&name);
                answered = true;
            }
            // Read every round: `lead` may relaunch a retired lead under its name.
            let stopping = !unretired(ledger, lead);
            if stopping && in_flight.is_empty() {
                // Cleared before the last read, so a relaunch that saw the record finds this broker serving.
                record.clear();
                if unretired(ledger, lead) {
                    record.write();
                    continue;
                }
                if let Ok(requests) = Dir::open(paths.state_dir(), &["broker", lead, "requests"]) {
                    let requests = Arc::new(requests);
                    // A request a relaunch let through is left for the relaunch's broker.
                    let retired = || retirement(ledger, lead).unwrap_or(true);
                    for name in requests.names() {
                        broker.job(&requests, &name, &replies, &retired);
                    }
                }
                return Ok(Served::Retired);
            }
            // Not kept: a restart that gives up withdraws its stop, and serving resumes.
            let asked = asked_to_stop(paths, lead);
            broker_stopping.store(asked, Ordering::SeqCst);
            if asked {
                if in_flight.is_empty() {
                    return Ok(Served::Stopped);
                }
                std::thread::sleep(poll);
                continue;
            }
            // Only between requests, so a move never holds up a lane behind a long feed.
            if let Some(installs) = installs.as_mut()
                && in_flight.is_empty()
                && (answered || installs.due())
                && let Some(failed) = installs.move_onto_newer(paths, lead, &lock)
            {
                record.failed(failed);
            }
            // Reopened each round: the lead can replace any folder on the way.
            if let Ok(requests) = Dir::open(paths.state_dir(), &["broker", lead, "requests"]) {
                let requests = Arc::new(requests);
                // Read per request, after listing: a retire or relaunch just before it was written decides it.
                // A failed reread keeps the round's read rather than undo a retirement it saw.
                let retired = || retirement(ledger, lead).unwrap_or(stopping);
                for name in requests.names() {
                    if in_flight.contains_key(&name) {
                        continue;
                    }
                    let Some(job) = broker.job(&requests, &name, &replies, &retired) else {
                        continue;
                    };
                    let lane = job.op.lane();
                    if in_flight.values().filter(|&&l| l == lane).count() > LANE_WAITING {
                        write_reply(&replies, &job.id, &Reply::Refused(lane.busy()));
                        requests.remove(&name);
                        continue;
                    }
                    in_flight.insert(name, lane);
                    let _ = senders[lane as usize].send(job);
                }
            }
            prune(&replies, &in_flight);
            std::thread::sleep(poll);
        }
    })
}

/// What a broker following installs knows of the loopherd on its PATH.
struct Installs<'a> {
    follow: &'a Follow<'a>,
    running: Option<FileId>,
    /// An install that couldn't take over, not tried again.
    failed: Option<FileId>,
    looked: Instant,
}

impl Installs<'_> {
    fn due(&self) -> bool {
        self.looked.elapsed() >= self.follow.every
    }

    /// Re-execs the loopherd on PATH when it is another file than this
    /// broker runs and not one that already failed to take over. Returns
    /// only when it didn't move, noting the lead's mailbox and returning the
    /// install if it tried.
    fn move_onto_newer(&mut self, paths: &Paths, lead: &str, lock: &Lock) -> Option<FileId> {
        self.looked = Instant::now();
        let exe = (self.follow.installed)()?;
        let file = FileId::of(&exe)?;
        if Some(file) == self.running || Some(file) == self.failed {
            return None;
        }
        let e = (self.follow.reexec)(&exe, lead, lock);
        self.failed = Some(file);
        let text = format!(
            "{lead}'s broker couldn't move onto the loopherd installed at {}, so it serves on \
             its old build until that changes again: {e:#}; the shepherd runs \
             loopherd broker-restart {lead}",
            exe.display()
        );
        let note = crate::report::Entry::new(SystemTime::now(), Body::Note { text });
        let _ = crate::report::append(&paths.state_dir().join("reports"), lead, &note);
        Some(file)
    }
}

/// An unreadable ledger is no proof of retirement.
fn unretired(ledger: &dyn Ledger, lead: &str) -> bool {
    !retirement(ledger, lead).unwrap_or(false)
}

/// Whether `lead` reads retired, as it does once gone from the ledger, or
/// `None` when the ledger can't be read.
fn retirement(ledger: &dyn Ledger, lead: &str) -> Option<bool> {
    let entries = ledger.entries().ok()?;
    Some(entries.get(lead).is_none_or(|r| r.retired))
}

/// How many requests a lane holds waiting behind the one it runs.
const LANE_WAITING: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Quick = 0,
    Herd = 1,
    Status = 2,
    Gate = 3,
}

impl Lane {
    fn busy(self) -> String {
        let queue = match self {
            Lane::Quick => "quick requests",
            Lane::Herd => "herd changes",
            Lane::Status => "status",
            Lane::Gate => "gates",
        };
        format!(
            "its queue for {queue} already holds {LANE_WAITING} waiting requests; ask again later"
        )
    }
}

/// A request read and waiting in its lane.
struct Job {
    requests: Arc<Dir>,
    name: String,
    id: String,
    op: Op,
}

/// The environment `lead`'s broker, and its unsandboxed watch, take on
/// before running anything. The `PATH` its launch record keeps, so nothing
/// the lead sets picks the gh, git or checks they run. And the lead's
/// narrowed credentials, as in its sandbox, so their git and gh reach origin
/// as it: none unless the herd its launch record names guards merges by
/// credentials (D95). Errs, naming the fix, while they're missing.
pub fn lead_env(paths: &Paths, lead: &str) -> Result<Vec<(String, String)>> {
    use crate::herd::MergeGuard;
    validate_name(lead)?;
    let Some(kept) = sandbox::Lead::kept(paths, lead)? else {
        return Ok(vec![]);
    };
    let mut env: Vec<(String, String)> =
        kept.path.map(|p| ("PATH".into(), p)).into_iter().collect();
    let herd = &kept.herd;
    let guard = crate::herd::merge_guard(paths, herd)
        .with_context(|| format!("whether herd {herd} guards merges by credentials is unknown"))?;
    if guard != Some(MergeGuard::Credentials) {
        return Ok(env);
    }
    let credentials = crate::credentials::lead_env(paths, herd)
        .with_context(|| format!("herd {herd} guards merges by credentials"))?;
    env.extend(
        credentials
            .into_iter()
            .map(|(name, value)| (name, value.as_str().unwrap_or_default().to_string())),
    );
    // Outside the sandbox the human's keys are readable: a core.sshCommand -i or
    // a `Host *` IdentityFile would offer them, so ssh reads loopherd's config alone.
    if let Some(missing) = crate::credentials::host_block_missing(paths, herd) {
        bail!("herd {herd} guards merges by credentials: {missing}");
    }
    let ssh_config = crate::credentials::ssh_config(paths);
    // Setting it turns off loopherd's own BatchMode, so a prompt would hang until the git timeout.
    env.push((
        "GIT_SSH_COMMAND".into(),
        format!(
            "ssh -F {} -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes",
            crate::shell::quote(&ssh_config.to_string_lossy())
        ),
    ));
    Ok(env)
}

/// Removes its file when dropped, so a broker's pid outlives it only if it is killed.
struct RemovedOnExit(PathBuf);

impl Drop for RemovedOnExit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What a serving broker keeps in its lock. Cleared on the way out, so its
/// start marks mislead no older broker after it, and a cleared record on a
/// held lock reads as a broker exiting.
struct LockRecord {
    path: PathBuf,
    text: String,
}

impl LockRecord {
    fn write(&self) {
        let _ = std::fs::write(&self.path, &self.text);
    }

    fn clear(&self) {
        let _ = std::fs::write(&self.path, "");
    }

    /// Records `file` as the install this broker couldn't move onto, for doctor.
    fn failed(&mut self, file: FileId) {
        let mut text: String = self
            .text
            .lines()
            .filter(|line| !line.starts_with(FAILED))
            .map(|line| format!("{line}\n"))
            .collect();
        text.push_str(&format!("{FAILED}{} {}\n", file.dev, file.ino));
        self.text = text;
        self.write();
    }
}

impl Drop for LockRecord {
    fn drop(&mut self) {
        self.clear();
    }
}

struct Broker<'a> {
    lead: &'a str,
    herd: &'a str,
    host: &'a Host<'a>,
    paths: &'a Paths,
}

impl Broker<'_> {
    /// Request `name` as a job for its lane, or `None` once it is answered
    /// here: refused unread, or because the lead is retired. A request whose
    /// name can't name a reply is only removed.
    fn job(
        &self,
        requests: &Arc<Dir>,
        name: &str,
        replies: &Path,
        retired: &dyn Fn() -> bool,
    ) -> Option<Job> {
        let Some(id) = name.strip_suffix(".json").filter(|id| is_id(id)) else {
            requests.remove(name);
            return None;
        };
        let reply = match requests.read_request(name) {
            Ok(_) if retired() => Reply::Refused(self.retired()),
            Ok(op) => {
                return Some(Job {
                    requests: requests.clone(),
                    name: name.into(),
                    id: id.into(),
                    op,
                });
            }
            Err(e) => Reply::Refused(format!("{e:#}")),
        };
        write_reply(replies, id, &reply);
        requests.remove(name);
        None
    }

    fn retired(&self) -> String {
        format!(
            "{} is retired, so its broker takes no new requests",
            self.lead
        )
    }

    /// Carries `job` out and answers it, marking it started first so a lead
    /// that gives up can tell; a request withdrawn before then never runs.
    fn take_up(&self, ledger: &mut dyn Ledger, job: &Job, replies: &Path, stopping: &AtomicBool) {
        let Job {
            requests,
            name,
            id,
            op,
        } = job;
        // Read now, not the round's: a request written after a retirement is always refused.
        if !unretired(ledger, self.lead) {
            write_reply(replies, id, &Reply::Refused(self.retired()));
            requests.remove(name);
            return;
        }
        // Left in the queue for the broker a restart starts next.
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let started = replies.join(format!("{id}.started"));
        if let Err(e) = std::fs::write(&started, "") {
            let why = format!(
                "the broker couldn't mark {} started, so it didn't run it: {e}",
                op.command()
            );
            write_reply(replies, id, &Reply::Failed(why));
            requests.remove(name);
            return;
        }
        // The lead withdraws, then looks for the mark, so marking first leaves no gap.
        if !requests.has(name) {
            let _ = std::fs::remove_file(&started);
            return;
        }
        let reply =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reply(ledger, op)))
                .unwrap_or_else(|_| Reply::Failed("the broker panicked on this request".into()));
        write_reply(replies, id, &reply);
        requests.remove(name);
    }

    fn reply(&self, ledger: &mut dyn Ledger, op: &Op) -> Reply {
        match self.refusal(ledger, op) {
            Ok(Some(why)) => return Reply::Refused(why),
            Ok(None) => {}
            Err(e) => return Reply::Failed(format!("{e:#}")),
        }
        let caller = match self.caller(ledger) {
            Ok(caller) => caller,
            Err(e) => return Reply::Failed(format!("{e:#}")),
        };
        let mut ctx = Ctx {
            herdr: self.host.herdr,
            ledger,
            paths: self.paths,
            procs: self.host.procs,
            claude_dir: self.host.claude_dir.clone(),
            caller,
            live: self.host.live,
            spawn_waiter: self.host.spawn_waiter,
            prs: self.host.prs,
        };
        let mut kept = Kept::default();
        let carried = op.carry_out(&mut ctx, &mut kept);
        if let Err(e) = &carried
            && kept.out.is_empty()
            && kept.err.is_empty()
        {
            return Reply::Failed(format!("{e:#}"));
        }
        // What it printed before failing is part of the answer, as it is run directly.
        let code = carried.unwrap_or_else(|e| {
            kept.err(&format!("loopherd {}: {e:#}", op.command()));
            1
        });
        Reply::Done {
            out: kept.out.join("\n"),
            err: kept.err.join("\n"),
            code,
        }
    }

    /// The lead as an operation's caller: in its own pane, with the herd its
    /// launch record names.
    fn caller(&self, ledger: &dyn Ledger) -> Result<Caller> {
        let lead = self.lead;
        let record = ledger
            .entries()?
            .remove(lead)
            .ok_or_else(|| anyhow!("{lead} is not in the ledger"))?;
        Ok(Caller {
            pane: Some(record.pane_id),
            socket: record.socket,
            lineage: vec![],
            workspace: Some(record.workspace),
            process: self.host.process,
            lead: Some(ForLead {
                name: lead.into(),
                herd: self.herd.into(),
            }),
        })
    }

    /// Why `op` is outside the lead's reach: its own herd's unretired
    /// workers, and the templates of its herd's file.
    fn refusal(&self, ledger: &dyn Ledger, op: &Op) -> Result<Option<String>> {
        let (lead, herd) = (self.lead, self.herd);
        let target = match op.reach() {
            Reach::Herd(other) if other != herd => {
                return Ok(Some(format!(
                    "herd {other} isn't {lead}'s; {lead} reaches only herd {herd}"
                )));
            }
            Reach::Herd(_) => return self.start_refusal(op),
            Reach::Lead if matches!(op, Op::Status { strays: true, .. }) => {
                return Ok(Some(format!(
                    "strays are herdr's agents outside every herd; {lead} sees only herd \
                     {herd}, so run loopherd status without --strays"
                )));
            }
            Reach::Lead => return Ok(None),
            Reach::Member(target) | Reach::AnyMember(target) => target,
        };
        let retired_too = matches!(op.reach(), Reach::AnyMember(_));
        if let Op::Restart { force: true, .. } = op {
            return Ok(Some(
                "restart --force skips the checks that a member is finished since its handoff, \
                 so the human forces a restart; restart it once its stop reads finished"
                    .into(),
            ));
        }
        if let Op::Retire { force: true, .. } = op {
            return Ok(Some(
                "retire --force skips the check that a member is finished, so the human forces \
                 a retire; retire it once its stop reads finished"
                    .into(),
            ));
        }
        if let Op::Release {
            drop_branch: true, ..
        } = op
        {
            return Ok(Some(
                "release --drop-branch deletes commits not on the base, so the human drops a \
                 branch; release it plain or with --keep-branch"
                    .into(),
            ));
        }
        let reach = format!("{lead} reaches only herd {herd}'s workers");
        if target == SHEPHERD {
            return Ok(Some(format!("{target} is the shepherd; {reach}")));
        }
        if target == lead {
            let hint = match op {
                Op::Restart { .. } => "; it restarts itself with loopherd restart --self",
                Op::Retire { .. } => "; retiring a lead is the shepherd's or the human's job",
                _ => "",
            };
            return Ok(Some(format!(
                "{target} is the lead itself; {} reaches only herd {herd}'s workers{hint}",
                op.command()
            )));
        }
        let entries = ledger.entries()?;
        let Some(record) = entries.get(target).filter(|r| retired_too || !r.retired) else {
            let which = if retired_too { "" } else { "unretired " };
            return Ok(Some(format!("no {which}member is named {target}; {reach}")));
        };
        Ok(match record.herd.as_deref() {
            None => Some(format!("{target} is in no herd; {reach}")),
            Some(other) if other != herd => Some(format!("{target} is in herd {other}; {reach}")),
            Some(_) if record.is_lead() => Some(format!("{target} is a lead; {reach}")),
            Some(_) if record.attended.is_some() && !matches!(op, Op::Read { .. }) => {
                let what_remains = if record.retired {
                    "it is retired, so its release is the shepherd's or the human's"
                } else {
                    "once merged, its done check reaps it, or the shepherd or the human retires it"
                };
                Some(format!(
                    "{target} is attended: the human works in its pane, so {lead} leaves it to them \
                 for its whole life; {what_remains} (D112)"
                ))
            }
            Some(_) if record.may_be_lead() => Some(format!(
                "{target}'s role is from another loopherd version; {reach}"
            )),
            Some(_) => None,
        })
    }

    /// Why `op` would start a worker other than as the herd file says.
    fn start_refusal(&self, op: &Op) -> Result<Option<String>> {
        if let Op::Feed {
            ready_timeout_secs, ..
        }
        | Op::Add {
            ready_timeout_secs, ..
        } = op
            && *ready_timeout_secs > MAX_READY_TIMEOUT_SECS
        {
            return Ok(Some(format!(
                "a ready timeout of {ready_timeout_secs}s is over the {MAX_READY_TIMEOUT_SECS}s a lead may ask for"
            )));
        }
        let Op::Add {
            template,
            claude_args,
            ..
        } = op
        else {
            return Ok(None);
        };
        let herd = self.herd;
        if !claude_args.is_empty() {
            return Ok(Some(format!(
                "a worker's claude args come only from herd {herd}'s file; {} adds without its own",
                self.lead
            )));
        }
        let lead = self.lead;
        Ok(
            match crate::herd::template_attended(self.paths, herd, template).context("herd")? {
                None => Some(format!(
                    "herd {herd} has no template {template}; {lead} adds only its herd's templates"
                )),
                Some(true) => Some(format!(
                    "template {template} is attended, so only the shepherd or the human adds it (D112)"
                )),
                Some(false) => None,
            },
        )
    }
}

/// A folder opened one part at a time without following symlinks, so a
/// lead that can write the parts can't point the broker anywhere else.
struct Dir(OwnedFd);

impl Dir {
    /// Opens `root`/`parts`…, creating missing parts; `root` is trusted.
    fn open(root: &Path, parts: &[&str]) -> std::io::Result<Dir> {
        let mut dir = Dir(std::fs::File::open(root)?.into());
        for part in parts {
            let name = CString::new(*part)?;
            // SAFETY: both calls take a descriptor `dir` keeps open and a C string.
            let fd = unsafe {
                libc::mkdirat(dir.0.as_raw_fd(), name.as_ptr(), 0o700);
                libc::openat(
                    dir.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: openat just returned this descriptor, owned by nothing else.
            dir = Dir(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        Ok(dir)
    }

    /// Entries ready to answer, oldest name first; dotfiles are still being written.
    fn names(&self) -> Vec<String> {
        let mut names = vec![];
        // SAFETY: fdopendir takes ownership of the dup, which closedir closes;
        // each entry is read before the next readdir.
        unsafe {
            let fd = libc::fcntl(self.0.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
            if fd < 0 {
                return names;
            }
            let dir = libc::fdopendir(fd);
            if dir.is_null() {
                libc::close(fd);
                return names;
            }
            // The dup shares the descriptor's offset, which an earlier listing moved.
            libc::rewinddir(dir);
            loop {
                let entry = libc::readdir(dir);
                if entry.is_null() {
                    break;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr());
                if let Ok(name) = name.to_str()
                    && !name.starts_with('.')
                {
                    names.push(name.to_string());
                }
            }
            libc::closedir(dir);
        }
        names.sort();
        names
    }

    /// Request `name`, refusing anything but a small regular file of one known op.
    fn read_request(&self, name: &str) -> Result<Op> {
        let c_name = CString::new(name)?;
        // SAFETY: openat on a descriptor `self` keeps open and a C string.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            bail!("not a request: not a plain file");
        }
        // SAFETY: openat just returned this descriptor, owned by nothing else.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let meta = file.metadata().context("not a request")?;
        // A hard link would hand the broker a file the lead can't read itself.
        if !meta.is_file() || meta.nlink() > 1 {
            bail!("not a request: not a plain file");
        }
        if meta.len() > MAX_REQUEST {
            bail!("not a request: over {MAX_REQUEST} bytes");
        }
        let mut text = vec![];
        file.take(MAX_REQUEST + 1).read_to_end(&mut text)?;
        let request: Request = serde_json::from_slice(&text).context(MALFORMED)?;
        Ok(request.op)
    }

    /// Whether entry `name` is still there, symlink or not.
    fn has(&self, name: &str) -> bool {
        let Ok(c_name) = CString::new(name) else {
            return false;
        };
        // SAFETY: fstatat on a descriptor `self` keeps open, a C string and a zeroed stat.
        unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            libc::fstatat(
                self.0.as_raw_fd(),
                c_name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
        }
    }

    /// Removes entry `name` itself, never what a symlink names; a folder only when empty.
    fn remove(&self, name: &str) {
        let Ok(c_name) = CString::new(name) else {
            return;
        };
        // SAFETY: unlinkat on a descriptor `self` keeps open and a C string.
        unsafe {
            if libc::unlinkat(self.0.as_raw_fd(), c_name.as_ptr(), 0) != 0 {
                libc::unlinkat(self.0.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR);
            }
        }
    }
}

const MALFORMED: &str = "malformed request";

/// A broker older than its lead's loopherd refused a request it can't read.
#[derive(Debug)]
pub struct TooOld(String);

impl std::fmt::Display for TooOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TooOld {}

/// The lead's side of the transport: writes requests, reads replies.
pub struct Client {
    lead: String,
    requests: PathBuf,
    replies: PathBuf,
    lock: PathBuf,
    stop: PathBuf,
    restart: PathBuf,
    /// Overrides the op's own wait.
    answer_within: Option<Duration>,
}

impl Client {
    /// The lead this client hands operations to.
    pub fn lead(&self) -> &str {
        &self.lead
    }

    pub fn for_lead(paths: &Paths, lead: &str) -> Client {
        Client {
            lead: lead.into(),
            requests: requests_dir(paths, lead),
            replies: replies_dir(paths, lead),
            lock: lock_file(paths, lead),
            stop: stop_file(paths, lead),
            restart: restart_file(paths, lead),
            answer_within: None,
        }
    }

    /// Hands `op` to the broker and waits for its answer. Fails at once when
    /// no broker runs, and as soon as one stops without answering, unless a
    /// restart is replacing it, then withdraws the request so no later
    /// broker carries it out.
    pub fn call(&self, op: &Op) -> Result<(String, String, i32)> {
        let lead = &self.lead;
        if !held(&self.lock) && live_stop(&self.stop, &self.restart).is_none() {
            bail!(
                "no broker is running for {lead}, so it can't {}: \
                 the shepherd's loopherd revive --name {lead} starts it",
                op.command()
            );
        }
        let id = new_id();
        write_atomically(
            &self.requests,
            &id,
            &serde_json::to_vec(&Request { op: op.clone() })?,
        )
        .with_context(|| format!("hand {} to {lead}'s broker", op.command()))?;
        let answer = self.answer(&id, op);
        if answer.is_err() {
            let _ = std::fs::remove_file(self.requests.join(format!("{id}.json")));
        }
        answer
    }

    fn answer(&self, id: &str, op: &Op) -> Result<(String, String, i32)> {
        let lead = &self.lead;
        let reply = self.replies.join(format!("{id}.json"));
        let answer_within = self
            .answer_within
            .unwrap_or_else(|| op.answer_within(broker_remote_timeout(&self.lock)));
        let given_up = Instant::now() + answer_within;
        loop {
            if let Ok(text) = std::fs::read(&reply) {
                let reply = serde_json::from_slice(&text).with_context(|| {
                    format!(
                        "{lead}'s broker answered {} in a form this loopherd can't read, so it may \
                         run an older loopherd; the shepherd can restart it with loopherd \
                         broker-restart {lead}",
                        op.command()
                    )
                })?;
                return match reply {
                    Reply::Done { out, err, code } => Ok((out, err, code)),
                    // This client wrote a well-formed request, so only an older broker can't read it.
                    Reply::Refused(why) if why.starts_with(MALFORMED) => Err(TooOld(format!(
                        "{lead}'s broker predates this loopherd, so it can't {}; the shepherd must \
                         restart it with loopherd broker-restart {lead}",
                        op.command()
                    ))
                    .into()),
                    Reply::Refused(why) => {
                        Err(anyhow!("{lead}'s broker refused {}: {why}", op.command()))
                    }
                    Reply::Failed(why) => Err(anyhow!("{why}")),
                };
            }
            // Read first: a restart removes its stop only once the new broker holds the lock.
            let restarting = live_stop(&self.stop, &self.restart).is_some();
            if !restarting && !held(&self.lock) {
                bail!(
                    "{lead}'s broker stopped before answering {}: \
                     the shepherd's loopherd revive --name {lead} starts it again",
                    op.command()
                );
            }
            if Instant::now() >= given_up {
                // Withdrawn before looking for the mark, so a broker that hasn't marked it won't run it.
                let _ = std::fs::remove_file(self.requests.join(format!("{id}.json")));
                if reply.exists() {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let waited = humantime::format_duration(answer_within);
                let started = self.replies.join(format!("{id}.started")).exists();
                if started || !marks_starts(&self.lock) {
                    let check = match op {
                        Op::Report { .. } => {
                            format!("check loopherd reports --all {lead} before reporting again")
                        }
                        _ => "check before asking again".into(),
                    };
                    bail!(
                        "{lead}'s broker didn't answer {} within {waited}; it may still be carrying it out, \
                         so {check}, and check the broker with loopherd doctor",
                        op.command(),
                    );
                }
                bail!(
                    "{lead}'s broker didn't start {} within {waited}, so it was not started and is \
                     withdrawn: the broker is busy with an earlier request; ask again later",
                    op.command(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    op: Op,
}

/// The broker of the lead running this loopherd, process `me`: the lead
/// whose sandboxed Claude is an ancestor, else the unretired lead the ledger
/// has in `pane`. Asks herdr nothing; a ledger it can't read names no lead.
/// A lead from another config dir falls back to the pane alone. `shepherd`
/// skips that fallback: the shepherd's pane may carry a dead lead's old id
/// after a herdr restart, but an ancestor can't be forged.
#[cfg(test)]
fn route(
    paths: &Paths,
    ledger: &dyn Ledger,
    procs: &dyn Procs,
    me: u32,
    pane: Option<&tool_hook::Pane>,
    shepherd: impl FnOnce() -> bool,
) -> Option<Client> {
    let found = find_lead(paths, ledger, procs, me, pane, shepherd)?;
    Some(Client::for_lead(paths, found.lead()))
}

/// How [`find_lead`] placed a lead here.
pub enum Found {
    /// A lead's Claude is an ancestor.
    Lineage(String),
    /// The ledger has the lead's unretired record in this pane, its Claude
    /// recorded as `process`.
    Pane {
        lead: String,
        process: Option<Process>,
    },
}

impl Found {
    pub fn lead(&self) -> &str {
        match self {
            Found::Lineage(lead) => lead,
            Found::Pane { lead, .. } => lead,
        }
    }
}

/// The lead whose Claude is an ancestor of `me`, else, unless `shepherd`
/// says this is the shepherd's pane, the unretired lead the ledger has in `pane`.
pub fn find_lead(
    paths: &Paths,
    ledger: &dyn Ledger,
    procs: &dyn Procs,
    me: u32,
    pane: Option<&tool_hook::Pane>,
    shepherd: impl FnOnce() -> bool,
) -> Option<Found> {
    if let Some(lead) = sandbox::calling_lead(paths, procs, me).ok().flatten() {
        return Some(Found::Lineage(lead));
    }
    if shepherd() {
        return None;
    }
    let record = tool_hook::lead_in(ledger, pane?).ok()??;
    (!record.retired).then_some(Found::Pane {
        lead: record.name,
        process: record.process,
    })
}

/// Starts lead `lead`'s broker unless one runs, saying whether it started one.
pub type StartBroker = fn(&Paths, &str) -> Result<bool>;

/// Restarts a lead's broker, or starts one when none runs, as [`restart_broker`] does.
pub type RestartBroker = fn(&Paths, &str) -> Result<()>;

/// How long `start` waits for the broker it started to take its lock.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts `loopherd broker <lead>` detached, in a session of its own, unless
/// a broker already serves `lead`, and waits until it serves. Fails, naming
/// the holder, when the lock stays held with nothing recorded in it.
pub fn start(paths: &Paths, lead: &str) -> Result<bool> {
    let exe = runnable_exe().with_context(|| {
        format!("{lead}'s broker can't start; run loopherd revive --name {lead} to try again")
    })?;
    start_with(paths, lead, START_TIMEOUT, &mut || {
        spawn(paths, lead, &exe).map(|_| ())
    })
}

/// This binary, or `loopherd` on PATH once a reinstall has moved a new one over it.
pub fn runnable_exe() -> Result<PathBuf> {
    Ok(runnable(
        std::env::current_exe().context("find the loopherd binary")?,
    ))
}

fn runnable(exe: PathBuf) -> PathBuf {
    match exe.to_string_lossy().ends_with(DELETED) {
        true => PathBuf::from("loopherd"),
        false => exe,
    }
}

/// What Linux appends to `current_exe` once a reinstall renamed over the running file.
const DELETED: &str = " (deleted)";

/// Names the install a broker re-execed, as `<dev> <ino> <path>`, which the
/// new image can't learn from its own once another install lands while it starts.
const STARTED_FROM_ENV: &str = "LOOPHERD_BROKER_STARTED_FROM";

/// Where a broker was started from.
pub struct Origin {
    /// The path it was started at, as it was before any reinstall.
    pub path: PathBuf,
    /// The install a moving broker re-execed, which may be a script that execs another.
    pub moved_onto: Option<FileId>,
}

/// Where this broker was started from: the install a moving broker
/// re-execed, else this binary. Call before any thread starts.
pub fn origin() -> Option<Origin> {
    let passed = std::env::var_os(STARTED_FROM_ENV);
    // SAFETY: no thread runs yet to read the environment.
    unsafe { std::env::remove_var(STARTED_FROM_ENV) };
    let moved = passed.filter(|_| std::env::var_os(LOCK_FD_ENV).is_some());
    if let Some(moved) = moved.as_deref().and_then(moved_from) {
        return Some(moved);
    }
    let exe = std::env::current_exe().ok()?;
    let path = match exe.to_string_lossy().strip_suffix(DELETED) {
        Some(path) => path.into(),
        None => exe,
    };
    Some(Origin {
        path,
        moved_onto: None,
    })
}

fn moved_from(passed: &std::ffi::OsStr) -> Option<Origin> {
    use std::os::unix::ffi::OsStrExt;
    let mut fields = passed.as_bytes().splitn(3, |&b| b == b' ');
    let mut number = || std::str::from_utf8(fields.next()?).ok()?.parse().ok();
    let (dev, ino) = (number()?, number()?);
    Some(Origin {
        path: std::ffi::OsStr::from_bytes(fields.next()?).into(),
        moved_onto: Some(FileId { dev, ino }),
    })
}

fn start_with(
    paths: &Paths,
    lead: &str,
    wait: Duration,
    spawn: &mut dyn FnMut() -> Result<()>,
) -> Result<bool> {
    validate_name(lead)?;
    let lock = lock_file(paths, lead);
    // A held lock with no record is a broker exiting (or one just starting, or one that records nothing).
    let given_up = Instant::now() + wait;
    while held(&lock) {
        if std::fs::metadata(&lock).is_ok_and(|m| m.len() > 0) {
            return Ok(false);
        }
        if Instant::now() >= given_up {
            let (holder, fix) = match serving_pid(paths, lead) {
                Some(pid) => (
                    format!("pid {pid}"),
                    format!("run loopherd broker-restart {lead} to replace it"),
                ),
                None => (
                    "a process that recorded no pid".into(),
                    format!(
                        "stop it yourself (pgrep -f 'loopherd broker {lead}' finds it), then run \
                         loopherd broker-restart {lead}; with none to find, the lock is held by \
                         a lead's loopherd, so try again once that command ends"
                    ),
                ),
            };
            bail!(
                "{} is held by {holder}, which records nothing in it, as a broker older than \
                 this loopherd does, and didn't let it go within {}; {fix}",
                lock.display(),
                humantime::format_duration(wait)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    spawn()?;
    Ok(true)
}

/// The broker serving once a start returns.
#[derive(Debug, Clone, Copy)]
enum Started {
    Own(u32),
    /// Another broker took the lock first, as a `revive` or `lead` may in a restart's gap.
    Another(u32),
}

/// Starts `loopherd broker <lead>` detached and waits until it, or another, serves.
fn spawn(paths: &Paths, lead: &str, exe: &Path) -> Result<Started> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(exe);
    command
        .args(["broker", lead])
        .env_remove("HERDR_PANE_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe. A session of its own outlives the
    // shell that started it.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let child = command.spawn().with_context(|| {
        format!(
            "start {} broker {lead}; run loopherd broker-restart {lead} to try again",
            exe.display()
        )
    })?;
    serving_once_started(paths, lead, child)
}

/// Waits until `child`, started as `lead`'s broker, or another broker serves.
fn serving_once_started(
    paths: &Paths,
    lead: &str,
    mut child: std::process::Child,
) -> Result<Started> {
    let given_up = Instant::now() + START_TIMEOUT;
    loop {
        let serving = serving_pid(paths, lead).filter(|_| running(paths, lead));
        let exited = child.try_wait()?;
        match (serving, exited) {
            (Some(pid), _) if pid == child.id() => return Ok(Started::Own(pid)),
            // Another broker serves, as when this one lost the lock to it.
            (Some(pid), Some(_)) => return Ok(Started::Another(pid)),
            (None, Some(status)) if !running(paths, lead) => bail!(
                "it exited at once ({status}); run loopherd broker {lead} to see why, \
                 then loopherd broker-restart {lead} to start it"
            ),
            _ => {}
        }
        if Instant::now() >= given_up {
            bail!(
                "it didn't take {} within {}, which a lead's loopherd may hold; run loopherd \
                 broker-restart {lead} to try again",
                lock_file(paths, lead).display(),
                humantime::format_duration(START_TIMEOUT)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Names the descriptor of the lock a broker hands the loopherd it re-execs.
pub const LOCK_FD_ENV: &str = "LOOPHERD_BROKER_LOCK_FD";

/// How long a new install has to pass its startup check before a broker moves onto it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Replaces this process with `exe` serving `lead`, handing it `lock` so no
/// client sees the lead without a broker. First runs `exe broker <lead>
/// --check`, so a binary that is missing, can't run, or can't serve `lead`'s
/// config leaves this one serving. Returns only when `exe` couldn't take over.
pub fn reexec(exe: &Path, lead: &str, lock: &Lock) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let probed = FileId::of(exe);
    if let Err(e) = probe(exe, lead) {
        return e;
    }
    let Some(file) = probed.filter(|&file| FileId::of(exe) == Some(file)) else {
        return anyhow!("{} changed during its startup check", exe.display());
    };
    let fd = lock.as_raw_fd();
    // SAFETY: fcntl only changes close-on-exec on a descriptor `lock` keeps open.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return anyhow!(std::io::Error::last_os_error()).context("hand over the broker's lock");
    }
    let mut command = std::process::Command::new(exe);
    command
        .args(["broker", lead])
        .env(LOCK_FD_ENV, fd.to_string());
    let mut origin = std::ffi::OsString::from(format!("{} {} ", file.dev, file.ino));
    origin.push(exe);
    let e = command.env(STARTED_FROM_ENV, origin).exec();
    // SAFETY: as above; children started later must not hold the lock.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    // A failed exec leaves SIGPIPE at its default, which Rust's runtime had ignored.
    // SAFETY: setting a signal's disposition to ignore has no preconditions.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    anyhow!(e).context(format!("exec {}", exe.display()))
}

fn probe(exe: &Path, lead: &str) -> Result<()> {
    let mut child = std::process::Command::new(exe)
        .args(["broker", lead, "--check"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", exe.display()))?;
    // Read aside: a child leaving its stderr to a process of its own must not block the broker.
    let (said_tx, said_rx) = mpsc::channel();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut said = String::new();
            let _ = stderr.take(4096).read_to_string(&mut said);
            let _ = said_tx.send(said);
        });
    }
    let given_up = Instant::now() + PROBE_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let said = said_rx
                .recv_timeout(Duration::from_millis(500))
                .unwrap_or_default();
            let said = match said.trim() {
                "" => String::new(),
                said => format!(": {said}"),
            };
            bail!(
                "{} failed its startup check ({status}){said}",
                exe.display()
            );
        }
        if Instant::now() >= given_up {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{}'s startup check didn't finish within {}",
                exe.display(),
                humantime::format_duration(PROBE_TIMEOUT)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The lock file the broker that re-execed this process handed it, named
/// by [`LOCK_FD_ENV`], which this removes so nothing it starts sees it.
/// Call before any thread starts.
pub fn inherited_lock() -> Option<std::fs::File> {
    let fd: i32 = std::env::var(LOCK_FD_ENV).ok()?.parse().ok()?;
    // SAFETY: no thread runs yet to read the environment.
    unsafe { std::env::remove_var(LOCK_FD_ENV) };
    // SAFETY: F_GETFD only reads the flags, failing on a descriptor that isn't open.
    if fd <= 2 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return None;
    }
    // SAFETY: open and handed over for this process to own; set close-on-exec again at once.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Some(file)
}

/// `file` as `lead`'s lock, when it is that lock file and holds its flock.
fn adopt_lock(paths: &Paths, lead: &str, file: std::fs::File) -> Option<Lock> {
    let held = file.metadata().ok()?;
    let lock = std::fs::metadata(lock_file(paths, lead)).ok()?;
    if (held.dev(), held.ino()) != (lock.dev(), lock.ino()) {
        return None;
    }
    Lock::try_take(file).ok().flatten()
}

/// Stops lead `lead`'s broker once its request in flight is answered, then
/// starts one from this loopherd's binary; with none running, just starts one.
/// Requests still queued wait for the new broker. Gives up, leaving the old
/// one running, after as long as its lead waits on a feed. `exe` is resolved
/// by the caller up front, since a reinstall during the wait renames over it.
pub fn restart_broker(paths: &Paths, lead: &str, exe: &Path, sink: &mut dyn Sink) -> Result<()> {
    restart_broker_with(paths, lead, FEED_WITHIN, exe, sink, &mut || {
        spawn(paths, lead, exe)
    })
}

fn restart_broker_with(
    paths: &Paths,
    lead: &str,
    within: Duration,
    exe: &Path,
    sink: &mut dyn Sink,
    start: &mut dyn FnMut() -> Result<Started>,
) -> Result<()> {
    validate_name(lead)?;
    // Held until it returns: what makes its stop live, to brokers and to leads in any pid namespace.
    let restart = restart_file(paths, lead);
    let Some(_restarting) = take_lock_at(&restart)? else {
        bail!(
            "another loopherd broker-restart {lead} is running, or a process (a sandboxed lead's \
             loopherd, say) holds {} locked",
            restart.display()
        );
    };
    let stop = stop_file(paths, lead);
    // A killed restart's leftover, which a broker given its pid would obey.
    let _ = std::fs::remove_file(&stop);
    // A broker that just took its lock writes its pid a moment later, and one exiting has removed it.
    let given_up = Instant::now() + START_TIMEOUT;
    let old = loop {
        if !running(paths, lead) {
            let started = start().with_context(|| format!("start {lead}'s broker"))?;
            sink.out(&format!("started {lead}'s broker: pid {}", started.pid()));
            warn_of_another_binary(paths, lead, started, exe, sink);
            return Ok(());
        }
        if let Some(pid) = serving_pid(paths, lead) {
            break pid;
        }
        if Instant::now() >= given_up {
            bail!(
                "{lead}'s broker lock is held by a process that recorded no pid: a broker that \
                 predates loopherd broker-restart, which can't be asked to stop, or a lead's \
                 loopherd that holds it locked. Stop such a broker yourself (pgrep -f 'loopherd \
                 broker {lead}' finds it); with none to find, the lock is the lead's, so run \
                 loopherd broker-restart {lead} again once its loopherd command ends"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    write_replacing(&stop, &old.to_string())?;
    sink.out(&format!(
        "asked {lead}'s broker (pid {old}) to stop once its request in flight is answered"
    ));
    // Not "while the lock is held": a revive's broker may take it as soon as the old one exits.
    let old_serves = || running(paths, lead) && serving_pid(paths, lead) == Some(old);
    let given_up = Instant::now() + within;
    while old_serves() {
        if Instant::now() >= given_up {
            let _ = std::fs::remove_file(&stop);
            // A broker that stopped at the bound holds its lock until its lanes are joined.
            let grace = Instant::now() + STOP_GRACE;
            while old_serves() && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(20));
            }
            if old_serves() {
                bail!(
                    "{lead}'s broker (pid {old}) didn't stop within {}, so it is left running; \
                     run loopherd broker-restart {lead} again once its request is answered",
                    humantime::format_duration(within)
                );
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let started = start();
    let _ = std::fs::remove_file(&stop);
    let new = started.with_context(|| {
        format!("{lead}'s broker (pid {old}) stopped, but none started in its place")
    })?;
    sink.out(&format!(
        "restarted {lead}'s broker: pid {old} stopped, pid {} serves",
        new.pid()
    ));
    warn_of_another_binary(paths, lead, new, exe, sink);
    Ok(())
}

impl Started {
    fn pid(self) -> u32 {
        match self {
            Started::Own(pid) | Started::Another(pid) => pid,
        }
    }
}

/// Warns when the broker that took the lock before the restart's own runs another binary than
/// `exe`; silent when `exe` is a bare name, as a reinstalled binary's is, since it can't be compared.
fn warn_of_another_binary(
    paths: &Paths,
    lead: &str,
    started: Started,
    exe: &Path,
    sink: &mut dyn Sink,
) {
    let Started::Another(pid) = started else {
        return;
    };
    let runs = match recorded_binary(paths, lead) {
        Some(b) if FileId::of(exe).is_none_or(|file| file == b.file) => return,
        Some(b) => format!(
            "runs {} ({}), not {}",
            b.path.display(),
            b.version,
            exe.display()
        ),
        None => "records no binary, as a loopherd older than this one does".into(),
    };
    sink.err(&format!(
        "warning: pid {pid} took {lead}'s broker lock first and {runs}; run loopherd \
         broker-restart {lead} again to replace it"
    ));
}

/// How long a restart past its bound still waits for the broker to let go of its lock.
const STOP_GRACE: Duration = Duration::from_millis(500);

/// Whether a running `broker-restart` asked this process, as `lead`'s broker, to stop.
fn asked_to_stop(paths: &Paths, lead: &str) -> bool {
    live_stop(&stop_file(paths, lead), &restart_file(paths, lead)) == Some(std::process::id())
}

/// The broker pid the stop at `stop` names, while a restart holds `restart`;
/// a restart that was killed leaves a stop nothing obeys.
fn live_stop(stop: &Path, restart: &Path) -> Option<u32> {
    if !restarting(restart) {
        return None;
    }
    std::fs::read_to_string(stop).ok()?.trim().parse().ok()
}

/// The pid of `lead`'s broker, as it recorded it, while that process lives.
fn serving_pid(paths: &Paths, lead: &str) -> Option<u32> {
    let text = std::fs::read_to_string(pid_file(paths, lead)).ok()?;
    let pid = text.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

fn write_replacing(path: &Path, text: &str) -> Result<()> {
    let partial = PathBuf::from(format!("{}.part", path.display()));
    std::fs::write(&partial, text).with_context(|| format!("write {}", partial.display()))?;
    std::fs::rename(&partial, path).with_context(|| format!("write {}", path.display()))
}

/// Whether a broker serves `lead` now.
pub fn running(paths: &Paths, lead: &str) -> bool {
    held(&lock_file(paths, lead))
}

/// The git timeout the broker holding `lock` recorded; an older broker's
/// record is empty, so this loopherd's own stands in.
fn broker_remote_timeout(lock: &Path) -> Duration {
    std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| text.lines().next()?.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| {
            crate::worktree::remote_timeout().unwrap_or(crate::worktree::REMOTE_TIMEOUT)
        })
}

/// What a broker's lock holds once it marks each request it starts; an older one doesn't.
const MARKS: &str = "marks";

const VERSION: &str = "version ";
const BINARY: &str = "binary ";
const FILE: &str = "file ";
const FAILED: &str = "failed ";

/// A loopherd binary, which a broker records in its lock so a newer one can tell it is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binary {
    pub path: PathBuf,
    /// What `loopherd --version` prints, which names its commit off a release tag.
    pub version: String,
    /// Tells two builds apart that print one version, such as two dirty builds of a commit.
    pub file: FileId,
}

impl Binary {
    /// The binary this process runs.
    fn running() -> Option<Binary> {
        let path = std::env::current_exe().ok()?;
        // /proc's is the running file even once a reinstall replaced its path; macOS has no /proc.
        let file = FileId::of(Path::new("/proc/self/exe")).or_else(|| FileId::of(&path))?;
        Some(Binary {
            path,
            version: env!("LOOPHERD_VERSION").into(),
            file,
        })
    }
}

/// A file's device and inode, which a rebuild or reinstall changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    /// The file at `path`, following links.
    pub fn of(path: &Path) -> Option<FileId> {
        let meta = std::fs::metadata(path).ok()?;
        Some(FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

/// The binary `lead`'s broker recorded; `None` from one that predates recording it.
pub fn recorded_binary(paths: &Paths, lead: &str) -> Option<Binary> {
    let text = std::fs::read_to_string(lock_file(paths, lead)).ok()?;
    let field = |prefix: &str| text.lines().find_map(|line| line.strip_prefix(prefix));
    let (dev, ino) = field(FILE)?.split_once(' ')?;
    Some(Binary {
        version: field(VERSION)?.into(),
        path: field(BINARY)?.into(),
        file: FileId {
            dev: dev.parse().ok()?,
            ino: ino.parse().ok()?,
        },
    })
}

/// The install `lead`'s broker last couldn't move onto, which it won't try again.
pub fn failed_install(paths: &Paths, lead: &str) -> Option<FileId> {
    let text = std::fs::read_to_string(lock_file(paths, lead)).ok()?;
    let (dev, ino) = text
        .lines()
        .find_map(|line| line.strip_prefix(FAILED))?
        .split_once(' ')?;
    Some(FileId {
        dev: dev.parse().ok()?,
        ino: ino.parse().ok()?,
    })
}

fn marks_starts(lock: &Path) -> bool {
    std::fs::read_to_string(lock).is_ok_and(|text| text.lines().any(|line| line == MARKS))
}

/// Whether a restart holds `restart` exclusively. Probed shared, so probes
/// running at once never mistake each other for a restart.
fn restarting(restart: &Path) -> bool {
    let Ok(file) = std::fs::File::open(restart) else {
        return false;
    };
    // SAFETY: flock on a descriptor `file` keeps open; closing it drops a shared lock taken here.
    let probed = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    probed != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

fn held(lock: &Path) -> bool {
    // Read-only, so a sandboxed lead can probe a lock in the config dir.
    let Ok(file) = std::fs::File::open(lock) else {
        return false;
    };
    matches!(Lock::try_take(file), Ok(None))
}

/// How long a starting broker retries its lock: a probe holds it for an instant.
const LOCK_GRACE: Duration = Duration::from_secs(1);

fn take_lock(paths: &Paths, lead: &str) -> Result<Option<Lock>> {
    take_lock_at(&lock_file(paths, lead))
}

fn take_lock_at(path: &Path) -> Result<Option<Lock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let given_up = Instant::now() + LOCK_GRACE;
    loop {
        let attempt = file
            .try_clone()
            .with_context(|| format!("open {}", path.display()))?;
        let lock = Lock::try_take(attempt).with_context(|| format!("lock {}", path.display()))?;
        if lock.is_some() || Instant::now() >= given_up {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Removes replies and start marks past [`REPLY_KEPT`], but not the mark
/// of a request still in flight.
fn prune(replies: &Path, in_flight: &HashMap<String, Lane>) {
    let Ok(entries) = std::fs::read_dir(replies) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let running = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".started"))
            .is_some_and(|id| in_flight.contains_key(&format!("{id}.json")));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > REPLY_KEPT);
        if old && !running {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn write_reply(replies: &Path, id: &str, reply: &Reply) {
    let _ = write_atomically(replies, id, &serde_json::to_vec(reply).unwrap_or_default());
}

fn write_atomically(dir: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    let partial = dir.join(format!(".{id}.part"));
    std::fs::write(&partial, bytes).with_context(|| format!("write {}", partial.display()))?;
    let done = dir.join(format!("{id}.json"));
    std::fs::rename(&partial, &done).with_context(|| format!("write {}", done.display()))
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos}-{}", std::process::id())
}

fn is_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where lead `lead` hands its broker requests, the one folder of the
/// broker's that the lead's sandbox lets it write.
pub fn requests_dir(paths: &Paths, lead: &str) -> PathBuf {
    paths.state_dir().join("broker").join(lead).join("requests")
}

fn replies_dir(paths: &Paths, lead: &str) -> PathBuf {
    dir(paths).join(lead).join("replies")
}

/// Each broker's lock, pid, stop and restart files, and its replies.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.config_dir().join("broker")
}

/// Holds `lead`'s broker lock, so [`running`] reads true until it drops.
#[cfg(test)]
pub fn stand_in(paths: &Paths, lead: &str) -> Lock {
    take_lock(paths, lead).unwrap().expect("no broker holds it")
}

fn lock_file(paths: &Paths, lead: &str) -> PathBuf {
    dir(paths).join(format!("{lead}.lock"))
}

fn pid_file(paths: &Paths, lead: &str) -> PathBuf {
    dir(paths).join(format!("{lead}.pid"))
}

/// Locked by `broker-restart` while it runs.
fn restart_file(paths: &Paths, lead: &str) -> PathBuf {
    dir(paths).join(format!("{lead}.restart"))
}

/// Names the pid of the broker `broker-restart` is stopping; in the config
/// dir, so a lead can read it but not write it.
fn stop_file(paths: &Paths, lead: &str) -> PathBuf {
    dir(paths).join(format!("{lead}.stop"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};
    use std::thread::JoinHandle;

    use serde_json::json;

    use super::*;

    #[test]
    fn a_binary_a_reinstall_replaced_runs_as_loopherd_on_path() {
        let gone = PathBuf::from("/home/u/.local/bin/loopherd (deleted)");
        let here = PathBuf::from("/home/u/.local/bin/loopherd");

        assert_eq!(runnable(gone), PathBuf::from("loopherd"));
        assert_eq!(runnable(here.clone()), here);
    }
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, FileLedger, Role};
    use crate::sandbox::{self, Unavailable};
    use crate::subagent_view::screens::MAIN_VIEW;

    const POLL: Duration = Duration::from_millis(10);

    /// A config and state dir holding herd `h` with lead `lead-h`, and herds'
    /// repos outside it, where a lead's guard lets git run.
    struct World {
        dir: tempfile::TempDir,
        repos: tempfile::TempDir,
        paths: Paths,
    }

    impl World {
        fn new() -> World {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let repos = tempfile::tempdir().unwrap();
            let world = World { dir, repos, paths };
            world.herd("h");
            world.launch("lead-h", "h");
            world
        }

        fn herd(&self, herd: &str) {
            let repo = self.repos.path().join(herd);
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::create_dir_all(&self.paths.herds).unwrap();
            let text = format!(
                "repo = {:?}\n\
                 [template.pr]\nparams = [\"number\"]\nname = \"pr-{{number}}\"\nprompt = \"review {{number}}\"\n\
                 [queue]\ntemplate = \"pr\"\nsource = \"true\"\nmax = 2\n\
                 [lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            );
            std::fs::write(self.paths.herds.join(format!("{herd}.toml")), text).unwrap();
        }

        /// Keeps `lead`'s launch record, as `loopherd lead` does, and records it in the ledger.
        fn launch(&self, lead: &str, herd: &str) {
            let kept = sandbox::Lead {
                herd: herd.into(),
                args: vec![],
                path: None,
            };
            fn no_sandbox() -> Result<sandbox::Sockets, Unavailable> {
                Err(Unavailable {
                    why: "none here".into(),
                    fix: "none".into(),
                })
            }
            sandbox::prepare(&self.paths, lead, &kept, no_sandbox).unwrap();
            let mut lead = member(lead, Some(herd));
            lead.role = Some(Role::Lead);
            self.ledger().record(lead).unwrap();
        }

        fn ledger(&self) -> FileLedger {
            FileLedger::new(self.dir.path().join("ledger.json"))
        }

        fn add(&self, record: AgentRecord) {
            self.ledger().record(record).unwrap();
        }

        fn serve(&self, herdr: FakeHerdr) -> JoinHandle<(Result<Served>, FakeHerdr)> {
            self.serve_every(herdr, POLL)
        }

        fn serve_every(
            &self,
            herdr: FakeHerdr,
            poll: Duration,
        ) -> JoinHandle<(Result<Served>, FakeHerdr)> {
            self.serve_on(herdr, poll, self.ledger())
        }

        fn serve_on<L: Ledger + Clone + Send + 'static>(
            &self,
            herdr: FakeHerdr,
            poll: Duration,
            ledger: L,
        ) -> JoinHandle<(Result<Served>, FakeHerdr)> {
            let paths = self.paths.clone();
            let serving = std::thread::spawn(move || {
                let served = serve(&host(&herdr), &ledger, &paths, "lead-h", poll, None);
                (served, herdr)
            });
            let given_up = Instant::now() + Duration::from_secs(5);
            while !running(&self.paths, "lead-h") && !serving.is_finished() {
                assert!(Instant::now() < given_up, "the broker never started");
                std::thread::sleep(POLL);
            }
            if !serving.is_finished() {
                assert!(requests_dir(&self.paths, "lead-h").is_dir());
            }
            serving
        }

        fn retire(&self, name: &str) {
            self.ledger()
                .update(name, &mut |r| {
                    r.retired = true;
                    Ok(())
                })
                .unwrap();
        }

        fn client(&self) -> Client {
            Client::for_lead(&self.paths, "lead-h")
        }

        /// The route from pane `id` inside a lead's sandbox, whose own pid
        /// namespace hides the lead's Claude: by pane and socket alone.
        fn route_from(&self, id: &str) -> Option<Client> {
            let socket = Path::new(crate::herdr::fake::FAKE_SOCKET);
            self.ledger()
                .update("lead-h", &mut |r| {
                    r.socket = Some(socket.into());
                    Ok(())
                })
                .unwrap();
            let pane = tool_hook::Pane {
                id,
                socket: Some(socket),
                lineage: &[],
            };
            let procs = procs(&["bwrap", "--unshare-pid"]);
            route(&self.paths, &self.ledger(), &procs, 50, Some(&pane), || {
                false
            })
        }

        /// Runs `f` with the state dir read-only, as in a lead's sandbox (D97).
        fn read_only<T>(&self, f: impl FnOnce() -> T) -> T {
            /// Makes the dir writable again, even if `f` panics, so it can be removed.
            struct Restore<'a>(&'a Path);
            impl Drop for Restore<'_> {
                fn drop(&mut self) {
                    mode(self.0, 0o755);
                    mode(&self.0.join("ledger.lock"), 0o644);
                }
            }
            fn mode(path: &Path, mode: u32) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
            }
            let root = self.dir.path();
            std::fs::write(root.join("ledger.lock"), "").unwrap();
            let _restore = Restore(root);
            mode(&root.join("ledger.lock"), 0o444);
            mode(root, 0o555);
            f()
        }
    }

    /// Root writes past file modes, so nothing is read-only to it.
    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    fn host(herdr: &FakeHerdr) -> Host<'_> {
        Host {
            herdr,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            process: me(),
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
            follow: None,
        }
    }

    fn same_session(session: &str) -> String {
        session.into()
    }

    const NO_PRS: crate::github::fake::FakePrs =
        crate::github::fake::FakePrs(Ok(crate::github::PrLookup::None));

    fn no_waiter(_: &str, _: Lock) -> Result<()> {
        Ok(())
    }

    fn me() -> Process {
        let pid = std::process::id();
        let start = crate::process::ProcFs.start(pid).unwrap();
        Process { pid, start }
    }

    /// What `op` printed when run in a process outside any pane.
    fn run_here(
        herdr: &FakeHerdr,
        world: &World,
        client: Option<&Client>,
        op: &Op,
    ) -> Result<(i32, Kept)> {
        let mut ledger = world.ledger();
        let mut ctx = Ctx {
            herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: None,
                socket: None,
                lineage: vec![],
                workspace: None,
                process: me(),
                lead: None,
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();
        let code = run(&mut ctx, client, op, &mut kept)?;
        Ok((code, kept))
    }

    fn member(name: &str, herd: Option<&str>) -> AgentRecord {
        serde_json::from_value(json!({
            "name": name, "pane_id": format!("p-{name}"), "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd,
        }))
        .unwrap()
    }

    fn tell(name: &str, text: &str) -> Op {
        Op::Tell {
            name: name.into(),
            text: text.into(),
        }
    }

    /// A herdr whose `lh-1` shows its main view and goes working when prompted.
    fn telling_herdr() -> FakeHerdr {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        herdr.set_screen("w9:p9", MAIN_VIEW);
        herdr.set_working_on_prompt();
        herdr
    }

    #[test]
    fn a_target_outside_the_herds_unretired_workers_is_refused_naming_why() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world.add(member("lh-8", None));
        let mut unknown_role = member("lh-5", Some("h"));
        unknown_role.role = Some(crate::ledger::Role::Unknown("reviewer".into()));
        world.add(unknown_role);
        let mut retired = member("lh-7", Some("h"));
        retired.retired = true;
        world.add(retired);
        let serving = world.serve(telling_herdr());
        let reach = "lead-h reaches only herd h's workers";

        for (target, why) in [
            ("shepherd", format!("shepherd is the shepherd; {reach}")),
            ("lh-9", format!("lh-9 is in herd g; {reach}")),
            ("lh-8", format!("lh-8 is in no herd; {reach}")),
            (
                "lh-5",
                format!("lh-5's role is from another loopherd version; {reach}"),
            ),
            (
                "lh-7",
                format!("no unretired member is named lh-7; {reach}"),
            ),
            (
                "lh-6",
                format!("no unretired member is named lh-6; {reach}"),
            ),
            (
                "lead-h",
                "lead-h is the lead itself; {command} reaches only herd h's workers".into(),
            ),
        ] {
            for op in [
                tell(target, "hi"),
                Op::Prompt {
                    name: target.into(),
                    force: true,
                },
                Op::Restart {
                    name: target.into(),
                    force: false,
                },
                Op::Read {
                    name: target.into(),
                    lines: 40,
                },
            ] {
                let err = world.client().call(&op).unwrap_err();
                let command = op.command();
                let hint = match (target, &op) {
                    ("lead-h", Op::Restart { .. }) => {
                        "; it restarts itself with loopherd restart --self"
                    }
                    _ => "",
                };
                assert_eq!(
                    format!("{err:#}"),
                    format!(
                        "lead-h's broker refused {command}: {}{hint}",
                        why.replace("{command}", command)
                    )
                );
            }
        }
        world.add(member("lh-1", Some("h")));
        let forced = Op::Restart {
            name: "lh-1".into(),
            force: true,
        };
        assert_eq!(
            format!("{:#}", world.client().call(&forced).unwrap_err()),
            "lead-h's broker refused restart: restart --force skips the checks that a member is \
             finished since its handoff, so the human forces a restart; restart it once its stop \
             reads finished"
        );

        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    /// Herd h's file with an attended `grill` template whose `done` check passes.
    fn with_grill(world: &World) {
        let herd_file = world.paths.herds.join("h.toml");
        let mut text = std::fs::read_to_string(&herd_file).unwrap();
        text = text.replace(
            "[queue]",
            "[template.grill]\nparams = [\"card\"]\nname = \"grill-{card}\"\nprompt = \"grill {card}\"\n\
             done = \"true\"\nattended = true\n[queue]",
        );
        std::fs::write(&herd_file, text).unwrap();
    }

    fn grill(name: &str) -> AgentRecord {
        let mut grill = member(name, Some("h"));
        grill.template = Some("grill".into());
        grill.params = Some([("card".to_string(), "1".to_string())].into());
        grill.attended = Some(Default::default());
        grill
    }

    #[test]
    fn a_lead_s_broker_leaves_an_attended_member_and_template_to_the_human() {
        let world = World::new();
        with_grill(&world);
        world.add(grill("grill-1"));
        let serving = world.serve(telling_herdr());
        let why = "grill-1 is attended: the human works in its pane, so lead-h leaves it to them \
                   for its whole life; once merged, its done check reaps it, or the shepherd or \
                   the human retires it (D112)";

        for op in [
            tell("grill-1", "hi"),
            Op::Prompt {
                name: "grill-1".into(),
                force: false,
            },
            Op::Restart {
                name: "grill-1".into(),
                force: false,
            },
            retire_op("grill-1", true, false),
            release_op("grill-1", false),
        ] {
            assert_eq!(
                format!("{:#}", world.client().call(&op).unwrap_err()),
                format!("lead-h's broker refused {}: {why}", op.command())
            );
        }
        let add = Op::Add {
            herd: "h".into(),
            template: "grill".into(),
            params: vec!["2".into()],
            claude_args: vec![],
            dry_run: false,
            ready_timeout_secs: 60,
            no_focus: false,
        };
        assert_eq!(
            format!("{:#}", world.client().call(&add).unwrap_err()),
            "lead-h's broker refused add: template grill is attended, so only the shepherd or \
             the human adds it (D112)"
        );

        assert!(!world.ledger().entries().unwrap()["grill-1"].retired);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    #[test]
    fn a_lead_s_broker_still_refuses_an_attended_member_once_it_is_retired() {
        let world = World::new();
        with_grill(&world);
        let mut retired = grill("grill-1");
        retired.retired = true;
        world.add(retired);
        let serving = world.serve(telling_herdr());

        for op in [
            retire_op("grill-1", true, false),
            release_op("grill-1", false),
        ] {
            assert_eq!(
                format!("{:#}", world.client().call(&op).unwrap_err()),
                format!(
                    "lead-h's broker refused {}: grill-1 is attended: the human works in its \
                     pane, so lead-h leaves it to them for its whole life; it is retired, so its \
                     release is the shepherd's or the human's (D112)",
                    op.command()
                )
            );
        }

        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    #[test]
    fn a_lead_s_feed_still_reaps_an_attended_member_once_its_done_check_passes() {
        let world = World::new();
        with_grill(&world);
        world.add(grill("grill-1"));
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, _, code) = world.client().call(&feed("h")).unwrap();

        assert_eq!(code, 0, "{out}");
        assert!(
            world.ledger().entries().unwrap()["grill-1"].retired,
            "{out}"
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    fn retire_op(name: &str, close: bool, force: bool) -> Op {
        Op::Retire {
            name: name.into(),
            close,
            force,
        }
    }

    fn release_op(name: &str, drop_branch: bool) -> Op {
        Op::Release {
            name: name.into(),
            keep_branch: false,
            drop_branch,
        }
    }

    #[test]
    fn retire_and_release_reach_the_herd_s_workers_retired_or_not_and_nothing_else() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world.add(member("lh-8", None));
        let mut other_lead = member("lead-x", Some("h"));
        other_lead.role = Some(Role::Lead);
        world.add(other_lead);
        let mut retired_lead = member("lead-y", Some("h"));
        retired_lead.role = Some(Role::Lead);
        retired_lead.retired = true;
        world.add(retired_lead);
        let mut retired_elsewhere = member("lh-10", Some("g"));
        retired_elsewhere.retired = true;
        world.add(retired_elsewhere);
        let serving = world.serve(telling_herdr());
        let reach = "lead-h reaches only herd h's workers";
        for (target, why) in [
            ("shepherd", format!("shepherd is the shepherd; {reach}")),
            ("lh-9", format!("lh-9 is in herd g; {reach}")),
            ("lh-8", format!("lh-8 is in no herd; {reach}")),
            ("lead-x", format!("lead-x is a lead; {reach}")),
            ("lead-y", format!("lead-y is a lead; {reach}")),
            ("lh-10", format!("lh-10 is in herd g; {reach}")),
            ("lh-6", format!("no member is named lh-6; {reach}")),
            (
                "lead-h",
                "lead-h is the lead itself; {command} reaches only herd h's workers".into(),
            ),
        ] {
            for op in [
                retire_op(target, false, false),
                retire_op(target, true, false),
                release_op(target, false),
            ] {
                let command = op.command();
                let hint = match (target, &op) {
                    ("lead-h", Op::Retire { .. }) => {
                        "; retiring a lead is the shepherd's or the human's job"
                    }
                    _ => "",
                };
                assert_eq!(
                    format!("{:#}", world.client().call(&op).unwrap_err()),
                    format!(
                        "lead-h's broker refused {command}: {}{hint}",
                        why.replace("{command}", command)
                    )
                );
            }
        }
        world.add(member("lh-1", Some("h")));
        for (op, why) in [
            (
                retire_op("lh-1", false, true),
                "retire --force skips the check that a member is finished, so the human forces \
                 a retire; retire it once its stop reads finished",
            ),
            (
                retire_op("lh-1", true, true),
                "retire --force skips the check that a member is finished, so the human forces \
                 a retire; retire it once its stop reads finished",
            ),
            (
                release_op("lh-1", true),
                "release --drop-branch deletes commits not on the base, so the human drops a \
                 branch; release it plain or with --keep-branch",
            ),
            (
                Op::Release {
                    name: "lh-1".into(),
                    keep_branch: true,
                    drop_branch: true,
                },
                "release --drop-branch deletes commits not on the base, so the human drops a \
                 branch; release it plain or with --keep-branch",
            ),
        ] {
            assert_eq!(
                format!("{:#}", world.client().call(&op).unwrap_err()),
                format!("lead-h's broker refused {}: {why}", op.command())
            );
        }
        assert!(!world.ledger().entries().unwrap()["lh-1"].retired);

        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_retire_and_release_go_through_its_broker_and_call_no_herdr() {
        let world = World::new();
        let repo = world.repos.path().join("h");
        let mut lh_1 = member("lh-1", Some("h"));
        lh_1.repo = repo;
        world.add(lh_1);
        let serving = world.serve(FakeHerdr::new(&[]));
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");

        let retired = run_here(
            &own,
            &world,
            client.as_ref(),
            &retire_op("lh-1", false, false),
        );
        let closed = run_here(
            &own,
            &world,
            client.as_ref(),
            &retire_op("lh-1", true, false),
        );
        let released = run_here(&own, &world, client.as_ref(), &release_op("lh-1", false));

        let out = |done: Result<(i32, Kept)>| done.map(|(code, kept)| (code, kept.out));
        assert_eq!(out(retired).unwrap(), (0, vec!["retired lh-1".to_string()]));
        // A rerun of --close on a retired member, as after a failed close.
        let (code, closed) = out(closed).unwrap();
        assert_eq!(code, 0);
        assert!(
            closed[0].starts_with("lh-1 is already retired"),
            "{closed:?}"
        );
        assert_eq!(
            out(released).unwrap(),
            (
                0,
                vec!["released lh-1: feed may spawn it again".to_string()]
            )
        );
        assert!(world.ledger().entries().unwrap()["lh-1"].retired);
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_broker_feed_waits_on_after_merge_as_long_as_its_lead_waits_on_the_feed() {
        let world = World::new();
        let herdr = FakeHerdr::new(&[]);
        let mut ledger = world.ledger();
        let ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: None,
                socket: None,
                lineage: vec![],
                workspace: None,
                process: me(),
                lead: None,
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };

        let req = feed_request(&ctx, "h", false, 60).unwrap();

        assert_eq!(req.after_merge_wait, Duration::from_secs(4 * 60 * 60));
    }

    /// Past the broker's reach check, as if it were ever loosened, the
    /// handlers' own guard still holds a lead to its herd.
    #[test]
    fn a_lead_s_retire_close_and_release_guard_its_herd_behind_the_reach_check() {
        let world = World::new();
        world.herd("g");
        let mut lh_9 = member("lh-9", Some("g"));
        lh_9.retired = true;
        world.add(lh_9);
        let herdr = FakeHerdr::new(&[]);
        let mut ledger = world.ledger();
        let mut ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: None,
                socket: None,
                lineage: vec![],
                workspace: None,
                process: me(),
                lead: Some(ForLead {
                    name: "lead-h".into(),
                    herd: "h".into(),
                }),
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();

        let close = retire_op("lh-9", true, false).carry_out(&mut ctx, &mut kept);
        let release = release_op("lh-9", false).carry_out(&mut ctx, &mut kept);

        let why = "refused: lh-9 isn't a member of lead-h's herd, and lead-h runs this loopherd";
        assert_eq!(format!("{:#}", close.unwrap_err()), why);
        assert_eq!(format!("{:#}", release.unwrap_err()), why);
    }

    #[test]
    fn a_plain_retire_of_a_lead_removes_its_mcp_servers_and_keeps_the_settings_its_claude_runs_on()
    {
        let world = World::new();
        let mut lead = member("lead-x", Some("x"));
        lead.role = Some(Role::Lead);
        world.add(lead);
        let sandbox = world.paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        for file in ["lead-x.json", "lead-x.mcp.json", "lead-x.lead.json"] {
            std::fs::write(sandbox.join(file), "{}").unwrap();
        }
        let herdr = FakeHerdr::new(&[]);
        let mut ledger = world.ledger();
        let mut ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: None,
                socket: None,
                lineage: vec![],
                workspace: None,
                process: me(),
                lead: None,
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();

        let code = retire_op("lead-x", false, true)
            .carry_out(&mut ctx, &mut kept)
            .unwrap();

        let mcp = sandbox.join("lead-x.mcp.json");
        assert_eq!(
            (code, kept.out, kept.err),
            (
                0,
                vec![
                    "retired lead-x".into(),
                    format!("removed {}", mcp.display())
                ],
                vec![]
            )
        );
        assert!(!mcp.exists());
        assert!(sandbox.join("lead-x.json").exists());
        assert!(sandbox.join("lead-x.lead.json").exists());
    }

    #[test]
    fn a_leads_retire_of_an_entry_forged_as_its_member_leaves_that_names_lead_files() {
        let world = World::new();
        world.add(member("lead-x", Some("h")));
        let sandbox = world.paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        for file in ["lead-x.json", "lead-x.mcp.json"] {
            std::fs::write(sandbox.join(file), "{}").unwrap();
        }
        let herdr = FakeHerdr::new(&[]);
        let mut ledger = world.ledger();
        let mut ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: Some("p-lead-h".into()),
                socket: None,
                lineage: vec![],
                workspace: Some("w1".into()),
                process: me(),
                lead: Some(ForLead {
                    name: "lead-h".into(),
                    herd: "h".into(),
                }),
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();

        let code = retire_op("lead-x", false, false)
            .carry_out(&mut ctx, &mut kept)
            .unwrap();

        assert_eq!((code, kept.out), (0, vec!["retired lead-x".to_string()]));
        assert!(sandbox.join("lead-x.mcp.json").exists());
        assert!(sandbox.join("lead-x.json").exists());
    }

    #[test]
    fn a_broker_s_plain_retire_removes_marks_and_records_tokens_in_the_state_dir_s_mailboxes() {
        let world = World::new();
        let mut worker = member("lh-1", Some("h"));
        worker.session_id = Some("s1".into());
        world.add(worker);
        let mailboxes = world.paths.state_dir().join("reports");
        std::fs::create_dir_all(mailboxes.join("lh-1")).unwrap();
        std::fs::write(mailboxes.join("lh-1.notified"), "question t1\n").unwrap();
        std::fs::write(mailboxes.join("lh-1.blocked"), "").unwrap();
        let stop = r#"{"at": "2026-11-01T09:30:00Z", "kind": "stop", "session": "s1", "crons": 0, "background": {"shells": 0, "subagents": 0, "other": 0}}"#;
        std::fs::write(mailboxes.join("lh-1/mailbox.jsonl"), format!("{stop}\n")).unwrap();
        let claude_dir = world.dir.path().join("claude");
        let project = claude_dir.join("projects/-r");
        std::fs::create_dir_all(&project).unwrap();
        let line = r#"{"type": "assistant", "timestamp": "2026-11-01T09:10:00Z", "message": {"id": "m1", "model": "opus", "usage": {"input_tokens": 1, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}}"#;
        std::fs::write(project.join("s1.jsonl"), format!("{line}\n")).unwrap();
        let herdr = FakeHerdr::new(&[]);
        let mut ledger = world.ledger();
        let mut ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: Some(claude_dir),
            caller: Caller {
                pane: Some("p-lead-h".into()),
                socket: None,
                lineage: vec![],
                workspace: Some("w1".into()),
                process: me(),
                lead: Some(ForLead {
                    name: "lead-h".into(),
                    herd: "h".into(),
                }),
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();

        let code = retire_op("lh-1", false, false)
            .carry_out(&mut ctx, &mut kept)
            .unwrap();

        assert_eq!(
            (code, kept.out, kept.err),
            (0, vec!["retired lh-1".into()], vec![])
        );
        assert!(!mailboxes.join("lh-1.notified").exists());
        assert!(!mailboxes.join("lh-1.blocked").exists());
        let text = std::fs::read_to_string(mailboxes.join("lh-1/mailbox.jsonl")).unwrap();
        let usage: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(usage["kind"], "usage", "{text}");
        assert_eq!(usage["models"]["opus"]["output"], 4);
        assert!(world.ledger().entries().unwrap()["lh-1"].retired);
    }

    #[test]
    fn the_herd_comes_from_the_launch_record_not_the_ledger() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.herd = Some("g".into());
                Ok(())
            })
            .unwrap();
        let serving = world.serve(telling_herdr());

        let err = world.client().call(&tell("lh-9", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker refused tell: lh-9 is in herd g; lead-h reaches only herd h's workers"
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn forged_and_malformed_requests_are_refused_and_the_broker_keeps_serving() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let requests = requests_dir(&world.paths, "lead-h");
        std::fs::create_dir_all(&requests).unwrap();
        let secret = world.dir.path().join("secret");
        std::fs::write(&secret, r#"{"op":{"tell":{"name":"lh-1","text":"hi"}}}"#).unwrap();
        std::os::unix::fs::symlink(&secret, requests.join("link.json")).unwrap();
        std::fs::hard_link(&secret, requests.join("hard.json")).unwrap();
        std::fs::write(requests.join("garbage.json"), "{ nope").unwrap();
        std::fs::write(
            requests.join("spawn.json"),
            r#"{"op":{"spawn":{"name":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            requests.join("extra.json"),
            r#"{"op":{"tell":{"name":"lh-1","text":"hi"}},"lead":"lead-g"}"#,
        )
        .unwrap();
        std::fs::create_dir(requests.join("dir.json")).unwrap();
        std::fs::write(requests.join("bad name.json"), "{}").unwrap();
        std::fs::write(
            requests.join("huge.json"),
            vec![b' '; MAX_REQUEST as usize + 1],
        )
        .unwrap();
        let serving = world.serve(telling_herdr());
        // The client's id sorts first, so in a shared round its answer could beat these refusals.
        let given_up = Instant::now() + Duration::from_secs(5);
        while std::fs::read_dir(&requests).unwrap().count() > 0 {
            assert!(
                Instant::now() < given_up,
                "the forged requests were never answered"
            );
            std::thread::sleep(POLL);
        }

        let (out, _, _) = world.client().call(&tell("lh-1", "hi")).unwrap();

        assert_eq!(out, "told lh-1; it's working");
        let replies = replies_dir(&world.paths, "lead-h");
        let refused = |id: &str| -> String {
            let text = std::fs::read_to_string(replies.join(format!("{id}.json"))).unwrap();
            match serde_json::from_str(&text).unwrap() {
                Reply::Refused(why) => why,
                other => panic!("{id}: {other:?}"),
            }
        };
        assert_eq!(refused("link"), "not a request: not a plain file");
        assert_eq!(refused("hard"), "not a request: not a plain file");
        assert!(refused("garbage").starts_with("malformed request: "));
        assert!(refused("spawn").starts_with("malformed request: unknown variant `spawn`"));
        assert!(refused("extra").starts_with("malformed request: unknown field `lead`"));
        assert_eq!(refused("dir"), "not a request: not a plain file");
        assert_eq!(refused("huge"), "not a request: over 1048576 bytes");
        assert!(!replies.join("bad name.json").exists());
        assert_eq!(std::fs::read_dir(&requests).unwrap().count(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_second_broker_for_the_same_lead_does_not_start() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));

        let second = serve(
            &host(&FakeHerdr::new(&[])),
            &world.ledger(),
            &world.paths,
            "lead-h",
            POLL,
            None,
        );

        assert_eq!(second.unwrap(), Served::AlreadyRunning);
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_serving_broker_records_its_git_timeout_for_its_leads_waits() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));

        let lock = lock_file(&world.paths, "lead-h");
        // The broker records it just after taking the lock.
        let given_up = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&lock).unwrap().is_empty() {
            assert!(
                Instant::now() < given_up,
                "the broker never recorded its timeout"
            );
            std::thread::sleep(POLL);
        }
        let recorded = broker_remote_timeout(&lock);

        let own = crate::worktree::remote_timeout().unwrap();
        assert!(
            std::fs::read_to_string(&lock)
                .unwrap()
                .starts_with(&format!("{}\nmarks\n", own.as_secs()))
        );
        assert_eq!(recorded, own);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_serving_broker_records_the_binary_it_runs() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));
        let given_up = Instant::now() + Duration::from_secs(5);
        while recorded_binary(&world.paths, "lead-h").is_none() {
            assert!(
                Instant::now() < given_up,
                "the broker never recorded its binary"
            );
            std::thread::sleep(POLL);
        }

        let recorded = recorded_binary(&world.paths, "lead-h").unwrap();

        let exe = std::env::current_exe().unwrap();
        assert_eq!(recorded.path, exe);
        assert_eq!(recorded.file, FileId::of(&exe).unwrap());
        assert_eq!(recorded.version, env!("LOOPHERD_VERSION"));
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_broker_that_records_no_binary_predates_recording_it() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(lock_file(&world.paths, "lead-h"), "300\nmarks\n").unwrap();

        assert_eq!(recorded_binary(&world.paths, "lead-h"), None);
    }

    #[test]
    fn a_lead_sizes_its_wait_by_the_brokers_git_timeout_else_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lead-h.lock");
        let own = crate::worktree::remote_timeout().unwrap();

        std::fs::write(&lock, "7").unwrap();
        assert_eq!(broker_remote_timeout(&lock), Duration::from_secs(7));
        std::fs::write(&lock, "").unwrap();
        assert_eq!(broker_remote_timeout(&lock), own);
    }

    #[test]
    fn with_no_broker_running_a_lead_s_command_fails_at_once_naming_it_and_revive() {
        let world = World::new();
        let started = Instant::now();

        let err = world.client().call(&tell("lh-1", "hi")).unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            format!("{err:#}"),
            "no broker is running for lead-h, so it can't tell: \
             the shepherd's loopherd revive --name lead-h starts it"
        );
    }

    /// herdr → shell → claude → bash 40 → loopherd 50, where the Claude was started with `args`.
    fn procs(args: &[&str]) -> crate::process::FakeProcs {
        let mut procs = crate::process::FakeProcs::pane();
        procs.set_cmdline(30, args);
        procs.add(40, "/bin/bash", 30, 400);
        procs.add(50, "/usr/bin/loopherd", 40, 500);
        procs
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_tell_goes_through_its_broker_and_calls_no_herdr() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let serving = world.serve(telling_herdr());
        let own = FakeHerdr::new(&[]);

        let client = world.route_from("p-lead-h");
        assert!(client.is_some());
        let (_, kept) = run_here(&own, &world, client.as_ref(), &tell("lh-1", "hi")).unwrap();

        assert_eq!(kept.out, ["told lh-1; it's working"]);
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_sandboxed_lead_s_report_is_written_to_its_own_mailbox_by_its_broker() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");
        let report = Op::Report {
            kind: crate::report::Kind::Handoff,
            text: "at step 3".into(),
        };

        let (code, _) = run_here(&own, &world, client.as_ref(), &report).unwrap();

        assert_eq!(code, 0);
        let mailbox = world.paths.state_dir().join("reports/lead-h/mailbox.jsonl");
        let mailbox = std::fs::read_to_string(mailbox).unwrap();
        assert!(
            mailbox.contains(r#""kind":"handoff","text":"at step 3""#),
            "{mailbox}"
        );
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_s_report_is_written_even_when_its_record_names_another_herdr() {
        let world = World::new();
        let client = world.route_from("p-lead-h");
        let serving = world.serve(FakeHerdr::new(&[]));
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some("/elsewhere.sock".into());
                Ok(())
            })
            .unwrap();
        let report = Op::Report {
            kind: Kind::Handoff,
            text: "at step 3".into(),
        };

        let (_, _, code) = client.unwrap().call(&report).unwrap();

        assert_eq!(code, 0);
        let mailbox = world.paths.state_dir().join("reports/lead-h/mailbox.jsonl");
        let mailbox = std::fs::read_to_string(mailbox).unwrap();
        assert!(mailbox.contains("at step 3"), "{mailbox}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_s_question_whose_record_names_another_herdr_is_written_and_warns_of_no_label() {
        let world = World::new();
        std::fs::write(
            crate::herd::config_file(&world.paths),
            "[notify]\nenabled = true\nherdr = false\npane_label = true\n",
        )
        .unwrap();
        let client = world.route_from("p-lead-h");
        let serving = world.serve(FakeHerdr::new(&[]));
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some("/elsewhere.sock".into());
                Ok(())
            })
            .unwrap();
        let report = Op::Report {
            kind: Kind::Question,
            text: "which DB?".into(),
        };

        let (_, err, code) = client.unwrap().call(&report).unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            err,
            "loopherd report: warning: the report went out, but its pane gets no label: \
             lead-h runs in another herdr than its broker's"
        );
        let mailbox = world.paths.state_dir().join("reports/lead-h/mailbox.jsonl");
        let mailbox = std::fs::read_to_string(mailbox).unwrap();
        assert!(mailbox.contains("which DB?"), "{mailbox}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    fn status_op(all: bool, strays: bool) -> Op {
        Op::Status {
            all,
            strays,
            json: false,
            color: false,
        }
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_status_shows_its_herd_alone_through_its_broker() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-1", Some("h")));
        world.add(member("lh-9", Some("g")));
        world.add(member("lh-8", None));
        let mut retired = member("lh-7", Some("h"));
        retired.retired = true;
        world.add(retired);
        let serving = world.serve(telling_herdr());
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");

        let (code, status) =
            run_here(&own, &world, client.as_ref(), &status_op(true, false)).unwrap();

        let pauses = crate::pause::Pauses {
            dir: world.paths.paused.clone(),
            herds: Some(world.paths.herds.clone()),
        };
        let direct = crate::status::herd_status(
            &telling_herdr(),
            &world.ledger(),
            &pauses,
            None,
            true,
            crate::status::Format::Text { color: false },
            "h",
            None,
        )
        .unwrap();
        assert_eq!((code, status.out.join("\n") + "\n"), (0, direct.stdout));
        let shown = status.out.join("\n");
        for hidden in ["lh-9", "lh-8", "stray"] {
            assert!(!shown.contains(hidden), "{shown}");
        }
        for seen in ["lh-1", "lh-7", "lead-h"] {
            assert!(shown.contains(seen), "{shown}");
        }
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn an_empty_screen_reads_as_nothing_through_the_broker_and_directly() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let blank = || FakeHerdr::new(&[]).with_agent("lh-1");
        let serving = world.serve(blank());
        let read = Op::Read {
            name: "lh-1".into(),
            lines: 40,
        };

        let (_, brokered) =
            run_here(&FakeHerdr::new(&[]), &world, Some(&world.client()), &read).unwrap();
        let (_, direct) = run_here(&blank(), &world, None, &read).unwrap();

        assert_eq!((brokered.out, brokered.err), (vec![], vec![]));
        assert_eq!((direct.out, direct.err), (vec![], vec![]));
        world.retire("lead-h");
        let (served, _) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_read_goes_through_its_broker_and_calls_no_herdr() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let herdr = telling_herdr();
        herdr.set_screen("w9:p9", "one\ntwo\nthree\n");
        let serving = world.serve(herdr);
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");
        let read = Op::Read {
            name: "lh-1".into(),
            lines: 2,
        };

        let (code, kept) = run_here(&own, &world, client.as_ref(), &read).unwrap();

        assert_eq!((code, kept.out), (0, vec!["two\nthree".to_string()]));
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn a_lead_s_status_strays_are_refused() {
        let world = World::new();
        let serving = world.serve(telling_herdr());

        let err = world.client().call(&status_op(false, true)).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker refused status: strays are herdr's agents outside every herd; \
             lead-h sees only herd h, so run loopherd status without --strays"
        );
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    #[test]
    fn a_hook_only_lead_routes_by_its_pane_and_others_go_to_herdr_directly() {
        let world = World::new();
        let socket = Path::new("/run/h.sock");
        let on_socket = |mut record: AgentRecord| {
            record.socket = Some(socket.into());
            record
        };
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some(socket.into());
                Ok(())
            })
            .unwrap();
        world.add(on_socket(member("lh-1", Some("h"))));
        let mut gone = on_socket(member("lead-g", Some("g")));
        gone.role = Some(Role::Lead);
        gone.retired = true;
        world.add(gone);
        let procs = procs(&["claude"]);
        let routed = |id: &str| {
            let pane = tool_hook::Pane {
                id,
                socket: Some(socket),
                lineage: &[],
            };
            route(
                &world.paths,
                &world.ledger(),
                &procs,
                50,
                Some(&pane),
                || false,
            )
            .map(|c| c.lead)
        };

        assert_eq!(routed("p-lead-h").as_deref(), Some("lead-h"));
        assert_eq!(routed("p-lh-1"), None);
        assert_eq!(routed("p-lead-g"), None);
        let herdr = telling_herdr();
        run_here(&herdr, &world, None, &tell("lh-1", "hi")).unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_hook_only_lead_s_release_goes_through_its_broker_by_its_pane() {
        let world = World::new();
        let socket = Path::new("/run/h.sock");
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some(socket.into());
                Ok(())
            })
            .unwrap();
        let repo = world.repos.path().join("h");
        let mut done = member("lh-1", Some("h"));
        done.repo = repo;
        done.retired = true;
        world.add(done);
        let serving = world.serve(FakeHerdr::new(&[]));
        let pane = tool_hook::Pane {
            id: "p-lead-h",
            socket: Some(socket),
            lineage: &[],
        };
        let client = route(
            &world.paths,
            &world.ledger(),
            &procs(&["claude"]),
            50,
            Some(&pane),
            || false,
        );
        let own = FakeHerdr::new(&[]);

        let (code, kept) =
            run_here(&own, &world, client.as_ref(), &release_op("lh-1", false)).unwrap();

        assert_eq!(client.unwrap().lead, "lead-h");
        let released = "released lh-1: feed may spawn it again".to_string();
        assert_eq!((code, kept.out), (0, vec![released]));
        assert_eq!(own.calls(), 0);
        assert!(
            world.ledger().entries().unwrap()["lh-1"]
                .released_at
                .is_some()
        );
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.calls() > 0);
    }

    #[test]
    fn a_lead_ancestor_routes_to_its_broker_even_in_the_shepherd_s_pane() {
        let world = World::new();
        let socket = Path::new("/run/h.sock");
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some(socket.into());
                Ok(())
            })
            .unwrap();
        let pane = tool_hook::Pane {
            id: "p-lead-h",
            socket: Some(socket),
            lineage: &[],
        };
        let settings = sandbox::settings_file(&world.paths, "lead-h");
        let lead_ancestor = procs(&["claude", "--settings", settings.to_str().unwrap()]);
        let routed = |procs: &crate::process::FakeProcs| {
            route(
                &world.paths,
                &world.ledger(),
                procs,
                50,
                Some(&pane),
                || true,
            )
            .map(|c| c.lead)
        };

        assert_eq!(routed(&lead_ancestor).as_deref(), Some("lead-h"));
        assert_eq!(routed(&procs(&["claude"])), None);
    }

    #[test]
    fn the_shepherd_s_release_runs_in_its_own_process_where_the_ledger_is_writable() {
        let world = World::new();
        let repo = world.repos.path().join("h");
        let mut done = member("lh-1", Some("h"));
        done.repo = repo;
        done.retired = true;
        world.add(done);
        let own = FakeHerdr::new(&[]);
        assert!(world.route_from("w1:p-shepherd").is_none());

        let (code, _) = run_here(&own, &world, None, &release_op("lh-1", false)).unwrap();

        assert_eq!(code, 0);
        assert!(own.calls() > 0);
        assert!(
            world.ledger().entries().unwrap()["lh-1"]
                .released_at
                .is_some()
        );
    }

    #[test]
    fn an_unreadable_ledger_routes_nowhere_rather_than_failing() {
        let world = World::new();
        std::fs::write(world.dir.path().join("ledger.json"), "{ nope").unwrap();
        let pane = tool_hook::Pane {
            id: "p-lead-h",
            socket: Some(Path::new("/run/h.sock")),
            lineage: &[],
        };

        let client = route(
            &world.paths,
            &world.ledger(),
            &procs(&["claude"]),
            50,
            Some(&pane),
            || false,
        );

        assert!(client.is_none());
    }

    fn swap_for_symlink(path: &Path, target: &Path) {
        // A serving broker recreates a missing folder each round, so move each one aside until the link lands.
        for aside in 0.. {
            std::fs::rename(path, path.with_extension(format!("aside-{aside}"))).unwrap();
            match std::os::unix::fs::symlink(target, path) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("symlink {}: {e}", path.display()),
            }
        }
    }

    #[test]
    fn a_requests_folder_swapped_for_a_symlink_is_never_followed() {
        let world = World::new();
        let victim = world.dir.path().join("victim");
        std::fs::create_dir_all(victim.join("projects")).unwrap();
        std::fs::write(victim.join("notes.json"), "{}").unwrap();
        let serving = world.serve(FakeHerdr::new(&[]));
        swap_for_symlink(&requests_dir(&world.paths, "lead-h"), &victim);
        std::thread::sleep(POLL * 10);
        swap_for_symlink(&world.dir.path().join("broker"), &victim);
        std::fs::create_dir_all(victim.join("lead-h/requests")).unwrap();
        std::fs::write(victim.join("lead-h/requests/kept.json"), "{}").unwrap();

        std::thread::sleep(POLL * 10);

        assert!(victim.join("projects").is_dir());
        assert!(victim.join("notes.json").is_file());
        assert!(victim.join("lead-h/requests/kept.json").is_file());
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_name_that_is_not_a_member_name_gets_no_broker() {
        let world = World::new();
        for lead in ["../x", "/tmp/x", "Lead"] {
            let served = serve(
                &host(&FakeHerdr::new(&[])),
                &world.ledger(),
                &world.paths,
                lead,
                POLL,
                None,
            );
            assert!(
                format!("{:#}", served.unwrap_err()).contains("must match"),
                "{lead}"
            );
            assert!(format!("{:#}", start(&world.paths, lead).unwrap_err()).contains("must match"));
        }
    }

    #[test]
    fn a_request_the_lead_gave_up_on_is_withdrawn() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(lock_file(&world.paths, "lead-h"), "300\nmarks\n").unwrap();
        let client = Client {
            answer_within: Some(Duration::from_millis(50)),
            ..world.client()
        };
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();

        let err = client.call(&tell("lh-1", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't start tell within 50ms, so it was not started and is \
             withdrawn: the broker is busy with an earlier request; ask again later"
        );
        let left = std::fs::read_dir(requests_dir(&world.paths, "lead-h")).unwrap();
        assert_eq!(left.count(), 0);
    }

    #[test]
    fn a_broker_that_writes_no_start_marks_may_still_be_running_what_timed_out() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(lock_file(&world.paths, "lead-h"), "300").unwrap();
        let client = Client {
            answer_within: Some(Duration::from_millis(50)),
            ..world.client()
        };
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();

        let err = client.call(&tell("lh-1", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't answer tell within 50ms; it may still be carrying it out, \
             so check before asking again, and check the broker with loopherd doctor"
        );
        let lock = lock_file(&world.paths, "lead-h");
        assert_eq!(broker_remote_timeout(&lock), Duration::from_secs(300));
    }

    #[test]
    fn a_report_that_timed_out_says_to_check_the_reports_before_reporting_again() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let client = Client {
            answer_within: Some(Duration::from_millis(50)),
            ..world.client()
        };
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();
        let report = Op::Report {
            kind: Kind::Note,
            text: "at step 3".into(),
        };

        let err = client.call(&report).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't answer report within 50ms; it may still be carrying it out, \
             so check loopherd reports --all lead-h before reporting again, and check the broker \
             with loopherd doctor"
        );
    }

    #[test]
    fn a_feed_the_broker_started_but_did_not_finish_in_time_may_still_be_running() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let client = Client {
            answer_within: Some(Duration::from_millis(300)),
            ..world.client()
        };

        let err = client.call(&feed("h")).unwrap_err();

        assert!(
            entered.try_recv().is_ok(),
            "the feed never reached start_claude"
        );
        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't answer feed within 300ms; it may still be carrying it out, \
             so check before asking again, and check the broker with loopherd doctor"
        );
        drop(release);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    /// A world whose herd h's queue holds PR 7, and a herdr whose `start_claude`
    /// signals `entered`, then blocks until `release` sends or drops.
    fn slow_feed() -> (World, FakeHerdr, Receiver<()>, Sender<()>) {
        let world = World::new();
        let herd_file = world.paths.herds.join("h.toml");
        let text = std::fs::read_to_string(&herd_file).unwrap();
        std::fs::write(
            &herd_file,
            text.replace("source = \"true\"", "source = \"echo 7\""),
        )
        .unwrap();
        world.add(member("lh-1", Some("h")));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
        let mut herdr = telling_herdr();
        herdr.workspaces = FakeHerdr::new(&[("w1", "main")]).workspaces;
        herdr.on_start = Some(Box::new(move || {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv();
        }));
        (world, herdr, entered, release)
    }

    /// Sends a feed from its own thread and waits until it reaches `start_claude`.
    fn feeding(world: &World, entered: &Receiver<()>) -> JoinHandle<Result<(String, String, i32)>> {
        let client = world.client();
        let fed = std::thread::spawn(move || client.call(&feed("h")));
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the feed never reached start_claude");
        fed
    }

    #[test]
    fn a_tell_is_answered_while_a_feed_runs() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let fed = feeding(&world, &entered);
        let client = Client {
            answer_within: Some(Duration::from_secs(5)),
            ..world.client()
        };

        let told = client.call(&tell("lh-1", "hi"));

        drop(release);
        assert_eq!(told.unwrap().2, 0);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        world.retire("lead-h");
        let (served, _) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
    }

    #[test]
    fn a_new_install_waits_for_the_feed_in_flight_and_a_tell_is_answered_meanwhile() {
        let (world, herdr, entered, release) = slow_feed();
        let installed = world.paths.state_dir().join("bin/loopherd");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "").unwrap();
        let tried = Mutex::new(vec![]);
        let reinstalled = AtomicBool::new(false);
        let installed_now = || {
            reinstalled
                .load(Ordering::SeqCst)
                .then(|| installed.clone())
        };
        let reexec = |exe: &Path, _: &str, _: &Lock| {
            tried.lock().unwrap().push(exe.to_path_buf());
            anyhow!("refused in this test")
        };
        let mut host = host(&herdr);
        host.follow = Some(Follow {
            installed: &installed_now,
            reexec: &reexec,
            every: Duration::from_millis(20),
            moved_onto: None,
        });
        let ledger = world.ledger();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&host, &ledger, &world.paths, "lead-h", POLL, None));
            // A failed assertion would otherwise leave the scope waiting on a broker that serves on.
            struct RetireOnDrop<'a>(&'a World);
            impl Drop for RetireOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.retire("lead-h");
                }
            }
            let _retire = RetireOnDrop(&world);
            while !running(&world.paths, "lead-h") {
                std::thread::sleep(POLL);
            }
            let fed = feeding(&world, &entered);
            reinstalled.store(true, Ordering::SeqCst);
            let client = Client {
                answer_within: Some(Duration::from_secs(5)),
                ..world.client()
            };

            std::thread::sleep(Duration::from_millis(200));
            let told = client.call(&tell("lh-1", "hi"));

            assert_eq!(told.unwrap().2, 0);
            assert!(tried.lock().unwrap().is_empty());
            drop(release);
            assert_eq!(fed.join().unwrap().unwrap().2, 0);
            let given_up = Instant::now() + Duration::from_secs(5);
            while tried.lock().unwrap().is_empty() {
                assert!(
                    Instant::now() < given_up,
                    "the broker never tried the install"
                );
                std::thread::sleep(POLL);
            }
            world.retire("lead-h");
            assert_eq!(serving.join().unwrap().unwrap(), Served::Retired);
        });
        assert_eq!(*tried.lock().unwrap(), vec![installed]);
    }

    #[test]
    fn a_second_feed_waits_for_the_first_and_times_out_not_started() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let fed = feeding(&world, &entered);
        let client = Client {
            answer_within: Some(Duration::from_millis(300)),
            ..world.client()
        };

        let second = client.call(&feed("h")).unwrap_err();
        let added = client.call(&add("h", "pr", &[])).unwrap_err();

        assert_eq!(
            format!("{second:#}"),
            "lead-h's broker didn't start feed within 300ms, so it was not started and is \
             withdrawn: the broker is busy with an earlier request; ask again later"
        );
        assert!(
            format!("{added:#}").contains("was not started"),
            "{added:#}"
        );
        drop(release);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
        // Only the first feed was marked started or answered: the withdrawn two never ran.
        let replies: Vec<_> = std::fs::read_dir(replies_dir(&world.paths, "lead-h"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            replies.iter().filter(|n| n.ends_with(".json")).count(),
            1,
            "{replies:?}"
        );
        assert_eq!(
            replies.iter().filter(|n| n.ends_with(".started")).count(),
            1,
            "{replies:?}"
        );
    }

    #[test]
    fn a_broker_whose_lead_retires_mid_feed_refuses_new_requests_then_stops() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let fed = feeding(&world, &entered);

        world.retire("lead-h");
        let told = world.client().call(&tell("lh-1", "hi")).unwrap_err();

        assert_eq!(
            format!("{told:#}"),
            "lead-h's broker refused tell: lead-h is retired, so its broker takes no new requests"
        );
        assert!(!serving.is_finished());
        drop(release);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        let (served, _) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
    }

    #[test]
    fn a_lead_relaunched_under_its_name_mid_feed_is_served_by_the_same_broker() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let fed = feeding(&world, &entered);
        world.retire("lead-h");
        let refused = world.client().call(&tell("lh-1", "hi")).unwrap_err();
        assert!(format!("{refused:#}").contains("is retired"), "{refused:#}");

        world.launch("lead-h", "h");
        let told = world.client().call(&tell("lh-1", "hi"));
        drop(release);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        let after_the_feed = world.client().call(&tell("lh-1", "again"));

        assert_eq!(told.unwrap().2, 0);
        assert_eq!(after_the_feed.unwrap().2, 0);
        assert!(!serving.is_finished());
        world.retire("lead-h");
        let (served, _) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
    }

    /// Hands the broker request `id` without waiting for its answer.
    fn hand_over(world: &World, id: &str, op: &Op) {
        let request = serde_json::to_vec(&Request { op: op.clone() }).unwrap();
        write_atomically(&requests_dir(&world.paths, "lead-h"), id, &request).unwrap();
    }

    fn reply_to(world: &World, id: &str) -> Option<Reply> {
        let text = std::fs::read(replies_dir(&world.paths, "lead-h").join(format!("{id}.json")));
        Some(serde_json::from_slice(&text.ok()?).unwrap())
    }

    #[test]
    fn a_lane_full_of_waiting_requests_refuses_the_next_as_busy() {
        let (world, herdr, entered, release) = slow_feed();
        let serving = world.serve(herdr);
        let fed = feeding(&world, &entered);
        // Named to sort before any client's id, so they queue first.
        let waiting: Vec<String> = (0..LANE_WAITING).map(|n| format!("0-{n:03}")).collect();
        for id in &waiting {
            hand_over(&world, id, &release_op("lh-9", false));
        }
        let client = Client {
            answer_within: Some(Duration::from_secs(5)),
            ..world.client()
        };

        let refused = client.call(&release_op("lh-9", false)).unwrap_err();

        assert_eq!(
            format!("{refused:#}"),
            format!(
                "lead-h's broker refused release: its queue for herd changes already holds \
                 {LANE_WAITING} waiting requests; ask again later"
            )
        );
        assert!(waiting.iter().all(|id| reply_to(&world, id).is_none()));
        drop(release);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
        assert!(waiting.iter().all(|id| reply_to(&world, id).is_some()));
    }

    #[test]
    fn a_broker_that_stopped_leaves_no_start_marks_for_an_older_broker_after_it() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
        // An older broker, which writes nothing to its lock.
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let client = Client {
            answer_within: Some(Duration::from_millis(50)),
            ..world.client()
        };

        let err = client.call(&tell("lh-1", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't answer tell within 50ms; it may still be carrying it out, \
             so check before asking again, and check the broker with loopherd doctor"
        );
    }

    #[test]
    fn a_broker_retiring_idle_answers_the_requests_still_waiting_as_retired() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let serving = world.serve_every(telling_herdr(), Duration::from_secs(1));
        let client = Client {
            answer_within: Some(Duration::from_secs(5)),
            ..world.client()
        };
        // Answered in a round, so the broker now sleeps out its poll.
        assert_eq!(client.call(&tell("lh-1", "hi")).unwrap().2, 0);

        hand_over(&world, "0-001", &tell("lh-1", "late"));
        world.retire("lead-h");

        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
        assert!(
            matches!(reply_to(&world, "0-001"), Some(Reply::Refused(why)) if why.contains("is retired"))
        );
    }

    type Entries = std::collections::BTreeMap<String, AgentRecord>;
    type NextRead = Box<dyn FnOnce(&FileLedger) -> Result<Entries> + Send>;

    /// The world's ledger, whose reads take the next of `reads` while any are left.
    #[derive(Clone)]
    struct Scripted {
        file: FileLedger,
        reads: Arc<Mutex<std::collections::VecDeque<NextRead>>>,
    }

    impl Scripted {
        fn new(world: &World, reads: Vec<NextRead>) -> Scripted {
            Scripted {
                file: world.ledger(),
                reads: Arc::new(Mutex::new(reads.into())),
            }
        }

        fn then(&self, reads: Vec<NextRead>) {
            self.reads.lock().unwrap().extend(reads);
        }
    }

    impl Ledger for Scripted {
        fn entries(&self) -> Result<Entries> {
            let next = self.reads.lock().unwrap().pop_front();
            next.map_or_else(|| self.file.entries(), |read| read(&self.file))
        }
        fn record(&mut self, record: AgentRecord) -> Result<()> {
            self.file.record(record)
        }
        fn update(
            &mut self,
            name: &str,
            change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.file.update(name, change)
        }
        fn remove_herd(
            &mut self,
            herd: &str,
            before: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<crate::ledger::RemovedHerd> {
            self.file.remove_herd(herd, before)
        }
        fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()> {
            self.file.started(name, at, resumed)
        }
    }

    fn lead_retired(file: &FileLedger) -> Result<Entries> {
        let mut entries = file.entries()?;
        entries.get_mut("lead-h").unwrap().retired = true;
        Ok(entries)
    }

    fn answer_to(world: &World, id: &str) -> Reply {
        let given_up = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reply) = reply_to(world, id) {
                return reply;
            }
            assert!(Instant::now() < given_up, "{id} was never answered");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn a_retire_landing_between_the_round_s_read_and_its_listing_refuses_the_request() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();
        hand_over(&world, "0-001", &tell("lh-1", "hi"));
        let readers = Arc::new(Mutex::new(vec![]));
        let (round, listing) = (readers.clone(), readers.clone());
        let ledger = Scripted::new(
            &world,
            vec![
                Box::new(move |file| {
                    round.lock().unwrap().push(std::thread::current().id());
                    file.entries()
                }),
                Box::new(move |file| {
                    listing.lock().unwrap().push(std::thread::current().id());
                    lead_retired(file)
                }),
            ],
        );

        let serving = world.serve_on(telling_herdr(), POLL, ledger);

        let answer = answer_to(&world, "0-001");
        assert!(
            matches!(&answer, Reply::Refused(why) if why == "lead-h is retired, so its broker takes no new requests"),
            "{answer:?}"
        );
        let readers = readers.lock().unwrap().clone();
        assert_eq!(
            readers[0], readers[1],
            "the retirement was read by a lane, not the listing"
        );
        world.retire("lead-h");
        let (served, herdr) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_relaunch_landing_during_the_final_scan_leaves_its_request_for_the_next_broker() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();
        hand_over(&world, "0-001", &tell("lh-1", "hi"));
        // The round reads retired, and so does the reread after clearing; the relaunch lands next.
        let ledger = Scripted::new(&world, vec![Box::new(lead_retired), Box::new(lead_retired)]);

        let (served, herdr) = world
            .serve_on(telling_herdr(), POLL, ledger)
            .join()
            .unwrap();

        assert_eq!(served.unwrap(), Served::Retired);
        assert!(reply_to(&world, "0-001").is_none());
        assert!(
            requests_dir(&world.paths, "lead-h")
                .join("0-001.json")
                .exists()
        );
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_reread_that_fails_keeps_the_round_s_read_that_the_lead_is_retired() {
        let (world, herdr, entered, release) = slow_feed();
        let ledger = Scripted::new(&world, vec![]);
        let serving = world.serve_on(herdr, POLL, ledger.clone());
        let fed = feeding(&world, &entered);
        let (written, handed_over) = mpsc::channel::<()>();
        // The next round's read waits for the request, so that round lists it and rereads.
        ledger.then(vec![
            Box::new(move |file| {
                handed_over.recv().unwrap();
                lead_retired(file)
            }),
            Box::new(|_| Err(anyhow!("ledger unreadable"))),
        ]);

        hand_over(&world, "0-001", &tell("lh-1", "hi"));
        written.send(()).unwrap();

        let answer = answer_to(&world, "0-001");
        assert!(
            matches!(&answer, Reply::Refused(why) if why == "lead-h is retired, so its broker takes no new requests"),
            "{answer:?}"
        );
        drop(release);
        assert_eq!(fed.join().unwrap().unwrap().2, 0);
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn starting_a_broker_while_the_last_one_exits_waits_for_it_and_starts_one() {
        let world = World::new();
        // An exiting broker: it still holds its lock, with its record cleared.
        let exiting = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let exited = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(exiting);
        });
        let mut spawned = 0;

        let started = start_with(&world.paths, "lead-h", START_TIMEOUT, &mut || {
            spawned += 1;
            Ok(())
        });

        assert!(started.unwrap());
        assert_eq!(spawned, 1);
        exited.join().unwrap();
    }

    #[test]
    fn a_lock_held_with_no_record_past_the_wait_fails_the_start_naming_its_holder() {
        let world = World::new();
        // A broker that never records, as one older than the lock's record does.
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(
            pid_file(&world.paths, "lead-h"),
            std::process::id().to_string(),
        )
        .unwrap();
        let mut spawned = 0;

        let started = start_with(
            &world.paths,
            "lead-h",
            Duration::from_millis(50),
            &mut || {
                spawned += 1;
                Ok(())
            },
        );

        let why = format!("{:#}", started.unwrap_err());
        assert!(
            why.contains(&format!("pid {}", std::process::id())),
            "{why}"
        );
        assert!(why.contains("loopherd broker-restart lead-h"), "{why}");
        assert_eq!(spawned, 0);
    }

    #[test]
    fn a_lock_held_with_no_record_or_pid_past_the_wait_says_how_to_find_its_holder() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();

        let started = start_with(
            &world.paths,
            "lead-h",
            Duration::from_millis(50),
            &mut || Ok(()),
        );

        let why = format!("{:#}", started.unwrap_err());
        assert!(why.contains("recorded no pid"), "{why}");
        assert!(why.contains("pgrep -f 'loopherd broker lead-h'"), "{why}");
    }

    #[test]
    fn starting_a_broker_while_one_serves_starts_none() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));
        let mut spawned = 0;

        let started = start_with(&world.paths, "lead-h", START_TIMEOUT, &mut || {
            spawned += 1;
            Ok(())
        });

        assert!(!started.unwrap());
        assert_eq!(spawned, 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_broker_older_than_its_lead_s_loopherd_is_named_with_how_to_restart_it() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let requests = requests_dir(&world.paths, "lead-h");
        let replies = replies_dir(&world.paths, "lead-h");
        std::fs::create_dir_all(&requests).unwrap();
        std::fs::create_dir_all(&replies).unwrap();
        // An older broker, which has no prompt op.
        let old = std::thread::spawn(move || {
            loop {
                let name = std::fs::read_dir(&requests)
                    .unwrap()
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .find(|n| n.ends_with(".json"));
                if let Some(name) = name {
                    let why = "malformed request: unknown variant `prompt`, expected `tell`";
                    let reply = serde_json::to_vec(&Reply::Refused(why.into())).unwrap();
                    write_atomically(&replies, name.trim_end_matches(".json"), &reply).unwrap();
                    return;
                }
                std::thread::sleep(POLL);
            }
        });
        let prompt = Op::Prompt {
            name: "lh-1".into(),
            force: false,
        };

        let err = world.client().call(&prompt).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker predates this loopherd, so it can't prompt; the shepherd must \
             restart it with loopherd broker-restart lead-h"
        );
        assert!(err.downcast_ref::<TooOld>().is_some());
        old.join().unwrap();
    }

    #[test]
    fn a_reply_the_client_cannot_read_names_a_stale_broker_and_its_restart() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let requests = requests_dir(&world.paths, "lead-h");
        let replies = replies_dir(&world.paths, "lead-h");
        std::fs::create_dir_all(&requests).unwrap();
        std::fs::create_dir_all(&replies).unwrap();
        // An older broker, answering in a form this loopherd doesn't know.
        let old = std::thread::spawn(move || {
            loop {
                let name = std::fs::read_dir(&requests)
                    .unwrap()
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .find(|n| n.ends_with(".json"));
                if let Some(name) = name {
                    let reply = br#"{"ok":"sent"}"#;
                    write_atomically(&replies, name.trim_end_matches(".json"), reply).unwrap();
                    return;
                }
                std::thread::sleep(POLL);
            }
        });

        let err = world.client().call(&tell("lh-1", "hi")).unwrap_err();

        assert!(
            format!("{err:#}").starts_with(
                "lead-h's broker answered tell in a form this loopherd can't read, so it may run \
                 an older loopherd; the shepherd can restart it with loopherd broker-restart lead-h: "
            ),
            "{err:#}"
        );
        old.join().unwrap();
    }

    #[test]
    fn a_broker_starting_while_its_lock_is_probed_still_starts() {
        let world = World::new();
        let probing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe = {
            let (paths, probing) = (world.paths.clone(), probing.clone());
            std::thread::spawn(move || {
                while probing.load(std::sync::atomic::Ordering::Relaxed) {
                    running(&paths, "lead-h");
                }
            })
        };

        let serving = world.serve(FakeHerdr::new(&[]));

        probing.store(false, std::sync::atomic::Ordering::Relaxed);
        probe.join().unwrap();
        assert!(!serving.is_finished());
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    fn feed(herd: &str) -> Op {
        Op::Feed {
            herd: herd.into(),
            dry_run: false,
            ready_timeout_secs: 60,
        }
    }

    fn add(herd: &str, template: &str, claude_args: &[&str]) -> Op {
        Op::Add {
            herd: herd.into(),
            template: template.into(),
            params: vec!["7".into()],
            claude_args: claude_args.iter().map(|a| a.to_string()).collect(),
            dry_run: true,
            ready_timeout_secs: 60,
            no_focus: false,
        }
    }

    #[test]
    fn feed_and_add_outside_the_herd_file_are_refused_naming_why() {
        let world = World::new();
        world.herd("g");
        let serving = world.serve(FakeHerdr::new(&[]));
        let other = "herd g isn't lead-h's; lead-h reaches only herd h";

        for (op, why) in [
            (feed("g"), other.to_string()),
            (add("g", "pr", &[]), other.to_string()),
            (
                add("h", "evil", &[]),
                "herd h has no template evil; lead-h adds only its herd's templates".into(),
            ),
            (
                Op::Feed {
                    herd: "h".into(),
                    dry_run: false,
                    ready_timeout_secs: u64::MAX,
                },
                format!(
                    "a ready timeout of {}s is over the 3600s a lead may ask for",
                    u64::MAX
                ),
            ),
            (
                add("h", "pr", &["--dangerously-skip-permissions"]),
                "a worker's claude args come only from herd h's file; lead-h adds without its own"
                    .into(),
            ),
        ] {
            let err = world.client().call(&op).unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!("lead-h's broker refused {}: {why}", op.command())
            );
        }

        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    /// Points herd h at a real git repo outside the state dir whose `[template.b]`
    /// checks out branch `{n}`; the repo lasts as long as the dir.
    fn branch_herd(world: &World) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let text = format!(
            "repo = {:?}\n\
             [template.b]\nparams = [\"n\"]\nname = \"b-{{n}}\"\nbranch = \"{{n}}\"\nprompt = \"go\"\n\
             [template.same]\nparams = [\"n\"]\nname = \"{{n}}\"\nbranch = \"{{n}}\"\nprompt = \"go\"\n\
             [queue]\ntemplate = \"b\"\nsource = \"echo lead-h\"\nmax = 2\n\
             [lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
            repo.to_str().unwrap()
        );
        std::fs::write(world.paths.herds.join("h.toml"), &text).unwrap();
        // Herd g shares the repo, so its lead's worktree sits beside h's.
        std::fs::write(world.paths.herds.join("g.toml"), &text).unwrap();
        world.launch("lead-g", "g");
        (root, repo)
    }

    fn add_branch(n: &str, dry_run: bool) -> Op {
        Op::Add {
            herd: "h".into(),
            template: "b".into(),
            params: vec![n.into()],
            claude_args: vec![],
            dry_run,
            ready_timeout_secs: 60,
            no_focus: false,
        }
    }

    #[test]
    fn a_lead_s_add_is_refused_a_worktree_the_guard_refuses_any_member() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("outside");
        crate::worktree::testing::git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "made",
                outside.to_str().unwrap(),
            ],
        )
        .unwrap();
        let lead_worktree = crate::worktree::worktree_path(&repo, "lead-h");
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        for (n, why) in [
            (
                "lead-h",
                format!(
                    "adding b-lead-h names lead lead-h's worktree {}",
                    lead_worktree.display()
                ),
            ),
            (
                "made",
                format!(
                    "adding b-made names worktree {}, which isn't beside herd h's \
                     worktrees in {}",
                    outside.display(),
                    lead_worktree.parent().unwrap().display()
                ),
            ),
        ] {
            for dry_run in [true, false] {
                let err = world.client().call(&add_branch(n, dry_run)).unwrap_err();
                assert!(format!("{err:#}").ends_with(&why), "{n}: {err:#}");
            }
        }

        let same = Op::Add {
            herd: "h".into(),
            template: "same".into(),
            params: vec!["lead-g".into()],
            claude_args: vec![],
            dry_run: true,
            ready_timeout_secs: 60,
            no_focus: false,
        };
        let err = world.client().call(&same).unwrap_err();
        let lead_g = crate::worktree::worktree_path(&repo, "lead-g");
        let why = format!(
            "adding lead-g names lead lead-g's worktree {}",
            lead_g.display()
        );
        assert!(format!("{err:#}").ends_with(&why), "{err:#}");

        let (out, _, code) = world.client().call(&add_branch("fine", true)).unwrap();
        assert_eq!(code, 0);
        assert!(out.lines().any(|l| l == "name b-fine"), "{out}");
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.tabs().is_empty());
    }

    /// Checks out `branch` at its usual place beside the repo's other worktrees.
    fn checked_out(repo: &Path, branch: &str) -> PathBuf {
        let path = crate::worktree::worktree_path(repo, branch);
        crate::worktree::testing::git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        )
        .unwrap();
        path
    }

    fn worker_at(name: &str, herd: &str, repo: &Path, worktree: &Path) -> AgentRecord {
        let mut worker = member(name, Some(herd));
        worker.repo = repo.into();
        worker.worktree = Some(worktree.into());
        worker
    }

    #[test]
    fn a_lead_s_add_reuses_only_a_worktree_its_own_herd_s_ledger_records() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let human_s = checked_out(&repo, "mine");
        let g_s = checked_out(&repo, "theirs");
        let mut g_retired = worker_at("g-theirs", "g", &repo, &g_s);
        g_retired.retired = true;
        world.add(g_retired);
        let own = checked_out(&repo, "ours");
        world.add(worker_at("b-ours", "h", &repo, &own));
        let released = checked_out(&repo, "kept");
        let mut retired = worker_at("b-kept", "h", &repo, &released);
        retired.retired = true;
        world.add(retired);
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        for (n, worktree) in [("mine", &human_s), ("theirs", &g_s)] {
            for dry_run in [true, false] {
                let err = world.client().call(&add_branch(n, dry_run)).unwrap_err();
                let why = format!(
                    "adding b-{n} would reuse worktree {}, which no ledger entry \
                     of herd h records",
                    worktree.display()
                );
                assert!(format!("{err:#}").ends_with(&why), "{n}: {err:#}");
            }
        }
        for n in ["ours", "kept"] {
            let (out, _, code) = world.client().call(&add_branch(n, true)).unwrap();

            assert_eq!(code, 0, "{n}: {out}");
            assert!(out.lines().any(|l| l == format!("name b-{n}")), "{out}");
        }
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.tabs().is_empty());
    }

    /// Herd h's retired `b-earlier` and herd g's live `g-shared` both record
    /// the worktree of branch `shared`, which feeding h would start.
    fn shared_with_g(world: &World, repo: &Path) -> PathBuf {
        let herd_file = world.paths.herds.join("h.toml");
        let text = std::fs::read_to_string(&herd_file).unwrap();
        std::fs::write(&herd_file, text.replace("echo lead-h", "echo shared")).unwrap();
        let shared = checked_out(repo, "shared");
        let mut retired = worker_at("b-earlier", "h", repo, &shared);
        retired.retired = true;
        world.add(retired);
        world.add(worker_at("g-shared", "g", repo, &shared));
        shared
    }

    #[test]
    fn a_lead_s_add_and_feed_won_t_reuse_a_worktree_another_herd_s_live_worker_holds() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let shared = shared_with_g(&world, &repo);
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));
        let why = format!(
            "adding b-shared would reuse worktree {}, which herd g's g-shared holds",
            shared.display()
        );

        let err = world
            .client()
            .call(&add_branch("shared", true))
            .unwrap_err();
        let (out, _, code) = world.client().call(&feed("h")).unwrap();

        assert!(format!("{err:#}").ends_with(&why), "{err:#}");
        assert_eq!(code, 1, "{out}");
        let refused = format!("error: shared: refused: {why}");
        assert!(out.lines().any(|l| l == refused), "{out}");
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn a_lead_s_feed_won_t_start_a_task_in_a_worktree_the_human_or_another_herd_checked_out() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let herd_file = world.paths.herds.join("h.toml");
        let text = std::fs::read_to_string(&herd_file).unwrap();
        let source = text.replace("echo lead-h", "echo mine; echo theirs");
        std::fs::write(&herd_file, source).unwrap();
        let human_s = checked_out(&repo, "mine");
        let g_s = checked_out(&repo, "theirs");
        let mut g_retired = worker_at("g-theirs", "g", &repo, &g_s);
        g_retired.retired = true;
        world.add(g_retired);
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        let (out, _, code) = world.client().call(&feed("h")).unwrap();

        assert_eq!(code, 1, "{out}");
        for (n, worktree) in [("mine", &human_s), ("theirs", &g_s)] {
            let refused = format!(
                "error: {n}: refused: adding b-{n} would reuse worktree {}, which no \
                 ledger entry of herd h records",
                worktree.display()
            );
            assert!(out.lines().any(|l| l == refused), "{n}: {out}");
        }
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.tabs().is_empty());
    }

    /// Points herd h's repo at `repo`, which need not be a git repo, with a
    /// `[template.b]` on branch `{n}`.
    fn herd_at(world: &World, repo: &Path) {
        let text = format!(
            "repo = {:?}\n\
             [template.b]\nparams = [\"n\"]\nname = \"b-{{n}}\"\nbranch = \"{{n}}\"\nprompt = \"go\"\n\
             [queue]\ntemplate = \"b\"\nsource = \"true\"\nmax = 2\n\
             [lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
            repo.to_str().unwrap()
        );
        std::fs::write(world.paths.herds.join("h.toml"), text).unwrap();
    }

    #[test]
    fn a_lead_s_add_to_a_herd_whose_repo_a_lead_can_write_is_refused_before_git_runs() {
        let world = World::new();
        let state = world.paths.state_dir().to_path_buf();
        let (_root, repo) = branch_herd(&world);
        let lead_g = crate::worktree::worktree_path(&repo, "lead-g");
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        // Neither is a git repo, so git listing their worktrees would fail first.
        for (writable, inside) in [(state.join("repo"), state), (lead_g.join("repo"), lead_g)] {
            std::fs::create_dir_all(&writable).unwrap();
            herd_at(&world, &writable);

            let err = world.client().call(&add_branch("x", true)).unwrap_err();

            let why = format!(
                "adding b-x names repo {}, inside {}, which a lead can write",
                writable.display(),
                inside.display()
            );
            assert!(format!("{err:#}").ends_with(&why), "{err:#}");
        }
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    /// Adds herd h a `[template.d]` with a detached worktree slugged `{n}`,
    /// and returns the dry-run add of it with `n`.
    fn add_detached(world: &World, n: &str) -> Op {
        let herd_file = world.paths.herds.join("h.toml");
        let mut text = std::fs::read_to_string(&herd_file).unwrap();
        text.push_str(
            "[template.d]\nparams = [\"n\"]\nname = \"d-{n}\"\nworktree = \"{n}\"\nprompt = \"go\"\n",
        );
        std::fs::write(&herd_file, text).unwrap();
        Op::Add {
            herd: "h".into(),
            template: "d".into(),
            params: vec![n.into()],
            claude_args: vec![],
            dry_run: true,
            ready_timeout_secs: 60,
            no_focus: false,
        }
    }

    #[test]
    fn a_lead_s_add_of_a_detached_worktree_named_like_its_lead_is_refused() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let detached = add_detached(&world, "lead-h");
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        let err = world.client().call(&detached).unwrap_err();

        let lead_worktree = crate::worktree::worktree_path(&repo, "lead-h");
        let why = format!(
            "adding d-lead-h names lead lead-h's worktree {}",
            lead_worktree.display()
        );
        assert!(format!("{err:#}").ends_with(&why), "{err:#}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_s_add_of_a_detached_worktree_won_t_reuse_one_the_human_checked_out() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let human_s = checked_out(&repo, "mine");
        let detached = add_detached(&world, "mine");
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        let err = world.client().call(&detached).unwrap_err();

        let why = format!(
            "adding d-mine would reuse worktree {}, which no ledger entry of herd h records",
            human_s.display()
        );
        assert!(format!("{err:#}").ends_with(&why), "{err:#}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_s_add_won_t_reuse_a_worktree_a_live_entry_outside_its_herd_names_any_way() {
        let links = tempfile::tempdir().unwrap();
        for (holder, herd, recorded) in [
            ("lh-loose", None, "as is"),
            ("g-shared", Some("g"), "through a symlink"),
            ("g-shared", Some("g"), "with a trailing slash"),
        ] {
            let world = World::new();
            let (_root, repo) = branch_herd(&world);
            let shared = checked_out(&repo, "shared");
            let mut retired = worker_at("b-earlier", "h", &repo, &shared);
            retired.retired = true;
            world.add(retired);
            let named = match recorded {
                "through a symlink" => {
                    let link = links.path().join("shared");
                    std::os::unix::fs::symlink(&shared, &link).unwrap();
                    link
                }
                "with a trailing slash" => PathBuf::from(format!("{}/", shared.display())),
                _ => shared.clone(),
            };
            let mut live = worker_at(holder, "g", &repo, &named);
            live.herd = herd.map(Into::into);
            world.add(live);
            let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

            let err = world
                .client()
                .call(&add_branch("shared", true))
                .unwrap_err();

            let of = herd.map_or("no herd's".into(), |h| format!("herd {h}'s"));
            let why = format!(
                "adding b-shared would reuse worktree {}, which {of} {holder} holds",
                shared.display()
            );
            assert!(format!("{err:#}").ends_with(&why), "{recorded}: {err:#}");
            world.retire("lead-h");
            serving.join().unwrap().0.unwrap();
        }
    }

    #[test]
    fn a_lead_s_feed_reports_a_task_whose_worktree_the_guard_refuses_and_starts_nothing() {
        let world = World::new();
        let (_root, repo) = branch_herd(&world);
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));

        let (out, _, code) = world.client().call(&feed("h")).unwrap();

        let lead_worktree = crate::worktree::worktree_path(&repo, "lead-h");
        assert_eq!(code, 1, "{out}");
        assert!(
            out.lines().any(|l| l
                == format!(
                    "error: lead-h: refused: adding b-lead-h names lead lead-h's worktree {}",
                    lead_worktree.display()
                )),
            "{out}"
        );
        assert!(!lead_worktree.exists());
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_feed_and_add_go_through_its_broker_and_call_no_herdr() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[("w1", "main")]));
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");
        assert!(client.is_some());

        let (fed, fed_out) = run_here(&own, &world, client.as_ref(), &feed("h")).unwrap();
        let (added, add_out) =
            run_here(&own, &world, client.as_ref(), &add("h", "pr", &[])).unwrap();

        assert_eq!((fed, added), (0, 0));
        assert_eq!(fed_out.out, ["h: 0 retired, 0 added, 0 skipped, 0 errors"]);
        assert!(add_out.out[0].contains("name pr-7"), "{:?}", add_out.out);
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert!(herdr.calls() > 0);
    }

    #[test]
    fn in_a_sandboxed_lead_s_pane_prompt_and_restart_go_through_its_broker_and_call_no_herdr() {
        let world = World::new();
        let mut unprompted = member("lh-1", Some("h"));
        unprompted.prompt = Some("go".into());
        unprompted.prompt_sent = false;
        world.add(unprompted);
        let serving = world.serve(telling_herdr());
        let own = FakeHerdr::new(&[]);
        let client = world.route_from("p-lead-h");
        let restart = Op::Restart {
            name: "lh-1".into(),
            force: false,
        };
        let prompt = Op::Prompt {
            name: "lh-1".into(),
            force: false,
        };

        let (prompted, kept) = run_here(&own, &world, client.as_ref(), &prompt).unwrap();
        let err = run_here(&own, &world, client.as_ref(), &restart)
            .map(|_| ())
            .unwrap_err();

        assert_eq!((prompted, kept.out), (0, vec!["prompted lh-1".to_string()]));
        assert!(world.ledger().entries().unwrap()["lh-1"].prompt_sent);
        // The broker's restart got as far as the entry, which has no socket to follow.
        assert!(
            format!("{err:#}").contains("lh-1 was recorded without a herdr socket"),
            "{err:#}"
        );
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.prompts(), [("lh-1".to_string(), "go".to_string())]);
    }

    /// Gives `name` a handoff file and a handoff report, as before `restart --self`.
    fn handed_off(world: &World, name: &str) {
        let state = world.paths.state_dir();
        std::fs::create_dir_all(crate::handoff::dir(state, name)).unwrap();
        std::fs::write(crate::handoff::path(state, name), "next").unwrap();
        let handoff = crate::report::Entry::new(
            SystemTime::now(),
            crate::report::Body::Handoff { text: "h".into() },
        );
        crate::report::append(&state.join("reports"), name, &handoff).unwrap();
    }

    #[test]
    fn a_sandboxed_lead_s_restart_self_is_scheduled_by_its_broker_for_its_own_pane() {
        let world = World::new();
        let client = world.route_from("p-lead-h");
        handed_off(&world, "lead-h");
        let serving = world.serve(FakeHerdr::new(&[]));
        let own = FakeHerdr::new(&[]);

        let (code, kept) = run_here(&own, &world, client.as_ref(), &Op::RestartSelf).unwrap();

        assert_eq!(code, 0);
        assert!(
            kept.out[0].starts_with("lead-h restarts in this pane"),
            "{:?}",
            kept.out
        );
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_stale_entry_on_the_lead_s_pane_doesn_t_stop_its_restart_self() {
        let world = World::new();
        let client = world.route_from("p-lead-h");
        // A worker's stale entry, sorting first, on a reused pane id.
        let mut stale = member("a-fix", Some("h"));
        stale.pane_id = "p-lead-h".into();
        stale.socket = Some(crate::herdr::fake::FAKE_SOCKET.into());
        world.add(stale);
        handed_off(&world, "lead-h");
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, _, code) = client.unwrap().call(&Op::RestartSelf).unwrap();

        assert_eq!(code, 0);
        assert!(out.starts_with("lead-h restarts in this pane"), "{out}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn restart_self_is_refused_when_the_lead_s_record_names_another_herdr() {
        let world = World::new();
        let client = world.route_from("p-lead-h");
        handed_off(&world, "lead-h");
        let serving = world.serve(FakeHerdr::new(&[]));
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some("/elsewhere.sock".into());
                Ok(())
            })
            .unwrap();

        let err = client.unwrap().call(&Op::RestartSelf).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h runs in another herdr than its broker's"
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn in_a_sandbox_a_pane_the_ledger_places_no_lead_in_is_refused_before_herdr() {
        if is_root() {
            return;
        }
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let own = FakeHerdr::new(&[("w1", "main")]);
        // herdr renamed the lead's pane; its processes keep the old id.
        let client = world.route_from("w1:p-moved");
        assert!(client.is_none());

        let refusals = world.read_only(|| {
            let ops = [
                tell("lh-1", "hi"),
                feed("h"),
                add("h", "pr", &[]),
                Op::RestartSelf,
                retire_op("lh-1", false, false),
                release_op("lh-1", false),
            ];
            let mut refusals: Vec<String> = ops
                .iter()
                .map(|op| format!("{:#}", run_here(&own, &world, None, op).err().unwrap()))
                .collect();
            let spawn = refuse_spawn(None, &world.ledger()).unwrap_err();
            refusals.push(format!("{spawn:#}"));
            refusals
        });

        for (refusal, command) in refusals.iter().zip([
            "tell",
            "feed",
            "add",
            "restart --self",
            "retire",
            "release",
            "spawn",
        ]) {
            assert!(
                refusal.starts_with(&format!(
                    "refused: this loopherd can't write the ledger (a lead's sandbox, or \
                     permissions on {}), so it can't run loopherd {command} itself",
                    world.ledger().dir().unwrap().display()
                )),
                "{refusal}"
            );
        }
        assert_eq!(own.calls(), 0);
    }

    #[test]
    fn a_lead_s_spawn_is_refused_and_the_shepherd_s_passes() {
        let world = World::new();
        let lead = world.route_from("p-lead-h");
        let shepherd = world.route_from("w1:p-shepherd");

        let refused = refuse_spawn(lead.as_ref(), &world.ledger()).unwrap_err();

        assert_eq!(
            format!("{refused:#}"),
            "a lead can't run loopherd spawn: add and feed start its herd's workers from its herd file"
        );
        assert!(shepherd.is_none());
        refuse_spawn(shepherd.as_ref(), &world.ledger()).unwrap();
    }

    #[test]
    fn a_broker_starts_workers_where_its_lead_is_and_gates_after_merge_by_the_launch_record() {
        let world = World::new();
        let repo = world.repos.path().join("h");
        let h = format!(
            "repo = {repo:?}\nargs = [\"--model\", \"opus\"]\nafter_merge = [\"scripts/install\"]\n\
             [template.pr]\nparams = [\"number\"]\nname = \"pr-{{number}}\"\nprompt = \"review {{number}}\"\n\
             merged = \"test -f merged-{{number}}\"\n\
             [queue]\ntemplate = \"pr\"\nsource = \"cat tasks\"\nmax = 5\n\
             [lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n"
        );
        std::fs::write(world.paths.herds.join("h.toml"), h).unwrap();
        // The ledger, which a lead could write, names a herd whose lead may install.
        world.herd("g");
        let g = world.paths.herds.join("g.toml");
        let text = std::fs::read_to_string(&g).unwrap();
        std::fs::write(&g, format!("{text}install = true\n")).unwrap();
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.herd = Some("g".into());
                r.workspace = "w2".into();
                r.socket = Some(crate::herdr::fake::FAKE_SOCKET.into());
                Ok(())
            })
            .unwrap();
        std::fs::write(repo.join("tasks"), "1\n").unwrap();
        let serving = world.serve(FakeHerdr::new(&[("w1", "main"), ("w2", "other")]));
        let client = world.client();
        let add = Op::Add {
            herd: "h".into(),
            template: "pr".into(),
            params: vec!["7".into()],
            claude_args: vec![],
            dry_run: false,
            ready_timeout_secs: 60,
            no_focus: false,
        };

        let (_, _, fed) = client.call(&feed("h")).unwrap();
        let (_, _, added) = client.call(&add).unwrap();
        std::fs::write(repo.join("merged-1"), "").unwrap();
        let (out, _, _) = client.call(&feed("h")).unwrap();

        assert_eq!((fed, added), (0, 0));
        assert!(
            out.contains(
                "after_merge pr-1: not run: `scripts/install` needs the install switch, off in h's [lead]"
            ),
            "{out}"
        );
        let entries = world.ledger().entries().unwrap();
        assert_eq!(entries["pr-7"].herd.as_deref(), Some("h"));
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        let started = herdr.started();
        assert_eq!(
            started.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["pr-1", "pr-7"]
        );
        for worker in &started {
            assert!(worker.pane_id.starts_with("w2:"), "{}", worker.pane_id);
            assert_eq!(worker.args, ["--model", "opus"]);
        }
    }

    /// A ledger that panics on recording a new member.
    struct PanicsOnRecord(crate::ledger::MemLedger);

    impl Ledger for PanicsOnRecord {
        fn entries(&self) -> Result<std::collections::BTreeMap<String, AgentRecord>> {
            self.0.entries()
        }
        fn record(&mut self, _: AgentRecord) -> Result<()> {
            panic!("record");
        }
        fn update(
            &mut self,
            name: &str,
            change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.0.update(name, change)
        }
        fn remove_herd(
            &mut self,
            herd: &str,
            before: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<crate::ledger::RemovedHerd> {
            self.0.remove_herd(herd, before)
        }
        fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()> {
            self.0.started(name, at, resumed)
        }
    }

    #[test]
    fn a_feed_that_panics_keeps_the_lines_noting_its_after_merge_interrupted() {
        let world = World::new();
        let repo = world.repos.path().join("h");
        let h = std::fs::read_to_string(world.paths.herds.join("h.toml")).unwrap();
        let h = h.replace("source = \"true\"", "source = \"echo 1\"");
        std::fs::write(world.paths.herds.join("h.toml"), h).unwrap();
        let mut pending = member("lh-1", Some("h"));
        pending.repo = repo;
        pending.after_merge = Some(crate::ledger::AfterMerge::Pending { owner: me() });
        let mut ledger = PanicsOnRecord(crate::ledger::MemLedger::default());
        ledger.0.0.insert("lh-1".into(), pending);
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let mut ctx = Ctx {
            herdr: &herdr,
            ledger: &mut ledger,
            paths: &world.paths,
            procs: &crate::process::ProcFs,
            claude_dir: None,
            caller: Caller {
                pane: None,
                socket: None,
                lineage: vec![],
                workspace: Some("w1".into()),
                process: me(),
                lead: None,
            },
            live: same_session,
            spawn_waiter: no_waiter,
            prs: &NO_PRS,
        };
        let mut kept = Kept::default();

        let err = feed("h").carry_out(&mut ctx, &mut kept).unwrap_err();

        assert_eq!(format!("{err:#}"), "the feed panicked: record");
        assert_eq!(kept.err, ["after_merge lh-1: interrupted"]);
        assert_eq!(
            ledger.0.0["lh-1"].after_merge,
            Some(crate::ledger::AfterMerge::Noted)
        );
    }

    #[test]
    fn a_folder_lists_its_ready_entries_in_order_each_time_it_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.json", "a.json", ".c.part"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let opened = Dir::open(dir.path(), &[]).unwrap();

        let first = opened.names();
        let second = opened.names();

        assert_eq!(first, ["a.json", "b.json"]);
        assert_eq!(second, first);
    }

    /// Herd `c` on a GitHub repo, whose lead `lead-c` guards merges by
    /// credentials, with its deploy key and `token`, if any.
    fn credentials_herd(world: &World, token: Option<&str>) {
        use crate::worktree::testing::git;
        let repo = world.repos.path().join("c");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]).unwrap();
        let url = "https://github.com/acme/widgets.git";
        git(&repo, &["remote", "add", "origin", url]).unwrap();
        let text = format!(
            "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
             [lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n",
            repo.to_str().unwrap()
        );
        std::fs::write(world.paths.herds.join("c.toml"), text).unwrap();
        let sandbox = world.paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(
            sandbox.join("lead-c.lead.json"),
            r#"{"herd": "c", "args": [], "path": "/usr/bin:/bin"}"#,
        )
        .unwrap();
        let files = crate::credentials::files(&world.paths, "c");
        std::fs::create_dir_all(files.key.parent().unwrap()).unwrap();
        std::fs::write(&files.key, "a private key\n").unwrap();
        if let Some(token) = token {
            std::fs::write(&files.token, token).unwrap();
        }
        crate::credentials::write_host_block(&world.paths, "c").unwrap();
    }

    #[test]
    fn a_credentials_herds_broker_takes_on_its_leads_token_and_drops_the_ssh_agent() {
        let world = World::new();
        credentials_herd(&world, Some("github_pat_1\n"));

        let env = lead_env(&world.paths, "lead-c").unwrap();

        let var = |name: &str| {
            env.iter()
                .find(|(n, _)| n == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(var("PATH"), Some("/usr/bin:/bin"));
        assert_eq!(var("GH_TOKEN"), Some("github_pat_1"));
        assert_eq!(var("SSH_AUTH_SOCK"), Some(""));
        assert_eq!(var("GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(var("GIT_CONFIG_VALUE_0"), Some(""));
        let ssh_config = world.paths.config_dir().join("ssh/config");
        assert_eq!(
            var("GIT_SSH_COMMAND"),
            Some(
                format!(
                    "ssh -F '{}' -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes",
                    ssh_config.display()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn a_hook_guarded_leads_broker_takes_on_only_its_recorded_path() {
        let world = World::new();
        let kept = sandbox::Lead::kept(&world.paths, "lead-h")
            .unwrap()
            .unwrap();
        let recorded = kept.path.expect("launching records the PATH");

        assert_eq!(
            lead_env(&world.paths, "lead-h").unwrap(),
            [("PATH".to_string(), recorded)]
        );
    }

    #[test]
    fn an_unrecorded_lead_or_a_record_without_a_path_keeps_its_starters_environment() {
        let world = World::new();
        let file = world.paths.config_dir().join("sandbox/lead-h.lead.json");
        std::fs::write(&file, r#"{"herd": "h", "args": []}"#).unwrap();

        assert_eq!(lead_env(&world.paths, "lead-h").unwrap(), []);
        assert_eq!(lead_env(&world.paths, "lead-x").unwrap(), []);
    }

    #[test]
    fn a_lead_waits_on_retire_and_release_for_eighteen_remote_calls_and_room_for_herdr() {
        let secs = |s| Duration::from_secs(s);

        assert_eq!(origin_wait(secs(120)), secs(18 * 120 + 300));
        assert_eq!(origin_wait(secs(3600)), secs(18 * 3600 + 300));
    }

    #[test]
    fn a_broker_whose_herd_file_cant_be_read_fails_to_start_rather_than_guess() {
        let world = World::new();
        std::fs::remove_file(world.paths.herds.join("h.toml")).unwrap();

        let err = lead_env(&world.paths, "lead-h").unwrap_err();

        assert!(
            format!("{err:#}")
                .starts_with("whether herd h guards merges by credentials is unknown: "),
            "{err:#}"
        );
    }

    #[test]
    fn a_credentials_herds_broker_without_its_token_fails_to_start_naming_the_fix() {
        let world = World::new();
        credentials_herd(&world, None);

        let err = lead_env(&world.paths, "lead-c").unwrap_err();

        let token = crate::credentials::files(&world.paths, "c").token;
        assert_eq!(
            format!("{err:#}"),
            format!(
                "herd c guards merges by credentials: no token at {}; \
                 run loopherd merge-guard c for how to create and store it",
                token.display()
            )
        );
    }

    #[test]
    fn a_credentials_herds_broker_without_its_host_block_fails_to_start_naming_the_fix() {
        let world = World::new();
        credentials_herd(&world, Some("github_pat_1\n"));
        let ssh_config = crate::credentials::ssh_config(&world.paths);
        std::fs::remove_file(&ssh_config).unwrap();

        let err = lead_env(&world.paths, "lead-c").unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!(
                "herd c guards merges by credentials: \
                 no current Host loopherd-c in {}; run loopherd merge-guard c",
                ssh_config.display()
            )
        );
    }

    /// A herdr showing `lh-1`'s screen as `screen`.
    fn reading_herdr(screen: &str) -> FakeHerdr {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        herdr.set_screen("w9:p9", screen);
        herdr
    }

    /// A [`reading_herdr`] whose `agents` calls each say so on the receiver, then wait until the
    /// sender sends or drops: a request that has said so is in flight.
    fn holding_herdr(screen: &str) -> (FakeHerdr, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let mut herdr = reading_herdr(screen);
        let (open, gate) = mpsc::channel();
        let (arrived, entered) = mpsc::channel();
        let gate = std::sync::Mutex::new(gate);
        herdr.on_agents = Some(Box::new(move || {
            let _ = arrived.send(());
            let _ = gate.lock().unwrap().recv();
        }));
        (herdr, open, entered)
    }

    fn read_lh_1(client: Client) -> JoinHandle<Result<(String, String, i32)>> {
        std::thread::spawn(move || {
            client.call(&Op::Read {
                name: "lh-1".into(),
                lines: 5,
            })
        })
    }

    #[test]
    fn a_broker_restart_lets_the_request_in_flight_finish_and_leaves_later_ones_to_the_new_broker()
    {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let (herdr, open, entered) = holding_herdr("old");
        let old = world.serve(herdr);
        let in_flight = read_lh_1(world.client());
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the read never reached herdr");
        // Taken up into its lane behind the one in flight, but not started.
        let queued = read_lh_1(world.client());
        std::thread::sleep(Duration::from_millis(200));
        let mut new = None;
        let mut said = Kept::default();

        std::thread::scope(|scope| {
            let restarting = scope.spawn(|| {
                restart_broker_with(
                    &world.paths,
                    "lead-h",
                    Duration::from_secs(10),
                    &std::env::current_exe().unwrap(),
                    &mut said,
                    &mut || {
                        // A gap between the old broker's exit and the new one's lock.
                        std::thread::sleep(Duration::from_millis(300));
                        // Both brokers share this test's pid; a real stop names the old one alone.
                        let stop = stop_file(&world.paths, "lead-h");
                        write_replacing(&stop, "0").unwrap();
                        new = Some(world.serve(reading_herdr("new")));
                        Ok(Started::Own(4242))
                    },
                )
            });
            while !stop_file(&world.paths, "lead-h").exists() {
                std::thread::sleep(POLL);
            }
            let later = read_lh_1(world.client());
            std::thread::sleep(Duration::from_millis(200));
            drop(open);

            restarting.join().unwrap().unwrap();
            assert_eq!(
                in_flight.join().unwrap().unwrap(),
                ("old".into(), "".into(), 0)
            );
            assert_eq!(later.join().unwrap().unwrap(), ("new".into(), "".into(), 0));
            assert_eq!(
                queued.join().unwrap().unwrap(),
                ("new".into(), "".into(), 0)
            );
        });

        let pid = std::process::id();
        assert_eq!(
            said.out,
            [
                format!(
                    "asked lead-h's broker (pid {pid}) to stop once its request in flight is answered"
                ),
                format!("restarted lead-h's broker: pid {pid} stopped, pid 4242 serves"),
            ]
        );
        assert_eq!(old.join().unwrap().0.unwrap(), Served::Stopped);
        assert!(!stop_file(&world.paths, "lead-h").exists());
        world.retire("lead-h");
        assert_eq!(new.unwrap().join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn an_idle_broker_looks_for_a_new_install_and_re_execs_it_holding_its_lock() {
        let world = World::new();
        let installed = world.paths.state_dir().join("bin/loopherd");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "").unwrap();
        let tried = Mutex::new(vec![]);
        let herdr = FakeHerdr::new(&[]);
        let mut host = host(&herdr);
        let installed_now = || Some(installed.clone());
        let reexec = |exe: &Path, lead: &str, lock: &Lock| {
            let lock_held = FileId::of(&lock_file(&world.paths, lead))
                == lock.metadata().ok().map(|m| FileId {
                    dev: m.dev(),
                    ino: m.ino(),
                });
            tried
                .lock()
                .unwrap()
                .push((exe.to_path_buf(), lead.to_string(), lock_held));
            world.retire("lead-h");
            anyhow!("refused in this test")
        };
        host.follow = Some(Follow {
            installed: &installed_now,
            reexec: &reexec,
            every: Duration::from_millis(50),
            moved_onto: None,
        });

        let served = serve(&host, &world.ledger(), &world.paths, "lead-h", POLL, None);

        assert_eq!(served.unwrap(), Served::Retired);
        assert_eq!(
            *tried.lock().unwrap(),
            vec![(installed.clone(), "lead-h".to_string(), true)]
        );
    }

    #[test]
    fn a_broker_handed_its_leads_held_lock_serves_under_it() {
        let world = World::new();
        let path = lock_file(&world.paths, "lead-h");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let handed = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        // The exec'ing image's copy: the lock stays held until serve adopts it.
        let _held = Lock::try_take(handed.try_clone().unwrap())
            .unwrap()
            .unwrap();
        world.retire("lead-h");

        let served = serve(
            &host(&FakeHerdr::new(&[])),
            &world.ledger(),
            &world.paths,
            "lead-h",
            POLL,
            Some(handed),
        );

        assert_eq!(served.unwrap(), Served::Retired);
    }

    #[test]
    fn a_broker_restart_with_no_broker_running_just_starts_one() {
        let world = World::new();
        let mut started = None;
        let mut said = Kept::default();

        restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_secs(10),
            &std::env::current_exe().unwrap(),
            &mut said,
            &mut || {
                started = Some(world.serve(FakeHerdr::new(&[])));
                Ok(Started::Own(4242))
            },
        )
        .unwrap();

        assert_eq!(said.out, ["started lead-h's broker: pid 4242"]);
        world.retire("lead-h");
        assert_eq!(started.unwrap().join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_broker_restart_with_no_broker_running_clears_a_stop_left_by_a_killed_restart() {
        let world = World::new();
        let stop = stop_file(&world.paths, "lead-h");
        std::fs::create_dir_all(stop.parent().unwrap()).unwrap();
        std::fs::write(&stop, "999999").unwrap();
        let mut stop_at_start = None;

        restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_secs(10),
            &std::env::current_exe().unwrap(),
            &mut Kept::default(),
            &mut || {
                stop_at_start = Some(stop.exists());
                Ok(Started::Own(4242))
            },
        )
        .unwrap();

        assert_eq!(stop_at_start, Some(false));
        assert!(!stop.exists());
    }

    #[test]
    fn a_broker_restart_that_finds_a_broker_it_cant_stop_clears_a_stop_left_by_a_killed_restart() {
        let world = World::new();
        // A broker older than pid files: it holds its lock and records no pid.
        let _old = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let stop = stop_file(&world.paths, "lead-h");
        std::fs::write(&stop, "999999").unwrap();

        let err = restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_secs(10),
            &std::env::current_exe().unwrap(),
            &mut Kept::default(),
            &mut || panic!("no new broker starts while the old one runs"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker lock is held by a process that recorded no pid: a broker that \
             predates loopherd broker-restart, which can't be asked to stop, or a lead's \
             loopherd that holds it locked. Stop such a broker yourself (pgrep -f 'loopherd \
             broker lead-h' finds it); with none to find, the lock is the lead's, so run \
             loopherd broker-restart lead-h again once its loopherd command ends"
        );
        assert!(!stop.exists());
    }

    #[test]
    fn a_broker_that_exits_before_the_restart_reads_its_pid_is_replaced() {
        let world = World::new();
        // Exiting: its pid file is gone, and its lanes still join.
        let lock = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let exiting = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(lock);
        });
        let mut said = Kept::default();

        restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_secs(10),
            &std::env::current_exe().unwrap(),
            &mut said,
            &mut || Ok(Started::Own(4242)),
        )
        .unwrap();

        exiting.join().unwrap();
        assert_eq!(said.out, ["started lead-h's broker: pid 4242"]);
    }

    #[test]
    fn a_broker_restart_waits_for_the_old_broker_to_exit_not_for_the_lock_to_be_free() {
        let world = World::new();
        let held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let pid_file = pid_file(&world.paths, "lead-h");
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();
        let stop_file = stop_file(&world.paths, "lead-h");
        // Once asked to stop, the old broker hands its lock to a revive's broker, pid 1,
        // with no gap in which a restart waiting on the lock could see it free.
        let revived = std::thread::spawn(move || {
            while !stop_file.exists() {
                std::thread::sleep(POLL);
            }
            write_replacing(&pid_file, "1").unwrap();
            held
        });
        let (done, restarted) = mpsc::channel();
        let paths = world.paths.clone();
        std::thread::spawn(move || {
            let mut said = Kept::default();
            // A bound far past the deadline below, so a restart that waits on the lock misses it.
            let result = restart_broker_with(
                &paths,
                "lead-h",
                Duration::from_secs(60),
                &std::env::current_exe().unwrap(),
                &mut said,
                &mut || Ok(Started::Another(1)),
            );
            let _ = done.send((result, said));
        });

        let (result, said) = restarted
            .recv_timeout(Duration::from_secs(5))
            .expect("the restart waited for the lock, not for the old broker");

        let _revived = revived.join().unwrap();
        result.unwrap();
        let pid = std::process::id();
        assert_eq!(
            said.out.last().unwrap(),
            &format!("restarted lead-h's broker: pid {pid} stopped, pid 1 serves")
        );
    }

    #[test]
    fn a_broker_that_cant_be_spawned_names_how_to_start_it() {
        let world = World::new();

        let err = spawn(&world.paths, "lead-h", Path::new("/nonexistent/loopherd")).unwrap_err();

        assert!(
            format!("{err:#}").contains("; run loopherd broker-restart lead-h to try again"),
            "{err:#}"
        );
    }

    /// Takes lead-h's broker lock as a broker running `binary` would record it.
    fn locked_by(paths: &Paths, binary: Option<(&str, FileId)>) -> Lock {
        let lock = take_lock(paths, "lead-h").unwrap().unwrap();
        let binary = binary.map_or(String::new(), |(path, file)| {
            format!(
                "{VERSION}v0\n{BINARY}{path}\n{FILE}{} {}\n",
                file.dev, file.ino
            )
        });
        std::fs::write(lock_file(paths, "lead-h"), format!("\n{MARKS}\n{binary}")).unwrap();
        lock
    }

    fn restarted_with(world: &World, started: Started, binary: Option<(&str, FileId)>) -> Kept {
        let exe = std::env::current_exe().unwrap();
        let mut said = Kept::default();
        let mut lock = None;
        restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_secs(10),
            &exe,
            &mut said,
            &mut || {
                lock = Some(locked_by(&world.paths, binary));
                Ok(started)
            },
        )
        .unwrap();
        said
    }

    #[test]
    fn a_restart_another_broker_beat_on_another_binary_warns_naming_that_binary() {
        let world = World::new();
        let other = FileId::of(Path::new("/")).unwrap();

        let said = restarted_with(&world, Started::Another(7), Some(("/old/loopherd", other)));

        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            said.err,
            [format!(
                "warning: pid 7 took lead-h's broker lock first and runs /old/loopherd (v0), \
                 not {}; run loopherd broker-restart lead-h again to replace it",
                exe.display()
            )]
        );
    }

    #[test]
    fn a_restart_another_broker_beat_that_records_no_binary_warns_it_is_older() {
        let world = World::new();

        let said = restarted_with(&world, Started::Another(7), None);

        assert_eq!(
            said.err,
            [
                "warning: pid 7 took lead-h's broker lock first and records no binary, as a \
              loopherd older than this one does; run loopherd broker-restart lead-h again to \
              replace it"
            ]
        );
    }

    #[test]
    fn a_restart_another_broker_beat_on_this_binary_or_its_own_broker_warns_nothing() {
        let world = World::new();
        let this = FileId::of(&std::env::current_exe().unwrap()).unwrap();
        let other = FileId::of(Path::new("/")).unwrap();

        let same = restarted_with(&world, Started::Another(7), Some(("/any/loopherd", this)));
        let own = restarted_with(&world, Started::Own(7), Some(("/old/loopherd", other)));

        assert!(same.err.is_empty(), "{:?}", same.err);
        assert!(own.err.is_empty(), "{:?}", own.err);
    }

    #[test]
    fn a_broker_start_that_loses_the_lock_to_another_broker_gives_that_one_s_pid() {
        let world = World::new();
        let _other = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(
            pid_file(&world.paths, "lead-h"),
            std::process::id().to_string(),
        )
        .unwrap();
        // A broker that found the lock taken exits at once, successfully.
        let lost = std::process::Command::new("true").spawn().unwrap();

        let serving = serving_once_started(&world.paths, "lead-h", lost);

        assert!(matches!(serving.unwrap(), Started::Another(pid) if pid == std::process::id()));
    }

    #[test]
    fn a_broker_that_doesnt_stop_in_time_fails_the_restart_naming_its_pid_and_keeps_serving() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let (herdr, open, entered) = holding_herdr("old");
        let serving = world.serve(herdr);
        let in_flight = read_lh_1(world.client());
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the read never reached herdr");

        let err = restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_millis(200),
            &std::env::current_exe().unwrap(),
            &mut Kept::default(),
            &mut || panic!("no new broker starts while the old one runs"),
        )
        .unwrap_err();

        let pid = std::process::id();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "lead-h's broker (pid {pid}) didn't stop within 200ms, so it is left running; \
                 run loopherd broker-restart lead-h again once its request is answered"
            )
        );
        drop(open);
        assert_eq!(in_flight.join().unwrap().unwrap().0, "old");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!serving.is_finished());
        assert!(running(&world.paths, "lead-h"));
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_broker_refuses_a_request_to_restart_brokers() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));
        let requests = requests_dir(&world.paths, "lead-h");
        let replies = replies_dir(&world.paths, "lead-h");
        let request = json!({"op": {"broker_restart": {"lead": "lead-h"}}});
        write_atomically(&requests, "restart", request.to_string().as_bytes()).unwrap();

        let reply = replies.join("restart.json");
        let given_up = Instant::now() + Duration::from_secs(5);
        while !reply.exists() {
            assert!(Instant::now() < given_up, "no reply");
            std::thread::sleep(POLL);
        }

        let why = match serde_json::from_slice(&std::fs::read(&reply).unwrap()).unwrap() {
            Reply::Refused(why) => why,
            other => panic!("{other:?}"),
        };
        assert!(
            why.starts_with("malformed request: unknown variant `broker_restart`"),
            "{why}"
        );
        assert!(running(&world.paths, "lead-h"));
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_stop_left_by_a_restart_that_died_is_ignored_by_the_broker_and_its_lead() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let serving = world.serve(reading_herdr("on"));
        // Its own pid, as a later process reusing the dead restart's pid would leave it.
        let stop = stop_file(&world.paths, "lead-h");
        std::fs::write(&stop, std::process::id().to_string()).unwrap();

        let read = read_lh_1(world.client()).join().unwrap();

        assert_eq!(read.unwrap(), ("on".into(), "".into(), 0));
        assert!(!serving.is_finished());
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_lead_waits_out_the_gap_while_a_restart_holds_its_lock_whatever_pids_it_sees() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let restarting = take_lock_at(&restart_file(&world.paths, "lead-h"))
            .unwrap()
            .unwrap();
        // The old broker's pid, which a lead in its own pid namespace can't see.
        std::fs::write(stop_file(&world.paths, "lead-h"), "999999").unwrap();
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();

        let read = read_lh_1(world.client());
        std::thread::sleep(Duration::from_millis(300));

        assert!(!read.is_finished(), "{:?}", read.join());
        let serving = world.serve(reading_herdr("new"));
        std::fs::remove_file(stop_file(&world.paths, "lead-h")).unwrap();
        drop(restarting);
        assert_eq!(read.join().unwrap().unwrap(), ("new".into(), "".into(), 0));
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_broker_that_lets_go_of_its_lock_just_after_the_bound_is_still_replaced() {
        let world = World::new();
        let lock = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        std::fs::write(
            pid_file(&world.paths, "lead-h"),
            std::process::id().to_string(),
        )
        .unwrap();
        // Still joining its lanes after it returned Stopped at the bound.
        let exiting = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        let mut said = Kept::default();

        restart_broker_with(
            &world.paths,
            "lead-h",
            Duration::from_millis(200),
            &std::env::current_exe().unwrap(),
            &mut said,
            &mut || Ok(Started::Own(4242)),
        )
        .unwrap();

        exiting.join().unwrap();
        let pid = std::process::id();
        assert_eq!(
            said.out.last().unwrap(),
            &format!("restarted lead-h's broker: pid {pid} stopped, pid 4242 serves")
        );
    }

    #[test]
    fn probes_racing_each_other_never_make_a_dead_restarts_stop_live() {
        let world = World::new();
        let restart = restart_file(&world.paths, "lead-h");
        std::fs::create_dir_all(restart.parent().unwrap()).unwrap();
        std::fs::write(&restart, "").unwrap();
        std::fs::write(
            stop_file(&world.paths, "lead-h"),
            std::process::id().to_string(),
        )
        .unwrap();
        let until = Instant::now() + Duration::from_millis(500);

        let probing: Vec<_> = (0..2)
            .map(|_| {
                let paths = world.paths.clone();
                std::thread::spawn(move || {
                    let mut live = 0;
                    while Instant::now() < until {
                        live += usize::from(asked_to_stop(&paths, "lead-h"));
                    }
                    live
                })
            })
            .collect();

        let live: usize = probing.into_iter().map(|p| p.join().unwrap()).sum();
        assert_eq!(live, 0);
    }

    /// A repo cloned from a bare origin, herd `h`'s, whose `[lead]` gates with `gate`.
    fn gating_world(gate: &[&str]) -> (World, PathBuf) {
        let world = World::new();
        let repo = crate::worktree::testing::repo_with_origin(world.repos.path());
        let text = format!(
            "repo = {repo:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\ngate = {gate:?}\n"
        );
        std::fs::write(world.paths.herds.join("h.toml"), text).unwrap();
        (world, repo)
    }

    /// Commits `file` on `branch` of the seed beside origin and pushes it there.
    fn push_file(world: &World, branch: &str, file: &str) -> String {
        push_file_on(world, branch, "main", file)
    }

    /// [`push_file`] on a `branch` cut from `from`, as a stacked PR's is.
    fn push_file_on(world: &World, branch: &str, from: &str, file: &str) -> String {
        use crate::worktree::testing::{git_in, head};
        let seed = world.repos.path().join("seed");
        let origin = world.repos.path().join("origin.git");
        git_in(&seed, &["checkout", "--quiet", "-B", branch, from]);
        std::fs::write(seed.join(file), format!("{file}\n")).unwrap();
        git_in(&seed, &["add", file]);
        crate::worktree::testing::commit(&seed, file);
        git_in(
            &seed,
            &["push", "--quiet", origin.to_str().unwrap(), branch],
        );
        let sha = head(&seed);
        git_in(&seed, &["checkout", "--quiet", "main"]);
        sha
    }

    fn gate(rev: &str) -> Op {
        Op::Gate {
            rev: rev.into(),
            base: None,
        }
    }

    fn gate_onto(rev: &str, base: &str) -> Op {
        Op::Gate {
            rev: rev.into(),
            base: Some(base.into()),
        }
    }

    #[test]
    fn a_lead_gates_a_pushed_head_merged_with_origin_s_moved_base() {
        let (world, _) = gating_world(&["test -f from-pr", "test -f from-main"]);
        let pr = push_file(&world, "feat/x", "from-pr");
        let base = push_file(&world, "main", "from-main");
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, err, code) = world.client().call(&gate(&pr[..12])).unwrap();

        assert_eq!(
            (out.as_str(), err.as_str(), code),
            (
                format!("gate {pr} merged with origin/main {base}: pass").as_str(),
                "",
                0
            )
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_failing_gate_names_its_command_and_the_output_s_tail() {
        let (world, _) =
            gating_world(&["true", "echo checking; echo broke >&2; exit 3", "touch ran"]);
        let pr = push_file(&world, "feat/x", "from-pr");
        let base = crate::worktree::testing::head_of(&world.repos.path().join("seed"), "main");
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, _, code) = world.client().call(&gate(&pr)).unwrap();

        assert_eq!(code, 1);
        assert_eq!(
            out,
            format!(
                "gate {pr} merged with origin/main {base}: failed: \
                 `echo checking; echo broke >&2; exit 3` exit 3\nchecking\nbroke"
            )
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_head_that_conflicts_with_the_base_fails_its_gate_unrun() {
        let (world, repo) = gating_world(&["touch ran"]);
        let pr = push_file(&world, "feat/x", "same");
        let seed = world.repos.path().join("seed");
        std::fs::write(seed.join("same"), "other\n").unwrap();
        crate::worktree::testing::git_in(&seed, &["add", "same"]);
        crate::worktree::testing::commit(&seed, "same, differently");
        let origin = world.repos.path().join("origin.git");
        crate::worktree::testing::git_in(
            &seed,
            &["push", "--quiet", origin.to_str().unwrap(), "main"],
        );
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, _, code) = world.client().call(&gate(&pr)).unwrap();

        assert_eq!(code, 1);
        assert!(
            out.ends_with(": failed: it doesn't merge cleanly; conflicts in:\nsame"),
            "{out}"
        );
        let worktree = crate::worktree::worktree_path(&repo, "loopherd-gate-lead-h");
        assert!(!worktree.join("ran").exists());
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_gate_refuses_a_rev_not_on_origin_and_makes_no_worktree() {
        let (world, repo) = gating_world(&["touch ran"]);
        std::fs::write(repo.join("local"), "").unwrap();
        crate::worktree::testing::git_in(&repo, &["add", "local"]);
        crate::worktree::testing::commit(&repo, "never pushed");
        let local = crate::worktree::testing::head(&repo);
        let serving = world.serve(FakeHerdr::new(&[]));

        for rev in [local.as_str(), "deadbeefdeadbeef", "HEAD", "--help"] {
            let refused = world.client().call(&gate(rev)).unwrap_err();
            assert!(
                format!("{refused:#}").contains("refused: "),
                "{rev}: {refused:#}"
            );
        }

        let worktrees = crate::worktree::testing::worktree_list(&repo);
        assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_herd_with_no_gate_refuses_one() {
        let (world, _) = gating_world(&[]);
        let pr = push_file(&world, "feat/x", "from-pr");
        let serving = world.serve(FakeHerdr::new(&[]));

        let refused = world.client().call(&gate(&pr)).unwrap_err();

        assert!(
            format!("{refused:#}").contains("refused: herd h sets no [lead] gate"),
            "{refused:#}"
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_refused_gate_leaves_origin_s_stale_branches_alone() {
        let (world, repo) = gating_world(&["true"]);
        let origin = world.repos.path().join("origin.git");
        crate::worktree::testing::git_in(&origin, &["branch", "-D", "feature/existing"]);
        let serving = world.serve(FakeHerdr::new(&[]));

        world.client().call(&gate("deadbeefdeadbeef")).unwrap_err();

        assert!(
            crate::worktree::testing::git(
                &repo,
                &["rev-parse", "--verify", "origin/feature/existing"]
            )
            .is_ok()
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_gate_reuses_its_worktree_and_recovers_one_deleted_by_hand() {
        let (world, repo) = gating_world(&["test -f from-pr", "test ! -f stray"]);
        let failing = push_file(&world, "feat/a", "other");
        let passing = push_file(&world, "feat/b", "from-pr");
        let serving = world.serve(FakeHerdr::new(&[]));
        let worktree = crate::worktree::worktree_path(&repo, "loopherd-gate-lead-h");

        let (_, _, first) = world.client().call(&gate(&failing)).unwrap();
        std::fs::write(worktree.join("stray"), "").unwrap();
        let (_, _, second) = world.client().call(&gate(&passing)).unwrap();
        std::fs::remove_dir_all(&worktree).unwrap();
        let (out, _, third) = world.client().call(&gate(&passing)).unwrap();

        assert_eq!((first, second, third), (1, 0, 0), "{out}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_gate_won_t_take_over_a_branch_s_worktree_at_its_path() {
        let (world, repo) = gating_world(&["true"]);
        let pr = push_file(&world, "feat/x", "from-pr");
        let worktree = crate::worktree::worktree_path(&repo, "loopherd-gate-lead-h");
        crate::worktree::testing::git_in(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "loopherd/gate-lead-h",
                worktree.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree.join("work"), "").unwrap();
        let serving = world.serve(FakeHerdr::new(&[]));

        let refused = world.client().call(&gate(&pr)).unwrap_err();

        assert!(format!("{refused:#}").contains("refused: "), "{refused:#}");
        assert!(worktree.join("work").exists());
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    /// Pushes a commit on `branch`, cut from `from`, writing each file with its text.
    fn push_files(world: &World, branch: &str, from: &str, files: &[(&str, &str)]) -> String {
        use crate::worktree::testing::{git_in, head};
        let seed = world.repos.path().join("seed");
        let origin = world.repos.path().join("origin.git");
        git_in(&seed, &["checkout", "--quiet", "-B", branch, from]);
        for (file, text) in files {
            std::fs::write(seed.join(file), text).unwrap();
            git_in(&seed, &["add", file]);
        }
        crate::worktree::testing::commit(&seed, branch);
        git_in(
            &seed,
            &["push", "--quiet", origin.to_str().unwrap(), branch],
        );
        let sha = head(&seed);
        git_in(&seed, &["checkout", "--quiet", "main"]);
        sha
    }

    #[test]
    fn a_gate_drops_a_file_an_earlier_gate_s_checkout_ignored_and_its_own_doesn_t() {
        let (world, _) = gating_world(&["test ! -f generated", "touch generated"]);
        let ignoring = push_files(&world, "feat/a", "main", &[(".gitignore", "generated\n")]);
        let plain = push_file(&world, "feat/b", "from-pr");
        let serving = world.serve(FakeHerdr::new(&[]));

        let (_, _, first) = world.client().call(&gate(&ignoring)).unwrap();
        let (out, _, second) = world.client().call(&gate(&plain)).unwrap();

        assert_eq!((first, second), (0, 0), "{out}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_stacked_head_gates_against_the_base_branch_it_names() {
        let (world, _) = gating_world(&["test -f parent", "test ! -f from-main"]);
        let parent = push_file(&world, "feat/parent", "parent");
        let child = push_file_on(&world, "feat/child", "feat/parent", "child");
        push_file(&world, "main", "from-main");
        let serving = world.serve(FakeHerdr::new(&[]));

        let (out, _, code) = world
            .client()
            .call(&gate_onto(&child, "feat/parent"))
            .unwrap();

        assert_eq!(
            (out, code),
            (
                format!("gate {child} merged with origin/feat/parent {parent}: pass"),
                0
            )
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_gate_refuses_a_base_origin_has_no_branch_for_and_makes_no_worktree() {
        let (world, repo) = gating_world(&["true"]);
        let pr = push_file(&world, "feat/x", "from-pr");
        let serving = world.serve(FakeHerdr::new(&[]));

        for base in ["feat/gone", "HEAD", "--help", "x"] {
            let refused = world.client().call(&gate_onto(&pr, base)).unwrap_err();
            assert!(
                format!("{refused:#}").contains(&format!("refused: origin has no branch {base}")),
                "{base}: {refused:#}"
            );
        }

        let worktrees = crate::worktree::testing::worktree_list(&repo);
        assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }
}
