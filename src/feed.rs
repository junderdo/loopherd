//! `loopherd feed`: fill a herd's free slots from its queue's source (D14).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::herd::{AddRequest, MemberCheck, Paths, Queue, lead_switches, members, queue, resolve};
use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger};
use crate::pause;
use crate::report::{Body, Entry, append};
use crate::retire::retire_and_close;
use crate::shell::run_within;
use crate::spawn::{BLOCKED_EXIT, spawn};
use crate::tool_hook::{Pane, lead_in, needs};

/// Feed's exit code when every error was a malformed source line.
pub const LINE_ERRORS_EXIT: i32 = 4;

pub struct FeedRequest {
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
}

/// One line per task, then a summary, and the exit code.
#[derive(Debug)]
pub struct Fed {
    pub lines: Vec<String>,
    pub code: i32,
}

/// Retires members whose done check passes, then spawns a member per task
/// until the herd's slots are full. A paused herd only retires; its source
/// isn't run.
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
        return Ok(Fed { lines, code });
    }
    let output = run_source(&queue, req.command_timeout).context("source")?;
    let reaped = reap(herdr, ledger, paths, &queue, req, &mut lines)?;
    let mut errors = reaped.errors;

    let held = members(ledger, &req.herd)?;
    let held = if req.dry_run {
        held.saturating_sub(reaped.retired)
    } else {
        held
    };
    let mut free = queue.max.saturating_sub(held);
    let mut taken: HashSet<String> = ledger.entries().context("ledger")?.into_keys().collect();

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
    Ok(Fed { lines, code })
}

/// The names feed would spawn now, as `feed --dry-run` plans them: tasks
/// with no member, while slots are free under `max`, `freed` being the
/// members whose done check passes. Fails if the source fails; a task feed
/// would error on is skipped.
pub fn claimable(
    ledger: &dyn Ledger,
    paths: &Paths,
    queue: &Queue,
    herd: &str,
    freed: usize,
    timeout: Duration,
) -> Result<Vec<String>> {
    let output = run_source(queue, timeout)?;
    let entries = ledger.entries().context("ledger")?;
    let mut free = queue
        .max
        .saturating_sub(members(ledger, herd)?.saturating_sub(freed));
    let mut taken: HashSet<String> = entries.into_keys().collect();
    let req = FeedRequest {
        herd: herd.into(),
        dry_run: true,
        ready_timeout: Duration::ZERO,
        command_timeout: timeout,
        own_pane: None,
        own_socket: None,
        mailboxes: PathBuf::new(),
        after_merge_timeout: Duration::ZERO,
    };
    let mut names = vec![];
    for task in output.lines().map(str::trim) {
        if free == 0 {
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
            free -= 1;
        }
    }
    Ok(names)
}

struct Reaped {
    retired: usize,
    errors: usize,
}

/// Runs the herd's after_merge for each active member whose merged check
/// first passes, then the done check of each other member that has one,
/// retiring and closing those it passes, one line per member to `lines`.
/// A member whose after_merge just ran keeps its slot until the next feed,
/// so a watch sees its note before it retires.
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
    };
    let mut held_back = None;
    if !queue.after_merge.is_empty() {
        held_back = gate(ledger, paths, req, &queue.after_merge)?;
    }
    let members = ledger.entries().context("ledger")?.into_values();
    let workers = members.filter(|e| !e.retired && !e.is_lead());
    for member in workers.filter(|e| e.herd.as_deref() == Some(&req.herd)) {
        let name = &member.name;
        let template = member.template.as_deref();
        let merged = template
            .and_then(|t| queue.merged.get(t))
            .filter(|_| !queue.after_merge.is_empty() && !member.after_merge_ran);
        let done = template.and_then(|t| queue.done.get(t));
        if merged.is_none() && done.is_none() {
            continue;
        }
        let Some(params) = &member.params else {
            lines.push(format!(
                "warning: {name} has no recorded params; skipping its done check"
            ));
            continue;
        };
        if let Some(check) = merged {
            let ran = after_merge(
                ledger, queue, req, &member, check, params, &held_back, lines,
            )?;
            match ran {
                AfterMerge::NotMerged => {}
                AfterMerge::Ran { failed } => {
                    reaped.errors += usize::from(failed);
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
                match retire_and_close(herdr, ledger, name, SystemTime::now(), own_pane, &mut out) {
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

/// A command of after_merge that needs a switch the lead running feed has off.
struct HeldBack {
    command: String,
    switch: &'static str,
    herd: String,
}

impl HeldBack {
    fn why(&self) -> String {
        format!(
            "`{}` needs the {} switch, off in {}'s [lead]",
            self.command, self.switch, self.herd
        )
    }
}

/// Which of `commands` may not run: feed runs in a lead's pane, and it needs
/// a switch that lead's herd file leaves off, as the tool hook would judge it.
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
    // An unreadable herd file counts every switch off, as in the tool hook.
    let switches = lead_switches(paths, &herd).unwrap_or_default();
    Ok(commands.iter().find_map(|command| {
        let off = needs(command).into_iter().find(|s| !s.on(&switches))?;
        Some(HeldBack {
            command: command.clone(),
            switch: off.name(),
            herd: herd.clone(),
        })
    }))
}

enum AfterMerge {
    NotMerged,
    /// Ran, or would have in a dry run, or was held back by a switch.
    Ran {
        failed: bool,
    },
}

/// Runs `queue.after_merge` for `member` if its merged `check` passes,
/// recording in the ledger first so a crash can't run it twice, and notes
/// the outcome in its mailbox. `held_back` runs none of it, noting why.
#[allow(clippy::too_many_arguments)]
fn after_merge(
    ledger: &mut dyn Ledger,
    queue: &Queue,
    req: &FeedRequest,
    member: &AgentRecord,
    check: &MemberCheck,
    params: &std::collections::BTreeMap<String, String>,
    held_back: &Option<HeldBack>,
    lines: &mut Vec<String>,
) -> Result<AfterMerge> {
    let name = &member.name;
    let merged = match check.render(member, params) {
        Ok(command) => run_check(&command, &queue.repo, req.command_timeout),
        Err(e) => Err(format!("{e:#}")),
    };
    match merged {
        Ok(true) => {}
        Ok(false) => return Ok(AfterMerge::NotMerged),
        Err(why) => {
            lines.push(format!("warning: merged check failed {name}: {why}"));
            return Ok(AfterMerge::NotMerged);
        }
    }
    if req.dry_run {
        lines.push(match held_back {
            Some(held) => format!("would not run after_merge for {name}: {}", held.why()),
            None => format!(
                "would run after_merge for {name}: {}",
                queue.after_merge.join("; ")
            ),
        });
        return Ok(AfterMerge::Ran { failed: false });
    }
    ledger
        .update(name, &mut |r| {
            r.after_merge_ran = true;
            Ok(())
        })
        .context("ledger")?;
    let (line, note, failed) = match held_back {
        Some(held) => (
            format!("not run: {}", held.why()),
            format!(
                "after_merge not run: {}; run its commands by hand, or turn {} on",
                held.why(),
                held.switch
            ),
            false,
        ),
        None => match run_commands(&queue.after_merge, &queue.repo, req.after_merge_timeout) {
            None => ("ok".into(), "after_merge ok".into(), false),
            Some((why, tail)) => (
                format!("failed: {why}"),
                format!("after_merge failed: {why}{tail}"),
                true,
            ),
        },
    };
    lines.push(format!("after_merge {name}: {line}"));
    let entry = Entry::new(SystemTime::now(), Body::Note { text: note });
    if let Err(e) = append(&req.mailboxes, name, &entry) {
        lines.push(format!("warning: note to {name}'s mailbox: {e:#}"));
    }
    Ok(AfterMerge::Ran { failed })
}

/// Lines of a failed command's output a note keeps.
const TAIL_LINES: usize = 20;

/// Runs `commands` in order in `repo`, stopping at the first failure, which
/// it returns as the command and its exit, and its output's last lines.
fn run_commands(commands: &[String], repo: &Path, timeout: Duration) -> Option<(String, String)> {
    for command in commands {
        let ran = run_within(
            Command::new("sh")
                .arg("-c")
                // One stream, so the tail interleaves stdout and stderr as written.
                .arg(format!("exec 2>&1\n{command}"))
                .current_dir(repo),
            timeout,
        );
        let (why, output) = match ran {
            Ok(Some(output)) if output.status.success() => continue,
            Ok(Some(output)) => {
                let status = match output.status.code() {
                    Some(code) => format!("exit {code}"),
                    None => output.status.to_string(),
                };
                (status, output.stdout)
            }
            Ok(None) => (
                format!("timed out after {}", humantime::format_duration(timeout)),
                vec![],
            ),
            Err(e) => (
                format!("could not run in {}: {e:#}", repo.display()),
                vec![],
            ),
        };
        let output = String::from_utf8_lossy(&output);
        let lines: Vec<&str> = output.lines().collect();
        let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        let tail = if tail.is_empty() {
            tail
        } else {
            format!("\n{tail}")
        };
        return Some((format!("`{command}` {why}"), tail));
    }
    None
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

/// The source's stdout; fails if it exits non-zero or outruns `timeout`.
fn run_source(queue: &Queue, timeout: Duration) -> Result<String> {
    let output = run_within(
        Command::new("sh")
            .arg("-c")
            .arg(&queue.source)
            .current_dir(&queue.repo),
        timeout,
    )
    .with_context(|| format!("run in {}", queue.repo.display()))?
    .with_context(|| {
        format!(
            "{:?} timed out after {}",
            queue.source,
            humantime::format_duration(timeout)
        )
    })?;
    if !output.status.success() {
        let failed = format!("{:?} failed ({})", queue.source, output.status);
        match String::from_utf8_lossy(&output.stderr).trim() {
            "" => bail!(failed),
            stderr => bail!("{failed}: {stderr}"),
        }
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
    use crate::spawn::Checkout;

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
            let req = FeedRequest {
                herd: "prs".into(),
                dry_run,
                ready_timeout: Duration::from_secs(60),
                command_timeout: Duration::from_secs(1),
                own_pane: pane.map(str::to_string),
                own_socket: pane.map(|_| PathBuf::from("/s")),
                mailboxes: self.config.path().join("reports"),
                after_merge_timeout: Duration::from_secs(5),
            };
            feed(herdr, ledger, &self.paths(), &req, Some("w1"))
        }

        /// A herd `prs` whose template `pr` has done and merged checks
        /// testing for `done-<n>` and `merged-<n>`, with `after_merge` set
        /// to the TOML array `commands`, and `lead` appended.
        fn after_merge(commands: &str, lead: &str) -> Setup {
            let setup = Setup::new("cat tasks", 5);
            let herd = format!(
                "repo = {:?}\nafter_merge = {commands}\n\
                 [template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
                 prompt = \"review {{number}}\"\ndone = \"test -f done-{{number}}\"\n\
                 merged = \"test -f merged-{{number}}\"\n\
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
            herd: herd.map(str::to_string),
            template: herd.map(|_| "pr".to_string()),
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge_ran: false,
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
                "warning: zpr-9 has no recorded params; skipping its done check",
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

        fn started(&mut self, _: &str, _: std::time::SystemTime) -> Result<()> {
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

        let names = claimable(
            &MemLedger::default(),
            &paths,
            &queue,
            "prs",
            0,
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(names, ["research-1", "lh-2"]);
    }

    #[test]
    fn the_documented_routed_queue_sends_wayfinding_cards_to_their_own_template_on_the_herds_branches()
     {
        let setup = Setup::new("true", 1);
        let scripts = setup.repo.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(
            scripts.join("trello"),
            "#!/bin/sh\nprintf 'LH-7\\tBacklog\\tready-for-agent,impl:feat\\tA feature\\n\
             LH-8\\tBacklog\\tready-for-agent,wayfinding:research\\tA question\\n\
             LH-9\\tBacklog\\tready-for-agent,wayfinding:grilling\\tA grilling\\n\
             LH-10\\tBacklog\\tready-for-agent,impl:fix,need-human\\tStuck\\n'\n",
        )
        .unwrap();
        let executable = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        fs::set_permissions(scripts.join("trello"), executable).unwrap();
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
        let output = run_source(&queue, Duration::from_secs(1)).unwrap();
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
                "after_merge zpr-1: ok",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors"
            ]
        );
        assert_eq!(again.lines[0], "still open zpr-1");
        assert_eq!(setup.read("log").unwrap(), "one\ntwo\n");
        assert!(ledger.0["zpr-1"].after_merge_ran);
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
                "after_merge zpr-1: ok",
                "prs: 0 retired, 0 added, 0 skipped, 0 errors"
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
                format!("after_merge zpr-1: failed: `{failing}` exit 3"),
                "prs: 0 retired, 0 added, 0 skipped, 1 error".into(),
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
        assert!(ledger.0["zpr-1"].after_merge_ran);
    }

    /// `ledger` with `lead-prs`, the lead of `prs`, in pane `w1:p0` at `/s`.
    fn with_lead(mut ledger: MemLedger) -> MemLedger {
        let mut lead = member("lead-prs", Some("prs"), false);
        lead.pane_id = "w1:p0".into();
        lead.socket = Some("/s".into());
        lead.template = None;
        lead.role = Some(crate::ledger::Role::Lead);
        ledger.0.insert(lead.name.clone(), lead);
        ledger
    }

    #[test]
    fn from_a_leads_pane_after_merge_needing_a_switch_that_is_off_runs_none_and_notes_it() {
        for (command, switch) in [
            ("scripts/install --no-extras", "install"),
            ("git pull --rebase", "rebase"),
        ] {
            let commands = format!(r#"["echo one >> log", "{command}"]"#);
            let setup = Setup::after_merge(&commands, "[lead]\nprompt = \"go\"\n");
            let herdr = herdr();
            let mut ledger = with_lead(fed_one(&setup, &herdr));
            setup.touch("merged-1");

            let fed = setup
                .feed_from(&herdr, &mut ledger, false, Some("w1:p0"))
                .unwrap();

            let why = format!("`{command}` needs the {switch} switch, off in prs's [lead]");
            assert_eq!(fed.lines[0], format!("after_merge zpr-1: not run: {why}"));
            assert_eq!(setup.read("log"), None, "{command}");
            assert_eq!(
                setup.notes("zpr-1"),
                [format!(
                    "after_merge not run: {why}; run its commands by hand, or turn {switch} on"
                )]
            );
            assert!(ledger.0["zpr-1"].after_merge_ran);
        }
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
            fs::write(&install, "#!/bin/sh\necho installed \"$@\" >> log\n").unwrap();
            fs::set_permissions(
                &install,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            let herdr = herdr();
            let mut ledger = with_lead(fed_one(&setup, &herdr));
            setup.touch("merged-1");

            let fed = setup.feed_from(&herdr, &mut ledger, false, pane).unwrap();

            assert_eq!(fed.lines[0], "after_merge zpr-1: ok", "{lead} {pane:?}");
            assert_eq!(setup.read("log").unwrap(), "one\ninstalled --no-extras\n");
        }
    }
}
