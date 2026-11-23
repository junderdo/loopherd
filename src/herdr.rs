//! herdr, reached only through the [`Herdr`] trait.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

pub mod events;
#[cfg(test)]
pub mod fake;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
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
    pub cwd: Option<PathBuf>,
    /// The terminal title, without the spinner glyph herdr strips.
    pub title: Option<String>,
    /// A sidebar label some reporter set in place of the agent's name.
    pub display_agent: Option<String>,
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
    pub workspace_id: String,
    pub cwd: Option<PathBuf>,
    /// A plugin pane's manifest title, or a name the human gave the pane.
    pub label: Option<String>,
}

impl Pane {
    /// Whether the pane's cwd is `dir`, however either path is spelled.
    /// Falls back to an exact comparison when either can't be canonicalized.
    pub fn is_in(&self, dir: &Path) -> bool {
        let Some(cwd) = &self.cwd else {
            return false;
        };
        match (cwd.canonicalize(), dir.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => cwd == dir,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTab {
    pub tab_id: String,
    pub pane_id: String,
}

/// A herdr plugin as `herdr plugin list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub enabled: bool,
    pub version: String,
    /// The directory `herdr plugin link` registered; `None` when installed.
    pub linked: Option<PathBuf>,
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
    /// [`Herdr::agents`], or `None` once `timeout` passes, as with a wedged server.
    fn agents_within(&self, timeout: Duration) -> Result<Option<Vec<Agent>>>;
    fn agent(&self, name: &str) -> Result<Agent>;
    fn tabs_in(&self, workspace_id: &str) -> Result<Vec<Tab>>;
    fn panes_in(&self, workspace_id: &str) -> Result<Vec<Pane>>;
    fn pane(&self, pane_id: &str) -> Result<Pane>;
    /// The active tab of the workspace the human's client shows, if herdr knows one.
    fn visible_tab(&self) -> Result<Option<String>>;
    /// The shell is alone in the foreground and has drawn its prompt: no
    /// command, editor, agent, or rc file running.
    fn at_shell_prompt(&self, pane_id: &str) -> Result<bool>;
    /// The pid of the shell herdr started in `pane_id`, if it has one yet.
    fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>>;
    /// The pane's visible screen as plain text. Read-only.
    fn visible_text(&self, pane_id: &str) -> Result<String>;
    /// The agent in `pane_id`'s last `lines` lines of output, unwrapped, as plain text. Read-only.
    fn recent_text(&self, pane_id: &str, lines: usize) -> Result<String>;
    /// Creates a tab without focusing it.
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab>;
    fn close_tab(&self, tab_id: &str) -> Result<()>;
    /// Splits `pane_id` with a new shell pane on `side` taking `share` of the
    /// split, leaving focus where it was, with `env` set for its shell.
    /// Returns the new pane's ID.
    fn split(
        &self,
        pane_id: &str,
        side: Side,
        share: f64,
        cwd: &Path,
        env: &[(&str, &str)],
    ) -> Result<String>;
    fn close_pane(&self, pane_id: &str) -> Result<()>;
    /// Moves `pane_id` beside `target` on `side`, taking `share` of the split,
    /// keeping its process and leaving focus where it was.
    fn dock(&self, pane_id: &str, target: &str, side: Side, share: f64) -> Result<()>;
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
    /// Brings the agent in `pane_id` to the front of the human's herdr client.
    fn focus(&self, pane_id: &str) -> Result<()>;
    /// The pane the human's herdr client has focused, if any.
    fn focused_pane(&self) -> Result<Option<String>>;
    /// Brings the loopherd plugin pane `pane_id` to the front, switching tab
    /// and workspace. Refused for a pane that isn't a plugin's.
    fn focus_plugin_pane(&self, pane_id: &str) -> Result<()>;
    /// Shows `label` in place of the Claude's name in `pane_id`'s sidebar row,
    /// or clears it. Display-only: herdr's status for the pane is unchanged.
    fn label_pane(&self, pane_id: &str, label: Option<&str>) -> Result<()>;
    /// Opens the loopherd plugin's pane `entrypoint` at `placement` with `env`
    /// set for its process. A popup is refused while another herdr modal is open.
    fn open_plugin_pane(
        &self,
        entrypoint: &str,
        placement: &Placement,
        env: &[(&str, &str)],
    ) -> Result<()>;
    /// The plugin `id` in the reached herdr's registry; `None` when neither
    /// installed nor linked. Gives up after `timeout`.
    fn plugin(&self, id: &str, timeout: Duration) -> Result<Option<Plugin>>;
    /// The socket of the herdr these calls reach; `None` when herdr can't say.
    fn socket(&self) -> Option<PathBuf>;
    /// The server as `herdr status server` reports it, giving up after `timeout`.
    fn server(&self, timeout: Duration) -> Result<Server>;
}

/// Where a plugin pane opens.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// As the plugin's manifest declares it.
    Manifest,
    /// Split off `pane` on `side`, the new pane taking `share` of the split,
    /// at most half. Focus moves to the new pane only when `focus` is set;
    /// otherwise it stays where it was, as far as herdr allows.
    Split {
        pane: String,
        side: Side,
        share: f64,
        focus: bool,
    },
}

/// The side of its target a split's new pane takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    /// herdr splits only right or down, so a pane left of or above its
    /// target opens on the far side, then swaps with it.
    fn direction(self) -> &'static str {
        match self {
            Side::Left | Side::Right => "right",
            Side::Above | Side::Below => "down",
        }
    }

    fn swaps(self) -> bool {
        matches!(self, Side::Left | Side::Above)
    }
}

/// herdr's `--ratio` for a pane on `side` of its target taking `share`: the
/// target's share, which a swap hands to the pane.
fn target_ratio(side: Side, share: f64) -> Result<String> {
    if !(share > 0.0 && share < 1.0) {
        bail!("a split pane's share must be between 0 and 1, not {share}");
    }
    Ok(match side.swaps() {
        true => share,
        false => 1.0 - share,
    }
    .to_string())
}

/// How to resize a split's target after a split, which opens at half, so
/// the new pane gets `share`: the direction toward the new pane and the
/// amount. Only shrinking the new pane is supported: growing it means moving
/// the target's far edge, an outer divider when the target sits in a split.
pub fn split_resize(side: Side, share: f64) -> Result<(&'static str, f64)> {
    if !(0.0..=0.5).contains(&share) {
        bail!("a split pane's share must be at most half, not {share}");
    }
    let toward_new = match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Above => "up",
        Side::Below => "down",
    };
    Ok((toward_new, 0.5 - share))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Server {
    /// No `herdr` on PATH.
    Missing,
    /// The status probe outlived its deadline, as with a wedged server.
    NoAnswer {
        socket: Option<PathBuf>,
    },
    NotRunning {
        socket: Option<PathBuf>,
    },
    Running {
        socket: Option<PathBuf>,
        version: Option<String>,
    },
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
        reply(args, out)
    }

    fn call_ok(&self, args: &[&str]) -> Result<Value> {
        ok_reply(args, self.call(args)?)
    }

    /// Swaps a pane just split off `target` into its place, focusing the new
    /// pane only when `focus_new` is set.
    fn swap(&self, target: &str, new: &str, focus_new: bool) -> Result<()> {
        let focused = match focus_new {
            true => None,
            false => self
                .call_ok(&["pane", "layout", "--pane", target])
                .ok()
                .and_then(|r| r["layout"]["focused_pane_id"].as_str().map(String::from)),
        };
        // herdr's swap focuses its source and brings that pane's tab forward.
        let (source, other) = match focus_new {
            true => (new, target),
            false => (target, new),
        };
        let args = [
            "pane",
            "swap",
            "--source-pane",
            source,
            "--target-pane",
            other,
        ];
        self.call_ok(&args)?;
        if let Some(focused) = focused.filter(|f| f != source) {
            // Best effort: the layout is right whether or not focus goes back.
            let _ = self.focus_through_neighbor(&focused);
        }
        Ok(())
    }

    /// Focuses `pane` by moving focus from a neighbor toward it, herdr having
    /// no focus by ID for a shell's pane.
    fn focus_through_neighbor(&self, pane: &str) -> Result<()> {
        let neighbor = |of: &str, direction: &str| {
            self.call_ok(&["pane", "neighbor", "--pane", of, "--direction", direction])
                .ok()
                .and_then(|r| r["neighbor"]["neighbor_pane_id"].as_str().map(String::from))
        };
        for (out, back) in [
            ("left", "right"),
            ("right", "left"),
            ("up", "down"),
            ("down", "up"),
        ] {
            let Some(next) = neighbor(pane, out) else {
                continue;
            };
            if neighbor(&next, back).as_deref() == Some(pane) {
                let args = ["pane", "focus", "--pane", &next, "--direction", back];
                return self.call_ok(&args).map(drop);
            }
        }
        bail!("no neighbor of {pane} leads back to it")
    }

    fn call_ok_within(&self, args: &[&str], timeout: Duration) -> Result<Value> {
        let ran = crate::shell::run_within(&mut self.command(args), timeout)
            .with_context(|| format!("run herdr {}", args.join(" ")))?;
        let Some(out) = ran else {
            bail!(
                "herdr didn't answer {} within {}",
                args[..2].join(" "),
                humantime::format_duration(timeout)
            );
        };
        ok_reply(args, reply(args, out)?)
    }
}

fn ok_reply(args: &[&str], reply: std::result::Result<Value, ApiError>) -> Result<Value> {
    reply.map_err(|e| anyhow!("herdr {}: {} ({})", args[..2].join(" "), e.message, e.code))
}

fn reply(args: &[&str], out: std::process::Output) -> Result<std::result::Result<Value, ApiError>> {
    if out.status.success() {
        let json: Value =
            serde_json::from_slice(&out.stdout).with_context(|| MalformedReply(args.join(" ")))?;
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
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    terminal_title: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    display_agent: Option<String>,
}

#[derive(Deserialize)]
struct PaneJson {
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    cwd: Option<PathBuf>,
    label: Option<String>,
}

impl From<PaneJson> for Pane {
    fn from(p: PaneJson) -> Self {
        Pane {
            id: p.pane_id,
            tab_id: p.tab_id,
            workspace_id: p.workspace_id,
            cwd: p.cwd,
            label: p.label,
        }
    }
}

#[derive(Deserialize)]
struct PluginJson {
    plugin_id: String,
    enabled: bool,
    version: String,
    plugin_root: PathBuf,
    source: PluginSourceJson,
}

#[derive(Deserialize)]
struct PluginSourceJson {
    kind: String,
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
            cwd: a.cwd,
            title: a.terminal_title_stripped.or(a.terminal_title),
            display_agent: a.display_agent,
        }
    }
}

/// herdr answered, but not in a shape loopherd understands: unlike a failed
/// call, retrying or starting the server won't help. Find it with
/// `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct MalformedReply(pub String);

impl std::fmt::Display for MalformedReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse herdr {} output", self.0)
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T> {
    T::deserialize(value).with_context(|| MalformedReply(what.into()))
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

    fn agents_within(&self, timeout: Duration) -> Result<Option<Vec<Agent>>> {
        let args = ["agent", "list"];
        let mut command = self.command(&args);
        let Some(out) =
            crate::shell::run_within(&mut command, timeout).context("run herdr agent list")?
        else {
            return Ok(None);
        };
        let result = ok_reply(&args, reply(&args, out)?)?;
        let list: Vec<AgentJson> = parse(&result["agents"], "agent list")?;
        Ok(Some(list.into_iter().map(Agent::from).collect()))
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
        let result = self.call_ok(&["pane", "list", "--workspace", workspace_id])?;
        let list: Vec<PaneJson> = parse(&result["panes"], "pane list")?;
        Ok(list.into_iter().map(Pane::from).collect())
    }

    fn pane(&self, pane_id: &str) -> Result<Pane> {
        let result = self.call_ok(&["pane", "get", pane_id])?;
        Ok(parse::<PaneJson>(&result["pane"], "pane get")?.into())
    }

    fn visible_tab(&self) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct WorkspaceJson {
            focused: bool,
            active_tab_id: Option<String>,
        }
        let result = self.call_ok(&["workspace", "list"])?;
        let list: Vec<WorkspaceJson> = parse(&result["workspaces"], "workspace list")?;
        Ok(list
            .into_iter()
            .find(|w| w.focused)
            .and_then(|w| w.active_tab_id))
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

    fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>> {
        let result = self.call_ok(&["pane", "process-info", "--pane", pane_id])?;
        Ok(result["process_info"]["shell_pid"]
            .as_u64()
            .and_then(|p| p.try_into().ok()))
    }

    // `pane read` prints plain text, not a JSON envelope.
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

    // `agent read` prints plain text, or a JSON error envelope when it fails.
    fn recent_text(&self, pane_id: &str, lines: usize) -> Result<String> {
        let lines = lines.to_string();
        let args = [
            "agent",
            "read",
            pane_id,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines,
            "--format",
            "text",
        ];
        let out = self
            .command(&args)
            .output()
            .with_context(|| format!("run herdr {}", args.join(" ")))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        match serde_json::from_slice::<ErrorEnvelope>(&out.stdout)
            .or_else(|_| serde_json::from_slice::<ErrorEnvelope>(&out.stderr))
        {
            Ok(env) => bail!(
                "herdr agent read: {} ({})",
                env.error.message,
                env.error.code
            ),
            Err(_) => bail!(
                "herdr agent read failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
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

    fn split(
        &self,
        pane_id: &str,
        side: Side,
        share: f64,
        cwd: &Path,
        env: &[(&str, &str)],
    ) -> Result<String> {
        let ratio = target_ratio(side, share)?;
        let cwd = cwd.to_str().context("cwd is not UTF-8")?;
        let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut argv = vec![
            "pane",
            "split",
            pane_id,
            "--direction",
            side.direction(),
            "--ratio",
            &ratio,
            "--cwd",
            cwd,
            "--no-focus",
        ];
        for pair in &env {
            argv.extend(["--env", pair]);
        }
        let result = self.call_ok(&argv)?;
        let new: String = parse(&result["pane"]["pane_id"], "pane split")?;
        if side.swaps()
            && let Err(e) = self.swap(pane_id, &new, false)
        {
            let _ = self.close_pane(&new);
            return Err(e);
        }
        Ok(new)
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.call_ok(&["pane", "close", pane_id]).map(drop)
    }

    fn dock(&self, pane_id: &str, target: &str, side: Side, share: f64) -> Result<()> {
        let ratio = target_ratio(side, share)?;
        let tab = self.pane(target)?.tab_id;
        let moved = |args: &[&str], step: &str| -> Result<()> {
            let result = self.call_ok(args).with_context(|| step.to_string())?;
            match result["move_result"]["changed"].as_bool() {
                Some(true) => Ok(()),
                _ => bail!("{step}: herdr left it where it was"),
            }
        };
        // herdr won't re-dock a pane within its own tab, so it leaves through a tab of its own.
        moved(
            &["pane", "move", pane_id, "--new-tab", "--no-focus"],
            &format!("move {pane_id} out to a tab of its own"),
        )?;
        moved(
            &[
                "pane",
                "move",
                pane_id,
                "--tab",
                &tab,
                "--target-pane",
                target,
                "--split",
                side.direction(),
                "--ratio",
                &ratio,
                "--no-focus",
            ],
            &format!("move {pane_id} back beside {target}"),
        )?;
        if side.swaps() {
            self.swap(target, pane_id, false)
                .with_context(|| format!("swap {pane_id} to the {side:?} of {target}"))?;
        }
        Ok(())
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

    fn focus(&self, pane_id: &str) -> Result<()> {
        self.call_ok(&["agent", "focus", pane_id]).map(drop)
    }

    fn focused_pane(&self) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct FocusJson {
            pane_id: String,
            #[serde(default)]
            focused: bool,
        }
        let result = self.call_ok(&["pane", "list"])?;
        let list: Vec<FocusJson> = parse(&result["panes"], "pane list")?;
        Ok(list.into_iter().find(|p| p.focused).map(|p| p.pane_id))
    }

    fn focus_plugin_pane(&self, pane_id: &str) -> Result<()> {
        self.call_ok(&["plugin", "pane", "focus", pane_id])
            .map(drop)
    }

    // herdr 0.9.5 reads `--source` as an unknown option unless the pane comes
    // first, and prints nothing on success.
    fn label_pane(&self, pane_id: &str, label: Option<&str>) -> Result<()> {
        let mut argv = vec!["pane", "report-metadata", pane_id, "--source", "loopherd"];
        match label {
            Some(label) => argv.extend(["--agent", "claude", "--display-agent", label]),
            None => argv.push("--clear-display-agent"),
        }
        let out = self
            .command(&argv)
            .output()
            .with_context(|| format!("run herdr {}", argv.join(" ")))?;
        if out.status.success() {
            return Ok(());
        }
        match serde_json::from_slice::<ErrorEnvelope>(&out.stdout)
            .or_else(|_| serde_json::from_slice::<ErrorEnvelope>(&out.stderr))
        {
            Ok(env) => bail!(
                "herdr pane report-metadata: {} ({})",
                env.error.message,
                env.error.code
            ),
            Err(_) => bail!(
                "herdr pane report-metadata failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    fn open_plugin_pane(
        &self,
        entrypoint: &str,
        placement: &Placement,
        env: &[(&str, &str)],
    ) -> Result<()> {
        let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut argv = vec![
            "plugin",
            "pane",
            "open",
            "--plugin",
            "loopherd",
            "--entrypoint",
            entrypoint,
        ];
        let mut resize = None;
        if let Placement::Split {
            pane,
            side,
            share,
            focus,
        } = placement
        {
            resize = Some(split_resize(*side, *share)?);
            argv.extend([
                "--placement",
                "split",
                "--target-pane",
                pane,
                "--direction",
                side.direction(),
                if *focus { "--focus" } else { "--no-focus" },
            ]);
        }
        for pair in &env {
            argv.extend(["--env", pair]);
        }
        let out = self
            .command(&argv)
            .output()
            .with_context(|| format!("run herdr {}", argv[..3].join(" ")))?;
        if !out.status.success() {
            match [&out.stdout, &out.stderr]
                .into_iter()
                .find_map(|bytes| serde_json::from_slice::<ErrorEnvelope>(bytes).ok())
            {
                Some(env) => bail!(
                    "herdr plugin pane open: {} ({})",
                    env.error.message,
                    env.error.code
                ),
                None => bail!(
                    "herdr plugin pane open failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            }
        }
        let (
            Placement::Split {
                pane, side, focus, ..
            },
            Some((toward_new, amount)),
        ) = (placement, resize)
        else {
            return Ok(());
        };
        if side.swaps() {
            let opened = serde_json::from_slice::<Value>(&out.stdout)
                .context("herdr plugin pane open: a reply that isn't JSON")
                .and_then(|reply| {
                    parse::<String>(
                        &reply["result"]["plugin_pane"]["pane"]["pane_id"],
                        "plugin pane open",
                    )
                });
            if let Err(e) = opened.and_then(|new| self.swap(pane, &new, *focus)) {
                let instead = match side.direction() {
                    "down" => "below",
                    _ => "right of",
                };
                eprintln!("loopherd: the plugin pane opened {instead} its target instead: {e:#}");
                return Ok(());
            }
        }
        // Resizing the target, not the new pane, moves the divider between them.
        let amount = amount.to_string();
        let args = [
            "pane",
            "resize",
            "--pane",
            pane,
            "--direction",
            toward_new,
            "--amount",
            &amount,
        ];
        if let Err(e) = self.call_ok(&args) {
            eprintln!("loopherd: the plugin pane opened at half the split: {e:#}");
        }
        Ok(())
    }

    fn plugin(&self, id: &str, timeout: Duration) -> Result<Option<Plugin>> {
        let args = ["plugin", "list", "--plugin", id, "--json"];
        let listed = self.call_ok_within(&args, timeout)?;
        let plugins: Vec<PluginJson> = parse(&listed["plugins"], &args.join(" "))?;
        Ok(plugins
            .into_iter()
            .find(|p| p.plugin_id == id)
            .map(|p| Plugin {
                enabled: p.enabled,
                version: p.version,
                linked: (p.source.kind == "local").then_some(p.plugin_root),
            }))
    }

    fn socket(&self) -> Option<PathBuf> {
        socket_from_env().or_else(|| {
            let out = self
                .command(&["status", "server", "--json"])
                .output()
                .ok()?;
            let status: Value = serde_json::from_slice(&out.stdout).ok()?;
            status["socket"].as_str().map(PathBuf::from)
        })
    }

    fn server(&self, timeout: Duration) -> Result<Server> {
        let mut probe = self.command(&["status", "server", "--json"]);
        let out = match crate::shell::run_within(&mut probe, timeout) {
            Err(e)
                if e.downcast_ref::<std::io::Error>().map(std::io::Error::kind)
                    == Some(std::io::ErrorKind::NotFound) =>
            {
                return Ok(Server::Missing);
            }
            Err(e) => return Err(e.context("run herdr status server --json")),
            Ok(None) => {
                return Ok(Server::NoAnswer {
                    socket: socket_from_env(),
                });
            }
            Ok(Some(out)) => out,
        };
        if !out.status.success() {
            bail!(
                "herdr status server --json failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let status: Value = serde_json::from_slice(&out.stdout)
            .with_context(|| MalformedReply("status server --json".into()))?;
        let socket = status["socket"]
            .as_str()
            .map(PathBuf::from)
            .or_else(socket_from_env);
        Ok(match status["running"].as_bool() {
            Some(true) => Server::Running {
                socket,
                version: status["version"].as_str().map(str::to_string),
            },
            _ => Server::NotRunning { socket },
        })
    }
}

/// `$HERDR_SOCKET_PATH`, which herdr sets in its panes.
pub fn socket_from_env() -> Option<PathBuf> {
    std::env::var_os("HERDR_SOCKET_PATH")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeHerdr;

    #[test]
    fn a_split_shrinks_the_new_pane_from_half_by_growing_its_target_toward_it() {
        for (side, toward_new) in [
            (Side::Right, "right"),
            (Side::Left, "left"),
            (Side::Above, "up"),
            (Side::Below, "down"),
        ] {
            let (direction, amount) = split_resize(side, 0.4).unwrap();
            assert_eq!(direction, toward_new);
            assert!((amount - 0.1).abs() < 1e-9, "{side:?}: {amount}");
        }
        assert_eq!(split_resize(Side::Above, 0.5).unwrap(), ("up", 0.0));
    }

    #[test]
    fn a_split_wider_than_half_is_refused_and_opens_nothing() {
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let pane = herdr.add_tab("w1", "t", Path::new("/src"), true);
        let wide = Placement::Split {
            pane,
            side: Side::Above,
            share: 0.6,
            focus: false,
        };

        let err = herdr.open_plugin_pane("view", &wide, &[]).unwrap_err();

        assert!(format!("{err:#}").contains("at most half"), "{err:#}");
        assert!(herdr.plugin_panes().is_empty());
    }
}
