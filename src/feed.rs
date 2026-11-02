//! `loopherd feed`: fill a herd's free slots from its queue's source (D14).

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::herd::{AddRequest, Paths, Queue, queue, resolve};
use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::retire::retire_and_close;
use crate::shell::run_within;
use crate::spawn::{BLOCKED_EXIT, spawn};

pub struct FeedRequest {
    pub herd: String,
    /// Run the source and the done checks and report; retire and spawn nothing.
    pub dry_run: bool,
    pub ready_timeout: Duration,
    /// How long the source and each done check may run.
    pub command_timeout: Duration,
    /// The pane running loopherd, whose tab retire never closes.
    pub own_pane: Option<String>,
}

/// One line per task, then a summary, and the exit code.
#[derive(Debug)]
pub struct Fed {
    pub lines: Vec<String>,
    pub code: i32,
}

/// Retires members whose done check passes, then spawns a member per task
/// until the herd's slots are full.
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
    let output = run_source(&queue, req.command_timeout).context("source")?;
    let mut lines = vec![];
    let reaped = reap(herdr, ledger, &queue, req, &mut lines)?;
    let mut errors = reaped.errors;

    let entries = ledger.entries().context("ledger")?;
    let held = entries
        .values()
        .filter(|e| !e.retired && e.herd.as_deref() == Some(&req.herd))
        .count();
    let held = if req.dry_run {
        held.saturating_sub(reaped.retired)
    } else {
        held
    };
    let mut free = queue.max.saturating_sub(held);
    let mut taken: HashSet<String> = entries.into_keys().collect();

    let (mut added, mut skipped) = (0, 0);
    let mut code = 0;
    for task in output.lines().map(str::trim) {
        if task.is_empty() || task.starts_with('#') {
            continue;
        }
        let spawn_req = match task_request(paths, req, &queue, task) {
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
            lines.push(format!("would add {name}"));
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
        code = 1;
    }
    Ok(Fed { lines, code })
}

struct Reaped {
    retired: usize,
    errors: usize,
}

/// Runs the done check of each active member that has one, retiring and
/// closing those it passes, one line per member to `lines`.
fn reap(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    queue: &Queue,
    req: &FeedRequest,
    lines: &mut Vec<String>,
) -> Result<Reaped> {
    let mut reaped = Reaped {
        retired: 0,
        errors: 0,
    };
    let members = ledger.entries().context("ledger")?.into_values();
    for member in members.filter(|e| !e.retired && e.herd.as_deref() == Some(&req.herd)) {
        let name = &member.name;
        let Some(check) = member.template.as_ref().and_then(|t| queue.done.get(t)) else {
            continue;
        };
        let Some(params) = &member.params else {
            lines.push(format!(
                "warning: {name} has no recorded params; skipping its done check"
            ));
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
                    }
                }
            }
        }
    }
    Ok(reaped)
}

/// Whether `command` reports its member finished: exit 0 yes, exit 1 no,
/// anything else an error describing what happened.
fn run_check(command: &str, repo: &Path, timeout: Duration) -> Result<bool, String> {
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

fn task_request(
    paths: &Paths,
    req: &FeedRequest,
    queue: &Queue,
    task: &str,
) -> Result<crate::spawn::SpawnRequest> {
    let params: Vec<String> = task.split_whitespace().map(String::from).collect();
    if params.len() != queue.params.len() {
        bail!(
            "template {} takes {} param{} ({}), got {}",
            queue.template,
            queue.params.len(),
            if queue.params.len() == 1 { "" } else { "s" },
            queue.params.join(", "),
            params.len()
        );
    }
    resolve(
        paths,
        &AddRequest {
            herd: req.herd.clone(),
            template: queue.template.clone(),
            params,
            claude_args: vec![],
            ready_timeout: req.ready_timeout,
        },
    )
}

/// An exclusive lock on `path`, held until the file is dropped; fails at once
/// if another run holds it.
pub fn lock(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = File::create(path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            bail!("feed already running");
        }
        return Err(err).with_context(|| format!("lock {}", path.display()));
    }
    Ok(file)
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
            }
        }

        fn feed(&self, herdr: &FakeHerdr, ledger: &mut MemLedger, dry_run: bool) -> Result<Fed> {
            let req = FeedRequest {
                herd: "prs".into(),
                dry_run,
                ready_timeout: Duration::from_secs(60),
                command_timeout: Duration::from_secs(1),
                own_pane: None,
            };
            feed(herdr, ledger, &self.paths(), &req, Some("w1"))
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
        }
    }

    fn ledger(members: &[AgentRecord]) -> MemLedger {
        MemLedger(
            members
                .iter()
                .map(|m| (m.name.clone(), m.clone()))
                .collect(),
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
                "would add zpr-2",
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
                "would add zpr-2",
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
    fn the_done_check_renders_params_and_built_ins_and_runs_in_the_repo() {
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
        };

        let fed = feed(&herdr, &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(
            fed.lines[0],
            format!("retired zpr-1; kept tab {tab}: it is running this command")
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn a_retire_that_fails_is_an_error_and_the_run_continues() {
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
        assert_eq!(fed.lines[2], "prs: 0 retired, 1 added, 0 skipped, 1 error");
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
                "would add zpr-2",
                "prs: 1 would retire, 1 would add, 0 skipped, 0 errors",
            ]
        );
        assert!(setup.repo.path().join("checked").exists());
        assert_eq!(ledger.0, before);
        assert_eq!(herdr.mutations(), mutations);
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
    fn a_second_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loopherd/feed.lock");
        let first = lock(&path).unwrap();
        let err = lock(&path).unwrap_err();
        assert_eq!(format!("{err:#}"), "feed already running");
        drop(first);
        lock(&path).unwrap();
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
}
