//! herdr, reached only through the [`Herdr`] trait.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

#[cfg(test)]
pub mod fake;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: Option<String>,
    pub pane_id: String,
    pub tab_id: String,
    pub status: AgentStatus,
    pub session_id: Option<String>,
    /// herdr is still launching or resuming it.
    pub launch_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    pub tab_id: String,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTab {
    pub tab_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Ready(Agent),
    /// herdr saw an approval or question dialog during startup.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Reached,
    TimedOut,
}

pub trait Herdr {
    fn workspaces(&self) -> Result<Vec<Workspace>>;
    fn agents(&self) -> Result<Vec<Agent>>;
    fn agent(&self, name: &str) -> Result<Agent>;
    fn tabs_in(&self, workspace_id: &str) -> Result<Vec<Tab>>;
    fn panes_in(&self, workspace_id: &str) -> Result<Vec<Pane>>;
    /// The shell is alone in the foreground and has drawn its prompt: no
    /// command, editor, agent, or rc file running.
    fn at_shell_prompt(&self, pane_id: &str) -> Result<bool>;
    /// Creates a tab without focusing it.
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab>;
    fn close_tab(&self, tab_id: &str) -> Result<()>;
    /// Starts a Claude agent named `name` in `pane_id`.
    fn start_claude(
        &self,
        name: &str,
        pane_id: &str,
        timeout: Duration,
        args: &[String],
    ) -> Result<StartOutcome>;
    fn wait(&self, name: &str, until: &[AgentStatus], timeout: Duration) -> Result<WaitOutcome>;
    /// Submits `text` without waiting for the agent's turn.
    fn prompt(&self, name: &str, text: &str) -> Result<()>;
}

/// Polls until `pane_id`'s shell is at its prompt. A new tab's shell takes a
/// moment to get there, and `agent start` refuses a pane that isn't.
pub fn wait_for_shell_prompt(
    herdr: &dyn Herdr,
    pane_id: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !herdr.at_shell_prompt(pane_id)? {
        if Instant::now() >= deadline {
            bail!(
                "pane {pane_id} shell not at its prompt after {}",
                humantime::format_duration(timeout)
            );
        }
        std::thread::sleep(poll);
    }
    Ok(())
}

/// herdr's longest `agent start --timeout`.
const MAX_START_TIMEOUT: Duration = Duration::from_millis(300_000);

/// The `herdr` CLI. It talks to whichever session its environment points at.
pub struct CliHerdr;

impl CliHerdr {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("herdr");
        cmd.args(args);
        cmd
    }

    fn call(&self, args: &[&str]) -> Result<std::result::Result<Value, ApiError>> {
        let out = self
            .command(args)
            .output()
            .with_context(|| format!("run herdr {}", args.join(" ")))?;
        if out.status.success() {
            let json: Value = serde_json::from_slice(&out.stdout)
                .with_context(|| format!("parse herdr {} output", args.join(" ")))?;
            return Ok(Ok(json["result"].clone()));
        }
        let parsed = [&out.stderr, &out.stdout]
            .into_iter()
            .find_map(|bytes| serde_json::from_slice::<ErrorEnvelope>(bytes).ok());
        match parsed {
            Some(env) => Ok(Err(env.error)),
            None => bail!(
                "herdr {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// `pane read` prints plain text, not a JSON envelope.
    fn visible_text(&self, pane_id: &str) -> Result<String> {
        let args = [
            "pane", "read", pane_id, "--source", "visible", "--format", "text",
        ];
        let out = self
            .command(&args)
            .output()
            .with_context(|| format!("run herdr {}", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "herdr pane read failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn call_ok(&self, args: &[&str]) -> Result<Value> {
        self.call(args)?
            .map_err(|e| anyhow!("herdr {}: {} ({})", args[..2].join(" "), e.message, e.code))
    }
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct AgentJson {
    name: Option<String>,
    pane_id: String,
    tab_id: String,
    agent_status: AgentStatus,
    agent_session: Option<SessionJson>,
    #[serde(default)]
    launch_pending: Option<bool>,
}

#[derive(Deserialize)]
struct SessionJson {
    value: String,
}

impl From<AgentJson> for Agent {
    fn from(a: AgentJson) -> Self {
        Agent {
            name: a.name,
            pane_id: a.pane_id,
            tab_id: a.tab_id,
            status: a.agent_status,
            session_id: a.agent_session.map(|s| s.value),
            launch_pending: a.launch_pending.unwrap_or(false),
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T> {
    T::deserialize(value).with_context(|| format!("parse herdr {what}"))
}

fn millis(d: Duration) -> String {
    d.as_millis().max(1).to_string()
}

impl Herdr for CliHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>> {
        #[derive(Deserialize)]
        struct Ws {
            workspace_id: String,
            label: String,
        }
        let result = self.call_ok(&["workspace", "list"])?;
        let list: Vec<Ws> = parse(&result["workspaces"], "workspace list")?;
        Ok(list
            .into_iter()
            .map(|w| Workspace {
                id: w.workspace_id,
                label: w.label,
            })
            .collect())
    }

    fn agents(&self) -> Result<Vec<Agent>> {
        let result = self.call_ok(&["agent", "list"])?;
        let list: Vec<AgentJson> = parse(&result["agents"], "agent list")?;
        Ok(list.into_iter().map(Agent::from).collect())
    }

    fn agent(&self, name: &str) -> Result<Agent> {
        let result = self.call_ok(&["agent", "get", name])?;
        Ok(parse::<AgentJson>(&result["agent"], "agent get")?.into())
    }

    fn tabs_in(&self, workspace_id: &str) -> Result<Vec<Tab>> {
        #[derive(Deserialize)]
        struct TabJson {
            tab_id: String,
            label: String,
        }
        let result = self.call_ok(&["tab", "list", "--workspace", workspace_id])?;
        let list: Vec<TabJson> = parse(&result["tabs"], "tab list")?;
        Ok(list
            .into_iter()
            .map(|t| Tab {
                id: t.tab_id,
                label: t.label,
            })
            .collect())
    }

    fn panes_in(&self, workspace_id: &str) -> Result<Vec<Pane>> {
        #[derive(Deserialize)]
        struct PaneJson {
            pane_id: String,
            tab_id: String,
            cwd: Option<PathBuf>,
        }
        let result = self.call_ok(&["pane", "list", "--workspace", workspace_id])?;
        let list: Vec<PaneJson> = parse(&result["panes"], "pane list")?;
        Ok(list
            .into_iter()
            .map(|p| Pane {
                id: p.pane_id,
                tab_id: p.tab_id,
                cwd: p.cwd,
            })
            .collect())
    }

    fn at_shell_prompt(&self, pane_id: &str) -> Result<bool> {
        #[derive(Deserialize)]
        struct ProcessInfo {
            shell_pid: Option<u32>,
            foreground_process_group_id: Option<u32>,
            #[serde(default)]
            foreground_processes: Vec<Process>,
        }
        #[derive(Deserialize)]
        struct Process {
            pid: u32,
        }
        let result = self.call_ok(&["pane", "process-info", "--pane", pane_id])?;
        let info: ProcessInfo = parse(&result["process_info"], "pane process-info")?;
        let shell_alone = info.shell_pid.is_some()
            && info.shell_pid == info.foreground_process_group_id
            && info
                .foreground_processes
                .iter()
                .all(|p| Some(p.pid) == info.shell_pid);
        // A shell still running its rc files is in the foreground too; only the
        // drawn prompt says it's done, and `agent start` refuses it until then.
        Ok(shell_alone && !self.visible_text(pane_id)?.trim().is_empty())
    }

    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab> {
        let cwd = cwd.to_str().context("cwd is not UTF-8")?;
        let result = self.call_ok(&[
            "tab",
            "create",
            "--workspace",
            workspace_id,
            "--cwd",
            cwd,
            "--label",
            label,
            "--no-focus",
        ])?;
        Ok(NewTab {
            tab_id: parse(&result["tab"]["tab_id"], "tab create")?,
            pane_id: parse(&result["root_pane"]["pane_id"], "tab create")?,
        })
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.call_ok(&["tab", "close", tab_id]).map(drop)
    }

    fn start_claude(
        &self,
        name: &str,
        pane_id: &str,
        timeout: Duration,
        args: &[String],
    ) -> Result<StartOutcome> {
        let timeout = millis(timeout.min(MAX_START_TIMEOUT));
        let mut argv = vec![
            "agent",
            "start",
            name,
            "--kind",
            "claude",
            "--pane",
            pane_id,
            "--timeout",
            &timeout,
        ];
        if !args.is_empty() {
            argv.push("--");
            argv.extend(args.iter().map(String::as_str));
        }
        match self.call(&argv)? {
            Ok(result) => Ok(StartOutcome::Ready(
                parse::<AgentJson>(&result["agent"], "agent start")?.into(),
            )),
            Err(e) if e.code == "agent_not_ready" => Ok(StartOutcome::Blocked),
            Err(e) => bail!("herdr agent start: {} ({})", e.message, e.code),
        }
    }

    fn wait(&self, name: &str, until: &[AgentStatus], timeout: Duration) -> Result<WaitOutcome> {
        let timeout = millis(timeout);
        let mut argv = vec!["agent", "wait", name, "--timeout", &timeout];
        for status in until {
            argv.extend(["--until", status.as_str()]);
        }
        match self.call(&argv)? {
            Ok(_) => Ok(WaitOutcome::Reached),
            Err(e) if e.code == "timeout" => Ok(WaitOutcome::TimedOut),
            Err(e) => bail!("herdr agent wait: {} ({})", e.message, e.code),
        }
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        self.call_ok(&["agent", "prompt", name, text]).map(drop)
    }
}
