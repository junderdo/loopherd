//! In-memory herdr for tests.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::{Agent, AgentStatus, Herdr, NewTab, Pane, StartOutcome, Tab, WaitOutcome, Workspace};

/// How a Claude agent started by the fake behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    Ready,
    /// Blocked at startup, then idle once waited on.
    BlockedThenIdle,
    BlockedForever,
    /// Never becomes ready and isn't blocked.
    Hangs,
    Fails,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeTab {
    pub id: String,
    pub workspace_id: String,
    pub cwd: PathBuf,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakePane {
    id: String,
    tab_id: String,
    workspace_id: String,
    cwd: PathBuf,
    at_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub name: String,
    pub pane_id: String,
    pub args: Vec<String>,
}

#[derive(Default)]
struct State {
    next_id: u32,
    tabs: Vec<FakeTab>,
    panes: Vec<FakePane>,
    agents: Vec<Agent>,
    started: Vec<Started>,
    prompts: Vec<(String, String)>,
    agent_polls: u32,
}

pub struct FakeHerdr {
    pub workspaces: Vec<Workspace>,
    pub startup: Startup,
    pub fail_create_tab: bool,
    /// Agents added with [`FakeHerdr::with_resuming_agent`] stay launch-pending,
    /// unnamed and sessionless for this many `agents()` calls; `None` is forever.
    pub resume_settles_after: Option<u32>,
    state: RefCell<State>,
}

impl FakeHerdr {
    pub fn new(workspaces: &[(&str, &str)]) -> Self {
        FakeHerdr {
            workspaces: workspaces
                .iter()
                .map(|(id, label)| Workspace {
                    id: id.to_string(),
                    label: label.to_string(),
                })
                .collect(),
            startup: Startup::Ready,
            fail_create_tab: false,
            resume_settles_after: Some(0),
            state: RefCell::default(),
        }
    }

    /// Adds a live agent that already exists in the session.
    pub fn with_agent(self, name: &str) -> Self {
        self.with_live_agent(Some(name), None, "w9:p9")
    }

    pub fn with_live_agent(self, name: Option<&str>, session: Option<&str>, pane_id: &str) -> Self {
        self.push_agent(name, session, pane_id, false);
        self
    }

    /// An agent herdr is still resuming after a restart.
    pub fn with_resuming_agent(self, name: &str, session: &str, pane_id: &str) -> Self {
        self.push_agent(Some(name), Some(session), pane_id, true);
        self
    }

    /// Adds a tab with one pane, as a restart restores it. Returns the pane ID.
    pub fn add_tab(&self, workspace_id: &str, label: &str, cwd: &Path, at_prompt: bool) -> String {
        let tab = self.new_tab(workspace_id, cwd, label);
        self.state
            .borrow_mut()
            .panes
            .iter_mut()
            .find(|p| p.id == tab.pane_id)
            .unwrap()
            .at_prompt = at_prompt;
        tab.pane_id
    }

    pub fn tabs(&self) -> Vec<FakeTab> {
        self.state.borrow().tabs.clone()
    }

    pub fn started(&self) -> Vec<Started> {
        self.state.borrow().started.clone()
    }

    pub fn prompts(&self) -> Vec<(String, String)> {
        self.state.borrow().prompts.clone()
    }

    fn push_agent(&self, name: Option<&str>, session: Option<&str>, pane_id: &str, pending: bool) {
        let (ws, n) = pane_id.split_once(":p").expect("fake pane id");
        self.state.borrow_mut().agents.push(Agent {
            name: name.map(str::to_string),
            pane_id: pane_id.into(),
            tab_id: format!("{ws}:t{n}"),
            status: AgentStatus::Idle,
            session_id: session.map(str::to_string),
            launch_pending: pending,
        });
    }

    fn new_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> NewTab {
        let mut state = self.state.borrow_mut();
        state.next_id += 1;
        let n = state.next_id;
        let tab = NewTab {
            tab_id: format!("{workspace_id}:t{n}"),
            pane_id: format!("{workspace_id}:p{n}"),
        };
        state.tabs.push(FakeTab {
            id: tab.tab_id.clone(),
            workspace_id: workspace_id.to_string(),
            cwd: cwd.to_path_buf(),
            label: label.to_string(),
        });
        state.panes.push(FakePane {
            id: tab.pane_id.clone(),
            tab_id: tab.tab_id.clone(),
            workspace_id: workspace_id.to_string(),
            cwd: cwd.to_path_buf(),
            at_prompt: true,
        });
        tab
    }

    fn set_status(&self, name: &str, status: AgentStatus) {
        let mut state = self.state.borrow_mut();
        if let Some(a) = state
            .agents
            .iter_mut()
            .find(|a| a.name.as_deref() == Some(name))
        {
            a.status = status;
            if status == AgentStatus::Idle && a.session_id.is_none() {
                a.session_id = Some(format!("session-{name}"));
            }
        }
    }
}

impl Herdr for FakeHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>> {
        Ok(self.workspaces.clone())
    }

    fn agents(&self) -> Result<Vec<Agent>> {
        let mut state = self.state.borrow_mut();
        state.agent_polls += 1;
        let settled = self
            .resume_settles_after
            .is_some_and(|n| state.agent_polls > n);
        if settled {
            state
                .agents
                .iter_mut()
                .for_each(|a| a.launch_pending = false);
        }
        Ok(state
            .agents
            .iter()
            .map(|a| match a.launch_pending {
                true => Agent {
                    name: None,
                    session_id: None,
                    ..a.clone()
                },
                false => a.clone(),
            })
            .collect())
    }

    fn agent(&self, name: &str) -> Result<Agent> {
        self.state
            .borrow()
            .agents
            .iter()
            .find(|a| a.name.as_deref() == Some(name))
            .cloned()
            .ok_or_else(|| anyhow!("agent {name} not found"))
    }

    fn tabs_in(&self, workspace_id: &str) -> Result<Vec<Tab>> {
        Ok(self
            .state
            .borrow()
            .tabs
            .iter()
            .filter(|t| t.workspace_id == workspace_id)
            .map(|t| Tab {
                id: t.id.clone(),
                label: t.label.clone(),
            })
            .collect())
    }

    fn panes_in(&self, workspace_id: &str) -> Result<Vec<Pane>> {
        Ok(self
            .state
            .borrow()
            .panes
            .iter()
            .filter(|p| p.workspace_id == workspace_id)
            .map(|p| Pane {
                id: p.id.clone(),
                tab_id: p.tab_id.clone(),
                cwd: Some(p.cwd.clone()),
            })
            .collect())
    }

    fn at_shell_prompt(&self, pane_id: &str) -> Result<bool> {
        let state = self.state.borrow();
        let pane = state
            .panes
            .iter()
            .find(|p| p.id == pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        Ok(pane.at_prompt)
    }

    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab> {
        if self.fail_create_tab {
            bail!("tab create refused");
        }
        if !self.workspaces.iter().any(|w| w.id == workspace_id) {
            bail!("workspace {workspace_id} not found");
        }
        Ok(self.new_tab(workspace_id, cwd, label))
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.tabs.retain(|t| t.id != tab_id);
        state.panes.retain(|p| p.tab_id != tab_id);
        state.agents.retain(|a| a.tab_id != tab_id);
        Ok(())
    }

    fn start_claude(
        &self,
        name: &str,
        pane_id: &str,
        _timeout: Duration,
        args: &[String],
    ) -> Result<StartOutcome> {
        if self.startup == Startup::Fails {
            bail!("claude exited during startup");
        }
        {
            let mut state = self.state.borrow_mut();
            let pane = state
                .panes
                .iter_mut()
                .find(|p| p.id == pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            if !pane.at_prompt {
                bail!("pane {pane_id} is not at a shell prompt");
            }
            pane.at_prompt = false;
            let tab_id = pane.tab_id.clone();
            let resumed = args
                .iter()
                .position(|a| a == "--resume")
                .and_then(|i| args.get(i + 1))
                .cloned();
            state.started.push(Started {
                name: name.to_string(),
                pane_id: pane_id.to_string(),
                args: args.to_vec(),
            });
            state.agents.push(Agent {
                name: Some(name.to_string()),
                pane_id: pane_id.to_string(),
                tab_id,
                status: AgentStatus::Unknown,
                session_id: resumed,
                launch_pending: false,
            });
        }
        match self.startup {
            Startup::Ready => {
                self.set_status(name, AgentStatus::Idle);
                Ok(StartOutcome::Ready(self.agent(name)?))
            }
            Startup::BlockedThenIdle | Startup::BlockedForever => {
                self.set_status(name, AgentStatus::Blocked);
                Ok(StartOutcome::Blocked)
            }
            Startup::Hangs => bail!("timed out waiting for agent readiness"),
            Startup::Fails => unreachable!(),
        }
    }

    fn wait(&self, name: &str, until: &[AgentStatus], _timeout: Duration) -> Result<WaitOutcome> {
        if self.startup == Startup::BlockedThenIdle {
            self.set_status(name, AgentStatus::Idle);
        }
        let status = self.agent(name)?.status;
        Ok(if until.contains(&status) {
            WaitOutcome::Reached
        } else {
            WaitOutcome::TimedOut
        })
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        if self.agent(name)?.status == AgentStatus::Blocked {
            bail!("agent {name} is blocked");
        }
        self.state
            .borrow_mut()
            .prompts
            .push((name.to_string(), text.to_string()));
        Ok(())
    }
}
