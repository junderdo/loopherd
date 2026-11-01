//! In-memory herdr for tests.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::{Agent, AgentStatus, Herdr, NewTab, StartOutcome, WaitOutcome, Workspace};

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
pub struct Tab {
    pub id: String,
    pub workspace_id: String,
    pub cwd: PathBuf,
    pub label: String,
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
    tabs: Vec<Tab>,
    agents: Vec<Agent>,
    started: Vec<Started>,
    prompts: Vec<(String, String)>,
}

pub struct FakeHerdr {
    pub workspaces: Vec<Workspace>,
    pub startup: Startup,
    pub fail_create_tab: bool,
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
            state: RefCell::default(),
        }
    }

    /// Adds a live agent that already exists in the session.
    pub fn with_agent(self, name: &str) -> Self {
        self.with_agent_in(name, AgentStatus::Idle)
    }

    pub fn with_agent_in(self, name: &str, status: AgentStatus) -> Self {
        self.state.borrow_mut().agents.push(Agent {
            name: Some(name.to_string()),
            pane_id: "w9:p9".into(),
            tab_id: "w9:t9".into(),
            status,
            session_id: None,
        });
        self
    }

    pub fn tabs(&self) -> Vec<Tab> {
        self.state.borrow().tabs.clone()
    }

    pub fn started(&self) -> Vec<Started> {
        self.state.borrow().started.clone()
    }

    pub fn prompts(&self) -> Vec<(String, String)> {
        self.state.borrow().prompts.clone()
    }

    fn set_status(&self, name: &str, status: AgentStatus) {
        let mut state = self.state.borrow_mut();
        if let Some(a) = state
            .agents
            .iter_mut()
            .find(|a| a.name.as_deref() == Some(name))
        {
            a.status = status;
            if status == AgentStatus::Idle {
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
        Ok(self.state.borrow().agents.clone())
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

    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab> {
        if self.fail_create_tab {
            bail!("tab create refused");
        }
        if !self.workspaces.iter().any(|w| w.id == workspace_id) {
            bail!("workspace {workspace_id} not found");
        }
        let mut state = self.state.borrow_mut();
        state.next_id += 1;
        let n = state.next_id;
        let tab = NewTab {
            tab_id: format!("{workspace_id}:t{n}"),
            pane_id: format!("{workspace_id}:p{n}"),
        };
        state.tabs.push(Tab {
            id: tab.tab_id.clone(),
            workspace_id: workspace_id.to_string(),
            cwd: cwd.to_path_buf(),
            label: label.to_string(),
        });
        Ok(tab)
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.tabs.retain(|t| t.id != tab_id);
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
            let (ws, n) = pane_id.split_once(":p").expect("fake pane id");
            let tab_id = format!("{ws}:t{n}");
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
                session_id: None,
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
