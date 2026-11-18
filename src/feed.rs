//! `loopherd feed`: fill a herd's free slots from its queue's source (D14).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::herd::{AddRequest, Paths, Queue, lead_switches, members, queue, resolve};
use crate::herdr::Herdr;
use crate::ledger::{AfterMerge, AgentRecord, Ledger, Process};
use crate::lock::Lock;
use crate::pause;
use crate::process::{Procs, live};
use crate::report::{Body, Entry, append};
use crate::retire::retire_and_close;
use crate::shell::run_within;
use crate::spawn::{BLOCKED_EXIT, spawn};
use crate::tool_hook::{Pane, Switch, lead_in, needs};

/// Feed's exit code when every error was a malformed source line.
pub const LINE_ERRORS_EXIT: i32 = 4;

pub struct FeedRequest<'a> {
    pub herd: String,
    /// Run the source and the done checks and report; retire and spawn nothing.
    pub dry_run: bool,
    pub ready_timeout: Duration,
    /// How long the source and each done check may run.
    pub command_timeout: Duration,
    /// The pane running loopherd, whose tab retire never closes.
    pub own_pane: Option<String>,
    /// `$HERDR_SOCKET_PATH`: which herdr `own_pane` is in.
    pub own_socket: Option<PathBuf>,
    /// Where members' mailboxes live, for after_merge's notes.
    pub mailboxes: PathBuf,
    /// How long each after_merge command may run.
    pub after_merge_timeout: Duration,
    /// This feed's process, recorded on the after_merge it takes on.
    pub owner: Process,
    /// Tells whether another feed's recorded process still runs.
    pub procs: &'a dyn Procs,
}

/// One line per task, then a summary, and the exit code.
#[derive(Debug)]
pub struct Fed {
    pub lines: Vec<String>,
    pub code: i32,
    /// For [`run_after_merge`].
    pending: Option<Pending>,
}

/// Retires members whose done check passes, then spawns a member per task
/// until the herd's slots are full. A paused herd only retires; its source
/// isn't run. after_merge due for merged members is left in the result for
/// [`run_after_merge`], paused or not.
///
/// Fails, changing nothing, if the source fails or herdr is unreachable.
/// A dry run never calls herdr.
pub fn feed(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &FeedRequest,
    inherited_workspace: Option<&str>,
) -> Result<Fed> {
    let queue = queue(paths, &req.herd)?;
    if !req.dry_run {
        herdr.workspaces().context("herdr")?;
    }
    let mut lines = vec![];
    let reading = pause::read(&paths.paused, &req.herd);
    if let Some(pause) = reading.pause() {
        lines.extend(reading.warning(&req.herd));
        let reaped = reap(herdr, ledger, paths, &queue, req, &mut lines)?;
        lines.push(format!(
            "{} paused {}: spawned nothing",
            req.herd,
            pause.when()
        ));
        let code = if reaped.errors > 0 { 1 } else { 0 };
        let pending = pending(&queue, reaped.after_merge);
        return Ok(Fed {
            lines,
            code,
            pending,
        });
    }
    let (output, notes) = run_source(&queue, req.command_timeout).context("source")?;
    if req.dry_run {
        lines.extend(notes.lines().map(|note| format!("source: {note}")));
    }
    let reaped = reap(herdr, ledger, paths, &queue, req, &mut lines)?;
    let mut errors = reaped.errors;
    let pending = pending(&queue, reaped.after_merge);

    let held = members(ledger, &req.herd)?;
    let held = if req.dry_run {
        held.saturating_sub(reaped.retired)
    } else {
        held
    };
    let mut free = queue.max.saturating_sub(held);
    let mut taken = taken(ledger.entries().context("ledger")?);

    let (mut added, mut skipped, mut line_errors) = (0, 0, 0);
    let mut code = 0;
    for task in output.lines().map(str::trim) {
        if task.is_empty() || task.starts_with('#') {
            continue;
        }
        let (template, params) = match task_params(&queue, task) {
            Ok(p) => p,
            Err(e) => {
                lines.push(format!("error: {task}: {e:#}"));
                errors += 1;
                line_errors += 1;
                continue;
            }
        };
        let spawn_req = match task_request(paths, req, template, params) {
            Ok(r) => r,
            Err(e) => {
                lines.push(format!("error: {task}: {e:#}"));
                errors += 1;
                continue;
            }
        };
        let name = spawn_req.name.clone();
        if !taken.insert(name.clone()) {
            lines.push(format!("skipped: exists {name}"));
            skipped += 1;
            continue;
        }
        if free == 0 {
            lines.push(format!("skipped: no slot {name}"));
            skipped += 1;
            continue;
        }
        free -= 1;
        if req.dry_run {
            lines.push(format!("would add {name} (template {template})"));
            added += 1;
            continue;
        }
        match spawn(herdr, ledger, &spawn_req, inherited_workspace) {
            Ok(spawned) => {
                lines.push(format!("added {name}"));
                lines.extend(
                    spawned
                        .warnings()
                        .iter()
                        .map(|w| format!("warning: {name}: {w}")),
                );
                added += 1;
                if spawned.blocked {
                    lines.push(format!(
                        "blocked: answer it in pane {}, then run loopherd prompt {name}; stopping",
                        spawned.pane_id
                    ));
                    code = BLOCKED_EXIT;
                    break;
                }
            }
            Err(e) => {
                lines.push(format!("error: {task}: {e:#}; stopping"));
                errors += 1;
                code = 1;
                break;
            }
        }
    }
    let (retire_verb, add_verb) = match req.dry_run {
        true => ("would retire", "would add"),
        false => ("retired", "added"),
    };
    lines.push(format!(
        "{}: {} {retire_verb}, {added} {add_verb}, {skipped} skipped, {errors} error{}",
        req.herd,
        reaped.retired,
        if errors == 1 { "" } else { "s" }
    ));
    if code == 0 && errors > 0 {
        code = if errors == line_errors {
            LINE_ERRORS_EXIT
        } else {
            1
        };
    }
    Ok(Fed {
        lines,
        code,
        pending,
    })
}

/// Names a task may not spawn: every ledger entry but a released one.
fn taken(entries: BTreeMap<String, AgentRecord>) -> HashSet<String> {
    entries
        .into_values()
        .filter(|r| r.released_at.is_none())
        .map(|r| r.name)
        .collect()
}

fn pending(queue: &Queue, members: Vec<String>) -> Option<Pending> {
    (!members.is_empty()).then(|| Pending {
        repo: queue.repo.clone(),
        commands: queue.after_merge.clone(),
        members,
    })
}

/// The names feed would spawn now from `tasks`, the source's output, as
/// `feed --dry-run` plans them: tasks with no member, while slots are free
/// under `max`, `freed` being the members whose done check passes. A task
/// feed would error on is skipped.
pub fn claimable(
    ledger: &dyn Ledger,
    paths: &Paths,
    queue: &Queue,
    herd: &str,
    freed: usize,
    tasks: &str,
) -> Result<Vec<String>> {
    let free = queue
        .max
        .saturating_sub(members(ledger, herd)?.saturating_sub(freed));
    unclaimed(ledger, paths, queue, herd, tasks, free)
}

/// How many tasks [`claimable`] would name with no cap on slots.
pub fn claimable_count(
    ledger: &dyn Ledger,
    paths: &Paths,
    queue: &Queue,
    herd: &str,
    tasks: &str,
) -> Result<usize> {
    Ok(unclaimed(ledger, paths, queue, herd, tasks, usize::MAX)?.len())
}

/// Up to `limit` names of tasks in `tasks` with no member.
fn unclaimed(
    ledger: &dyn Ledger,
    paths: &Paths,
    queue: &Queue,
    herd: &str,
    tasks: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let mut taken = taken(ledger.entries().context("ledger")?);
    let req = FeedRequest {
        herd: herd.into(),
        dry_run: true,
        ready_timeout: Duration::ZERO,
        command_timeout: Duration::ZERO,
        own_pane: None,
        own_socket: None,
        mailboxes: PathBuf::new(),
        after_merge_timeout: Duration::ZERO,
        owner: Process { pid: 0, start: 0 },
        procs: &crate::process::ProcFs,
    };
    let mut names = vec![];
    for task in tasks.lines().map(str::trim) {
        if names.len() == limit {
            break;
        }
        if task.is_empty() || task.starts_with('#') {
            continue;
        }
        let Ok(spawn_req) = task_params(queue, task)
            .and_then(|(template, params)| task_request(paths, &req, template, params))
        else {
            continue;
        };
        if taken.insert(spawn_req.name.clone()) {
            names.push(spawn_req.name);
        }
    }
    Ok(names)
}

struct Reaped {
    retired: usize,
    errors: usize,
    /// Members whose after_merge is recorded as run, to run once feed's lock is released.
    after_merge: Vec<String>,
}

/// Runs each active member's checks, one line per member to `lines`. A
/// member whose after_merge is due runs its merged check first; once that
/// passes, its after_merge is queued and its done check waits for the next
/// feed, so a watch sees the note before it retires. The rest are retired
/// and closed when their done check passes.
fn reap(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    queue: &Queue,
    req: &FeedRequest,
    lines: &mut Vec<String>,
) -> Result<Reaped> {
    let mut reaped = Reaped {
        retired: 0,
        errors: 0,
        after_merge: vec![],
    };
    let mut held_back = None;
    if !queue.after_merge.is_empty() {
        for template in queue.templates.keys() {
            if !queue.merged.contains_key(template) {
                lines.push(format!(
                    "warning: template {template} has no merged check, so after_merge never runs for its members"
                ));
            }
        }
        held_back = gate(ledger, paths, req, &queue.after_merge)?;
    }
    let members = ledger.entries().context("ledger")?.into_values();
    let workers = members.filter(|e| !e.retired && !e.is_lead());
    for member in workers.filter(|e| e.herd.as_deref() == Some(&req.herd)) {
        let name = &member.name;
        if let Some(AfterMerge::Pending { owner }) = member.after_merge {
            if live(req.procs, Some(owner)).is_some() {
                lines.push(format!("after_merge {name}: running in feed {}", owner.pid));
            } else {
                interrupted(ledger, req, name, owner, lines)?;
            }
            continue;
        }
        let template = member.template.as_deref();
        let merged = template
            .and_then(|t| queue.merged.get(t))
            .filter(|_| !queue.after_merge.is_empty() && member.after_merge.is_none());
        let done = template.and_then(|t| queue.done.get(t));
        if merged.is_none() && done.is_none() {
            continue;
        }
        let Some(params) = &member.params else {
            lines.push(format!(
                "warning: {name} has no recorded params; skipping its checks"
            ));
            continue;
        };
        if let Some(check) = merged {
            let passed = match check.render(&member, params) {
                Ok(command) => run_check(&command, &queue.repo, req.command_timeout),
                Err(e) => Err(format!("{e:#}")),
            };
            match passed {
                Ok(false) => {}
                Err(why) => {
                    lines.push(format!(
                        "warning: merged check failed {name}: {why}; keeping it until after_merge runs"
                    ));
                    continue;
                }
                Ok(true) => {
                    if after_merge(ledger, queue, req, name, &held_back, lines)? {
                        reaped.after_merge.push(name.clone());
                    }
                    continue;
                }
            }
        }
        let Some(check) = done else {
            continue;
        };
        let finished = match check.render(&member, params) {
            Ok(command) => run_check(&command, &queue.repo, req.command_timeout),
            Err(e) => Err(format!("{e:#}")),
        };
        if finished == Ok(true) && merged.is_some() {
            let never = "its done check passed before its merged check did, so after_merge never ran for it; if it merged, run its commands by hand";
            lines.push(format!("warning: {name}: {never}"));
            if !req.dry_run {
                note(req, name, &format!("after_merge never ran: {never}"), lines);
            }
        }
        match finished {
            Ok(false) => lines.push(format!("still open {name}")),
            Err(why) => lines.push(format!("warning: check failed {name}: {why}")),
            Ok(true) if req.dry_run => {
                lines.push(format!("would retire {name}"));
                reaped.retired += 1;
            }
            Ok(true) => {
                let mut out = vec![];
                let own_pane = req.own_pane.as_deref();
                let now = SystemTime::now();
                match retire_and_close(herdr, ledger, name, now, &req.mailboxes, own_pane, &mut out)
                {
                    Ok(()) => {
                        lines.push(out.join("; "));
                        reaped.retired += 1;
                    }
                    Err(e) => {
                        out.push(format!("error: {e:#}"));
                        lines.push(format!("error: retire {name}: {}", out.join("; ")));
                        reaped.errors += 1;
                        if is_retired(ledger, name) {
                            reaped.retired += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(reaped)
}

/// A command of after_merge that the lead running feed may not run.
struct HeldBack {
    command: String,
    switch: Switch,
    herd: String,
    /// Why the lead's switches couldn't be read, counting every switch off.
    unread: Option<String>,
}

impl HeldBack {
    fn note(&self) -> String {
        let command = &self.command;
        let mut why = match self.switch {
            Switch::Pause | Switch::ForceRestart => format!(
                "`{command}` is `loopherd {}`, which a lead can never run",
                self.switch.name()
            ),
            switch => format!(
                "`{command}` needs the {} switch, off in {}'s [lead]",
                switch.name(),
                self.herd
            ),
        };
        if let Some(unread) = &self.unread {
            why.push_str(&format!(
                " (its switches couldn't be read, so every switch counts as off: {unread})"
            ));
        }
        why
    }

    fn hint(&self) -> String {
        match self.switch {
            Switch::Pause | Switch::ForceRestart => "run its commands by hand".into(),
            switch => format!("run its commands by hand, or turn {} on", switch.name()),
        }
    }
}

/// The first of `commands` the lead running feed may not run: feed runs in
/// a lead's pane, and the tool hook would deny that command there.
fn gate(
    ledger: &dyn Ledger,
    paths: &Paths,
    req: &FeedRequest,
    commands: &[String],
) -> Result<Option<HeldBack>> {
    let pane = req.own_pane.as_deref().map(|id| Pane {
        id,
        socket: req.own_socket.as_deref(),
    });
    let Some(lead) = pane.map(|p| lead_in(ledger, &p)).transpose()?.flatten() else {
        return Ok(None);
    };
    let herd = lead.herd.unwrap_or_default();
    let (switches, unread) = match lead_switches(paths, &herd) {
        Ok(switches) => (switches, None),
        Err(e) => (Default::default(), Some(format!("{e:#}"))),
    };
    Ok(commands.iter().find_map(|command| {
        let switch = needs(command).into_iter().find(|s| !s.on(&switches))?;
        Some(HeldBack {
            command: command.clone(),
            switch,
            herd: herd.clone(),
            unread: unread.clone(),
        })
    }))
}

/// Handles the merged member `name`'s after_merge: prints it in a dry run,
/// else records it in the ledger as pending on this feed or, when
/// `held_back`, notes why it won't run. Returns whether its commands are
/// still to run.
fn after_merge(
    ledger: &mut dyn Ledger,
    queue: &Queue,
    req: &FeedRequest,
    name: &str,
    held_back: &Option<HeldBack>,
    lines: &mut Vec<String>,
) -> Result<bool> {
    if req.dry_run {
        lines.push(match held_back {
            Some(held) => format!("would not run after_merge for {name}: {}", held.note()),
            None => format!(
                "would run after_merge for {name}: {}",
                queue.after_merge.join("; ")
            ),
        });
        return Ok(false);
    }
    let Some(held) = held_back else {
        let owner = req.owner;
        set_after_merge(ledger, name, AfterMerge::Pending { owner }).context("ledger")?;
        return Ok(true);
    };
    set_after_merge(ledger, name, AfterMerge::Noted).context("ledger")?;
    lines.push(format!("after_merge {name}: not run: {}", held.note()));
    let text = format!("after_merge not run: {}; {}", held.note(), held.hint());
    note(req, name, &text, lines);
    Ok(false)
}

/// Marks `name`'s after_merge noted and notes it interrupted, when it is
/// still pending on the exited feed `owner`. A dry run only says so.
fn interrupted(
    ledger: &mut dyn Ledger,
    req: &FeedRequest,
    name: &str,
    owner: Process,
    lines: &mut Vec<String>,
) -> Result<()> {
    if req.dry_run {
        lines.push(format!("would note after_merge {name}: interrupted"));
        return Ok(());
    }
    let pending = Some(AfterMerge::Pending { owner });
    let mut claimed = false;
    ledger
        .update(name, &mut |r| {
            // `owner` may have noted it and exited since this feed read the ledger.
            claimed = r.after_merge == pending;
            if claimed {
                r.after_merge = Some(AfterMerge::Noted);
            }
            Ok(())
        })
        .context("ledger")?;
    if claimed {
        lines.push(format!("after_merge {name}: interrupted"));
        let text = "after_merge interrupted: the feed running it exited before noting an outcome; \
                    check what ran and finish its commands by hand";
        note(req, name, text, lines);
    }
    Ok(())
}

fn set_after_merge(ledger: &mut dyn Ledger, name: &str, state: AfterMerge) -> Result<()> {
    ledger.update(name, &mut |r| {
        r.after_merge = Some(state);
        Ok(())
    })?;
    Ok(())
}

/// Appends `text` to `name`'s mailbox as a note, warning in `lines` if it can't.
fn note(req: &FeedRequest, name: &str, text: &str, lines: &mut Vec<String>) {
    let entry = Entry::new(SystemTime::now(), Body::Note { text: text.into() });
    if let Err(e) = append(&req.mailboxes, name, &entry) {
        lines.push(format!("warning: note to {name}'s mailbox: {e:#}"));
    }
}

/// after_merge work `feed` recorded under its lock, for [`run_after_merge`].
#[derive(Debug)]
pub struct Pending {
    repo: PathBuf,
    commands: Vec<String>,
    members: Vec<String>,
}

/// Runs the after_merge `fed` left pending, once per member, appending a
/// line each to `fed.lines` and a note to the member's mailbox, then
/// marking it noted. Call it after releasing feed's lock: it waits for,
/// then holds, its own lock beside it, so two live feeds never run
/// after_merge at once.
pub fn run_after_merge(
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &FeedRequest,
    fed: &mut Fed,
) -> Result<()> {
    let Some(pending) = fed.pending.take() else {
        return Ok(());
    };
    let path = paths.feed_lock.with_file_name("after-merge.lock");
    let file = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    let _lock = Lock::wait(file).with_context(|| format!("lock {}", path.display()))?;
    for name in &pending.members {
        let (line, text) =
            match run_commands(&pending.commands, &pending.repo, req.after_merge_timeout) {
                None => ("ok".to_string(), "after_merge ok".to_string()),
                Some((why, tail)) => {
                    if fed.code == 0 {
                        fed.code = 1;
                    }
                    (
                        format!("failed: {why}"),
                        format!("after_merge failed: {why}{tail}"),
                    )
                }
            };
        fed.lines.push(format!("after_merge {name}: {line}"));
        // Marked first, so a failed write leaves only the next feed's interrupted note.
        match set_after_merge(ledger, name, AfterMerge::Noted) {
            Ok(()) => note(req, name, &text, &mut fed.lines),
            Err(e) => fed
                .lines
                .push(format!("warning: ledger: {name}: {e:#}; not noted")),
        }
    }
    Ok(())
}

/// Lines of a failed command's output a note keeps, and at most how many bytes.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 4000;

/// Runs `commands` in order in `repo`, stopping at the first failure,
/// which it returns as the command and its exit, and the last lines of its
/// output.
fn run_commands(commands: &[String], repo: &Path, timeout: Duration) -> Option<(String, String)> {
    // A file rather than a pipe, so a command killed at the timeout still leaves its output.
    let log = match tempfile::Builder::new()
        .prefix("loopherd-after-merge-")
        .suffix(".log")
        .tempfile()
    {
        Ok(log) => log,
        Err(e) => return Some((format!("could not create its log: {e}"), String::new())),
    };
    for command in commands {
        let ran = run_within(
            Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "exec >\"$LOOPHERD_AFTER_MERGE_LOG\" 2>&1\n{command}"
                ))
                .env("LOOPHERD_AFTER_MERGE_LOG", log.path())
                .current_dir(repo),
            timeout,
        );
        let why = match ran {
            Ok(Some(output)) if output.status.success() => continue,
            Ok(Some(output)) => match output.status.code() {
                Some(code) => format!("exit {code}"),
                None => output.status.to_string(),
            },
            Ok(None) => format!("timed out after {}", humantime::format_duration(timeout)),
            Err(e) => format!("could not run in {}: {e:#}", repo.display()),
        };
        let end = read_end(log.path()).unwrap_or_default();
        return Some((
            format!("`{command}` {why}"),
            tail(&String::from_utf8_lossy(&end)),
        ));
    }
    None
}

/// The file's last `2 * TAIL_BYTES` bytes: enough for [`tail`], whose cut
/// drops any line this one splits.
fn read_end(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(2 * TAIL_BYTES as u64)))?;
    let mut end = vec![];
    file.read_to_end(&mut end)?;
    Ok(end)
}

/// `output`'s last [`TAIL_LINES`] lines, cut to their last [`TAIL_BYTES`],
/// on a line of its own; empty for no output.
fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    if tail.is_empty() {
        return tail;
    }
    if tail.len() <= TAIL_BYTES {
        return format!("\n{tail}");
    }
    let mut start = tail.len() - TAIL_BYTES;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    format!("\n…{}", &tail[start..])
}

/// Whether the ledger marks `name` retired; false if it can't be read.
fn is_retired(ledger: &dyn Ledger, name: &str) -> bool {
    ledger
        .entries()
        .is_ok_and(|e| e.get(name).is_some_and(|r| r.retired))
}

/// Whether `command` reports its member finished: exit 0 yes, exit 1 no,
/// anything else an error describing what happened.
pub(crate) fn run_check(command: &str, repo: &Path, timeout: Duration) -> Result<bool, String> {
    let ran = run_within(
        Command::new("sh").arg("-c").arg(command).current_dir(repo),
        timeout,
    );
    let output = match ran {
        Ok(Some(output)) => output,
        Ok(None) => {
            return Err(format!(
                "timed out after {}",
                humantime::format_duration(timeout)
            ));
        }
        Err(e) => return Err(format!("run in {}: {e:#}", repo.display())),
    };
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(&output)),
    }
}

/// `output`'s exit status, with its stderr if it wrote any.
fn failure(output: &Output) -> String {
    let status = match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => output.status.to_string(),
    };
    match String::from_utf8_lossy(&output.stderr).trim() {
        "" => status,
        stderr => format!("{status}: {stderr}"),
    }
}

/// The task's template and params. A template word naming none of the
/// queue's templates, or a wrong param count, is a bad line; nothing else is.
fn task_params<'q>(queue: &'q Queue, task: &str) -> Result<(&'q str, Vec<String>)> {
    let mut words = task.split_whitespace();
    let (template, names) = if queue.routed {
        let word = words.next().unwrap_or_default();
        queue.templates.get_key_value(word).with_context(|| {
            let known: Vec<&str> = queue.templates.keys().map(String::as_str).collect();
            format!(
                "the queue has no template {word} (it has: {})",
                known.join(", ")
            )
        })?
    } else {
        queue
            .templates
            .first_key_value()
            .context("the queue has no template")?
    };
    let params: Vec<String> = words.map(String::from).collect();
    if params.len() != names.len() {
        bail!(
            "template {template} takes {} param{} ({}), got {}",
            names.len(),
            if names.len() == 1 { "" } else { "s" },
            names.join(", "),
            params.len()
        );
    }
    Ok((template, params))
}

fn task_request(
    paths: &Paths,
    req: &FeedRequest,
    template: &str,
    params: Vec<String>,
) -> Result<crate::spawn::SpawnRequest> {
    resolve(
        paths,
        &AddRequest {
            herd: req.herd.clone(),
            template: template.into(),
            params,
            claude_args: vec![],
            ready_timeout: req.ready_timeout,
        },
    )
}

/// The source's stdout and stderr; fails if it exits non-zero or outruns
/// `timeout`.
pub(crate) fn run_source(queue: &Queue, timeout: Duration) -> Result<(String, String)> {
    run_in_repo(&queue.source, &queue.repo, timeout)
}

/// `command`'s stdout and stderr, run by `sh` in `repo`; fails like [`run_source`].
pub(crate) fn run_in_repo(
    command: &str,
    repo: &Path,
    timeout: Duration,
) -> Result<(String, String)> {
    let output = run_within(
        Command::new("sh").arg("-c").arg(command).current_dir(repo),
        timeout,
    )
    .with_context(|| format!("run in {}", repo.display()))?
    .with_context(|| {
        format!(
            "{command:?} timed out after {}",
            humantime::format_duration(timeout)
        )
    })?;
    if !output.status.success() {
        let failed = format!("{command:?} failed ({})", output.status);
        match String::from_utf8_lossy(&output.stderr).trim() {
            "" => bail!(failed),
            stderr => bail!("{failed}: {stderr}"),
        }
    }
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::herd::Paths;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::process::FakeProcs;
    use crate::spawn::Checkout;

    /// The process each test feed runs as, live in [`LIVE`].
    const OWNER: Process = Process { pid: 7, start: 70 };

    static LIVE: std::sync::LazyLock<FakeProcs> = std::sync::LazyLock::new(|| {
        let mut procs = FakeProcs::default();
        procs.add(OWNER.pid, "/usr/bin/loopherd", 1, OWNER.start);
        procs
    });

    struct Setup {
        config: tempfile::TempDir,
        repo: tempfile::TempDir,
    }

    impl Setup {
        /// A herd `prs` whose queue runs `source` with at most `max` members.
        fn new(source: &str, max: i64) -> Setup {
            Setup::with_template(source, max, "")
        }

        /// Like `new`, with `done` as template `pr`'s done check.
        fn with_done(source: &str, max: i64, done: &str) -> Setup {
            Setup::with_template(source, max, &format!("done = {done:?}\n"))
        }

        fn with_template(source: &str, max: i64, extra: &str) -> Setup {
            let config = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            fs::create_dir(config.path().join("herds")).unwrap();
            let herd = format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
                 prompt = \"review {{number}}\"\n{extra}[queue]\ntemplate = \"pr\"\nsource = {:?}\nmax = {max}\n",
                repo.path().display().to_string(),
                source
            );
            fs::write(config.path().join("herds/prs.toml"), herd).unwrap();
            Setup { config, repo }
        }

        /// A herd `prs` whose queue routes each task to template `card` or
        /// `research`, by its first word.
        fn routed(source: &str, max: i64) -> Setup {
            let setup = Setup::new(source, max);
            let herd = format!(
                "repo = {:?}\n\
                 [template.card]\nparams = [\"card\"]\nname = \"lh-{{card}}\"\n\
                 prompt = \"build {{card}}\"\ndone = \"test -f done-{{card}}\"\n\
                 [template.research]\nparams = [\"card\"]\nname = \"research-{{card}}\"\n\
                 prompt = \"research {{card}}\"\ndone = \"test -f researched-{{card}}\"\n\
                 [queue]\ntemplates = [\"card\", \"research\"]\nsource = {:?}\nmax = {max}\n",
                setup.repo.path().display().to_string(),
                source
            );
            fs::write(setup.config.path().join("herds/prs.toml"), herd).unwrap();
            setup
        }

        fn tasks(&self, tasks: &str) {
            fs::write(self.repo.path().join("tasks"), tasks).unwrap();
        }

        fn touch(&self, file: &str) {
            fs::write(self.repo.path().join(file), "").unwrap();
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.config.path().join("herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: self.config.path().join("feed.lock"),
                paused: self.config.path().join("paused"),
            }
        }

        fn feed(&self, herdr: &FakeHerdr, ledger: &mut MemLedger, dry_run: bool) -> Result<Fed> {
            self.feed_from(herdr, ledger, dry_run, None)
        }

        /// Like `feed`, run in `pane` of the herdr at socket `/s`.
        fn feed_from(
            &self,
            herdr: &FakeHerdr,
            ledger: &mut MemLedger,
            dry_run: bool,
            pane: Option<&str>,
        ) -> Result<Fed> {
            let req = self.request(dry_run, pane);
            let mut fed = feed(herdr, ledger, &self.paths(), &req, Some("w1"))?;
            run_after_merge(ledger, &self.paths(), &req, &mut fed)?;
            Ok(fed)
        }

        fn request(&self, dry_run: bool, pane: Option<&str>) -> FeedRequest<'static> {
            FeedRequest {
                herd: "prs".into(),
                dry_run,
                ready_timeout: Duration::from_secs(60),
                command_timeout: Duration::from_secs(1),
                own_pane: pane.map(str::to_string),
                own_socket: pane.map(|_| PathBuf::from("/s")),
                mailboxes: self.config.path().join("reports"),
                after_merge_timeout: Duration::from_secs(2),
                owner: OWNER,
                procs: &*LIVE,
            }
        }

        /// A herd `prs` whose template `pr` has done and merged checks
        /// testing for `done-<n>` and `merged-<n>`, with `after_merge` set
        /// to the TOML array `commands`, and `lead` appended.
        fn after_merge(commands: &str, lead: &str) -> Setup {
            let checks = "done = \"test -f done-{number}\"\nmerged = \"test -f merged-{number}\"\n";
            Setup::after_merge_checks(commands, checks, lead)
        }

        /// Like `after_merge`, with `checks` as template `pr`'s check lines.
        fn after_merge_checks(commands: &str, checks: &str, lead: &str) -> Setup {
            let setup = Setup::new("cat tasks", 5);
            let herd = format!(
                "repo = {:?}\nafter_merge = {commands}\n\
                 [template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
                 prompt = \"review {{number}}\"\n{checks}\
                 [queue]\ntemplate = \"pr\"\nsource = \"cat tasks\"\nmax = 5\n{lead}",
                setup.repo.path().display().to_string(),
            );
            fs::write(setup.config.path().join("herds/prs.toml"), herd).unwrap();
            setup
        }

        fn read(&self, file: &str) -> Option<String> {
            fs::read_to_string(self.repo.path().join(file)).ok()
        }

        /// The text of each note in `name`'s mailbox.
        fn notes(&self, name: &str) -> Vec<String> {
            let mailbox = self.config.path().join(format!("reports/{name}.jsonl"));
            let Ok(text) = fs::read_to_string(mailbox) else {
                return vec![];
            };
            text.lines()
                .filter_map(|line| match serde_json::from_str(line).unwrap() {
                    Entry {
                        body: Body::Note { text },
                        ..
                    } => Some(text),
                    _ => None,
                })
                .collect()
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd")])
    }

    fn member(name: &str, herd: Option<&str>, retired: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p9".into(),
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired,
            retired_at: None,
            released_at: None,
            herd: herd.map(str::to_string),
            template: herd.map(|_| "pr".to_string()),
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            earlier: vec![],
        }
    }

    fn ledger(members: &[AgentRecord]) -> MemLedger {
        MemLedger(
            members
                .iter()
                .map(|m| (m.name.clone(), m.clone()))
                .collect(),
            vec![],
        )
    }

    #[test]
    fn fills_free_slots_in_source_order_counting_every_unretired_member() {
        let setup = Setup::new("printf '1\\n2\\n3\\n'", 2);
        let herdr = herdr();
        let mut ledger = ledger(&[
            member("zpr-9", Some("prs"), false),
            member("zpr-8", Some("prs"), true),
            member("other", Some("issues"), false),
            member("loner", None, false),
        ]);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "added zpr-1",
                "skipped: no slot zpr-2",
                "skipped: no slot zpr-3",
                "prs: 0 retired, 1 added, 2 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
        assert_eq!(herdr.prompts(), [("zpr-1".into(), "review 1".into())]);
        assert_eq!(
            ledger.0["zpr-1"].params,
            Some([("number".to_string(), "1".to_string())].into())
        );
    }

    #[test]
    fn skips_names_already_in_the_ledger_retired_or_not_and_repeats_in_the_source() {
        let setup = Setup::new("printf '# open PRs\\n\\n7\\n  8  \\n1\\n7\\n'", 5);
        let herdr = herdr();
        let mut ledger = ledger(&[member("zpr-1", Some("prs"), true)]);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "added zpr-7",
                "added zpr-8",
                "skipped: exists zpr-1",
                "skipped: exists zpr-7",
                "prs: 0 retired, 2 added, 2 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
    }

    #[test]
    fn a_released_name_is_fed_again_as_a_fresh_member() {
        let setup = Setup::new("printf '1\\n2\\n'", 5);
        let herdr = herdr();
        let mut released = member("zpr-1", Some("prs"), true);
        released.released_at = Some("2026-11-16T12:00:00Z".into());
        let mut ledger = ledger(&[released, member("zpr-2", Some("prs"), true)]);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "added zpr-1",
                "skipped: exists zpr-2",
                "prs: 0 retired, 1 added, 1 skipped, 0 errors",
            ]
        );
        let fresh = &ledger.0["zpr-1"];
        assert!(!fresh.retired);
        assert_eq!(fresh.released_at, None);
    }

    #[test]
    fn stats_keeps_a_released_members_earlier_run_beside_its_new_one() {
        let setup = Setup::new("printf '1\\n'", 5);
        let herdr = herdr();
        let mut released = member("zpr-1", Some("prs"), true);
        released.retired_at = Some("2026-11-02T00:00:00Z".into());
        released.released_at = Some("2026-11-16T12:00:00Z".into());
        let mut ledger = ledger(&[released]);

        setup.feed(&herdr, &mut ledger, false).unwrap();

        let reports = tempfile::tempdir().unwrap();
        let query = crate::stats::Query {
            herd: None,
            since: None,
            now: SystemTime::now(),
        };
        let (herds, _) = crate::stats::stats(&ledger, reports.path(), &query).unwrap();
        let earlier: Vec<_> = herds[0]
            .retired
            .iter()
            .map(|r| (r.name.as_str(), r.started.as_str(), r.total_secs))
            .collect();
        assert_eq!(earlier, [("zpr-1", "2026-11-01T00:00:00Z", Some(86_400))]);
        let open: Vec<_> = herds[0].open.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(open, ["zpr-1"]);
        assert_ne!(herds[0].open[0].started, "2026-11-01T00:00:00Z");
    }

    #[test]
    fn claimable_counts_a_released_name_as_free() {
        let setup = Setup::new("true", 5);
        let mut released = member("zpr-1", Some("prs"), true);
        released.released_at = Some("2026-11-16T12:00:00Z".into());
        let ledger = ledger(&[released, member("zpr-2", Some("prs"), true)]);
        let paths = setup.paths();
        let queue = crate::herd::queue(&paths, "prs").unwrap();

        let names = claimable(&ledger, &paths, &queue, "prs", 0, "1\n2\n").unwrap();

        assert_eq!(names, ["zpr-1"]);
    }

    #[test]
    fn a_line_with_the_wrong_param_count_is_an_error_and_the_run_continues() {
        let setup = Setup::new("printf '1 2\\n3\\n'", 5);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "error: 1 2: template pr takes 1 param (number), got 2",
                "added zpr-3",
                "prs: 0 retired, 1 added, 0 skipped, 1 error",
            ]
        );
        assert_eq!(fed.code, LINE_ERRORS_EXIT);
    }

    #[test]
    fn a_malformed_line_with_another_error_exits_1() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        let mut herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        herdr.fail_close_tab = true;
        setup.tasks("1 2\n");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines.last().unwrap(),
            "prs: 1 retired, 0 added, 0 skipped, 2 errors"
        );
        assert_eq!(fed.code, 1);
    }

    #[test]
    fn a_broken_template_is_not_a_bad_line_and_exits_1() {
        let setup = Setup::new("echo 1", 5);
        let herd = setup.config.path().join("herds/prs.toml");
        let toml = fs::read_to_string(&herd).unwrap();
        fs::write(
            &herd,
            toml.replace(
                "prompt = \"review {number}\"",
                "prompt_file = \"missing.md\"",
            ),
        )
        .unwrap();

        let fed = setup
            .feed(&herdr(), &mut MemLedger::default(), false)
            .unwrap();

        assert!(fed.lines[0].starts_with("error: 1: "), "{:?}", fed.lines);
        assert!(fed.lines[0].contains("prompt_file"), "{:?}", fed.lines);
        assert_eq!(fed.lines[1], "prs: 0 retired, 0 added, 0 skipped, 1 error");
        assert_eq!(fed.code, 1);
    }

    #[test]
    fn a_malformed_line_then_a_blocked_spawn_exits_blocked() {
        use crate::herdr::fake::Startup;
        let setup = Setup::new("printf '1 2\\n3\\n'", 5);
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;

        let fed = setup
            .feed(&herdr, &mut MemLedger::default(), false)
            .unwrap();

        assert_eq!(
            fed.lines.last().unwrap(),
            "prs: 0 retired, 1 added, 0 skipped, 1 error"
        );
        assert_eq!(fed.code, BLOCKED_EXIT);
    }

    #[test]
    fn a_malformed_line_then_a_failed_spawn_exits_1() {
        use crate::herdr::fake::Startup;
        let setup = Setup::new("printf '1 2\\n3\\n'", 5);
        let mut herdr = herdr();
        herdr.startup = Startup::Fails;

        let fed = setup
            .feed(&herdr, &mut MemLedger::default(), false)
            .unwrap();

        assert_eq!(
            fed.lines.last().unwrap(),
            "prs: 0 retired, 0 added, 0 skipped, 2 errors"
        );
        assert_eq!(fed.code, 1);
    }

    #[test]
    fn a_spawn_warning_is_passed_on_under_its_member() {
        let setup = Setup::new("printf '1\\n'", 1);
        let mut herdr = herdr();
        herdr.session_after_gets = 1;
        herdr.agent_get_fails_after_start = true;

        let fed = setup
            .feed(&herdr, &mut MemLedger::default(), false)
            .unwrap();

        assert_eq!(
            fed.lines,
            [
                "added zpr-1",
                "warning: zpr-1: could not read its session from herdr: herdr agent get zpr-1 failed; \
                 the Stop hook records it at the member's first stop",
                "prs: 0 retired, 1 added, 0 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
    }

    #[test]
    fn the_source_runs_in_the_herds_repo() {
        let setup = Setup::new("cat tasks", 5);
        fs::write(setup.repo.path().join("tasks"), "4\n").unwrap();
        let fed = setup
            .feed(&herdr(), &mut MemLedger::default(), false)
            .unwrap();
        assert_eq!(fed.lines[0], "added zpr-4");
    }

    #[test]
    fn a_failing_source_spawns_nothing() {
        let setup = Setup::new("echo 1; echo boom >&2; exit 2", 5);
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let err = setup.feed(&herdr, &mut ledger, false).unwrap_err();
        let err = format!("{err:#}");
        assert_eq!(
            err,
            "source: \"echo 1; echo boom >&2; exit 2\" failed (exit status: 2): boom"
        );
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn an_unreachable_herdr_spawns_nothing() {
        let setup = Setup::new("echo 1", 5);
        let mut herdr = herdr();
        herdr.unreachable = true;
        let mut ledger = MemLedger::default();
        let err = setup.feed(&herdr, &mut ledger, false).unwrap_err();
        assert_eq!(format!("{err:#}"), "herdr: herdr server not running");
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn a_failed_spawn_stops_the_run_after_that_task() {
        use crate::herdr::fake::Startup;
        let setup = Setup::new("printf '1\\n2\\n'", 5);
        let mut herdr = herdr();
        herdr.startup = Startup::Fails;
        let mut ledger = MemLedger::default();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines.len(), 2, "{:?}", fed.lines);
        assert!(
            fed.lines[0].starts_with("error: 1: start: "),
            "{:?}",
            fed.lines
        );
        assert!(fed.lines[0].ends_with("; stopping"), "{:?}", fed.lines);
        assert_eq!(fed.lines[1], "prs: 0 retired, 0 added, 0 skipped, 1 error");
        assert_eq!(fed.code, 1);
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn a_blocked_spawn_stops_the_run_after_that_task() {
        use crate::herdr::fake::Startup;
        let setup = Setup::new("printf '1\\n2\\n'", 5);
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;
        let mut ledger = MemLedger::default();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines[0], "added zpr-1");
        assert!(
            fed.lines[1].starts_with("blocked: answer it in pane "),
            "{:?}",
            fed.lines
        );
        assert!(
            fed.lines[1].ends_with("loopherd prompt zpr-1; stopping"),
            "{:?}",
            fed.lines
        );
        assert_eq!(fed.lines[2], "prs: 0 retired, 1 added, 0 skipped, 0 errors");
        assert_eq!(fed.lines.len(), 3);
        assert_eq!(fed.code, 3);
        assert_eq!(ledger.0.keys().collect::<Vec<_>>(), ["zpr-1"]);
    }

    #[test]
    fn dry_run_reports_what_would_happen_and_spawns_nothing() {
        let setup = Setup::new("printf '1\\n2\\n3\\n'", 2);
        let herdr = herdr();
        let mut ledger = ledger(&[member("zpr-1", Some("prs"), false)]);

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "skipped: exists zpr-1",
                "would add zpr-2 (template pr)",
                "skipped: no slot zpr-3",
                "prs: 0 would retire, 1 would add, 2 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
        assert!(herdr.tabs().is_empty());
        assert!(herdr.started().is_empty());
        assert_eq!(ledger.0.keys().collect::<Vec<_>>(), ["zpr-1"]);
    }

    #[test]
    fn only_a_dry_run_prints_a_succeeding_sources_stderr() {
        let setup = Setup::new("echo 1; echo 'skipped 2: blocked' >&2", 2);

        let dry = setup
            .feed(&herdr(), &mut MemLedger::default(), true)
            .unwrap();
        let real = setup
            .feed(&herdr(), &mut MemLedger::default(), false)
            .unwrap();

        assert_eq!(
            dry.lines,
            [
                "source: skipped 2: blocked",
                "would add zpr-1 (template pr)",
                "prs: 0 would retire, 1 would add, 0 skipped, 0 errors",
            ]
        );
        assert!(!real.lines.iter().any(|l| l.starts_with("source:")));
    }

    #[test]
    fn dry_run_makes_no_herdr_call() {
        let setup = Setup::with_done("printf '1\\n2\\n'", 1, "test -f done-{number}");
        setup.touch("done-1");
        let mut herdr = herdr();
        herdr.unreachable = true;
        let mut zpr = member("zpr-1", Some("prs"), false);
        zpr.params = Some([("number".to_string(), "1".to_string())].into());
        let mut ledger = ledger(&[zpr]);

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "would retire zpr-1",
                "skipped: exists zpr-1",
                "would add zpr-2 (template pr)",
                "prs: 1 would retire, 1 would add, 1 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
    }

    #[test]
    fn a_member_whose_done_check_exits_0_is_retired_and_closed_and_its_slot_refilled() {
        let setup = Setup::with_done("cat tasks", 1, "test -f done-{number}");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        setup.tasks("1\n");
        setup.feed(&herdr, &mut ledger, false).unwrap();
        let tab = herdr.tabs()[0].id.clone();
        setup.tasks("2\n");
        setup.touch("done-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                format!("retired zpr-1; closed tab {tab}"),
                "added zpr-2".into(),
                "prs: 1 retired, 1 added, 0 skipped, 0 errors".into(),
            ]
        );
        assert_eq!(fed.code, 0);
        assert!(ledger.0["zpr-1"].retired);
        assert_eq!(
            herdr.tabs().iter().map(|t| &t.label).collect::<Vec<_>>(),
            ["zpr-2"]
        );
    }

    #[test]
    fn retiring_a_done_member_removes_its_notify_marks_only() {
        let setup = Setup::with_done("cat tasks", 1, "test -f done-{number}");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        setup.tasks("1\n");
        setup.feed(&herdr, &mut ledger, false).unwrap();
        let mailboxes = setup.config.path().join("reports");
        fs::create_dir_all(&mailboxes).unwrap();
        for member in ["zpr-1", "zpr-9"] {
            fs::write(
                mailboxes.join(format!("{member}.notified")),
                "question t1\n",
            )
            .unwrap();
            fs::write(mailboxes.join(format!("{member}.blocked")), "").unwrap();
        }
        setup.touch("done-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert!(ledger.0["zpr-1"].retired, "{:?}", fed.lines);
        assert!(!mailboxes.join("zpr-1.notified").exists());
        assert!(!mailboxes.join("zpr-1.blocked").exists());
        assert!(mailboxes.join("zpr-9.notified").exists());
        assert!(mailboxes.join("zpr-9.blocked").exists());
        assert_eq!(fed.code, 0);
    }

    #[test]
    fn retiring_a_done_member_without_notify_marks_is_not_an_error() {
        let setup = Setup::with_done("cat tasks", 1, "test -f done-{number}");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        setup.tasks("1\n");
        setup.feed(&herdr, &mut ledger, false).unwrap();
        let tab = herdr.tabs()[0].id.clone();
        setup.touch("done-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines[0], format!("retired zpr-1; closed tab {tab}"));
        assert!(ledger.0["zpr-1"].retired);
        assert_eq!(fed.code, 0);
    }

    /// A ledger holding `zpr-1`, a member of `prs` spawned by an earlier feed.
    fn fed_one(setup: &Setup, herdr: &FakeHerdr) -> MemLedger {
        let mut ledger = MemLedger::default();
        setup.tasks("1\n");
        setup.feed(herdr, &mut ledger, false).unwrap();
        setup.tasks("");
        ledger
    }

    #[test]
    fn a_done_check_exiting_1_leaves_the_member_open_and_other_exits_warn() {
        for (done, line) in [
            ("exit 1", "still open zpr-1"),
            ("exit 2", "warning: check failed zpr-1: exit 2"),
            (
                "echo nope >&2; exit 127",
                "warning: check failed zpr-1: exit 127: nope",
            ),
            (
                "kill -9 $$",
                "warning: check failed zpr-1: signal: 9 (SIGKILL)",
            ),
        ] {
            let setup = Setup::with_done("cat tasks", 1, done);
            let herdr = herdr();
            let mut ledger = fed_one(&setup, &herdr);
            let before = ledger.0.clone();

            let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

            assert_eq!(
                fed.lines,
                [line, "prs: 0 retired, 0 added, 0 skipped, 0 errors"],
                "{done}"
            );
            assert_eq!(fed.code, 0, "{done}");
            assert_eq!(ledger.0, before, "{done}");
            assert_eq!(herdr.tabs().len(), 1, "{done}");
        }
    }

    #[test]
    fn a_hung_done_check_is_killed_at_the_timeout_and_warns() {
        let setup = Setup::with_done("cat tasks", 1, "sleep 30");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        let before = ledger.0.clone();
        let started = std::time::Instant::now();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            fed.lines[0],
            "warning: check failed zpr-1: timed out after 1s"
        );
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn a_done_check_that_leaves_a_child_running_does_not_hang_feed() {
        let setup = Setup::with_done("cat tasks", 1, "sleep 30 & exit 1");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        let started = std::time::Instant::now();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(fed.lines[0], "still open zpr-1");
    }

    #[test]
    fn members_without_params_or_a_done_check_are_skipped() {
        let setup = Setup::with_template(
            "cat tasks",
            5,
            "done = \"exit 0\"\n[template.chore]\nparams = []\nprompt = \"tidy\"\n",
        );
        setup.tasks("");
        let herdr = herdr();
        let mut chore = member("chore", Some("prs"), false);
        chore.template = Some("chore".into());
        chore.params = Some(Default::default());
        let mut gone = member("gone-template", Some("prs"), false);
        gone.template = Some("removed".into());
        let mut ledger = ledger(&[member("zpr-9", Some("prs"), false), chore, gone]);
        let before = ledger.0.clone();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "warning: zpr-9 has no recorded params; skipping its checks",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
            ]
        );
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn members_of_other_herds_and_retired_members_are_never_checked_or_retired() {
        let setup = Setup::with_done("cat tasks", 5, "touch ran-{name}; exit 0");
        setup.tasks("");
        let herdr = herdr();
        let params = Some([("number".to_string(), "1".to_string())].into());
        let mut other = member("other", Some("issues"), false);
        other.params = params.clone();
        let mut retired = member("zpr-1", Some("prs"), true);
        retired.params = params;
        let mut ledger = ledger(&[other, retired]);
        let before = ledger.0.clone();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines, ["prs: 0 retired, 0 added, 0 skipped, 0 errors"]);
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.mutations(), 0);
        let ran: Vec<_> = fs::read_dir(setup.repo.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with("ran-"))
            .collect();
        assert!(ran.is_empty(), "{ran:?}");
    }

    #[test]
    fn a_done_check_that_cannot_render_for_a_member_warns_and_leaves_it_alone() {
        let setup = Setup::with_done("cat tasks", 5, "exit 0");
        fs::write(
            setup.config.path().join("herds/prs.toml"),
            format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\", \"repo\"]\n\
                 prompt = \"go\"\ndone = \"gh pr view {{number}} -R {{repo}}\"\n\
                 [queue]\ntemplate = \"pr\"\nsource = \"true\"\nmax = 5\n",
                setup.repo.path().display().to_string()
            ),
        )
        .unwrap();
        let herdr = herdr();
        let mut zpr = member("zpr-1", Some("prs"), false);
        zpr.params = Some([("number".to_string(), "1".to_string())].into());
        let mut ledger = ledger(&[zpr]);
        let before = ledger.0.clone();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "warning: check failed zpr-1: unknown placeholder {repo}",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
            ]
        );
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn the_done_check_renders_params_built_ins_and_an_unrecorded_branch_from_the_template() {
        let setup = Setup::with_template(
            "cat tasks",
            5,
            "branch = \"review/{number}-{name}\"\n\
             done = \"printf '%s|' {number} {name} {herd} {branch} {worktree} {pane} > seen; exit 1\"\n",
        );
        setup.tasks("");
        let mut zpr = member("zpr-4", Some("prs"), false);
        zpr.params = Some([("number".to_string(), "4".to_string())].into());
        zpr.worktree = Some("/src/wt/zpr-4".into());
        let mut ledger = ledger(&[zpr]);

        let fed = setup.feed(&herdr(), &mut ledger, false).unwrap();

        assert_eq!(fed.lines[0], "still open zpr-4");
        assert_eq!(
            fs::read_to_string(setup.repo.path().join("seen")).unwrap(),
            "4|zpr-4|prs|review/4-zpr-4|/src/wt/zpr-4|w1:p9|"
        );
    }

    #[test]
    fn the_done_check_sees_the_branch_and_worktree_recorded_at_spawn() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let config = tempfile::tempdir().unwrap();
        fs::create_dir(config.path().join("herds")).unwrap();
        let write_herd = |branch: &str| {
            let herd = format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
                 branch = {branch:?}\nprompt = \"review {{number}}\"\n\
                 done = \"printf '%s|' {{branch}} {{worktree}} > seen; exit 1\"\n\
                 [queue]\ntemplate = \"pr\"\nsource = \"cat tasks\"\nmax = 1\n",
                repo.display().to_string(),
            );
            fs::write(config.path().join("herds/prs.toml"), herd).unwrap();
        };
        let paths = Paths {
            herds: config.path().join("herds"),
            home: PathBuf::from("/home/me"),
            feed_lock: config.path().join("feed.lock"),
            paused: config.path().join("paused"),
        };
        let req = FeedRequest {
            herd: "prs".into(),
            dry_run: false,
            ready_timeout: Duration::from_secs(60),
            command_timeout: Duration::from_secs(1),
            own_pane: None,
            own_socket: None,
            mailboxes: config.path().join("reports"),
            after_merge_timeout: Duration::from_secs(5),
            owner: OWNER,
            procs: &*LIVE,
        };
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        write_herd("old/{number}");
        fs::write(repo.join("tasks"), "7\n").unwrap();
        feed(&herdr, &mut ledger, &paths, &req, Some("w1")).unwrap();

        write_herd("new/{number}");
        let fed = feed(&herdr, &mut ledger, &paths, &req, Some("w1")).unwrap();

        assert_eq!(fed.lines[0], "still open zpr-7");
        let wt = root.path().join("proj/worktrees/old-7");
        assert_eq!(
            fs::read_to_string(repo.join("seen")).unwrap(),
            format!("old/7|{}|", wt.display())
        );
    }

    #[test]
    fn a_member_with_a_recorded_empty_branch_gets_an_empty_branch() {
        let setup = Setup::with_template(
            "cat tasks",
            5,
            "branch = \"review/{number}\"\ndone = \"printf '[%s]' {branch} > seen; exit 1\"\n",
        );
        setup.tasks("");
        let mut zpr = member("zpr-4", Some("prs"), false);
        zpr.params = Some([("number".to_string(), "4".to_string())].into());
        zpr.branch = Some(String::new());
        let mut ledger = ledger(&[zpr]);

        setup.feed(&herdr(), &mut ledger, false).unwrap();

        assert_eq!(
            fs::read_to_string(setup.repo.path().join("seen")).unwrap(),
            "[]"
        );
    }

    #[test]
    fn values_rendered_into_the_done_check_are_shell_quoted() {
        let setup = Setup::with_done("cat tasks", 5, "printf '%s\\n' {number} > seen; exit 1");
        setup.tasks("");
        for (i, value) in ["1;touch pwned", "$(touch pwned)", "`touch pwned`", "it's"]
            .into_iter()
            .enumerate()
        {
            let mut zpr = member(&format!("zpr-{i}"), Some("prs"), false);
            zpr.params = Some([("number".to_string(), value.to_string())].into());
            let mut ledger = ledger(&[zpr]);

            let fed = setup.feed(&herdr(), &mut ledger, false).unwrap();

            assert_eq!(fed.lines[0], format!("still open zpr-{i}"), "{value}");
            assert_eq!(
                fs::read_to_string(setup.repo.path().join("seen")).unwrap(),
                format!("{value}\n")
            );
            assert!(!setup.repo.path().join("pwned").exists(), "{value}");
        }
    }

    #[test]
    fn a_member_whose_tab_is_gone_is_still_checked_and_retired() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        setup.tasks("");
        let mut zpr = member("zpr-1", Some("prs"), false);
        zpr.params = Some([("number".to_string(), "1".to_string())].into());
        let mut ledger = ledger(&[zpr]);

        let fed = setup.feed(&herdr(), &mut ledger, false).unwrap();

        assert_eq!(fed.lines[0], "retired zpr-1; tab already gone");
        assert!(ledger.0["zpr-1"].retired);
    }

    #[test]
    fn reaping_keeps_retires_safety_rules_such_as_sparing_the_tab_running_feed() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        let tab = herdr.tabs()[0].id.clone();
        let req = FeedRequest {
            herd: "prs".into(),
            dry_run: false,
            ready_timeout: Duration::from_secs(60),
            command_timeout: Duration::from_secs(1),
            own_pane: Some(ledger.0["zpr-1"].pane_id.clone()),
            own_socket: None,
            mailboxes: PathBuf::from("/nonexistent/reports"),
            after_merge_timeout: Duration::from_secs(5),
            owner: OWNER,
            procs: &*LIVE,
        };

        let fed = feed(&herdr, &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(
            fed.lines[0],
            format!("retired zpr-1; kept tab {tab}: it is running this command")
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    /// A ledger that reads but refuses every write.
    struct ReadOnly(MemLedger);

    impl Ledger for ReadOnly {
        fn entries(&self) -> Result<std::collections::BTreeMap<String, AgentRecord>> {
            self.0.entries()
        }

        fn record(&mut self, _: AgentRecord) -> Result<()> {
            anyhow::bail!("read-only")
        }

        fn update(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            anyhow::bail!("read-only")
        }

        fn remove_herd(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
        ) -> anyhow::Result<crate::ledger::RemovedHerd> {
            unreachable!()
        }
        fn started(&mut self, _: &str, _: std::time::SystemTime, _resumed: bool) -> Result<()> {
            anyhow::bail!("read-only")
        }
    }

    #[test]
    fn a_retire_that_fails_before_marking_counts_only_as_an_error() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        let herdr = herdr();
        let mut ledger = ReadOnly(fed_one(&setup, &herdr));
        let req = FeedRequest {
            herd: "prs".into(),
            dry_run: false,
            ready_timeout: Duration::from_secs(60),
            command_timeout: Duration::from_secs(1),
            own_pane: None,
            own_socket: None,
            mailboxes: PathBuf::from("/nonexistent/reports"),
            after_merge_timeout: Duration::from_secs(5),
            owner: OWNER,
            procs: &*LIVE,
        };

        let fed = feed(&herdr, &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(
            fed.lines,
            [
                "error: retire zpr-1: error: read-only",
                "prs: 0 retired, 0 added, 0 skipped, 1 error",
            ]
        );
        assert_eq!(fed.code, 1);
        assert!(!ledger.0.0["zpr-1"].retired);
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn a_retire_that_fails_after_marking_counts_as_retired_and_as_an_error() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        let mut herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        herdr.fail_close_tab = true;
        setup.tasks("2\n");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert!(
            fed.lines[0].starts_with("error: retire zpr-1: retired zpr-1; error: "),
            "{:?}",
            fed.lines
        );
        assert_eq!(fed.lines[1], "added zpr-2");
        assert_eq!(fed.lines[2], "prs: 1 retired, 1 added, 0 skipped, 1 error");
        assert_eq!(fed.code, 1);
    }

    #[test]
    fn dry_run_runs_done_checks_retires_nothing_and_counts_the_freed_slot() {
        let setup = Setup::with_done("cat tasks", 1, "touch checked; exit 0");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.tasks("2\n");
        let before = ledger.0.clone();
        let mutations = herdr.mutations();

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "would retire zpr-1",
                "would add zpr-2 (template pr)",
                "prs: 1 would retire, 1 would add, 0 skipped, 0 errors",
            ]
        );
        assert!(setup.repo.path().join("checked").exists());
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.mutations(), mutations);
    }

    fn pause_herd(setup: &Setup) {
        let at = humantime::parse_rfc3339("2026-11-05T09:00:00Z").unwrap();
        let herds = crate::pause::Herds::One("prs");
        crate::pause::pause(&setup.paths(), herds, Some("freeze"), at).unwrap();
    }

    #[test]
    fn a_paused_herd_retires_finished_members_and_spawns_nothing() {
        let setup = Setup::with_done("cat tasks; touch source-ran", 2, "test -f done-{number}");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        let tab = herdr.tabs()[0].id.clone();
        fs::remove_file(setup.repo.path().join("source-ran")).unwrap();
        setup.tasks("2\n3\n");
        setup.touch("done-1");
        pause_herd(&setup);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                format!("retired zpr-1; closed tab {tab}"),
                "prs paused since 2026-11-05T09:00:00Z: spawned nothing".into(),
            ]
        );
        assert_eq!(fed.code, 0);
        assert!(ledger.0["zpr-1"].retired);
        assert_eq!(ledger.0.len(), 1);
        assert!(herdr.tabs().is_empty());
        assert!(!setup.repo.path().join("source-ran").exists());
    }

    #[test]
    fn a_paused_herds_dry_run_reports_what_it_would_retire_and_spawns_nothing() {
        let setup = Setup::with_done("cat tasks", 2, "test -f done-{number}");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.tasks("2\n");
        setup.touch("done-1");
        pause_herd(&setup);
        let mutations = herdr.mutations();

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "would retire zpr-1",
                "prs paused since 2026-11-05T09:00:00Z: spawned nothing",
            ]
        );
        assert!(!ledger.0["zpr-1"].retired);
        assert_eq!(herdr.mutations(), mutations);
    }

    #[test]
    fn a_paused_herd_whose_retire_fails_exits_1() {
        let setup = Setup::with_done("cat tasks", 1, "exit 0");
        let herdr = herdr();
        let mut ledger = ReadOnly(fed_one(&setup, &herdr));
        pause_herd(&setup);
        let req = FeedRequest {
            herd: "prs".into(),
            dry_run: false,
            ready_timeout: Duration::from_secs(60),
            command_timeout: Duration::from_secs(1),
            own_pane: None,
            own_socket: None,
            mailboxes: PathBuf::from("/nonexistent/reports"),
            after_merge_timeout: Duration::from_secs(5),
            owner: OWNER,
            procs: &*LIVE,
        };

        let fed = feed(&herdr, &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(
            fed.lines,
            [
                "error: retire zpr-1: error: read-only",
                "prs paused since 2026-11-05T09:00:00Z: spawned nothing",
            ]
        );
        assert_eq!(fed.code, 1);
    }

    #[test]
    fn an_unreadable_pause_retires_warns_and_spawns_nothing() {
        for dry_run in [false, true] {
            let setup = Setup::with_done("cat tasks; touch source-ran", 2, "test -f done-{number}");
            let herdr = herdr();
            let mut ledger = fed_one(&setup, &herdr);
            fs::remove_file(setup.repo.path().join("source-ran")).unwrap();
            setup.tasks("2\n");
            setup.touch("done-1");
            let paused = setup.paths().paused;
            fs::create_dir_all(&paused).unwrap();
            fs::write(paused.join("prs"), "").unwrap();

            let fed = setup.feed(&herdr, &mut ledger, dry_run).unwrap();

            assert_eq!(fed.lines.len(), 3, "{:?}", fed.lines);
            let warning = "warning: prs's pause is unreadable, counted as paused: parse ";
            assert!(fed.lines[0].starts_with(warning), "{:?}", fed.lines);
            let retired = if dry_run {
                "would retire zpr-1"
            } else {
                "retired zpr-1"
            };
            assert!(fed.lines[1].starts_with(retired), "{:?}", fed.lines);
            assert_eq!(fed.lines[2], "prs paused (time unknown): spawned nothing");
            assert_eq!(fed.code, 0);
            assert_eq!(ledger.0["zpr-1"].retired, !dry_run);
            assert_eq!(ledger.0.len(), 1);
            assert!(!setup.repo.path().join("source-ran").exists());
        }
    }

    #[test]
    fn a_resumed_herd_spawns_again() {
        let setup = Setup::new("cat tasks", 1);
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        setup.tasks("1\n");
        pause_herd(&setup);
        let herds = crate::pause::Herds::One("prs");
        let reports = setup.config.path().join("reports");
        crate::pause::resume(&setup.paths(), &ledger, &reports, herds).unwrap();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines[0], "added zpr-1");
    }

    #[test]
    fn a_hung_source_is_killed_at_the_timeout_and_changes_nothing() {
        let setup = Setup::new("sleep 30", 5);
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let started = std::time::Instant::now();

        let err = setup.feed(&herdr, &mut ledger, false).unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            format!("{err:#}"),
            "source: \"sleep 30\" timed out after 1s"
        );
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn a_source_that_leaves_a_child_running_does_not_hang_feed() {
        let setup = Setup::new("echo 1; sleep 30 &", 5);
        let started = std::time::Instant::now();

        let fed = setup
            .feed(&herdr(), &mut MemLedger::default(), false)
            .unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(fed.lines[0], "added zpr-1");
    }

    #[test]
    fn the_herds_lead_takes_no_slot_and_is_never_checked_or_retired() {
        let setup = Setup::with_done("printf '1\\n'", 1, "touch checked; true");
        let herdr = herdr().with_agent("lead-prs");
        let mut lead = member("lead-prs", Some("prs"), false);
        lead.role = Some(crate::ledger::Role::Lead);
        lead.params = Some(Default::default());
        let mut ledger = ledger(&[lead]);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "added zpr-1",
                "prs: 0 retired, 1 added, 0 skipped, 0 errors"
            ]
        );
        assert!(!ledger.0["lead-prs"].retired);
        assert!(!setup.repo.path().join("checked").exists());
    }

    #[test]
    fn a_herd_without_a_queue_is_an_error() {
        let setup = Setup::new("echo 1", 1);
        fs::write(
            setup.config.path().join("herds/prs.toml"),
            "repo = \"/r\"\n[template.pr]\nparams = []\nprompt = \"go\"\n",
        )
        .unwrap();
        let err = setup
            .feed(&herdr(), &mut MemLedger::default(), false)
            .unwrap_err();
        assert_eq!(format!("{err:#}"), "herd: prs has no [queue]");
    }

    #[test]
    fn a_routed_queue_spawns_each_task_from_the_template_its_first_word_names() {
        let setup = Setup::routed("printf 'card 1\\nresearch 2\\ncard 3\\n'", 2);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "added lh-1",
                "added research-2",
                "skipped: no slot lh-3",
                "prs: 0 retired, 2 added, 1 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
        assert_eq!(
            herdr.prompts(),
            [
                ("lh-1".into(), "build 1".into()),
                ("research-2".into(), "research 2".into())
            ]
        );
        assert_eq!(ledger.0["research-2"].template.as_deref(), Some("research"));
        assert_eq!(
            ledger.0["research-2"].params,
            Some([("card".to_string(), "2".to_string())].into())
        );
    }

    #[test]
    fn a_routed_task_naming_no_listed_template_is_a_bad_line() {
        let setup = Setup::routed("printf 'pr 1\\nresearch\\n7\\ncard 2\\n'", 5);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "error: pr 1: the queue has no template pr (it has: card, research)",
                "error: research: template research takes 1 param (card), got 0",
                "error: 7: the queue has no template 7 (it has: card, research)",
                "added lh-2",
                "prs: 0 retired, 1 added, 0 skipped, 3 errors",
            ]
        );
        assert_eq!(fed.code, LINE_ERRORS_EXIT);
    }

    #[test]
    fn a_routed_queue_counts_every_templates_members_under_one_max_and_reaps_each_by_its_own_done()
    {
        let setup = Setup::routed("printf 'card 3\\n'", 2);
        setup.touch("researched-2");
        let herdr = herdr().with_agent("lh-1").with_agent("research-2");
        let mut card = member("lh-1", Some("prs"), false);
        card.template = Some("card".into());
        card.params = Some([("card".to_string(), "1".to_string())].into());
        let mut research = member("research-2", Some("prs"), false);
        research.template = Some("research".into());
        research.params = Some([("card".to_string(), "2".to_string())].into());
        let mut ledger = ledger(&[card, research]);

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "still open lh-1",
                "would retire research-2",
                "would add lh-3 (template card)",
                "prs: 1 would retire, 1 would add, 0 skipped, 0 errors",
            ]
        );
    }

    #[test]
    fn a_queue_setting_both_template_and_templates_or_neither_is_an_error() {
        let setup = Setup::new("echo 1", 1);
        for (queue, message) in [
            (
                "template = \"pr\"\ntemplates = [\"pr\"]",
                "queue: set one of template and templates, not both",
            ),
            ("", "queue: set one of template and templates"),
        ] {
            fs::write(
                setup.config.path().join("herds/prs.toml"),
                format!(
                    "repo = \"/r\"\n[template.pr]\nparams = []\nprompt = \"go\"\n\
                     [queue]\n{queue}\nsource = \"echo 1\"\nmax = 1\n"
                ),
            )
            .unwrap();

            let err = setup
                .feed(&herdr(), &mut MemLedger::default(), false)
                .unwrap_err();

            assert!(format!("{err:#}").ends_with(message), "{err:#}");
        }
    }

    #[test]
    fn a_routed_queues_claimable_names_each_tasks_member_whatever_its_template() {
        let setup = Setup::routed("printf 'research 1\\nnope 2\\ncard 2\\ncard 3\\n'", 2);
        let paths = setup.paths();
        let queue = crate::herd::queue(&paths, "prs").unwrap();

        let (tasks, _) = run_source(&queue, Duration::from_secs(1)).unwrap();
        let names = claimable(&MemLedger::default(), &paths, &queue, "prs", 0, &tasks).unwrap();

        assert_eq!(names, ["research-1", "lh-2"]);
    }

    #[test]
    fn the_documented_routed_queue_sends_wayfinding_cards_to_their_own_template_on_the_herds_branches()
     {
        let setup = Setup::new("true", 1);
        let scripts = setup.repo.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        crate::test_fakes::executable(
            &scripts.join("trello"),
            "#!/bin/sh\nprintf 'LH-7\\tBacklog\\tready-for-agent,impl:feat\\tA feature\\n\
             LH-8\\tBacklog\\tready-for-agent,wayfinding:research\\tA question\\n\
             LH-9\\tBacklog\\tready-for-agent,wayfinding:grilling\\tA grilling\\n\
             LH-10\\tBacklog\\tready-for-agent,impl:fix,need-human\\tStuck\\n'\n",
        );
        fs::write(
            setup.config.path().join("herds/prs.toml"),
            format!(
                "repo = {:?}\n[template.card]\nparams = [\"card\", \"type\", \"slug\"]\n\
                 name = \"lh-{{card}}\"\nbranch = \"{{type}}/LH-{{card}}-{{slug}}\"\n\
                 prompt = \"build {{card}}\"\n{}",
                setup.repo.path().display().to_string(),
                include_str!("../docs/examples/loopherd-cards-routed.toml")
            ),
        )
        .unwrap();

        let fed = setup
            .feed(&herdr(), &mut MemLedger::default(), true)
            .unwrap();

        assert_eq!(
            fed.lines,
            [
                "would add lh-7 (template card)",
                "would add research-8 (template wayfinding)",
                "prs: 0 would retire, 2 would add, 0 skipped, 0 errors",
            ]
        );
        let queue = crate::herd::queue(&setup.paths(), "prs").unwrap();
        let (output, _) = run_source(&queue, Duration::from_secs(1)).unwrap();
        let checkouts: Vec<_> = output
            .lines()
            .map(|task| {
                let (template, params) = task_params(&queue, task).unwrap();
                let req = FeedRequest {
                    herd: "prs".into(),
                    dry_run: true,
                    ready_timeout: Duration::ZERO,
                    command_timeout: Duration::from_secs(1),
                    own_pane: None,
                    own_socket: None,
                    mailboxes: PathBuf::from("/nonexistent/reports"),
                    after_merge_timeout: Duration::from_secs(5),
                    owner: OWNER,
                    procs: &*LIVE,
                };
                task_request(&setup.paths(), &req, template, params)
                    .unwrap()
                    .worktree
            })
            .collect();
        assert_eq!(
            checkouts,
            [
                Some(Checkout::Branch("feat/LH-7-a-feature".into())),
                Some(Checkout::Branch("docs/LH-8-a-question".into())),
            ]
        );
    }

    #[test]
    fn after_merge_runs_once_in_order_in_the_repo_the_first_time_merged_passes() {
        let setup = Setup::after_merge(r#"["echo one >> log", "echo two >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);

        let unmerged = setup.feed(&herdr, &mut ledger, false).unwrap();
        setup.touch("merged-1");
        let merged = setup.feed(&herdr, &mut ledger, false).unwrap();
        let again = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(unmerged.lines[0], "still open zpr-1");
        assert_eq!(
            merged.lines,
            [
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
                "after_merge zpr-1: ok",
            ]
        );
        assert_eq!(merged.code, 0);
        assert_eq!(again.lines[0], "still open zpr-1");
        assert_eq!(setup.read("log").unwrap(), "one\ntwo\n");
        assert_eq!(ledger.0["zpr-1"].after_merge, Some(AfterMerge::Noted));
        assert_eq!(setup.notes("zpr-1"), ["after_merge ok"]);
    }

    #[test]
    fn after_merge_runs_before_the_done_check_reaps_the_member_and_never_again() {
        let setup = Setup::after_merge(r#"["echo ran >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");
        setup.touch("done-1");

        let first = setup.feed(&herdr, &mut ledger, false).unwrap();
        let second = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            first.lines,
            [
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
                "after_merge zpr-1: ok",
            ]
        );
        assert!(
            second.lines[0].starts_with("retired zpr-1"),
            "{:?}",
            second.lines
        );
        assert!(ledger.0["zpr-1"].retired);
        assert_eq!(setup.read("log").unwrap(), "ran\n");
    }

    #[test]
    fn a_paused_herd_still_runs_after_merge() {
        let setup = Setup::after_merge(r#"["echo ran >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        pause_herd(&setup);
        setup.touch("merged-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(fed.lines.last().unwrap(), "after_merge zpr-1: ok");
        assert!(
            fed.lines.iter().any(|l| l.starts_with("prs paused ")),
            "{:?}",
            fed.lines
        );
        assert_eq!(setup.read("log").unwrap(), "ran\n");
    }

    #[test]
    fn dry_run_prints_the_after_merge_commands_and_runs_none() {
        let setup = Setup::after_merge(r#"["echo one >> log", "echo two >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");
        let before = ledger.0.clone();

        let fed = setup.feed(&herdr, &mut ledger, true).unwrap();

        assert_eq!(
            fed.lines,
            [
                "would run after_merge for zpr-1: echo one >> log; echo two >> log",
                "prs: 0 would retire, 0 would add, 0 skipped, 0 errors",
            ]
        );
        assert_eq!(setup.read("log"), None);
        assert_eq!(ledger.0, before);
        assert!(setup.notes("zpr-1").is_empty());
    }

    #[test]
    fn a_failing_after_merge_command_stops_the_rest_and_notes_its_exit_and_last_20_lines() {
        let failing = "seq 1 25; echo oops >&2; exit 3";
        let setup = Setup::after_merge(
            &format!(r#"["echo one >> log", "{failing}", "echo three >> log"]"#),
            "",
        );
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "prs: 0 retired, 0 added, 0 skipped, 0 errors".to_string(),
                format!("after_merge zpr-1: failed: `{failing}` exit 3"),
            ]
        );
        assert_eq!(fed.code, 1);
        assert_eq!(setup.read("log").unwrap(), "one\n");
        let tail: Vec<String> = (7..=25).map(|n| n.to_string()).collect();
        assert_eq!(
            setup.notes("zpr-1"),
            [format!(
                "after_merge failed: `{failing}` exit 3\n{}\noops",
                tail.join("\n")
            )]
        );
        assert_eq!(ledger.0["zpr-1"].after_merge, Some(AfterMerge::Noted));
    }

    #[test]
    fn a_timed_out_after_merge_command_still_notes_its_output() {
        let setup = Setup::after_merge(r#"["echo started; sleep 30"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        let why = "`echo started; sleep 30` timed out after 2s";
        assert_eq!(fed.lines[1], format!("after_merge zpr-1: failed: {why}"));
        assert_eq!(
            setup.notes("zpr-1"),
            [format!("after_merge failed: {why}\nstarted")]
        );
    }

    #[test]
    fn a_failed_commands_note_keeps_only_the_end_of_a_huge_line() {
        let command = "head -c 100000 /dev/zero | tr '\\\\0' x; exit 1";
        let setup = Setup::after_merge(&format!(r#"["{command}"]"#), "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");

        setup.feed(&herdr, &mut ledger, false).unwrap();

        let notes = setup.notes("zpr-1");
        let (head, tail) = notes[0].split_once('\n').unwrap();
        assert!(head.starts_with("after_merge failed: "), "{head}");
        assert_eq!(tail, format!("…{}", "x".repeat(TAIL_BYTES)));
    }

    #[test]
    fn a_second_feed_keeps_a_member_whose_after_merge_is_still_running() {
        let setup = Setup::after_merge(r#"["echo ran >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");
        setup.touch("done-1");
        let req = setup.request(false, None);

        let mut first = feed(&herdr, &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();
        let second = setup.feed(&herdr, &mut ledger, false).unwrap();
        run_after_merge(&mut ledger, &setup.paths(), &req, &mut first).unwrap();

        assert_eq!(
            second.lines,
            [
                "after_merge zpr-1: running in feed 7",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
            ]
        );
        assert!(!ledger.0["zpr-1"].retired, "retired before its note");
        assert_eq!(setup.notes("zpr-1"), ["after_merge ok"]);
        assert_eq!(setup.read("log").unwrap(), "ran\n");

        let third = setup.feed(&herdr, &mut ledger, false).unwrap();
        assert!(ledger.0["zpr-1"].retired, "{:?}", third.lines);
    }

    #[test]
    fn a_feed_finding_after_merge_left_by_a_dead_feed_notes_it_interrupted_and_runs_nothing() {
        let setup = Setup::after_merge(r#"["echo ran >> log"]"#, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);
        setup.touch("merged-1");
        setup.touch("done-1");
        let reused_pid = Process {
            pid: OWNER.pid,
            start: OWNER.start + 1,
        };
        let pending = Some(AfterMerge::Pending { owner: reused_pid });
        ledger.0.get_mut("zpr-1").unwrap().after_merge = pending;

        let dry = setup.feed(&herdr, &mut ledger, true).unwrap();
        assert_eq!(dry.lines[0], "would note after_merge zpr-1: interrupted");
        assert_eq!(ledger.0["zpr-1"].after_merge, pending);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "after_merge zpr-1: interrupted",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
            ]
        );
        assert_eq!(
            setup.notes("zpr-1"),
            [
                "after_merge interrupted: the feed running it exited before noting an outcome; \
                 check what ran and finish its commands by hand"
            ]
        );
        assert_eq!(ledger.0["zpr-1"].after_merge, Some(AfterMerge::Noted));
        assert_eq!(setup.read("log"), None);

        setup.feed(&herdr, &mut ledger, false).unwrap();
        assert!(ledger.0["zpr-1"].retired);
    }

    /// A feed's after_merge of `commands`, left pending for `members`.
    fn pending_fed(setup: &Setup, commands: &[&str], members: &[&str]) -> Fed {
        Fed {
            lines: vec![],
            code: 0,
            pending: Some(Pending {
                repo: setup.repo.path().into(),
                commands: commands.iter().map(|c| c.to_string()).collect(),
                members: members.iter().map(|m| m.to_string()).collect(),
            }),
        }
    }

    /// Runs `fed`'s after_merge with each of its members in the ledger.
    fn run_pending(setup: &Setup, mut fed: Fed) -> Vec<String> {
        let names = fed.pending.as_ref().unwrap().members.clone();
        let records: Vec<_> = names
            .iter()
            .map(|m| member(m, Some("prs"), false))
            .collect();
        let req = setup.request(false, None);
        run_after_merge(&mut ledger(&records), &setup.paths(), &req, &mut fed).unwrap();
        fed.lines
    }

    /// Polls `ready` until it holds, failing after 10s.
    fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "never {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_second_feeds_after_merge_waits_for_the_first_to_finish() {
        let setup = Setup::new("true", 1);
        let commands = |mark: &str| {
            [
                "mkdir busy".to_string(),
                format!("echo {mark} >> log; touch started-{mark}; sleep 0.3"),
                "rmdir busy".to_string(),
            ]
        };
        let (x, y) = (commands("x"), commands("y"));
        let x: Vec<&str> = x.iter().map(String::as_str).collect();
        let y: Vec<&str> = y.iter().map(String::as_str).collect();

        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| run_pending(&setup, pending_fed(&setup, &x, &["zpr-1"])));
            wait_for("started", || setup.read("started-x").is_some());
            let second = scope.spawn(|| run_pending(&setup, pending_fed(&setup, &y, &["zpr-2"])));
            (first.join().unwrap(), second.join().unwrap())
        });

        assert_eq!(first, ["after_merge zpr-1: ok"]);
        assert_eq!(second, ["after_merge zpr-2: ok"]);
        assert_eq!(setup.read("log").unwrap(), "x\ny\n");
    }

    #[test]
    fn a_feed_holds_the_after_merge_lock_from_its_first_member_to_its_last() {
        let setup = Setup::new("true", 1);
        let members: Vec<String> = (1..=20).map(|n| format!("zpr-{n}")).collect();
        let members: Vec<&str> = members.iter().map(String::as_str).collect();
        let lock = setup.config.path().join("after-merge.lock");
        let ran = || setup.read("runs").map_or(0, |runs| runs.lines().count());

        let free_after = std::thread::scope(|scope| {
            let feed = pending_fed(&setup, &["echo >> runs"], &members);
            let feeding = scope.spawn(|| run_pending(&setup, feed));
            wait_for("ran", || ran() > 0);
            let file = fs::File::create(&lock).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            // Spins rather than waits: a feed retaking the lock between members wins any waiter.
            let free_after = loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the lock never came free"
                );
                if let Some(_held) = Lock::try_take(file.try_clone().unwrap()).unwrap() {
                    break ran();
                }
            };
            feeding.join().unwrap();
            free_after
        });

        assert_eq!(
            free_after,
            members.len(),
            "the lock came free between members"
        );
    }

    /// Reports every process gone, after marking `zpr-1`'s after_merge noted
    /// in the ledger at its path, as the feed that owned it would on exiting.
    struct OwnerFinishesFirst(PathBuf);

    impl Procs for OwnerFinishesFirst {
        fn exe(&self, _: u32) -> Option<PathBuf> {
            None
        }
        fn parent(&self, _: u32) -> Option<u32> {
            None
        }
        fn start(&self, _: u32) -> Option<u64> {
            let mut ledger = crate::ledger::FileLedger::new(self.0.clone());
            set_after_merge(&mut ledger, "zpr-1", AfterMerge::Noted).unwrap();
            None
        }
        fn cmdline(&self, _: u32) -> Option<Vec<String>> {
            None
        }
        fn resolve(&self, _: u32, _: &str) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn a_feed_never_notes_interrupted_an_after_merge_its_owner_noted_meanwhile() {
        let setup = Setup::after_merge(r#"["echo ran >> log"]"#, "");
        setup.tasks("");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut ledger = crate::ledger::FileLedger::new(path.clone());
        let owner = Process { pid: 8, start: 80 };
        ledger
            .record(AgentRecord {
                after_merge: Some(AfterMerge::Pending { owner }),
                ..member("zpr-1", Some("prs"), false)
            })
            .unwrap();
        let procs = OwnerFinishesFirst(path);
        let req = FeedRequest {
            procs: &procs,
            ..setup.request(false, None)
        };

        let fed = feed(&herdr(), &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(fed.lines, ["prs: 0 retired, 0 added, 0 skipped, 0 errors"]);
        assert_eq!(setup.notes("zpr-1"), Vec::<String>::new());
        let record = &ledger.entries().unwrap()["zpr-1"];
        assert_eq!(record.after_merge, Some(AfterMerge::Noted));
        assert!(!record.retired);
    }

    #[test]
    fn an_outcome_the_ledger_cant_record_is_printed_and_not_noted() {
        let setup = Setup::new("true", 1);
        let mut fed = pending_fed(&setup, &["true"], &["zpr-1"]);
        let req = setup.request(false, None);

        run_after_merge(&mut MemLedger::default(), &setup.paths(), &req, &mut fed).unwrap();

        assert_eq!(fed.lines[0], "after_merge zpr-1: ok");
        assert!(
            fed.lines[1].starts_with("warning: ledger: zpr-1: "),
            "{:?}",
            fed.lines
        );
        assert!(fed.lines[1].ends_with("; not noted"), "{:?}", fed.lines);
        assert_eq!(setup.notes("zpr-1"), Vec::<String>::new());
    }

    #[test]
    fn a_merged_check_error_keeps_the_member_and_never_runs_after_merge() {
        let checks = "done = \"exit 0\"\nmerged = \"exit 2\"\n";
        let setup = Setup::after_merge_checks(r#"["echo ran >> log"]"#, checks, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines,
            [
                "warning: merged check failed zpr-1: exit 2; keeping it until after_merge runs",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors",
            ]
        );
        assert!(!ledger.0["zpr-1"].retired);
        assert_eq!(ledger.0["zpr-1"].after_merge, None);
        assert_eq!(setup.read("log"), None);
    }

    #[test]
    fn a_member_done_before_its_merged_check_passes_retires_with_a_warning_and_a_note() {
        let checks = "done = \"exit 0\"\nmerged = \"exit 1\"\n";
        let setup = Setup::after_merge_checks(r#"["echo ran >> log"]"#, checks, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        let never = "its done check passed before its merged check did, so after_merge never ran for it; if it merged, run its commands by hand";
        assert_eq!(fed.lines[0], format!("warning: zpr-1: {never}"));
        assert!(fed.lines[1].starts_with("retired zpr-1"), "{:?}", fed.lines);
        assert_eq!(
            setup.notes("zpr-1"),
            [format!("after_merge never ran: {never}")]
        );
        assert_eq!(setup.read("log"), None);
    }

    #[test]
    fn a_template_without_a_merged_check_warns_when_the_herd_has_after_merge() {
        let checks = "done = \"exit 1\"\n";
        let setup = Setup::after_merge_checks(r#"["echo ran >> log"]"#, checks, "");
        let herdr = herdr();
        let mut ledger = fed_one(&setup, &herdr);

        let fed = setup.feed(&herdr, &mut ledger, false).unwrap();

        assert_eq!(
            fed.lines[0],
            "warning: template pr has no merged check, so after_merge never runs for its members"
        );
    }

    /// `ledger` with `lead-prs`, the lead of `herd`, in pane `w1:p0` at `/s`.
    fn with_lead(mut ledger: MemLedger, herd: &str) -> MemLedger {
        let mut lead = member("lead-prs", Some(herd), false);
        lead.pane_id = "w1:p0".into();
        lead.socket = Some("/s".into());
        lead.template = None;
        lead.role = Some(crate::ledger::Role::Lead);
        ledger.0.insert(lead.name.clone(), lead);
        ledger
    }

    /// The lines and the member's notes when a feed from the lead's pane
    /// finds `zpr-1` merged, the herd's after_merge being `command` after an
    /// `echo`, and the lead leading `lead_herd`.
    fn held_back(command: &str, lead_herd: &str) -> (Vec<String>, Vec<String>, Setup) {
        let commands = format!(r#"["echo one >> log", "{command}"]"#);
        let setup = Setup::after_merge(&commands, "[lead]\nprompt = \"go\"\n");
        let herdr = herdr();
        let mut ledger = with_lead(fed_one(&setup, &herdr), lead_herd);
        setup.touch("merged-1");

        let fed = setup
            .feed_from(&herdr, &mut ledger, false, Some("w1:p0"))
            .unwrap();

        assert_eq!(
            ledger.0["zpr-1"].after_merge,
            Some(AfterMerge::Noted),
            "{command}"
        );
        assert_eq!(setup.read("log"), None, "{command}");
        let notes = setup.notes("zpr-1");
        (fed.lines, notes, setup)
    }

    #[test]
    fn from_a_leads_pane_after_merge_needing_a_switch_that_is_off_runs_none_and_notes_it() {
        for (command, switch) in [
            ("scripts/install --no-extras", "install"),
            ("git pull --rebase", "rebase"),
            ("gh pr merge 7 --merge", "merge"),
        ] {
            let (lines, notes, _setup) = held_back(command, "prs");

            let why = format!("`{command}` needs the {switch} switch, off in prs's [lead]");
            assert_eq!(lines[0], format!("after_merge zpr-1: not run: {why}"));
            assert_eq!(
                notes,
                [format!(
                    "after_merge not run: {why}; run its commands by hand, or turn {switch} on"
                )]
            );
        }
    }

    #[test]
    fn from_a_leads_pane_commands_no_switch_allows_say_a_lead_can_never_run_them() {
        for (command, denied) in [
            ("loopherd pause prs", "pause"),
            ("loopherd restart zpr-2 --force", "restart --force"),
        ] {
            let (_, notes, _setup) = held_back(command, "prs");

            assert_eq!(
                notes,
                [format!(
                    "after_merge not run: `{command}` is `loopherd {denied}`, which a lead can never run; run its commands by hand"
                )]
            );
        }
    }

    #[test]
    fn from_a_lead_whose_herd_file_is_unreadable_every_gated_command_is_held_back() {
        let (_, notes, _setup) = held_back("scripts/install", "gone");

        let note = &notes[0];
        assert!(
            note.starts_with(
                "after_merge not run: `scripts/install` needs the install switch, off in gone's [lead] \
                 (its switches couldn't be read, so every switch counts as off: herd: "
            ),
            "{note}"
        );
    }

    #[test]
    fn after_merge_runs_from_a_lead_whose_switch_is_on_or_from_any_other_pane() {
        let commands = r#"["echo one >> log", "scripts/install --no-extras"]"#;
        for (lead, pane) in [
            ("[lead]\nprompt = \"go\"\ninstall = true\n", Some("w1:p0")),
            ("[lead]\nprompt = \"go\"\n", Some("w1:p7")),
            ("[lead]\nprompt = \"go\"\n", None),
        ] {
            let setup = Setup::after_merge(commands, lead);
            let install = setup.repo.path().join("scripts/install");
            fs::create_dir(install.parent().unwrap()).unwrap();
            crate::test_fakes::executable(&install, "#!/bin/sh\necho installed \"$@\" >> log\n");
            let herdr = herdr();
            let mut ledger = with_lead(fed_one(&setup, &herdr), "prs");
            setup.touch("merged-1");

            let fed = setup.feed_from(&herdr, &mut ledger, false, pane).unwrap();

            assert_eq!(fed.lines[1], "after_merge zpr-1: ok", "{lead} {pane:?}");
            assert_eq!(setup.read("log").unwrap(), "one\ninstalled --no-extras\n");
        }
    }
}
