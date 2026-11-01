//! `loopherd spawn`: start a named Claude agent in a new tab and record it.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::herdr::{Agent, AgentStatus, Herdr, NewTab, StartOutcome, WaitOutcome};
use crate::ledger::{AgentRecord, Ledger};
use crate::worktree::ensure_worktree;

pub const BLOCKED_EXIT: i32 = 3;

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub repo: PathBuf,
    pub name: String,
    pub worktree: Option<String>,
    pub workspace: Option<String>,
    pub prompt: Option<String>,
    pub ready_timeout: Duration,
    pub claude_args: Vec<String>,
}

/// Where the agent went, and whether it got past startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    pub tab_id: String,
    pub pane_id: String,
    pub worktree: Option<PathBuf>,
    pub session_id: Option<String>,
    /// Still at a dialog when the ready timeout ran out.
    pub blocked: bool,
}

impl Spawned {
    /// One `key value` line per fact, then the blocked message if any.
    pub fn report(&self, name: &str) -> (String, i32) {
        let mut lines = vec![
            format!("tab {}", self.tab_id),
            format!("pane {}", self.pane_id),
        ];
        if let Some(path) = &self.worktree {
            lines.push(format!("worktree {}", path.display()));
        }
        lines.push(format!(
            "session {}",
            self.session_id.as_deref().unwrap_or("unknown")
        ));
        if !self.blocked {
            return (lines.join("\n"), 0);
        }
        lines.push(format!(
            "blocked: answer it in pane {}, then run loopherd prompt {name}",
            self.pane_id
        ));
        (lines.join("\n"), BLOCKED_EXIT)
    }
}

enum Readiness {
    Ready(Agent),
    Blocked,
}

/// Runs the spawn. `inherited_workspace` is `$HERDR_WORKSPACE_ID`, if set.
///
/// Errors are prefixed with the step that failed.
pub fn spawn(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &SpawnRequest,
    inherited_workspace: Option<&str>,
) -> Result<Spawned> {
    validate_name(&req.name).context("name")?;
    ensure_name_free(herdr, &req.name).context("name")?;
    let workspace = resolve_workspace(herdr, req.workspace.as_deref(), inherited_workspace)
        .context("workspace")?;
    let worktree = match &req.worktree {
        Some(branch) => Some(ensure_worktree(&req.repo, branch).context("worktree")?),
        None => None,
    };
    let cwd = worktree.as_ref().unwrap_or(&req.repo);
    let tab = herdr
        .create_tab(&workspace, cwd, &req.name)
        .context("tab")?;

    let readiness = match start_until_ready(herdr, req, &tab) {
        Ok(r) => r,
        Err(e) => return Err(close_after_failure(herdr, &tab, e.context("start"))),
    };

    let mut record = AgentRecord {
        name: req.name.clone(),
        pane_id: tab.pane_id.clone(),
        session_id: None,
        repo: req.repo.clone(),
        worktree: worktree.clone(),
        workspace,
        prompt: req.prompt.clone(),
        prompt_sent: false,
        created: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        retired: false,
        retired_at: None,
    };
    let agent = match readiness {
        Readiness::Blocked => {
            ledger.record(record).context("ledger")?;
            return Ok(Spawned {
                tab_id: tab.tab_id,
                pane_id: tab.pane_id,
                worktree,
                session_id: None,
                blocked: true,
            });
        }
        Readiness::Ready(agent) => agent,
    };
    record.session_id = agent.session_id.clone();
    let sent = match &req.prompt {
        Some(text) => herdr.prompt(&req.name, text).context("prompt"),
        None => Ok(()),
    };
    record.prompt_sent = sent.is_ok() && req.prompt.is_some();
    ledger.record(record).context("ledger")?;
    sent?;
    Ok(Spawned {
        tab_id: tab.tab_id,
        pane_id: tab.pane_id,
        worktree,
        session_id: agent.session_id,
        blocked: false,
    })
}

fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        bail!("{name:?} must match [a-z][a-z0-9_-]{{0,31}}");
    }
    Ok(())
}

fn ensure_name_free(herdr: &dyn Herdr, name: &str) -> Result<()> {
    if let Some(agent) = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
    {
        bail!("{name} is already a live agent in pane {}", agent.pane_id);
    }
    Ok(())
}

fn resolve_workspace(
    herdr: &dyn Herdr,
    label: Option<&str>,
    inherited: Option<&str>,
) -> Result<String> {
    let Some(label) = label else {
        return inherited
            .map(str::to_string)
            .ok_or_else(|| anyhow!("not running inside herdr; pass --workspace <label>"));
    };
    let mut matches = herdr.workspaces()?.into_iter().filter(|w| w.label == label);
    match (matches.next(), matches.next()) {
        (Some(w), None) => Ok(w.id),
        (None, _) => bail!("no workspace labeled {label:?}"),
        (Some(_), Some(_)) => bail!("more than one workspace is labeled {label:?}"),
    }
}

fn start_until_ready(herdr: &dyn Herdr, req: &SpawnRequest, tab: &NewTab) -> Result<Readiness> {
    let deadline = Instant::now() + req.ready_timeout;
    match herdr.start_claude(&req.name, &tab.pane_id, req.ready_timeout, &req.claude_args)? {
        StartOutcome::Ready(agent) => return Ok(Readiness::Ready(agent)),
        StartOutcome::Blocked => {}
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match herdr.wait(&req.name, &[AgentStatus::Idle], remaining)? {
        WaitOutcome::Reached => Ok(Readiness::Ready(herdr.agent(&req.name)?)),
        WaitOutcome::TimedOut => match herdr.agent(&req.name)?.status {
            AgentStatus::Blocked => Ok(Readiness::Blocked),
            status => bail!(
                "agent not idle after {}s (status {status:?})",
                req.ready_timeout.as_secs()
            ),
        },
    }
}

fn close_after_failure(herdr: &dyn Herdr, tab: &NewTab, err: anyhow::Error) -> anyhow::Error {
    match herdr.close_tab(&tab.tab_id) {
        Ok(()) => err,
        Err(close) => err.context(format!(
            "also failed to close tab {}: {close:#}",
            tab.tab_id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdr, Startup};
    use crate::ledger::MemLedger;
    use crate::worktree::testing::{current_branch, repo_with_origin};

    fn request(name: &str) -> SpawnRequest {
        SpawnRequest {
            repo: PathBuf::from("/src/repo"),
            name: name.into(),
            worktree: None,
            workspace: None,
            prompt: Some("start work".into()),
            ready_timeout: Duration::from_secs(60),
            claude_args: vec![],
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "zagops")])
    }

    fn run(herdr: &FakeHerdr, ledger: &mut MemLedger, req: &SpawnRequest) -> Result<Spawned> {
        spawn(herdr, ledger, req, Some("w1"))
    }

    #[test]
    fn rejects_names_outside_herdrs_pattern_before_creating_anything() {
        let long = format!("a{}", "b".repeat(32));
        for bad in [
            "",
            "Agent",
            "1st",
            "-x",
            "has space",
            "dot.name",
            long.as_str(),
        ] {
            let herdr = herdr();
            let mut ledger = MemLedger::default();
            let err = run(&herdr, &mut ledger, &request(bad)).unwrap_err();
            assert!(format!("{err:#}").starts_with("name: "), "{bad:?}: {err:#}");
            assert!(herdr.tabs().is_empty());
        }
        let max = format!("a{}", "b".repeat(31));
        for good in ["a", "lh-9", "x_1-y", max.as_str()] {
            assert!(
                run(&herdr(), &mut MemLedger::default(), &request(good)).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn a_name_held_by_a_live_agent_fails_before_anything_is_created() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = herdr().with_agent("taken");
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo,
            worktree: Some("feat/new".into()),
            ..request("taken")
        };
        let err = run(&herdr, &mut ledger, &req).unwrap_err();
        assert!(
            format!("{err:#}").contains("already a live agent"),
            "{err:#}"
        );
        assert!(herdr.tabs().is_empty());
        assert!(!root.path().join("proj/worktrees").exists());
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn workspace_defaults_to_the_inherited_one() {
        let herdr = herdr();
        run(&herdr, &mut MemLedger::default(), &request("a")).unwrap();
        assert_eq!(herdr.tabs()[0].workspace_id, "w1");
    }

    #[test]
    fn workspace_flag_picks_a_workspace_by_label() {
        let herdr = herdr();
        let req = SpawnRequest {
            workspace: Some("zagops".into()),
            ..request("a")
        };
        run(&herdr, &mut MemLedger::default(), &req).unwrap();
        assert_eq!(herdr.tabs()[0].workspace_id, "w2");
    }

    #[test]
    fn outside_herdr_a_missing_workspace_flag_is_an_error() {
        let herdr = herdr();
        let err = spawn(&herdr, &mut MemLedger::default(), &request("a"), None).unwrap_err();
        assert!(format!("{err:#}").starts_with("workspace: "), "{err:#}");
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn an_unknown_workspace_label_is_an_error() {
        let req = SpawnRequest {
            workspace: Some("nope".into()),
            ..request("a")
        };
        let err = run(&herdr(), &mut MemLedger::default(), &req).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "workspace: no workspace labeled \"nope\""
        );
    }

    #[test]
    fn without_a_worktree_the_agent_runs_in_the_repo() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(herdr.tabs()[0].cwd, PathBuf::from("/src/repo"));
        assert_eq!(ledger.0["a"].worktree, None);
    }

    #[test]
    fn with_a_worktree_the_agent_runs_in_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo: repo.clone(),
            worktree: Some("feature/existing".into()),
            ..request("a")
        };
        let spawned = run(&herdr, &mut ledger, &req).unwrap();
        let wt = root.path().join("proj/worktrees/feature-existing");
        assert_eq!(
            spawned.report("a").0,
            format!(
                "tab w1:t1\npane w1:p1\nworktree {}\nsession session-a",
                wt.display()
            )
        );
        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(current_branch(&wt), "feature/existing");
        assert_eq!(ledger.0["a"].worktree, Some(wt));
        assert_eq!(ledger.0["a"].repo, repo);
    }

    #[test]
    fn starts_claude_in_a_tab_labeled_with_the_name_and_passes_args_through() {
        let herdr = herdr();
        let req = SpawnRequest {
            claude_args: vec!["--model".into(), "opus".into()],
            ..request("worker")
        };
        run(&herdr, &mut MemLedger::default(), &req).unwrap();
        let tabs = herdr.tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].label, "worker");
        let started = herdr.started();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].name, "worker");
        assert_eq!(started[0].pane_id, "w1:p1");
        assert_eq!(started[0].args, vec!["--model", "opus"]);
    }

    #[test]
    fn a_ready_agent_gets_the_prompt_and_is_recorded() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(
            spawned.report("a"),
            ("tab w1:t1\npane w1:p1\nsession session-a".into(), 0)
        );
        assert_eq!(herdr.prompts(), vec![("a".into(), "start work".into())]);
        let rec = &ledger.0["a"];
        assert_eq!(rec.pane_id, "w1:p1");
        assert_eq!(rec.session_id.as_deref(), Some("session-a"));
        assert_eq!(rec.workspace, "w1");
        assert_eq!(rec.prompt.as_deref(), Some("start work"));
        assert!(rec.prompt_sent);
        assert!(
            humantime::parse_rfc3339(&rec.created).is_ok(),
            "{}",
            rec.created
        );
    }

    #[test]
    fn an_agent_blocked_at_startup_that_turns_idle_gets_the_prompt() {
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedThenIdle;
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert!(!spawned.blocked);
        assert_eq!(herdr.prompts().len(), 1);
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn an_agent_still_blocked_at_the_timeout_is_kept_and_recorded_unprompted() {
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(
            spawned.report("a"),
            (
                "tab w1:t1\npane w1:p1\nsession unknown\n\
                 blocked: answer it in pane w1:p1, then run loopherd prompt a"
                    .into(),
                3
            )
        );
        assert!(herdr.prompts().is_empty());
        assert_eq!(herdr.tabs().len(), 1);
        let rec = &ledger.0["a"];
        assert!(!rec.prompt_sent);
        assert_eq!(rec.prompt.as_deref(), Some("start work"));
    }

    #[test]
    fn a_failed_start_closes_the_tab_keeps_the_worktree_and_records_nothing() {
        for startup in [Startup::Fails, Startup::Hangs] {
            let root = tempfile::tempdir().unwrap();
            let repo = repo_with_origin(root.path());
            let mut herdr = herdr();
            herdr.startup = startup;
            let mut ledger = MemLedger::default();
            let req = SpawnRequest {
                repo,
                worktree: Some("feat/new".into()),
                ..request("a")
            };
            let err = run(&herdr, &mut ledger, &req).unwrap_err();
            assert!(format!("{err:#}").starts_with("start: "), "{err:#}");
            assert!(herdr.tabs().is_empty(), "{startup:?}");
            assert!(root.path().join("proj/worktrees/feat-new").is_dir());
            assert!(ledger.0.is_empty());
        }
    }

    #[test]
    fn a_failed_tab_creation_names_the_step() {
        let mut herdr = herdr();
        herdr.fail_create_tab = true;
        let mut ledger = MemLedger::default();
        let err = run(&herdr, &mut ledger, &request("a")).unwrap_err();
        assert!(format!("{err:#}").starts_with("tab: "), "{err:#}");
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn without_a_prompt_the_agent_is_recorded_as_unprompted() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            prompt: None,
            ..request("a")
        };
        let spawned = run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(spawned.report("a").1, 0);
        assert!(herdr.prompts().is_empty());
        assert!(!ledger.0["a"].prompt_sent);
    }
}
