//! `loopherd spawn`: start a named Claude agent in a new tab and record it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::context::SHEPHERD;
use crate::herdr::{
    Agent, AgentStatus, Herdr, NewTab, StartOutcome, WaitOutcome, Workspace, wait_for_shell_prompt,
};
use crate::ledger::{AgentRecord, Ledger, Role};
use crate::template::Template;
use crate::worktree::{ensure_detached_worktree, ensure_worktree, planned_worktree, worktree_path};

pub const BLOCKED_EXIT: i32 = 3;

const SHELL_POLL: Duration = Duration::from_millis(50);
const SESSION_POLL: Duration = Duration::from_millis(200);
/// How long a ready agent's session gets to show in herdr, within `ready_timeout`.
const SESSION_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_LATER: &str = "the Stop hook records it at the member's first stop";

/// How long a new tab's shell gets to run its rc files, apart from `ready_timeout`.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(60);

const PROMPT_KEYS: [&str; 4] = ["name", "branch", "worktree", "pane"];

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub repo: PathBuf,
    pub name: String,
    pub worktree: Option<Checkout>,
    pub workspace: Option<String>,
    pub prompt: Option<Prompt>,
    /// How long Claude gets to become idle.
    pub ready_timeout: Duration,
    pub shell_timeout: Duration,
    pub claude_args: Vec<String>,
    pub member_of: Option<MemberOf>,
}

/// What the agent's worktree has checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout {
    /// A worktree on this branch, created if needed.
    Branch(String),
    /// A worktree at this slug's path, detached at origin's default branch.
    Detached(String),
}

impl Checkout {
    fn branch(&self) -> Option<&str> {
        match self {
            Checkout::Branch(branch) => Some(branch),
            Checkout::Detached(_) => None,
        }
    }

    fn ensure(&self, repo: &Path) -> Result<PathBuf> {
        match self {
            Checkout::Branch(branch) => ensure_worktree(repo, branch),
            Checkout::Detached(slug) => ensure_detached_worktree(repo, slug),
        }
    }

    fn planned(&self, repo: &Path) -> Result<PathBuf> {
        match self {
            Checkout::Branch(branch) => planned_worktree(repo, branch),
            Checkout::Detached(slug) => Ok(worktree_path(repo, slug)),
        }
    }
}

#[cfg(test)]
impl From<&str> for Checkout {
    fn from(branch: &str) -> Self {
        Checkout::Branch(branch.into())
    }
}

/// Set by `loopherd add` and `lead`: the herd and template a member came from.
#[derive(Debug, Clone)]
pub struct MemberOf {
    pub herd: String,
    /// `None` for a lead.
    pub template: Option<String>,
    /// Each param's name and value, also prompt placeholders.
    pub params: Vec<(String, String)>,
    /// Prompt placeholders the ledger doesn't record.
    pub values: Vec<(String, String)>,
    pub role: Option<Role>,
}

/// Prompt text and the flag it came from, which errors name.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub source: String,
    pub text: String,
}

#[cfg(test)]
impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Prompt {
            source: "--prompt".into(),
            text: text.into(),
        }
    }
}

/// Where the agent went, and whether it got past startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    pub tab_id: String,
    pub pane_id: String,
    pub worktree: Option<PathBuf>,
    pub session_id: Option<String>,
    /// Why herdr gave no session for a ready agent.
    pub no_session: Option<String>,
    /// Still at a dialog when the ready timeout ran out.
    pub blocked: bool,
    /// Why the mailbox has no `start` entry for it, if it hasn't.
    pub unmarked: Option<String>,
}

impl Spawned {
    /// What went wrong around a spawn that still went ahead.
    pub fn warnings(&self) -> Vec<String> {
        let unmarked = self
            .unmarked
            .iter()
            .map(|why| format!("no start entry in its mailbox: {why}"));
        let no_session = self
            .no_session
            .iter()
            .map(|why| format!("{why}; {SESSION_LATER}"));
        no_session.chain(unmarked).collect()
    }

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
        lines.extend(self.warnings().iter().map(|w| format!("warning: {w}")));
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
    /// With why herdr gave no session, if it didn't.
    Ready(Agent, Option<String>),
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
    let (template, workspace, socket) = check(herdr, req, inherited_workspace)?;
    let worktree = match &req.worktree {
        Some(checkout) => Some(checkout.ensure(&req.repo).context("worktree")?),
        None => None,
    };
    let cwd = worktree.as_ref().unwrap_or(&req.repo);
    let tab = herdr
        .create_tab(&workspace.id, cwd, &req.name)
        .context("tab")?;

    let prompt = match template {
        Some(t) => match render_prompt(&t, req, cwd, &tab) {
            Ok(text) => Some(text),
            Err(e) => return Err(close_after_failure(herdr, &tab, e.context("prompt"))),
        },
        None => None,
    };

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
        branch: Some(req.branch().unwrap_or_default().to_string()),
        workspace: workspace.id,
        workspace_label: Some(workspace.label),
        claude_args: req.claude_args.clone(),
        prompt: prompt.clone(),
        prompt_sent: false,
        created: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        retired: false,
        retired_at: None,
        released_at: None,
        herd: req.member_of.as_ref().map(|m| m.herd.clone()),
        template: req.member_of.as_ref().and_then(|m| m.template.clone()),
        params: req
            .member_of
            .as_ref()
            .map(|m| m.params.iter().cloned().collect()),
        socket: Some(socket),
        process: None,
        role: req.member_of.as_ref().and_then(|m| m.role),
        after_merge: None,
    };
    let (agent, no_session) = match readiness {
        Readiness::Blocked => {
            let unmarked = record_started(ledger, record)?;
            return Ok(Spawned {
                tab_id: tab.tab_id,
                pane_id: tab.pane_id,
                worktree,
                session_id: None,
                no_session: None,
                blocked: true,
                unmarked,
            });
        }
        Readiness::Ready(agent, no_session) => (agent, no_session),
    };
    record.session_id = agent.session_id.clone();
    let sent = match &prompt {
        Some(text) => herdr.prompt(&req.name, text).context("prompt"),
        None => Ok(()),
    };
    record.prompt_sent = sent.is_ok() && prompt.is_some();
    let unmarked = record_started(ledger, record)?;
    sent?;
    Ok(Spawned {
        tab_id: tab.tab_id,
        pane_id: tab.pane_id,
        worktree,
        session_id: agent.session_id,
        no_session,
        blocked: false,
        unmarked,
    })
}

/// Records the member, then marks its start; a failed mark is only a
/// warning, since the member is already running.
fn record_started(ledger: &mut dyn Ledger, record: AgentRecord) -> Result<Option<String>> {
    let name = record.name.clone();
    ledger.record(record).context("ledger")?;
    Ok(ledger
        .started(&name, SystemTime::now(), false)
        .err()
        .map(|e| format!("{e:#}")))
}

impl SpawnRequest {
    /// The worktree's branch; `None` without a worktree or when detached.
    pub fn branch(&self) -> Option<&str> {
        self.worktree.as_ref().and_then(Checkout::branch)
    }
}

/// What spawn would do, found with read-only herdr and git calls.
#[derive(Debug)]
pub struct Preview {
    pub workspace_label: String,
    pub worktree: Option<PathBuf>,
    /// Rendered, with `{pane}` left in since no pane exists yet.
    pub prompt: Option<String>,
}

/// Runs spawn's checks and renders its prompt, creating nothing.
pub fn preview(
    herdr: &dyn Herdr,
    req: &SpawnRequest,
    inherited_workspace: Option<&str>,
) -> Result<Preview> {
    let (template, workspace, _) = check(herdr, req, inherited_workspace)?;
    let worktree = match &req.worktree {
        Some(checkout) => Some(checkout.planned(&req.repo).context("worktree")?),
        None => None,
    };
    let cwd = worktree.as_ref().unwrap_or(&req.repo);
    let unborn = NewTab {
        tab_id: String::new(),
        pane_id: "{pane}".into(),
    };
    let prompt = template
        .map(|t| render_prompt(&t, req, cwd, &unborn))
        .transpose()
        .context("prompt")?;
    Ok(Preview {
        workspace_label: workspace.label,
        worktree,
        prompt,
    })
}

/// Everything spawn checks before it creates anything.
fn check(
    herdr: &dyn Herdr,
    req: &SpawnRequest,
    inherited_workspace: Option<&str>,
) -> Result<(Option<Template>, Workspace, PathBuf)> {
    validate_name(&req.name).context("name")?;
    let template = parse_prompt(req).context("prompt")?;
    ensure_name_free(herdr, &req.name).context("name")?;
    let workspace = resolve_workspace(herdr, req.workspace.as_deref(), inherited_workspace)
        .context("workspace")?;
    let socket = herdr.socket().context(
        "herdr didn't say which socket it serves; set HERDR_SOCKET_PATH, or the member's stops can't be matched to it",
    )?;
    Ok((template, workspace, socket))
}

fn parse_prompt(req: &SpawnRequest) -> Result<Option<Template>> {
    let Some(Prompt { source, text }) = &req.prompt else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        bail!("{source} is empty");
    }
    let template = Template::parse(text)?;
    template.check(member_values(req).keys().copied().chain(PROMPT_KEYS))?;
    Ok(Some(template))
}

/// `{herd}` and the params, for a herd member; nothing otherwise.
fn member_values(req: &SpawnRequest) -> HashMap<&str, &str> {
    let Some(member) = &req.member_of else {
        return HashMap::new();
    };
    let params = member.params.iter().chain(&member.values);
    params
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain([("herd", member.herd.as_str())])
        .collect()
}

fn render_prompt(
    template: &Template,
    req: &SpawnRequest,
    cwd: &Path,
    tab: &NewTab,
) -> Result<String> {
    let worktree = cwd.display().to_string();
    let mut values = member_values(req);
    values.extend([
        ("name", req.name.as_str()),
        ("branch", req.branch().unwrap_or("")),
        ("worktree", worktree.as_str()),
        ("pane", tab.pane_id.as_str()),
    ]);
    let text = template.render(&values)?;
    if let Some(Prompt { source, .. }) = &req.prompt
        && text.trim().is_empty()
    {
        bail!("{source} is empty once rendered");
    }
    Ok(text)
}

fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        bail!("{name:?} must match [a-z][a-z0-9_-]{{0,31}}");
    }
    if name == SHEPHERD {
        bail!("{name} is reserved: the shepherd's handoff is handoffs/{SHEPHERD}.md");
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
) -> Result<Workspace> {
    let workspaces = herdr.workspaces()?;
    let Some(label) = label else {
        let id = inherited
            .ok_or_else(|| anyhow!("not running inside herdr; pass --workspace <label>"))?;
        return workspaces
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow!("no workspace {id}"));
    };
    let mut matches = workspaces.into_iter().filter(|w| w.label == label);
    match (matches.next(), matches.next()) {
        (Some(w), None) => Ok(w),
        (None, _) => bail!("no workspace labeled {label:?}"),
        (Some(_), Some(_)) => bail!("more than one workspace is labeled {label:?}"),
    }
}

fn start_until_ready(herdr: &dyn Herdr, req: &SpawnRequest, tab: &NewTab) -> Result<Readiness> {
    wait_for_shell_prompt(herdr, &tab.pane_id, req.shell_timeout, SHELL_POLL)?;
    let deadline = Instant::now() + req.ready_timeout;
    match herdr.start_claude(&req.name, &tab.pane_id, req.ready_timeout, &req.claude_args)? {
        StartOutcome::Ready(agent) => return Ok(with_session(herdr, &req.name, agent, deadline)),
        StartOutcome::Blocked => {}
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match herdr.wait(&req.name, &[AgentStatus::Idle], remaining)? {
        WaitOutcome::Reached => Ok(with_session(
            herdr,
            &req.name,
            herdr.agent(&req.name)?,
            deadline,
        )),
        WaitOutcome::TimedOut => match herdr.agent(&req.name)?.status {
            AgentStatus::Blocked => Ok(Readiness::Blocked),
            status => bail!(
                "agent not idle after {}s (status {status:?})",
                req.ready_timeout.as_secs()
            ),
        },
    }
}

/// herdr can call an agent ready a moment before it has read the agent's
/// session id, so poll for it a while. Without herdr's Claude integration it
/// never comes, and a failed read must not fail a spawn whose agent is ready.
fn with_session(herdr: &dyn Herdr, name: &str, mut agent: Agent, deadline: Instant) -> Readiness {
    let limit = deadline
        .saturating_duration_since(Instant::now())
        .min(SESSION_TIMEOUT);
    let until = Instant::now() + limit;
    while agent.session_id.is_none() {
        if Instant::now() >= until {
            let why = format!(
                "herdr reported no session within {}s",
                limit.as_millis().div_ceil(1000)
            );
            return Readiness::Ready(agent, Some(why));
        }
        std::thread::sleep(SESSION_POLL);
        match herdr.agent(name) {
            Ok(read) => agent = read,
            Err(e) => {
                let why = format!("could not read its session from herdr: {e:#}");
                return Readiness::Ready(agent, Some(why));
            }
        }
    }
    Readiness::Ready(agent, None)
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
            shell_timeout: SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "my-app")])
    }

    fn run(herdr: &FakeHerdr, ledger: &mut MemLedger, req: &SpawnRequest) -> Result<Spawned> {
        spawn(herdr, ledger, req, Some("w1"))
    }

    #[test]
    fn the_shepherds_name_is_reserved_for_its_handoff() {
        let herdr = herdr();
        let err = run(&herdr, &mut MemLedger::default(), &request("shepherd")).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "name: shepherd is reserved: the shepherd's handoff is handoffs/shepherd.md"
        );
        assert!(herdr.tabs().is_empty());
        assert!(run(&herdr, &mut MemLedger::default(), &request("shepherd-2")).is_ok());
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
            workspace: Some("my-app".into()),
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
        assert_eq!(ledger.0["a"].branch.as_deref(), Some(""));
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
        assert_eq!(ledger.0["a"].branch.as_deref(), Some("feature/existing"));
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
    fn records_the_workspace_label_and_claude_args_for_revive() {
        for (flag, inherited) in [(Some("my-app"), None), (None, Some("w2"))] {
            let herdr = herdr();
            let mut ledger = MemLedger::default();
            let req = SpawnRequest {
                workspace: flag.map(str::to_string),
                claude_args: vec!["--model".into(), "opus".into()],
                ..request("a")
            };
            spawn(&herdr, &mut ledger, &req, inherited).unwrap();
            let rec = &ledger.0["a"];
            assert_eq!(rec.workspace, "w2");
            assert_eq!(rec.workspace_label.as_deref(), Some("my-app"));
            assert_eq!(rec.claude_args, vec!["--model", "opus"]);
        }
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
    fn waits_for_the_session_of_an_agent_herdr_reports_ready_before_knowing_it() {
        let mut herdr = herdr();
        herdr.session_after_gets = 3;
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(
            spawned.report("a"),
            ("tab w1:t1\npane w1:p1\nsession session-a".into(), 0)
        );
        assert_eq!(ledger.0["a"].session_id.as_deref(), Some("session-a"));
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn waits_for_the_session_of_an_agent_idle_after_a_startup_dialog() {
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedThenIdle;
        herdr.session_after_gets = 3;
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(spawned.session_id.as_deref(), Some("session-a"));
        assert_eq!(ledger.0["a"].session_id.as_deref(), Some("session-a"));
    }

    #[test]
    fn a_session_still_unknown_at_the_ready_timeout_is_recorded_unknown() {
        let mut herdr = herdr();
        // Arrives after about 5s of polls, well past the 1s ready timeout.
        herdr.session_after_gets = 25;
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            ready_timeout: Duration::from_secs(1),
            ..request("a")
        };
        let spawned = run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(
            spawned.report("a"),
            (
                "tab w1:t1\npane w1:p1\nsession unknown\n\
                 warning: herdr reported no session within 1s; \
                 the Stop hook records it at the member's first stop"
                    .into(),
                0
            )
        );
        assert_eq!(ledger.0["a"].session_id, None);
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn a_session_later_than_10s_stops_the_wait_short_of_the_ready_timeout() {
        let mut herdr = herdr();
        // Arrives after about 15s of polls: past the 10s cap, well inside the 60s ready timeout.
        herdr.session_after_gets = 75;
        let mut ledger = MemLedger::default();
        let started = Instant::now();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(spawned.session_id, None);
        assert!(
            spawned
                .report("a")
                .0
                .contains("warning: herdr reported no session within 10s")
        );
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn a_failed_session_read_keeps_the_ready_agent_with_a_warning() {
        let mut herdr = herdr();
        herdr.session_after_gets = 1;
        herdr.agent_get_fails_after_start = true;
        let mut ledger = MemLedger::default();
        let spawned = run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(
            spawned.report("a"),
            (
                "tab w1:t1\npane w1:p1\nsession unknown\n\
                 warning: could not read its session from herdr: herdr agent get a failed; \
                 the Stop hook records it at the member's first stop"
                    .into(),
                0
            )
        );
        assert_eq!(herdr.tabs().len(), 1);
        assert_eq!(ledger.0["a"].session_id, None);
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn a_start_the_mailbox_cannot_take_is_a_warning_not_a_failure() {
        let herdr = herdr();
        let mut ledger = crate::ledger::NoMailbox::default();
        let spawned = spawn(&herdr, &mut ledger, &request("a"), Some("w1")).unwrap();
        assert_eq!(
            spawned.report("a"),
            (
                "tab w1:t1\npane w1:p1\nsession session-a\n\
                 warning: no start entry in its mailbox: disk full"
                    .into(),
                0
            )
        );
        assert!(ledger.0.0["a"].prompt_sent);
    }

    #[test]
    fn records_herdrs_socket_and_marks_the_start_in_the_mailbox() {
        for startup in [Startup::Ready, Startup::BlockedForever] {
            let mut herdr = herdr();
            herdr.startup = startup;
            let mut ledger = MemLedger::default();
            run(&herdr, &mut ledger, &request("a")).unwrap();
            assert_eq!(
                ledger.0["a"].socket.as_deref(),
                Some(Path::new(crate::herdr::fake::FAKE_SOCKET))
            );
            assert_eq!(ledger.1, ["a"]);
        }
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
            assert!(ledger.1.is_empty());
        }
    }

    #[test]
    fn waits_for_a_new_tabs_shell_prompt_before_starting_claude() {
        let mut herdr = herdr();
        herdr.new_tab_prompt_after = Some(3);
        let mut ledger = MemLedger::default();
        run(&herdr, &mut ledger, &request("a")).unwrap();
        assert_eq!(herdr.started().len(), 1);
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn a_short_ready_timeout_leaves_the_shell_its_own_time_to_reach_its_prompt() {
        let mut herdr = herdr();
        herdr.new_tab_prompt_after = Some(3);
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            ready_timeout: Duration::from_millis(20),
            ..request("a")
        };
        run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(herdr.started().len(), 1);
    }

    #[test]
    fn a_shell_that_never_reaches_its_prompt_is_a_start_failure_that_closes_the_tab() {
        let mut herdr = herdr();
        herdr.new_tab_prompt_after = None;
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            shell_timeout: Duration::from_millis(20),
            ..request("a")
        };
        let err = run(&herdr, &mut ledger, &req).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "start: pane w1:p1 shell not at its prompt after 20ms"
        );
        assert!(herdr.started().is_empty());
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());
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

    const BUILT_INS: &str = "{name}|{branch}|{worktree}|{pane}|{{x}}";

    #[test]
    fn the_prompt_gets_name_branch_worktree_and_pane() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo,
            worktree: Some("feat/new".into()),
            prompt: Some(BUILT_INS.into()),
            ..request("a")
        };
        run(&herdr, &mut ledger, &req).unwrap();
        let wt = root.path().join("proj/worktrees/feat-new");
        let rendered = format!("a|feat/new|{}|w1:p1|{{x}}", wt.display());
        assert_eq!(herdr.prompts(), vec![("a".into(), rendered.clone())]);
        assert_eq!(ledger.0["a"].prompt, Some(rendered));
    }

    #[test]
    fn without_a_worktree_branch_is_empty_and_worktree_is_the_repo() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            prompt: Some(BUILT_INS.into()),
            ..request("a")
        };
        run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(herdr.prompts()[0].1, "a||/src/repo|w1:p1|{x}");
    }

    #[test]
    fn a_blocked_agent_is_recorded_with_the_rendered_prompt() {
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            prompt: Some("I am {name} in {pane}".into()),
            ..request("a")
        };
        run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(ledger.0["a"].prompt.as_deref(), Some("I am a in w1:p1"));
    }

    #[test]
    fn spawn_records_no_herd_or_template() {
        let mut ledger = MemLedger::default();
        run(&herdr(), &mut ledger, &request("a")).unwrap();
        assert_eq!(ledger.0["a"].herd, None);
        assert_eq!(ledger.0["a"].template, None);
    }

    fn member_of() -> MemberOf {
        MemberOf {
            herd: "prs".into(),
            template: Some("pr".into()),
            params: vec![("number".into(), "1050".into())],
            values: vec![],
            role: None,
        }
    }

    #[test]
    fn a_herd_member_gets_its_params_and_herd_in_the_prompt_and_ledger() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            prompt: Some("{name} {herd} {number}".into()),
            member_of: Some(member_of()),
            ..request("a")
        };
        run(&herdr, &mut ledger, &req).unwrap();
        assert_eq!(herdr.prompts()[0].1, "a prs 1050");
        assert_eq!(ledger.0["a"].herd.as_deref(), Some("prs"));
        assert_eq!(ledger.0["a"].template.as_deref(), Some("pr"));
    }

    #[test]
    fn params_are_not_placeholders_outside_a_herd() {
        let req = SpawnRequest {
            prompt: Some("{herd} {number}".into()),
            ..request("a")
        };
        let err = run(&herdr(), &mut MemLedger::default(), &req).unwrap_err();
        assert_eq!(format!("{err:#}"), "prompt: unknown placeholder {herd}");
    }

    #[test]
    fn an_invalid_prompt_template_fails_before_anything_is_created() {
        for (prompt, message) in [
            ("hi {nmae}", "prompt: unknown placeholder {nmae}"),
            ("hi {name", "prompt: unclosed placeholder {name"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let repo = repo_with_origin(root.path());
            let herdr = herdr();
            let mut ledger = MemLedger::default();
            let req = SpawnRequest {
                repo,
                worktree: Some("feat/new".into()),
                prompt: Some(prompt.into()),
                ..request("a")
            };
            let err = run(&herdr, &mut ledger, &req).unwrap_err();
            assert_eq!(format!("{err:#}"), message);
            assert!(herdr.tabs().is_empty());
            assert!(!root.path().join("proj/worktrees").exists());
            assert!(ledger.0.is_empty());
        }
    }

    #[test]
    fn an_empty_prompt_fails_before_anything_is_created() {
        for (prompt, message) in [
            (Prompt::from(""), "prompt: --prompt is empty"),
            (
                Prompt {
                    source: "--prompt-file p.md".into(),
                    text: " \n\t".into(),
                },
                "prompt: --prompt-file p.md is empty",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let repo = repo_with_origin(root.path());
            // A clash on the name would surface if herdr were asked first.
            let herdr = herdr().with_agent("a");
            let mut ledger = MemLedger::default();
            let req = SpawnRequest {
                repo,
                worktree: Some("feat/new".into()),
                prompt: Some(prompt),
                ..request("a")
            };
            let err = run(&herdr, &mut ledger, &req).unwrap_err();
            assert_eq!(format!("{err:#}"), message);
            assert!(herdr.tabs().is_empty());
            assert!(!root.path().join("proj/worktrees").exists());
            assert!(ledger.0.is_empty());
        }
    }

    #[test]
    fn a_prompt_that_renders_empty_closes_the_tab_and_records_nothing() {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            prompt: Some(" {branch} ".into()),
            ..request("a")
        };
        let err = run(&herdr, &mut ledger, &req).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "prompt: --prompt is empty once rendered"
        );
        assert!(herdr.tabs().is_empty());
        assert!(herdr.prompts().is_empty());
        assert!(ledger.0.is_empty());
    }
}
