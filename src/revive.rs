//! `loopherd revive`: bring ledger agents back after a restart or closed tab.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::herdr::{
    Agent, AgentStatus, Herdr, Pane, StartOutcome, Workspace, wait_for_shell_prompt,
};
use crate::ledger::{AgentRecord, Ledger};
use crate::lock::Lock;
use crate::process::{self, Procs};
use crate::report::{Body, Entry, append};
use crate::subagent_view::{ShowsSubagentView, TEXT_REACHES_SUBAGENT, ensure_main_view};
use crate::worktree::same_dir;

/// Sent to a lead whose watch isn't running: after a restart nothing else wakes it (D17).
pub const REARM_PROMPT: &str = "You were revived after a restart, so your \
`loopherd watch` is no longer running. Run `loopherd status` and \
`loopherd reports --all --tail 40` (for a fuller history, `--since <time>` \
or plain `--all`), then read the skill's Lead section \
(`loopherd --skill --section lead`, not the whole skill) and re-arm your \
watch as it says.";

#[derive(Debug, Clone)]
pub struct ReviveRequest {
    /// Only this entry.
    pub name: Option<String>,
    pub dry_run: bool,
    /// How long to wait for herdr's own resume to settle.
    pub settle_timeout: Duration,
    pub poll_interval: Duration,
    pub start_timeout: Duration,
    /// Members' mailboxes, where watches also hold their locks.
    pub reports_dir: PathBuf,
    /// How long to wait for another revive to finish before giving up.
    pub lock_timeout: Duration,
    /// Run by herdr's startup hook, with the socket herdr gave it: only entries
    /// recorded on it, and a note of its line in each lead's mailbox unless its watch runs.
    pub startup: Option<PathBuf>,
}

/// One line per entry, and whether any entry failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revived {
    pub lines: Vec<String>,
    pub failed: bool,
}

impl Revived {
    pub fn report(&self) -> (String, i32) {
        (self.lines.join("\n"), i32::from(self.failed))
    }
}

/// Brings back each ledger entry, taking its session as `live` resolves it,
/// so a session Claude continued elsewhere is matched and resumed there.
pub fn revive(
    herdr: &dyn Herdr,
    procs: &dyn Procs,
    ledger: &mut dyn Ledger,
    req: &ReviveRequest,
    live: &dyn Fn(&str) -> String,
) -> Result<Revived> {
    // A second revive, such as herdr's startup hook beside a manual one, waits,
    // then finds what the first started alive.
    let _only_revive = only_revive(&req.reports_dir, req.lock_timeout)?;
    let mut agents = settled_agents(herdr, req).context("settle")?;
    // herdr may still list a continued session under its old id.
    for agent in &mut agents {
        agent.session_id = agent.session_id.take().map(|s| live(&s));
    }
    let here = req.startup.clone().or_else(|| herdr.socket());
    let mut revived = Revived::default();
    let mut entries = ledger.entries()?;
    if let Some(name) = &req.name {
        let only = entries
            .remove(name)
            .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
        entries = BTreeMap::from([(name.clone(), only)]);
    }
    if let Some(here) = &req.startup {
        entries.retain(|_, r| r.socket.as_ref() == Some(here));
    }
    for mut record in entries.into_values() {
        if !record.retired {
            record.session_id = record.session_id.map(|s| live(&s));
        }
        let mut warnings = vec![];
        let mut outcome = revive_one(herdr, req, here.as_deref(), &agents, &record, &mut warnings);
        if let Outcome::Started {
            pane_id,
            tab_id,
            workspace_id,
            session_id,
            ..
        } = &outcome
        {
            let restarted = ledger.update(&record.name, &mut |current| {
                if let (Some(here), Some(there)) = (&here, &current.socket)
                    && !same_dir(here, there)
                {
                    bail!(
                        "recorded on {} since revive read the ledger; \
                         its resumed session is left running in pane {pane_id}",
                        there.display()
                    );
                }
                current.env_pane_id = None;
                current.pane_id = pane_id.clone();
                current.tab_id = Some(tab_id.clone());
                current.workspace = workspace_id.clone();
                current.session_id = session_id
                    .clone()
                    .or(current.session_id.take())
                    .map(|s| live(&s));
                current.socket = current.socket.take().or_else(|| here.clone());
                current.process = None;
                Ok(())
            });
            match restarted {
                Ok(revived) => {
                    if revived.socket.is_none() {
                        warnings.push(
                            "herdr didn't say which socket it serves and none is recorded; \
                             set HERDR_SOCKET_PATH and revive it again, or its stops and reports can't be matched to it"
                                .into(),
                        );
                    }
                    if let Err(e) = ledger.started(&record.name, SystemTime::now(), true) {
                        warnings.push(format!("no start entry in its mailbox: {e:#}"));
                    }
                }
                Err(error) => {
                    outcome = Outcome::Failed {
                        step: "ledger",
                        error,
                    };
                }
            }
        }
        if matches!(outcome, Outcome::Alive { .. })
            && !req.dry_run
            && let Err(e) = forget_dead_env_pane(ledger, procs, &record)
        {
            warnings.push(format!("its old pane id is kept: {e:#}"));
        }
        rearm_lead(herdr, req, &record, &mut outcome);
        let line = outcome.line(&record);
        // Nobody reads a startup hook's output, so a lead's line goes to the
        // shepherd's next watch unless its own watch already runs.
        let noted = req.startup.is_some() && !req.dry_run && record.is_lead() && !record.retired;
        if noted && !matches!(outcome.rearm(), Some(Rearm::WatchRunning)) {
            let text = format!("herdr's startup revive: {line}");
            let note = Entry::new(SystemTime::now(), Body::Note { text });
            if let Err(e) = append(&req.reports_dir, &record.name, &note) {
                warnings.push(format!("no note in its mailbox: {e:#}"));
            }
        }
        revived.lines.extend(
            warnings
                .iter()
                .map(|w| format!("warning {}: {w}", record.name)),
        );
        revived.failed |= matches!(outcome, Outcome::Failed { .. })
            || matches!(
                outcome.rearm(),
                Some(Rearm::Failed(_) | Rearm::Unchecked(_) | Rearm::SubagentView)
            );
        revived.lines.push(line);
    }
    Ok(revived)
}

/// A live agent whose recorded process is gone was started again, so its
/// `$HERDR_PANE_ID` is its pane's current id, not the one kept from before a move.
fn forget_dead_env_pane(
    ledger: &mut dyn Ledger,
    procs: &dyn Procs,
    record: &AgentRecord,
) -> Result<()> {
    if record.env_pane_id.is_none()
        || record.process.is_none()
        || process::live(procs, record.process).is_some()
    {
        return Ok(());
    }
    ledger.update(&record.name, &mut |current| {
        if current.process == record.process {
            current.env_pane_id = None;
        }
        Ok(())
    })?;
    Ok(())
}

fn only_revive(dir: &Path, timeout: Duration) -> Result<Lock> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("revive.lock");
    let file = File::create(&path).with_context(|| format!("open {}", path.display()))?;
    let given_up = Instant::now() + timeout;
    let mut said = false;
    loop {
        let attempt = file
            .try_clone()
            .with_context(|| format!("open {}", path.display()))?;
        if let Some(lock) =
            Lock::try_take(attempt).with_context(|| format!("lock {}", path.display()))?
        {
            return Ok(lock);
        }
        if Instant::now() >= given_up {
            bail!(
                "another revive is still running after {} ({}); retry once it ends",
                humantime::format_duration(timeout),
                path.display()
            );
        }
        if !said {
            eprintln!("waiting for another revive ({})", path.display());
            said = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Prompts a lead that is alive or just started, unless its watch is running
/// or its pane shows a subagent's view.
fn rearm_lead(herdr: &dyn Herdr, req: &ReviveRequest, record: &AgentRecord, outcome: &mut Outcome) {
    let pane = match outcome {
        Outcome::Alive { pane_id, .. } | Outcome::Started { pane_id, .. } => Some(pane_id.clone()),
        _ => None,
    };
    let pane = pane.as_deref();
    let Some(rearm) = outcome.rearm_mut() else {
        return;
    };
    if !record.is_lead() {
        return;
    }
    let name = &record.name;
    let main_view = |pane| {
        let retry = format!("run loopherd revive --name {name}");
        ensure_main_view(herdr, name, pane, TEXT_REACHES_SUBAGENT, &retry)
    };
    *rearm = match crate::watch::lead_watch_running(&req.reports_dir, name) {
        Ok(true) => Rearm::WatchRunning,
        Ok(false) => match pane.map(main_view).transpose() {
            Err(error) if error.is::<ShowsSubagentView>() => Rearm::SubagentView,
            Err(error) if req.dry_run => Rearm::Unchecked(error),
            Err(error) => Rearm::Failed(error),
            Ok(_) if req.dry_run => Rearm::WouldSend,
            Ok(_) => match herdr.prompt(name, REARM_PROMPT) {
                Ok(()) => Rearm::Sent,
                Err(error) => Rearm::Failed(error),
            },
        },
        Err(error) => Rearm::Failed(error),
    };
}

/// Polls until no agent is launch-pending, so revive never races herdr's resume.
fn settled_agents(herdr: &dyn Herdr, req: &ReviveRequest) -> Result<Vec<Agent>> {
    let deadline = Instant::now() + req.settle_timeout;
    loop {
        let agents = herdr.agents()?;
        let pending = agents.iter().filter(|a| a.launch_pending).count();
        if pending == 0 {
            return Ok(agents);
        }
        if Instant::now() >= deadline {
            bail!(
                "herdr is still launching {pending} agent(s) after {}",
                humantime::format_duration(req.settle_timeout)
            );
        }
        std::thread::sleep(req.poll_interval);
    }
}

enum Rearm {
    NotSent,
    WatchRunning,
    WouldSend,
    Sent,
    /// Text typed there would reach the subagent, not the lead.
    SubagentView,
    /// Dry run: the pane couldn't be read, so nothing says whether it would be prompted.
    Unchecked(anyhow::Error),
    Failed(anyhow::Error),
}

impl Rearm {
    fn suffix(&self, name: &str) -> String {
        match self {
            Rearm::NotSent => String::new(),
            Rearm::WatchRunning => " (watch running)".into(),
            Rearm::WouldSend => format!(" (would prompt: {REARM_PROMPT})"),
            Rearm::Sent => " (prompted to re-arm its watch)".into(),
            Rearm::SubagentView => format!(
                " (not prompted to re-arm: its pane shows a subagent's view; \
                 switch it back to the main conversation, then run loopherd revive --name {name})"
            ),
            Rearm::Unchecked(error) => {
                format!(" (can't tell whether it would be prompted: {error:#})")
            }
            Rearm::Failed(error) => {
                format!(" (re-arm failed: {error:#}; run loopherd revive --name {name})")
            }
        }
    }
}

enum Outcome {
    Skipped(String),
    Alive {
        pane_id: String,
        blocked: bool,
        rearm: Rearm,
    },
    Collision {
        pane_id: String,
    },
    /// Dry run: the pane it would reuse, or `None` for a new tab.
    WouldResume {
        pane_id: Option<String>,
        workspace: String,
        rearm: Rearm,
    },
    Started {
        pane_id: String,
        tab_id: String,
        workspace_id: String,
        session_id: Option<String>,
        blocked: bool,
        rearm: Rearm,
    },
    Failed {
        step: &'static str,
        error: anyhow::Error,
    },
}

impl Outcome {
    fn rearm(&self) -> Option<&Rearm> {
        match self {
            Outcome::Alive { rearm, .. }
            | Outcome::WouldResume { rearm, .. }
            | Outcome::Started { rearm, .. } => Some(rearm),
            _ => None,
        }
    }

    /// Where a lead's re-arm goes: alive, about to resume, or started and not blocked.
    fn rearm_mut(&mut self) -> Option<&mut Rearm> {
        match self {
            Outcome::Alive {
                blocked: false,
                rearm,
                ..
            }
            | Outcome::WouldResume { rearm, .. }
            | Outcome::Started {
                blocked: false,
                rearm,
                ..
            } => Some(rearm),
            _ => None,
        }
    }

    fn line(&self, record: &AgentRecord) -> String {
        let name = &record.name;
        let (line, placed) = match self {
            Outcome::Skipped(reason) => (format!("skipped {name}: {reason}"), false),
            Outcome::Alive {
                pane_id,
                blocked,
                rearm,
            } => {
                let held = if *blocked && record.is_lead() {
                    " (blocked, not prompted)".to_string()
                } else {
                    rearm.suffix(name)
                };
                (format!("alive {name} {pane_id}{held}"), true)
            }
            Outcome::Collision { pane_id } => (
                format!(
                    "collision {name}: the name is held by pane {pane_id} under another session"
                ),
                false,
            ),
            Outcome::WouldResume {
                pane_id,
                workspace,
                rearm,
            } => {
                let place = match pane_id {
                    Some(pane_id) => format!("pane {pane_id}"),
                    None => format!("a new tab in workspace {workspace}"),
                };
                (
                    format!("would resume {name} in {place}{}", rearm.suffix(name)),
                    false,
                )
            }
            Outcome::Started {
                pane_id,
                blocked,
                rearm,
                ..
            } => {
                let verb = if *blocked { "blocked" } else { "resumed" };
                (
                    format!("{verb} {name} {pane_id}{}", rearm.suffix(name)),
                    true,
                )
            }
            Outcome::Failed { step, error } => (format!("failed {name}: {step}: {error:#}"), false),
        };
        if placed && !record.prompt_sent {
            format!("{line} (prompt not sent, run loopherd prompt {name})")
        } else {
            line
        }
    }
}

fn revive_one(
    herdr: &dyn Herdr,
    req: &ReviveRequest,
    here: Option<&Path>,
    agents: &[Agent],
    record: &AgentRecord,
    warnings: &mut Vec<String>,
) -> Outcome {
    if record.retired {
        return Outcome::Skipped("retired".into());
    }
    if let (Some(here), Some(there)) = (here, &record.socket)
        && !same_dir(here, there)
    {
        return Outcome::Skipped(format!("recorded on {}", there.display()));
    }
    let cwd = record.worktree.as_ref().unwrap_or(&record.repo);
    if !cwd.is_dir() {
        return Outcome::Skipped(format!("cwd {} is gone", cwd.display()));
    }
    if let Some(live) = agents
        .iter()
        .find(|a| a.session_id.is_some() && a.session_id == record.session_id)
    {
        return Outcome::Alive {
            pane_id: live.pane_id.clone(),
            blocked: live.status == AgentStatus::Blocked,
            rearm: Rearm::NotSent,
        };
    }
    if let Some(holder) = agents
        .iter()
        .find(|a| a.name.as_deref() == Some(record.name.as_str()))
    {
        return Outcome::Collision {
            pane_id: holder.pane_id.clone(),
        };
    }
    let workspace = match &record.workspace_label {
        Some(label) => find_workspace(herdr, |w| &w.label == label),
        None => {
            warnings.push(format!(
                "no workspace label recorded, using workspace ID {}",
                record.workspace
            ));
            find_workspace(herdr, |w| w.id == record.workspace)
        }
    };
    let workspace = match workspace {
        Ok(Some(w)) => w,
        Ok(None) => {
            let shown = record.workspace_label.as_ref().unwrap_or(&record.workspace);
            return Outcome::Skipped(format!("workspace {shown} gone"));
        }
        Err(error) => {
            return Outcome::Failed {
                step: "workspace",
                error,
            };
        }
    };
    let Some(session) = &record.session_id else {
        return Outcome::Skipped("no session ID recorded".into());
    };
    let reusable = match shell_pane(herdr, &workspace.id, &record.name, cwd) {
        Ok(pane) => pane,
        Err(error) => {
            return Outcome::Failed {
                step: "pane",
                error,
            };
        }
    };
    if req.dry_run {
        return Outcome::WouldResume {
            pane_id: reusable.map(|p| p.id),
            workspace: workspace.label,
            rearm: Rearm::NotSent,
        };
    }
    let (pane_id, tab_id, new_tab) = match reusable {
        Some(pane) => (pane.id, pane.tab_id, false),
        None => match herdr.create_tab(&workspace.id, cwd, &record.name) {
            Ok(tab) => (tab.pane_id, tab.tab_id, true),
            Err(error) => return Outcome::Failed { step: "tab", error },
        },
    };
    let mut args = vec!["--resume".to_string(), session.clone()];
    args.extend(record.claude_args.iter().cloned());
    let started = match new_tab {
        true => wait_for_shell_prompt(herdr, &pane_id, req.start_timeout, req.poll_interval),
        false => Ok(()),
    }
    .and_then(|()| herdr.start_claude(&record.name, &pane_id, req.start_timeout, &args));
    match started {
        Ok(StartOutcome::Ready(agent)) => Outcome::Started {
            pane_id,
            tab_id,
            workspace_id: workspace.id,
            session_id: agent.session_id,
            blocked: false,
            rearm: Rearm::NotSent,
        },
        Ok(StartOutcome::Blocked) => Outcome::Started {
            pane_id,
            tab_id,
            workspace_id: workspace.id,
            session_id: None,
            blocked: true,
            rearm: Rearm::NotSent,
        },
        Err(error) => {
            let error = match new_tab.then(|| herdr.close_tab(&tab_id)) {
                Some(Err(close)) => {
                    error.context(format!("also failed to close its tab: {close:#}"))
                }
                _ => error,
            };
            Outcome::Failed {
                step: "start",
                error,
            }
        }
    }
}

pub(crate) fn find_workspace(
    herdr: &dyn Herdr,
    matches: impl FnMut(&Workspace) -> bool,
) -> Result<Option<Workspace>> {
    let found: Vec<_> = herdr.workspaces()?.into_iter().filter(matches).collect();
    match found.as_slice() {
        [] => Ok(None),
        [w] => Ok(Some(w.clone())),
        [_, w, ..] => bail!("more than one workspace is labeled {:?}", w.label),
    }
}

/// A pane in a tab labeled `name`, in `cwd`, with its shell in the foreground.
fn shell_pane(
    herdr: &dyn Herdr,
    workspace_id: &str,
    name: &str,
    cwd: &Path,
) -> Result<Option<Pane>> {
    let tabs: Vec<_> = herdr
        .tabs_in(workspace_id)?
        .into_iter()
        .filter(|t| t.label == name)
        .map(|t| t.id)
        .collect();
    for pane in herdr.panes_in(workspace_id)? {
        if tabs.contains(&pane.tab_id) && pane.is_in(cwd) && herdr.at_shell_prompt(&pane.id)? {
            return Ok(Some(pane));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdr, Startup};
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::process::{CLAUDE, FakeProcs};
    use crate::subagent_view::screens::{SUBAGENT_PICKER, SUBAGENT_PROMPT};

    fn request() -> ReviveRequest {
        ReviveRequest {
            name: None,
            dry_run: false,
            settle_timeout: Duration::from_secs(5),
            poll_interval: Duration::ZERO,
            start_timeout: Duration::from_secs(60),
            reports_dir: own_reports_dir(),
            lock_timeout: Duration::from_secs(10),
            startup: None,
        }
    }

    /// An empty dir for this test's thread, removed when the thread ends.
    fn own_reports_dir() -> PathBuf {
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().unwrap();
        }
        DIR.with(|d| d.path().to_path_buf())
    }

    fn record(name: &str, cwd: &Path) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            tab_id: None,
            session_id: Some(format!("sess-{name}")),
            repo: cwd.into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec!["--model".into(), "opus".into()],
            prompt: Some("go".into()),
            prompt_sent: true,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            earlier: vec![],
        }
    }

    fn ledger(records: impl IntoIterator<Item = AgentRecord>) -> MemLedger {
        MemLedger(
            records.into_iter().map(|r| (r.name.clone(), r)).collect(),
            vec![],
        )
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd")])
    }

    fn run(herdr: &FakeHerdr, ledger: &mut MemLedger, req: &ReviveRequest) -> (String, i32) {
        revive(herdr, &FakeProcs::default(), ledger, req, &|s| {
            s.to_string()
        })
        .unwrap()
        .report()
    }

    fn continued(session: &str) -> String {
        match session {
            "stub" => "live".into(),
            other => other.into(),
        }
    }

    fn stub(name: &str, cwd: &Path) -> AgentRecord {
        AgentRecord {
            session_id: Some("stub".into()),
            ..record(name, cwd)
        }
    }

    #[test]
    fn an_entry_whose_session_continued_resumes_and_records_the_live_session() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([stub("a", cwd.path())]);
        let (out, _) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &continued,
        )
        .unwrap()
        .report();
        assert_eq!(out, "resumed a w1:p1");
        assert_eq!(
            herdr.started()[0].args,
            vec!["--resume", "live", "--model", "opus"]
        );
        assert_eq!(ledger.0["a"].session_id.as_deref(), Some("live"));
    }

    #[test]
    fn a_member_moved_to_another_workspace_resumed_in_a_new_pane_takes_that_pane_as_its_own() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let moved = AgentRecord {
            pane_id: "w3:p2".into(),
            env_pane_id: Some("w2:p1".into()),
            ..record("a", cwd.path())
        };
        let mut ledger = ledger([moved]);

        let (out, _) = run(&herdr, &mut ledger, &request());

        assert_eq!(out, "resumed a w1:p1");
        assert!(ledger.0["a"].is_own_pane("w1:p1"));
        assert!(!ledger.0["a"].is_own_pane("w2:p1"));
    }

    #[test]
    fn an_entry_whose_session_continued_into_a_live_agent_is_alive() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("live"), "w1:p4");
        let mut ledger = ledger([stub("a", cwd.path())]);
        let (out, _) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &continued,
        )
        .unwrap()
        .report();
        assert_eq!(out, "alive a w1:p4");
        assert!(herdr.started().is_empty());
    }

    #[test]
    fn a_live_agent_herdr_still_lists_under_the_stub_is_alive() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("stub"), "w1:p4");
        let mut ledger = ledger([stub("a", cwd.path())]);
        let (out, _) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &continued,
        )
        .unwrap()
        .report();
        assert_eq!(out, "alive a w1:p4");
        assert!(herdr.started().is_empty());
    }

    #[test]
    fn a_retired_entrys_session_is_not_resolved() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let retired = AgentRecord {
            retired: true,
            ..stub("a", cwd.path())
        };
        let mut ledger = ledger([retired]);
        let live = |s: &str| match s {
            "stub" => panic!("resolved a retired entry's session"),
            other => other.to_string(),
        };
        let (out, _) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &live,
        )
        .unwrap()
        .report();
        assert_eq!(out, "skipped a: retired");
    }

    #[test]
    fn a_continued_entry_resumed_at_a_dialog_records_the_live_session() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedThenIdle;
        let mut ledger = ledger([stub("a", cwd.path())]);
        let (out, _) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &continued,
        )
        .unwrap()
        .report();
        assert_eq!(out, "blocked a w1:p1");
        assert_eq!(ledger.0["a"].session_id.as_deref(), Some("live"));
    }

    #[test]
    fn a_live_agent_on_the_entrys_session_is_alive_and_nothing_starts() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("sess-a"), "w1:p4");
        let mut ledger = ledger([record("a", cwd.path())]);
        let before = ledger.0.clone();
        assert_eq!(
            run(&herdr, &mut ledger, &request()),
            ("alive a w1:p4".into(), 0)
        );
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn waits_for_herdrs_own_resume_to_settle_before_acting() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr().with_resuming_agent("a", "sess-a", "w1:p4");
        herdr.resume_settles_after = Some(3);
        let mut ledger = ledger([record("a", cwd.path())]);
        assert_eq!(
            run(&herdr, &mut ledger, &request()),
            ("alive a w1:p4".into(), 0)
        );
        assert!(herdr.started().is_empty());
    }

    #[test]
    fn stops_without_acting_when_herdr_never_settles() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr().with_resuming_agent("a", "sess-a", "w1:p4");
        herdr.resume_settles_after = None;
        let mut ledger = ledger([record("a", cwd.path())]);
        let req = ReviveRequest {
            settle_timeout: Duration::from_millis(20),
            ..request()
        };
        let err = revive(&herdr, &FakeProcs::default(), &mut ledger, &req, &|s| {
            s.to_string()
        })
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "settle: herdr is still launching 1 agent(s) after 20ms"
        );
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn skips_retired_entries_and_entries_whose_cwd_is_gone() {
        let cwd = tempfile::tempdir().unwrap();
        let retired = AgentRecord {
            retired: true,
            ..record("a", cwd.path())
        };
        let no_repo = record("b", &cwd.path().join("gone-repo"));
        let no_worktree = AgentRecord {
            worktree: Some(cwd.path().join("gone-wt")),
            ..record("c", cwd.path())
        };
        let herdr = herdr();
        let mut ledger = ledger([retired, no_repo, no_worktree]);
        let before = ledger.0.clone();
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            format!(
                "skipped a: retired\n\
                 skipped b: cwd {0}/gone-repo is gone\n\
                 skipped c: cwd {0}/gone-wt is gone",
                cwd.path().display()
            )
        );
        assert_eq!(code, 0);
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn a_name_held_under_another_session_is_a_collision_and_never_replaced() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("other"), "w1:p4");
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            "collision a: the name is held by pane w1:p4 under another session"
        );
        assert_eq!(code, 0);
        assert!(herdr.started().is_empty());
        assert_eq!(
            herdr.agent("a").unwrap().session_id.as_deref(),
            Some("other")
        );
    }

    #[test]
    fn a_gone_workspace_is_skipped_and_never_recreated() {
        let cwd = tempfile::tempdir().unwrap();
        // w1 survives under a new label: the ID was reused after a restart.
        let herdr = FakeHerdr::new(&[("w1", "something-else")]);
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "skipped a: workspace loopherd gone");
        assert_eq!(code, 0);
        assert!(herdr.tabs().is_empty());
        assert!(herdr.started().is_empty());
    }

    #[test]
    fn an_entry_without_a_label_falls_back_to_the_workspace_id_with_a_warning() {
        let cwd = tempfile::tempdir().unwrap();
        let unlabeled = AgentRecord {
            workspace: "w2".into(),
            workspace_label: None,
            ..record("a", cwd.path())
        };
        let herdr = herdr();
        let mut ledger = ledger([unlabeled]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            "warning a: no workspace label recorded, using workspace ID w2\n\
             skipped a: workspace w2 gone"
        );
    }

    #[test]
    fn with_no_matching_pane_resumes_in_a_new_tab_named_for_the_agent() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([AgentRecord {
            pane_id: "w1:p7".into(),
            tab_id: Some("w1:t7".into()),
            ..record("a", cwd.path())
        }]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!((out.as_str(), code), ("resumed a w1:p1", 0));
        assert_eq!(ledger.0["a"].tab_id.as_deref(), Some("w1:t1"));
        let tabs = herdr.tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            (tabs[0].workspace_id.as_str(), tabs[0].label.as_str()),
            ("w1", "a")
        );
        assert_eq!(tabs[0].cwd, cwd.path());
        let started = herdr.started();
        assert_eq!(started.len(), 1);
        assert_eq!(
            (started[0].name.as_str(), started[0].pane_id.as_str()),
            ("a", "w1:p1")
        );
        assert_eq!(
            started[0].args,
            vec!["--resume", "sess-a", "--model", "opus"]
        );
        assert_eq!(ledger.0["a"].pane_id, "w1:p1");
        assert_eq!(ledger.0["a"].session_id.as_deref(), Some("sess-a"));
        assert_eq!(
            ledger.0["a"].socket.as_deref(),
            Some(Path::new(crate::herdr::fake::FAKE_SOCKET))
        );
        assert_eq!(ledger.1, ["a"]);
    }

    const OLD_PROCESS: crate::ledger::Process = crate::ledger::Process {
        pid: 4242,
        start: 99,
    };

    #[test]
    fn starting_a_new_claude_clears_the_recorded_process() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([AgentRecord {
            process: Some(OLD_PROCESS),
            ..record("a", cwd.path())
        }]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "resumed a w1:p1");
        assert_eq!(ledger.0["a"].process, None);
    }

    #[test]
    fn a_member_already_alive_keeps_its_recorded_process() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("sess-a"), "w1:p4");
        let mut ledger = ledger([AgentRecord {
            process: Some(OLD_PROCESS),
            ..record("a", cwd.path())
        }]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "alive a w1:p4");
        assert_eq!(ledger.0["a"].process, Some(OLD_PROCESS));
    }

    fn moved(cwd: &Path) -> AgentRecord {
        AgentRecord {
            pane_id: "w3:p2".into(),
            env_pane_id: Some("w2:p1".into()),
            process: Some(OLD_PROCESS),
            ..record("a", cwd)
        }
    }

    #[test]
    fn a_moved_member_herdr_resumed_after_a_restart_no_longer_owns_its_old_pane_id() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("sess-a"), "w3:p2");
        let mut ledger = ledger([moved(cwd.path())]);

        let (out, _) = run(&herdr, &mut ledger, &request());

        assert_eq!(out, "alive a w3:p2");
        let entries = ledger.entries().unwrap();
        assert_eq!(
            crate::report::member_of_own_pane(&entries, "w2:p1", None),
            None
        );
        assert!(entries["a"].is_own_pane("w3:p2"));
    }

    #[test]
    fn a_moved_member_whose_process_survives_keeps_its_old_pane_id() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("a"), Some("sess-a"), "w3:p2");
        let mut ledger = ledger([moved(cwd.path())]);
        let mut procs = FakeProcs::default();
        procs.add(OLD_PROCESS.pid, CLAUDE, 1, OLD_PROCESS.start);

        let (out, _) = revive(&herdr, &procs, &mut ledger, &request(), &|s| s.to_string())
            .unwrap()
            .report();

        assert_eq!(out, "alive a w3:p2");
        assert!(ledger.0["a"].is_own_pane("w2:p1"));
    }

    #[test]
    fn a_moved_member_resumed_in_its_own_pane_no_longer_owns_its_old_pane_id() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([AgentRecord {
            pane_id: "w1:p1".into(),
            ..moved(cwd.path())
        }]);

        let (out, _) = run(&herdr, &mut ledger, &request());

        assert_eq!(out, "resumed a w1:p1");
        assert!(!ledger.0["a"].is_own_pane("w2:p1"));
    }

    #[test]
    fn a_start_the_mailbox_cannot_take_is_a_warning_not_a_failure() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = crate::ledger::NoMailbox(ledger([record("a", cwd.path())]));
        let (out, code) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &|s| s.to_string(),
        )
        .unwrap()
        .report();
        assert_eq!(
            (out.as_str(), code),
            (
                "warning a: no start entry in its mailbox: disk full\nresumed a w1:p1",
                0
            )
        );
        assert_eq!(ledger.0.0["a"].pane_id, "w1:p1");
    }

    #[test]
    fn a_revive_outside_herdr_keeps_the_recorded_socket() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.socket = None;
        let mut ledger = ledger([AgentRecord {
            socket: Some("/run/live.sock".into()),
            process: None,
            ..record("a", cwd.path())
        }]);
        run(&herdr, &mut ledger, &request());
        assert_eq!(ledger.0["a"].pane_id, "w1:p1");
        assert_eq!(
            ledger.0["a"].socket.as_deref(),
            Some(Path::new("/run/live.sock"))
        );
    }

    #[test]
    fn a_revive_that_learns_no_socket_warns_its_stops_cannot_be_matched() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.socket = None;
        let mut ledger = ledger([AgentRecord {
            socket: None,
            ..record("a", cwd.path())
        }]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            "warning a: herdr didn't say which socket it serves and none is recorded; \
             set HERDR_SOCKET_PATH and revive it again, or its stops and reports can't be matched to it\n\
             resumed a w1:p1"
        );
    }

    #[test]
    fn reuses_a_restored_shell_pane_in_a_tab_named_for_the_agent() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, format!("resumed a {pane}"));
        assert_eq!(herdr.tabs().len(), 1);
        assert_eq!(herdr.started()[0].pane_id, pane);
        assert_eq!(ledger.0["a"].pane_id, pane);
        assert_eq!(ledger.0["a"].tab_id, Some(herdr.tabs()[0].id.clone()));
    }

    #[test]
    fn records_the_workspace_id_its_label_has_now() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w3", "loopherd")]);
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "resumed a w3:p1");
        assert_eq!(ledger.0["a"].workspace, "w3");
        assert_eq!(ledger.0["a"].tab_id.as_deref(), Some("w3:t1"));
    }

    #[test]
    fn reuses_a_shell_pane_whose_cwd_is_a_symlink_to_the_entrys_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(cwd.path(), &link).unwrap();
        let herdr = herdr();
        let pane = herdr.add_tab("w1", "a", &link, true);
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, format!("resumed a {pane}"));
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn opens_a_new_tab_unless_label_cwd_and_shell_prompt_all_match() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let near_misses = [
            ("a", elsewhere.path(), true),
            ("b", cwd.path(), true),
            ("a", cwd.path(), false),
        ];
        for (label, pane_cwd, at_prompt) in near_misses {
            let herdr = herdr();
            let busy = herdr.add_tab("w1", label, pane_cwd, at_prompt);
            let mut ledger = ledger([record("a", cwd.path())]);
            let (out, _) = run(&herdr, &mut ledger, &request());
            let started = &herdr.started()[0].pane_id;
            assert_ne!(started, &busy, "{label} {at_prompt}");
            assert_eq!(out, format!("resumed a {started}"));
            assert_eq!(herdr.tabs().len(), 2);
        }
    }

    #[test]
    fn a_resumed_agent_at_a_dialog_is_reported_blocked_and_left_alone() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedThenIdle;
        let mut ledger = ledger([record("a", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!((out.as_str(), code), ("blocked a w1:p1", 0));
        assert_eq!(herdr.agent("a").unwrap().status, AgentStatus::Blocked);
        assert!(herdr.prompts().is_empty());
        assert_eq!(ledger.0["a"].pane_id, "w1:p1");
    }

    #[test]
    fn never_sends_the_opening_prompt_but_says_how_to() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("b"), Some("sess-b"), "w1:p4");
        let unprompted = |name| AgentRecord {
            prompt_sent: false,
            ..record(name, cwd.path())
        };
        let mut ledger = ledger([unprompted("a"), unprompted("b")]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            "resumed a w1:p1 (prompt not sent, run loopherd prompt a)\n\
             alive b w1:p4 (prompt not sent, run loopherd prompt b)"
        );
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_failed_start_is_reported_closes_its_new_tab_and_the_run_continues() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr().with_live_agent(Some("b"), Some("sess-b"), "w1:p4");
        herdr.startup = Startup::Fails;
        let mut ledger = ledger([record("a", cwd.path()), record("b", cwd.path())]);
        let before = ledger.0.clone();
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            out,
            "failed a: start: claude exited during startup\nalive b w1:p4"
        );
        assert_eq!(code, 1);
        assert!(herdr.tabs().is_empty());
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn waits_for_a_new_tabs_shell_prompt_before_resuming() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.new_tab_prompt_after = Some(3);
        let mut ledger = ledger([record("a", cwd.path())]);
        assert_eq!(
            run(&herdr, &mut ledger, &request()),
            ("resumed a w1:p1".into(), 0)
        );
        assert_eq!(herdr.started().len(), 1);
    }

    #[test]
    fn a_new_tab_whose_shell_never_reaches_its_prompt_fails_start_and_closes() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.new_tab_prompt_after = None;
        let mut ledger = ledger([record("a", cwd.path())]);
        let before = ledger.0.clone();
        let req = ReviveRequest {
            start_timeout: Duration::from_millis(20),
            ..request()
        };
        let (out, code) = run(&herdr, &mut ledger, &req);
        assert_eq!(
            (out.as_str(), code),
            (
                "failed a: start: pane w1:p1 shell not at its prompt after 20ms",
                1
            )
        );
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn dry_run_says_what_it_would_do_and_changes_nothing() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("c"), Some("sess-c"), "w1:p9");
        let pane = herdr.add_tab("w1", "b", cwd.path(), true);
        let mut ledger = ledger([
            record("a", cwd.path()),
            record("b", cwd.path()),
            record("c", cwd.path()),
        ]);
        let before = ledger.0.clone();
        let req = ReviveRequest {
            dry_run: true,
            ..request()
        };
        let (out, code) = run(&herdr, &mut ledger, &req);
        assert_eq!(
            out,
            format!(
                "would resume a in a new tab in workspace loopherd\n\
                 would resume b in pane {pane}\n\
                 alive c w1:p9"
            )
        );
        assert_eq!(code, 0);
        assert_eq!(herdr.tabs().len(), 1);
        assert!(herdr.started().is_empty());
        assert_eq!(ledger.0, before);
        assert!(ledger.1.is_empty());
    }

    #[test]
    fn name_limits_the_run_to_one_entry() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([record("a", cwd.path()), record("b", cwd.path())]);
        let req = ReviveRequest {
            name: Some("b".into()),
            ..request()
        };
        assert_eq!(
            run(&herdr, &mut ledger, &req),
            ("resumed b w1:p1".into(), 0)
        );
        assert_eq!(herdr.started().len(), 1);

        let req = ReviveRequest {
            name: Some("ghost".into()),
            ..request()
        };
        let err = revive(&herdr, &FakeProcs::default(), &mut ledger, &req, &|s| {
            s.to_string()
        })
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "ghost is not in the ledger");
    }

    fn lead(name: &str, cwd: &Path) -> AgentRecord {
        AgentRecord {
            role: Some(crate::ledger::Role::Lead),
            after_merge: None,
            earlier: vec![],
            herd: Some("h".into()),
            ..record(name, cwd)
        }
    }

    /// Holds `lead`'s watch lock in `dir` as a running watch would.
    fn hold_watch(dir: &Path, lead: &str) -> crate::lock::Lock {
        let file = std::fs::File::create(crate::watch::lead_lock_path(dir, lead)).unwrap();
        crate::lock::Lock::try_take(file).unwrap().unwrap()
    }

    fn rearmed(lead: &str) -> Vec<(String, String)> {
        vec![(lead.into(), REARM_PROMPT.into())]
    }

    #[test]
    fn an_alive_lead_with_no_watch_running_is_prompted_to_re_arm() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            (out.as_str(), code),
            ("alive lead-h w1:p4 (prompted to re-arm its watch)", 0)
        );
        assert_eq!(herdr.prompts(), rearmed("lead-h"));
    }

    const SKIPPED_REARM: &str = " (not prompted to re-arm: its pane shows a subagent's view; \
        switch it back to the main conversation, then run loopherd revive --name lead-h)";

    #[test]
    fn an_alive_lead_whose_pane_shows_a_subagent_view_is_not_prompted() {
        for screen in [SUBAGENT_PROMPT, SUBAGENT_PICKER] {
            let cwd = tempfile::tempdir().unwrap();
            let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
            herdr.set_screen("w1:p4", screen);
            let mut ledger = ledger([lead("lead-h", cwd.path())]);
            let (out, code) = run(&herdr, &mut ledger, &request());
            assert_eq!(out, format!("alive lead-h w1:p4{SKIPPED_REARM}"));
            assert_eq!(code, 1);
            assert!(herdr.prompts().is_empty());
        }
    }

    #[test]
    fn a_started_lead_whose_pane_shows_a_subagent_view_is_not_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        herdr.set_screen("w1:p1", SUBAGENT_PICKER);
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, format!("resumed lead-h w1:p1{SKIPPED_REARM}"));
        assert_eq!(code, 1);
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn dry_run_reports_the_skip_for_a_lead_whose_pane_shows_a_subagent_view() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        herdr.set_screen("w1:p4", SUBAGENT_PROMPT);
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let req = ReviveRequest {
            dry_run: true,
            ..request()
        };
        let (out, code) = run(&herdr, &mut ledger, &req);
        assert_eq!(out, format!("alive lead-h w1:p4{SKIPPED_REARM}"));
        assert_eq!(code, 1, "the real run would exit 1");
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn dry_run_says_it_could_not_check_a_lead_whose_pane_it_cannot_read() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        herdr.fail_visible_text = true;
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let req = ReviveRequest {
            dry_run: true,
            ..request()
        };
        let (out, code) = run(&herdr, &mut ledger, &req);
        assert_eq!(
            (out.as_str(), code),
            (
                "alive lead-h w1:p4 (can't tell whether it would be prompted: read lead-h's pane \
                 w1:p4 to check for a subagent view: pane read refused)",
                1
            )
        );
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_whose_watch_is_running_is_left_alone() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let _watch = hold_watch(state.path(), "lead-h");
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let req = ReviveRequest {
            reports_dir: state.path().into(),
            ..request()
        };
        let (out, code) = run(&herdr, &mut ledger, &req);
        assert_eq!(
            (out.as_str(), code),
            ("alive lead-h w1:p4 (watch running)", 0)
        );
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_free_lock_left_by_an_ended_watch_still_gets_a_prompt() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        drop(hold_watch(state.path(), "lead-h"));
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let req = ReviveRequest {
            reports_dir: state.path().into(),
            ..request()
        };
        run(&herdr, &mut ledger, &req);
        assert_eq!(herdr.prompts(), rearmed("lead-h"));
    }

    #[test]
    fn a_lead_revive_starts_is_prompted_to_re_arm() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            (out.as_str(), code),
            ("resumed lead-h w1:p1 (prompted to re-arm its watch)", 0)
        );
        assert_eq!(herdr.prompts(), rearmed("lead-h"));
    }

    #[test]
    fn a_revived_lead_keeps_its_created_time() {
        let cwd = tempfile::tempdir().unwrap();
        let alive = herdr().with_live_agent(Some("lead-a"), Some("sess-lead-a"), "w1:p4");
        let mut ledger = ledger([lead("lead-a", cwd.path()), lead("lead-b", cwd.path())]);

        let (out, _) = run(&alive, &mut ledger, &request());

        assert!(out.contains("alive lead-a"), "{out}");
        assert!(out.contains("resumed lead-b"), "{out}");
        for name in ["lead-a", "lead-b"] {
            assert_eq!(ledger.0[name].created, "2026-10-31T00:00:00Z", "{name}");
        }
    }

    #[test]
    fn workers_are_never_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("b"), Some("sess-b"), "w1:p4");
        let mut ledger = ledger([record("a", cwd.path()), record("b", cwd.path())]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "resumed a w1:p1\nalive b w1:p4");
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_resumed_at_a_dialog_is_reported_blocked_and_not_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedThenIdle;
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!((out.as_str(), code), ("blocked lead-h w1:p1", 0));
        assert_eq!(herdr.agent("lead-h").unwrap().status, AgentStatus::Blocked);
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn an_alive_lead_at_a_dialog_is_not_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        herdr.set_status("lead-h", AgentStatus::Blocked);
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            (out.as_str(), code),
            ("alive lead-h w1:p4 (blocked, not prompted)", 0)
        );
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_retired_lead_is_skipped_and_not_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([AgentRecord {
            retired: true,
            ..lead("lead-h", cwd.path())
        }]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "skipped lead-h: retired");
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn a_re_arm_prompt_herdr_refuses_is_a_failure_after_the_resume() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.fail_prompt = true;
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            (out.as_str(), code),
            (
                "resumed lead-h w1:p1 (re-arm failed: prompt refused; run loopherd revive --name lead-h)",
                1
            )
        );
        assert_eq!(ledger.0["lead-h"].pane_id, "w1:p1");
    }

    #[test]
    fn dry_run_shows_which_leads_would_be_prompted() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let _watch = hold_watch(state.path(), "lead-w");
        let herdr = herdr()
            .with_live_agent(Some("lead-a"), Some("sess-lead-a"), "w1:p4")
            .with_live_agent(Some("lead-w"), Some("sess-lead-w"), "w1:p5");
        let mut ledger = ledger([
            lead("lead-a", cwd.path()),
            lead("lead-s", cwd.path()),
            lead("lead-w", cwd.path()),
            record("x", cwd.path()),
        ]);
        let req = ReviveRequest {
            dry_run: true,
            reports_dir: state.path().into(),
            ..request()
        };
        let (out, _) = run(&herdr, &mut ledger, &req);
        assert_eq!(
            out,
            format!(
                "alive lead-a w1:p4 (would prompt: {REARM_PROMPT})\n\
                 would resume lead-s in a new tab in workspace loopherd (would prompt: {REARM_PROMPT})\n\
                 alive lead-w w1:p5 (watch running)\n\
                 would resume x in a new tab in workspace loopherd"
            )
        );
        assert!(herdr.prompts().is_empty());
    }

    fn on_socket(record: AgentRecord, socket: Option<&str>) -> AgentRecord {
        AgentRecord {
            socket: socket.map(PathBuf::from),
            ..record
        }
    }

    fn startup(reports_dir: &Path) -> ReviveRequest {
        ReviveRequest {
            reports_dir: reports_dir.into(),
            startup: Some("/run/dev.sock".into()),
            ..request()
        }
    }

    #[test]
    fn a_startup_revive_touches_only_entries_recorded_on_the_hooks_socket() {
        for (here, touched, other) in [
            ("/run/dev.sock", "dev", "live"),
            ("/run/live.sock", "live", "dev"),
        ] {
            let cwd = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let mut herdr = herdr();
            herdr.socket = Some(here.into());
            let mut ledger = ledger([
                on_socket(record("dev", cwd.path()), Some("/run/dev.sock")),
                on_socket(record("live", cwd.path()), Some("/run/live.sock")),
                on_socket(record("unknown", cwd.path()), None),
            ]);
            let before = ledger.0.clone();
            let req = ReviveRequest {
                startup: Some(here.into()),
                ..startup(state.path())
            };

            let (out, code) = run(&herdr, &mut ledger, &req);

            assert_eq!((out, code), (format!("resumed {touched} w1:p1"), 0));
            let started: Vec<_> = herdr.started().into_iter().map(|s| s.name).collect();
            assert_eq!(started, [touched]);
            assert_eq!(ledger.0[other], before[other], "{here}");
            assert_eq!(ledger.0["unknown"], before["unknown"], "{here}");
        }
    }

    #[test]
    fn a_manual_revive_skips_entries_recorded_on_another_servers_socket() {
        for (here, touched, other) in [
            ("/run/dev.sock", "dev", "live"),
            ("/run/live.sock", "live", "dev"),
        ] {
            let cwd = tempfile::tempdir().unwrap();
            let mut herdr = herdr();
            herdr.socket = Some(here.into());
            let mut ledger = ledger([
                on_socket(record("dev", cwd.path()), Some("/run/dev.sock")),
                on_socket(record("live", cwd.path()), Some("/run/live.sock")),
            ]);
            let before = ledger.0.clone();

            let (out, code) = run(&herdr, &mut ledger, &request());

            let skipped = format!("skipped {other}: recorded on /run/{other}.sock");
            let mut lines: Vec<_> = out.lines().collect();
            lines.sort_unstable();
            assert_eq!(
                lines,
                [format!("resumed {touched} w1:p1"), skipped],
                "{here}"
            );
            assert_eq!(code, 0, "{here}");
            let started: Vec<_> = herdr.started().into_iter().map(|s| s.name).collect();
            assert_eq!(started, [touched], "{here}");
            assert_eq!(ledger.0[other], before[other], "{here}");
            assert_eq!(
                ledger.0[touched].socket.as_deref(),
                Some(Path::new(here)),
                "{here}"
            );
        }
    }

    #[test]
    fn a_manual_dry_run_and_a_named_revive_also_skip_another_servers_entry() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.socket = Some("/run/dev.sock".into());
        for req in [
            ReviveRequest {
                dry_run: true,
                ..request()
            },
            ReviveRequest {
                name: Some("live".into()),
                ..request()
            },
        ] {
            let mut ledger = ledger([on_socket(
                record("live", cwd.path()),
                Some("/run/live.sock"),
            )]);
            let (out, _) = run(&herdr, &mut ledger, &req);
            assert_eq!(out, "skipped live: recorded on /run/live.sock");
        }
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
    }

    /// Lists `stale` while `update` sees `current`, as when another
    /// command writes the ledger mid-revive.
    struct StaleLedger {
        stale: MemLedger,
        current: MemLedger,
    }

    impl Ledger for StaleLedger {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            self.stale.entries()
        }
        fn record(&mut self, record: AgentRecord) -> Result<()> {
            self.current.record(record)
        }
        fn update(
            &mut self,
            name: &str,
            change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.current.update(name, change)
        }
        fn remove_herd(
            &mut self,
            herd: &str,
            before: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<crate::ledger::RemovedHerd> {
            self.current.remove_herd(herd, before)
        }
        fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()> {
            self.current.started(name, at, resumed)
        }
    }

    #[test]
    fn a_socket_recorded_since_revive_read_the_ledger_fails_the_entry_and_saves_nothing() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.socket = Some("/run/dev.sock".into());
        let mut ledger = StaleLedger {
            stale: ledger([on_socket(record("a", cwd.path()), None)]),
            current: ledger([on_socket(record("a", cwd.path()), Some("/run/live.sock"))]),
        };
        let before = ledger.current.0.clone();

        let (out, code) = revive(
            &herdr,
            &FakeProcs::default(),
            &mut ledger,
            &request(),
            &|s| s.to_string(),
        )
        .unwrap()
        .report();

        assert_eq!(
            (out.as_str(), code),
            (
                "failed a: ledger: recorded on /run/live.sock since revive read the ledger; \
                 its resumed session is left running in pane w1:p1",
                1
            )
        );
        assert_eq!(ledger.current.0, before);
    }

    #[test]
    fn a_socket_recorded_through_a_symlinked_dir_is_the_same_server() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = tempfile::tempdir().unwrap();
        let real = dirs.path().join("real");
        let link = dirs.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let recorded = link.join("herdr.sock");
        let mut herdr = herdr();
        herdr.socket = Some(real.join("herdr.sock"));
        let mut ledger = ledger([AgentRecord {
            socket: Some(recorded.clone()),
            ..record("a", cwd.path())
        }]);

        let (out, _) = run(&herdr, &mut ledger, &request());

        assert_eq!(out, "resumed a w1:p1");
        assert_eq!(ledger.0["a"].socket, Some(recorded));
    }

    #[test]
    fn a_manual_revive_records_the_reached_socket_for_an_entry_with_none() {
        let cwd = tempfile::tempdir().unwrap();
        let mut herdr = herdr();
        herdr.socket = Some("/run/dev.sock".into());
        let mut ledger = ledger([on_socket(record("a", cwd.path()), None)]);
        let (out, _) = run(&herdr, &mut ledger, &request());
        assert_eq!(out, "resumed a w1:p1");
        assert_eq!(
            ledger.0["a"].socket.as_deref(),
            Some(Path::new("/run/dev.sock"))
        );
    }

    #[test]
    fn a_startup_revive_with_nothing_to_revive_starts_nothing() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut herdr = herdr().with_live_agent(Some("a"), Some("sess-a"), "w1:p4");
        herdr.socket = Some("/run/dev.sock".into());
        for records in [
            vec![],
            vec![on_socket(record("a", cwd.path()), Some("/run/dev.sock"))],
        ] {
            let mut ledger = ledger(records);
            let (out, code) = run(&herdr, &mut ledger, &startup(state.path()));
            assert!(out.is_empty() || out == "alive a w1:p4", "{out}");
            assert_eq!(code, 0, "{out}");
        }
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
    }

    fn notes(dir: &Path, name: &str) -> Vec<String> {
        let (entries, warnings) = crate::report::history(dir, name).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        entries
            .iter()
            .filter_map(|e| match &e.body {
                crate::report::Body::Note { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn dev_herdr() -> FakeHerdr {
        let mut herdr = herdr();
        herdr.socket = Some("/run/dev.sock".into());
        herdr
    }

    fn dev(record: AgentRecord) -> AgentRecord {
        on_socket(record, Some("/run/dev.sock"))
    }

    #[test]
    fn a_startup_revive_leaves_a_note_for_each_lead_it_revives() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let herdr = dev_herdr();
        let mut ledger = ledger([
            dev(lead("lead-h", cwd.path())),
            dev(record("w", cwd.path())),
        ]);

        let (out, code) = run(&herdr, &mut ledger, &startup(state.path()));

        assert_eq!(code, 0, "{out}");
        assert_eq!(
            notes(state.path(), "lead-h"),
            ["herdr's startup revive: resumed lead-h w1:p1 (prompted to re-arm its watch)"]
        );
        assert!(notes(state.path(), "w").is_empty());
    }

    #[test]
    fn a_startup_revive_leaves_a_note_for_a_lead_it_skips_for_a_subagent_view() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let herdr = dev_herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        herdr.set_screen("w1:p4", SUBAGENT_PROMPT);
        let mut ledger = ledger([dev(lead("lead-h", cwd.path()))]);

        let (out, code) = run(&herdr, &mut ledger, &startup(state.path()));

        assert_eq!(code, 1, "{out}");
        assert_eq!(
            notes(state.path(), "lead-h"),
            [format!(
                "herdr's startup revive: alive lead-h w1:p4{SKIPPED_REARM}"
            )]
        );
    }

    #[test]
    fn a_startup_revive_leaves_a_note_for_a_lead_it_fails_to_start() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut herdr = dev_herdr();
        herdr.startup = Startup::Fails;
        let mut ledger = ledger([dev(lead("lead-h", cwd.path()))]);

        let (out, code) = run(&herdr, &mut ledger, &startup(state.path()));

        assert_eq!(code, 1, "{out}");
        assert_eq!(
            notes(state.path(), "lead-h"),
            ["herdr's startup revive: failed lead-h: start: claude exited during startup"]
        );
    }

    #[test]
    fn a_startup_revive_leaves_a_note_for_an_alive_lead_it_prompts() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let herdr = dev_herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([dev(lead("lead-h", cwd.path()))]);

        run(&herdr, &mut ledger, &startup(state.path()));

        assert_eq!(
            notes(state.path(), "lead-h"),
            ["herdr's startup revive: alive lead-h w1:p4 (prompted to re-arm its watch)"]
        );
    }

    #[test]
    fn a_startup_dry_run_leaves_no_notes() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let herdr = dev_herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        herdr.set_screen("w1:p4", SUBAGENT_PROMPT);
        let mut ledger = ledger([dev(lead("lead-h", cwd.path()))]);
        let req = ReviveRequest {
            dry_run: true,
            ..startup(state.path())
        };

        run(&herdr, &mut ledger, &req);

        assert!(notes(state.path(), "lead-h").is_empty());
    }

    #[test]
    fn a_startup_revive_leaves_no_note_for_a_lead_already_running_its_watch() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let _watch = hold_watch(state.path(), "lead-h");
        let herdr = dev_herdr().with_live_agent(Some("lead-h"), Some("sess-lead-h"), "w1:p4");
        let mut ledger = ledger([dev(lead("lead-h", cwd.path()))]);

        let (out, code) = run(&herdr, &mut ledger, &startup(state.path()));

        assert_eq!(
            (out.as_str(), code),
            ("alive lead-h w1:p4 (watch running)", 0)
        );
        assert!(notes(state.path(), "lead-h").is_empty());
    }

    #[test]
    fn a_manual_revive_leaves_no_notes() {
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([lead("lead-h", cwd.path())]);
        let req = ReviveRequest {
            reports_dir: state.path().into(),
            ..request()
        };

        let (out, _) = run(&herdr, &mut ledger, &req);

        assert_eq!(out, "resumed lead-h w1:p1 (prompted to re-arm its watch)");
        assert!(notes(state.path(), "lead-h").is_empty());
    }

    #[test]
    fn a_revive_beside_a_startup_one_waits_then_finds_its_member_alive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cwd = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("ledger.json");
        crate::ledger::FileLedger::new(path.clone())
            .record(dev(record("a", cwd.path())))
            .unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        let first_call = AtomicBool::new(true);
        let mut herdr = dev_herdr();
        // Holds the first revive, and only it, inside its first `agent list`.
        herdr.on_agents = Some(Box::new(move || {
            if first_call.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                let _ = released.lock().unwrap().recv();
            }
        }));
        let manual_req = ReviveRequest {
            startup: None,
            ..startup(state.path())
        };
        let run_on = |req: &ReviveRequest| {
            let mut ledger = crate::ledger::FileLedger::new(path.clone());
            revive(&herdr, &FakeProcs::default(), &mut ledger, req, &|s| {
                s.to_string()
            })
            .unwrap()
        };

        let (first, manual) = std::thread::scope(|scope| {
            let first = scope.spawn(|| run_on(&startup(state.path())));
            entered.recv().unwrap();
            let manual = scope.spawn(|| run_on(&manual_req));
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !manual.is_finished(),
                "the manual revive ran during the startup one"
            );
            drop(release);
            (first.join().unwrap(), manual.join().unwrap())
        });

        assert_eq!(first.lines, ["resumed a w1:p1"]);
        assert_eq!(manual.lines, ["alive a w1:p1"]);
        assert_eq!(herdr.started().len(), 1);
    }

    #[test]
    fn a_revive_gives_up_on_one_that_never_ends() {
        let state = tempfile::tempdir().unwrap();
        let held = File::create(state.path().join("revive.lock")).unwrap();
        let _held = Lock::try_take(held).unwrap().unwrap();
        let req = ReviveRequest {
            lock_timeout: Duration::from_millis(100),
            ..startup(state.path())
        };

        let err = revive(
            &dev_herdr(),
            &FakeProcs::default(),
            &mut ledger([]),
            &req,
            &|s| s.to_string(),
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").starts_with("another revive is still running after 100ms"),
            "{err:#}"
        );
    }

    #[test]
    fn an_entry_without_a_session_cannot_be_resumed() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = herdr();
        let mut ledger = ledger([AgentRecord {
            session_id: None,
            ..record("a", cwd.path())
        }]);
        let (out, code) = run(&herdr, &mut ledger, &request());
        assert_eq!(
            (out.as_str(), code),
            ("skipped a: no session ID recorded", 0)
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn the_rearm_prompt_points_to_the_skills_lead_section_only() {
        assert!(
            REARM_PROMPT.contains(
                "then read the skill's Lead section (`loopherd --skill --section lead`, not the whole \
                 skill) and re-arm your watch as it says."
            ),
            "{REARM_PROMPT}"
        );
        assert!(crate::skill::section("lead").is_some());
        assert!(!REARM_PROMPT.contains("docs/"), "{REARM_PROMPT}");
    }

    #[test]
    fn the_rearm_prompt_reads_only_recent_reports() {
        assert!(
            REARM_PROMPT.contains(
                "`loopherd reports --all --tail 40` (for a fuller history, \
                 `--since <time>` or plain `--all`)"
            ),
            "{REARM_PROMPT}"
        );
    }
}
