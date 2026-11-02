//! `loopherd feed`: fill a herd's free slots from its queue's source (D14).

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::herd::{AddRequest, Paths, Queue, queue, resolve};
use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::spawn::{BLOCKED_EXIT, spawn};

pub struct FeedRequest {
    pub herd: String,
    /// Run the source and report; spawn nothing.
    pub dry_run: bool,
    pub ready_timeout: Duration,
}

/// One line per task, then a summary, and the exit code.
#[derive(Debug)]
pub struct Fed {
    pub lines: Vec<String>,
    pub code: i32,
}

/// Spawns a member per task until the herd's slots are full.
///
/// Fails, spawning nothing, if the source fails or herdr is unreachable.
pub fn feed(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &FeedRequest,
    inherited_workspace: Option<&str>,
) -> Result<Fed> {
    let queue = queue(paths, &req.herd)?;
    herdr.workspaces().context("herdr")?;
    let output = run_source(&queue).context("source")?;
    let entries = ledger.entries().context("ledger")?;
    let held = entries
        .values()
        .filter(|e| !e.retired && e.herd.as_deref() == Some(&req.herd))
        .count();
    let mut free = queue.max.saturating_sub(held);
    let mut taken: HashSet<String> = entries.into_keys().collect();

    let mut lines = vec![];
    let (mut added, mut skipped, mut errors) = (0, 0, 0);
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
    let verb = if req.dry_run { "would add" } else { "added" };
    lines.push(format!(
        "{}: {added} {verb}, {skipped} skipped, {errors} error{}",
        req.herd,
        if errors == 1 { "" } else { "s" }
    ));
    if code == 0 && errors > 0 {
        code = 1;
    }
    Ok(Fed { lines, code })
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

/// The source's stdout; fails if it exits non-zero.
fn run_source(queue: &Queue) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(&queue.source)
        .current_dir(&queue.repo)
        .output()
        .with_context(|| format!("run in {}", queue.repo.display()))?;
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
            let config = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            fs::create_dir(config.path().join("herds")).unwrap();
            let herd = format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
                 prompt = \"review {{number}}\"\n[queue]\ntemplate = \"pr\"\nsource = {:?}\nmax = {max}\n",
                repo.path().display().to_string(),
                source
            );
            fs::write(config.path().join("herds/prs.toml"), herd).unwrap();
            Setup { config, repo }
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
                "prs: 1 added, 2 skipped, 0 errors",
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
                "prs: 2 added, 2 skipped, 0 errors",
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
                "prs: 1 added, 0 skipped, 1 error",
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
        assert_eq!(fed.lines[1], "prs: 0 added, 0 skipped, 1 error");
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
        assert_eq!(fed.lines[2], "prs: 1 added, 0 skipped, 0 errors");
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
                "prs: 1 would add, 2 skipped, 0 errors",
            ]
        );
        assert_eq!(fed.code, 0);
        assert!(herdr.tabs().is_empty());
        assert!(herdr.started().is_empty());
        assert_eq!(ledger.0.keys().collect::<Vec<_>>(), ["zpr-1"]);
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
