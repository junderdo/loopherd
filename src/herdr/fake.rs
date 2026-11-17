//! In-memory herdr for tests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::events::{Connection, OnPush, PaneNotFound, Pushed, Subscriber, Subscription};
use super::{
    Agent, AgentStatus, Herdr, MalformedReply, NewTab, Pane, StartOutcome, Tab, WaitOutcome,
    Workspace,
};

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
    /// `at_shell_prompt` polls left before the shell reaches its prompt; `None` is never.
    polls_until_prompt: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub name: String,
    pub pane_id: String,
    pub args: Vec<String>,
}

#[derive(Default)]
struct State {
    /// Set by [`FakeHerdr::set_unreachable`], for a herdr going down mid-test.
    down: bool,
    /// Set by [`FakeHerdr::set_malformed`].
    malformed: bool,
    next_id: u32,
    tabs: Vec<FakeTab>,
    panes: Vec<FakePane>,
    agents: Vec<Agent>,
    started: Vec<Started>,
    prompts: Vec<(String, String)>,
    focused: Vec<String>,
    /// Visible text by pane; a pane without one reads as empty.
    screens: Vec<(String, String)>,
    /// Replaces a pane's screen when its agent is next prompted.
    screen_on_prompt: Option<(String, String)>,
    /// A prompted agent goes `working`, as Claude does on a new turn.
    working_on_prompt: bool,
    agent_polls: u32,
    /// `agent` calls left that answer without the session.
    sessionless_gets: u32,
    /// `agent` fails, as when herdr stops answering after a start.
    agent_unreadable: bool,
    mutations: u32,
}

pub const FAKE_SOCKET: &str = "/run/fake/herdr.sock";

pub struct FakeHerdr {
    pub workspaces: Vec<Workspace>,
    pub startup: Startup,
    pub fail_create_tab: bool,
    pub fail_close_tab: bool,
    pub fail_prompt: bool,
    /// After `start_claude`, this many `agent` calls answer without the
    /// session, as herdr does before it has read it; a ready start's reply too.
    pub session_after_gets: u32,
    /// After `start_claude`, every `agent` call fails.
    pub agent_get_fails_after_start: bool,
    /// A tab from `create_tab` reaches its shell prompt after this many
    /// `at_shell_prompt` polls; `None` is never.
    pub new_tab_prompt_after: Option<u32>,
    /// Agents added with [`FakeHerdr::with_resuming_agent`] stay launch-pending,
    /// unnamed and sessionless for this many `agents()` calls; `None` is forever.
    pub resume_settles_after: Option<u32>,
    /// Every call fails, as when no herdr server is running.
    pub unreachable: bool,
    /// `agents()` fails as herdr answering in a shape loopherd can't parse.
    pub malformed_agents: bool,
    /// `agents()` blocks until a value arrives or the sender drops, like a hung herdr.
    pub agents_gate: Option<Receiver<()>>,
    /// Runs inside every `agents()` call, before it answers.
    pub on_agents: Option<Box<dyn Fn() + Send>>,
    pub socket: Option<PathBuf>,
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
            fail_close_tab: false,
            fail_prompt: false,
            session_after_gets: 0,
            agent_get_fails_after_start: false,
            new_tab_prompt_after: Some(0),
            resume_settles_after: Some(0),
            unreachable: false,
            malformed_agents: false,
            agents_gate: None,
            on_agents: None,
            socket: Some(FAKE_SOCKET.into()),
            state: RefCell::default(),
        }
    }

    pub fn set_working_on_prompt(&self) {
        self.state.borrow_mut().working_on_prompt = true;
    }

    /// Adds a live agent that already exists in the session.
    pub fn with_agent(self, name: &str) -> Self {
        self.with_live_agent(Some(name), None, "w9:p9")
    }

    pub fn with_agent_in(self, name: &str, status: AgentStatus) -> Self {
        self.push_agent(Some(name), None, "w9:p9", false);
        self.state.borrow_mut().agents.last_mut().unwrap().status = status;
        self
    }

    pub fn with_live_agent(self, name: Option<&str>, session: Option<&str>, pane_id: &str) -> Self {
        self.push_agent(name, session, pane_id, false);
        self
    }

    pub fn with_raw_agent(self, agent: Agent) -> Self {
        self.state.borrow_mut().agents.push(agent);
        self
    }

    /// Calls that create, close, start or prompt anything.
    pub fn mutations(&self) -> u32 {
        self.state.borrow().mutations
    }

    fn mutating(&self) {
        self.state.borrow_mut().mutations += 1;
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

    /// Splits a new shell pane into `tab_id`. Returns the pane ID.
    pub fn split(&self, tab_id: &str, cwd: &Path) -> String {
        let mut state = self.state.borrow_mut();
        state.next_id += 1;
        let workspace_id = tab_id.split_once(':').expect("fake tab id").0.to_string();
        let id = format!("{workspace_id}:p{}", state.next_id);
        state.panes.push(FakePane {
            id: id.clone(),
            tab_id: tab_id.to_string(),
            workspace_id,
            cwd: cwd.to_path_buf(),
            at_prompt: true,
            polls_until_prompt: Some(0),
        });
        id
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

    /// Panes passed to `focus`, in order. Focusing isn't a mutation.
    pub fn focused(&self) -> Vec<String> {
        self.state.borrow().focused.clone()
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
            cwd: None,
            title: None,
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
            polls_until_prompt: Some(0),
        });
        tab
    }

    pub fn set_screen(&self, pane_id: &str, text: &str) {
        let mut state = self.state.borrow_mut();
        state.screens.retain(|(p, _)| p != pane_id);
        state.screens.push((pane_id.to_string(), text.to_string()));
    }

    /// The pane shows `text` once its agent is next prompted, as a dialog
    /// that a prompt opens does.
    pub fn set_screen_on_prompt(&self, pane_id: &str, text: &str) {
        self.state.borrow_mut().screen_on_prompt = Some((pane_id.to_string(), text.to_string()));
    }

    /// The agent's pane closes outside loopherd.
    pub fn remove_agent(&self, name: &str) {
        self.state
            .borrow_mut()
            .agents
            .retain(|a| a.name.as_deref() != Some(name));
    }

    pub fn set_unreachable(&self, down: bool) {
        self.state.borrow_mut().down = down;
    }

    pub fn set_malformed(&self, malformed: bool) {
        self.state.borrow_mut().malformed = malformed;
    }

    /// Puts `name` back in `pane_id` as a restart's resume does, pending
    /// until [`FakeHerdr::settle`] or `resume_settles_after`.
    pub fn resume_agent(&self, name: &str, session: &str, pane_id: &str) {
        self.push_agent(Some(name), Some(session), pane_id, true);
    }

    pub fn settle(&self) {
        self.state
            .borrow_mut()
            .agents
            .iter_mut()
            .for_each(|a| a.launch_pending = false);
    }

    fn is_down(&self) -> bool {
        self.unreachable || self.state.borrow().down
    }

    fn find(&self, name: &str) -> Result<Agent> {
        self.state
            .borrow()
            .agents
            .iter()
            .find(|a| a.name.as_deref() == Some(name))
            .cloned()
            .ok_or_else(|| anyhow!("agent {name} not found"))
    }

    pub fn set_status(&self, name: &str, status: AgentStatus) {
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
        if self.is_down() {
            bail!("herdr server not running");
        }
        Ok(self.workspaces.clone())
    }

    fn agents(&self) -> Result<Vec<Agent>> {
        if let Some(gate) = &self.agents_gate {
            let _ = gate.recv();
        }
        if let Some(on_agents) = &self.on_agents {
            on_agents();
        }
        if self.is_down() {
            bail!("herdr server not running");
        }
        if self.malformed_agents || self.state.borrow().malformed {
            return Err(anyhow!("expected an array").context(MalformedReply("agent list".into())));
        }
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
        if self.state.borrow().agent_unreadable {
            bail!("herdr agent get {name} failed");
        }
        let mut agent = self.find(name)?;
        let mut state = self.state.borrow_mut();
        if state.sessionless_gets > 0 {
            state.sessionless_gets -= 1;
            agent.session_id = None;
        }
        Ok(agent)
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
        let mut state = self.state.borrow_mut();
        let pane = state
            .panes
            .iter_mut()
            .find(|p| p.id == pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        match &mut pane.polls_until_prompt {
            Some(0) => {}
            Some(n) => *n -= 1,
            None => return Ok(false),
        }
        Ok(pane.at_prompt && pane.polls_until_prompt == Some(0))
    }

    fn visible_text(&self, pane_id: &str) -> Result<String> {
        if self.is_down() {
            bail!("herdr server not running");
        }
        Ok(self
            .state
            .borrow()
            .screens
            .iter()
            .find(|(p, _)| p == pane_id)
            .map(|(_, text)| text.clone())
            .unwrap_or_default())
    }

    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab> {
        self.mutating();
        if self.fail_create_tab {
            bail!("tab create refused");
        }
        if !self.workspaces.iter().any(|w| w.id == workspace_id) {
            bail!("workspace {workspace_id} not found");
        }
        let tab = self.new_tab(workspace_id, cwd, label);
        let mut state = self.state.borrow_mut();
        let pane = state.panes.last_mut().unwrap();
        pane.polls_until_prompt = self.new_tab_prompt_after;
        Ok(tab)
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.mutating();
        if self.fail_close_tab {
            bail!("tab close refused");
        }
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
        self.mutating();
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
            if !pane.at_prompt || pane.polls_until_prompt != Some(0) {
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
                cwd: None,
                title: None,
            });
        }
        {
            let mut state = self.state.borrow_mut();
            state.sessionless_gets = self.session_after_gets;
            state.agent_unreadable = self.agent_get_fails_after_start;
        }
        match self.startup {
            Startup::Ready => {
                self.set_status(name, AgentStatus::Idle);
                let mut agent = self.find(name)?;
                if self.session_after_gets > 0 {
                    agent.session_id = None;
                }
                Ok(StartOutcome::Ready(agent))
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
        let status = self.find(name)?.status;
        Ok(if until.contains(&status) {
            WaitOutcome::Reached
        } else {
            WaitOutcome::TimedOut
        })
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        self.mutating();
        if self.find(name)?.status == AgentStatus::Blocked {
            bail!("agent {name} is blocked");
        }
        if self.fail_prompt {
            bail!("prompt refused");
        }
        self.state
            .borrow_mut()
            .prompts
            .push((name.to_string(), text.to_string()));
        let next = self.state.borrow_mut().screen_on_prompt.take();
        if let Some((pane, screen)) = next {
            self.set_screen(&pane, &screen);
        }
        if self.state.borrow().working_on_prompt {
            self.set_status(name, AgentStatus::Working);
        }
        Ok(())
    }

    fn focus(&self, pane_id: &str) -> Result<()> {
        if self.is_down() {
            bail!("herdr server not running");
        }
        if !self
            .state
            .borrow()
            .agents
            .iter()
            .any(|a| a.pane_id == pane_id)
        {
            bail!("no agent in pane {pane_id}");
        }
        self.state.borrow_mut().focused.push(pane_id.to_string());
        Ok(())
    }

    fn socket(&self) -> Option<PathBuf> {
        self.socket.clone()
    }
}

/// How [`FakeSubscriber`] refuses its next subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    PaneNotFound,
    Failed,
}

/// Which of the herd view's connections a [`FakeSubscriber`] call means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Lifecycle,
    Status,
}

struct FakeConnection {
    subscriptions: Vec<Subscription>,
    on_push: Arc<OnPush>,
    closed: Arc<AtomicBool>,
}

#[derive(Default)]
struct Subscriptions {
    refusals: VecDeque<Refusal>,
    connections: Vec<FakeConnection>,
    attempts: usize,
}

/// In-memory `events.subscribe`. Like the socket, a connection pushes
/// [`Pushed::Ended`] once, when it ends or is dropped.
#[derive(Clone, Default)]
pub struct FakeSubscriber(Arc<Mutex<Subscriptions>>);

impl FakeSubscriber {
    pub fn refuse_next(&self, refusal: Refusal) {
        self.0.lock().unwrap().refusals.push_back(refusal);
    }

    /// Every subscribe so far, refused or not.
    pub fn attempts(&self) -> usize {
        self.0.lock().unwrap().attempts
    }

    /// The subscriptions of each open connection, oldest first.
    pub fn open(&self) -> Vec<Vec<Subscription>> {
        let state = self.0.lock().unwrap();
        state
            .connections
            .iter()
            .filter(|c| !c.closed.load(Ordering::SeqCst))
            .map(|c| c.subscriptions.clone())
            .collect()
    }

    /// Pushes an event on the open `which` connection.
    pub fn push(&self, which: Which) {
        (self.find(which).on_push)(Pushed::Event);
    }

    /// Ends the open `which` connection, as `events_lost` or a hang-up does.
    pub fn end(&self, which: Which) {
        let connection = self.find(which);
        connection.closed.store(true, Ordering::SeqCst);
        (connection.on_push)(Pushed::Ended);
    }

    fn find(&self, which: Which) -> FakeHandle {
        let state = self.0.lock().unwrap();
        let connection = state
            .connections
            .iter()
            .rev()
            .filter(|c| !c.closed.load(Ordering::SeqCst))
            .find(|c| {
                let status = c
                    .subscriptions
                    .iter()
                    .any(|s| matches!(s, Subscription::AgentStatusChanged(_)));
                status == (which == Which::Status)
            })
            .unwrap_or_else(|| panic!("no open {which:?} connection"));
        FakeHandle {
            on_push: connection.on_push.clone(),
            closed: connection.closed.clone(),
        }
    }
}

/// Lets a push run without holding the lock, as the subscriber's callback may.
struct FakeHandle {
    on_push: Arc<OnPush>,
    closed: Arc<AtomicBool>,
}

impl Subscriber for FakeSubscriber {
    fn subscribe(&self, subscriptions: &[Subscription], on_push: OnPush) -> Result<Connection> {
        let mut state = self.0.lock().unwrap();
        state.attempts += 1;
        match state.refusals.pop_front() {
            Some(Refusal::PaneNotFound) => bail!(PaneNotFound),
            Some(Refusal::Failed) => bail!("fake refusal"),
            None => {}
        }
        let on_push = Arc::new(on_push);
        let closed = Arc::new(AtomicBool::new(false));
        state.connections.push(FakeConnection {
            subscriptions: subscriptions.to_vec(),
            on_push: on_push.clone(),
            closed: closed.clone(),
        });
        Ok(Connection::new(move || {
            if !closed.swap(true, Ordering::SeqCst) {
                on_push(Pushed::Ended);
            }
        }))
    }
}
