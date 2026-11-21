//! Herd files (D7, D9) and `loopherd add`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Attended, FileLedger, Ledger};
use crate::lock::Lock;
use crate::sandbox::{Probe, SandboxMode, UnixSockets};
use crate::spawn::{Checkout, MemberOf, Prompt, SHELL_TIMEOUT, SpawnRequest, Spawned, preview};
use crate::template::Template;
use crate::worker_sandbox::WorkerSandbox;

mod edit;
mod remove;

pub use edit::{get, set, unset};
pub use remove::{Removal, remove};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HerdFile {
    repo: String,
    workspace: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    template: BTreeMap<String, TemplateFile>,
    queue: Option<QueueFile>,
    lead: Option<LeadFile>,
    worker: Option<WorkerFile>,
    /// Commands `feed` runs in `repo` once per member, after it merges.
    #[serde(default)]
    after_merge: Vec<String>,
}

/// A herd's lead member (D17).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeadFile {
    prompt: Option<String>,
    prompt_file: Option<String>,
    args: Option<Vec<String>>,
    merge: Option<bool>,
    install: Option<bool>,
    rebase: Option<bool>,
    new_cards: Option<bool>,
    design_escalation: Option<bool>,
    blocked_dialogs: Option<bool>,
    review_rounds: Option<NonZeroU32>,
    context_limit: Option<u64>,
    #[serde(default)]
    merge_guard: MergeGuard,
    #[serde(default)]
    sandbox: SandboxMode,
    /// MCP servers, by name, from the human's own Claude config; a lead gets no others.
    #[serde(default)]
    mcp_servers: Vec<String>,
    unix_sockets: Option<UnixSockets>,
}

/// A herd's workers: every member but its lead.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFile {
    #[serde(default)]
    sandbox: WorkerSandbox,
    unix_sockets: Option<UnixSockets>,
}

/// How a lead with `merge` off is kept from merging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MergeGuard {
    /// The lead's tool hook refuses its merges, but it holds the human's
    /// credentials, so the human is also told of every merge.
    #[default]
    Hook,
    /// The lead holds a deploy key and a token that can't merge, and can't
    /// read the human's credentials.
    Credentials,
}

/// What a lead may do on its own; off means the human does it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Switches {
    pub merge: bool,
    pub install: bool,
    pub rebase: bool,
    pub new_cards: bool,
    pub design_escalation: bool,
    pub blocked_dialogs: bool,
}

impl MergeGuard {
    pub fn name(self) -> &'static str {
        match self {
            MergeGuard::Hook => "hook",
            MergeGuard::Credentials => "credentials",
        }
    }
}

impl Switches {
    /// One `<switch> on|off` line each, for the lead's `{switches}`.
    pub fn lines(&self) -> String {
        self.named()
            .map(|(name, on)| format!("{name} {}", on_off(on)))
            .join("\n")
    }

    pub fn named(&self) -> [(&'static str, bool); 6] {
        [
            ("merge", self.merge),
            ("install", self.install),
            ("rebase", self.rebase),
            ("new_cards", self.new_cards),
            ("design_escalation", self.design_escalation),
            ("blocked_dialogs", self.blocked_dialogs),
        ]
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The switches and review-round limit one layer sets: a herd file's
/// `[lead]`, or `config.toml`'s `[lead_defaults]`. A switch neither sets is
/// off, and the limit is [`DEFAULT_REVIEW_ROUNDS`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchValues {
    pub merge: Option<bool>,
    pub install: Option<bool>,
    pub rebase: Option<bool>,
    pub new_cards: Option<bool>,
    pub design_escalation: Option<bool>,
    pub blocked_dialogs: Option<bool>,
    pub review_rounds: Option<NonZeroU32>,
}

/// How many review rounds a lead runs on a PR before asking the human.
const DEFAULT_REVIEW_ROUNDS: u32 = 3;

impl SwitchValues {
    pub const NAMES: [&str; 6] = [
        "merge",
        "install",
        "rebase",
        "new_cards",
        "design_escalation",
        "blocked_dialogs",
    ];

    /// Whether this layer sets any switch; `review_rounds` alone doesn't count.
    pub fn sets_a_switch(&self) -> bool {
        self.values().iter().any(Option::is_some)
    }

    fn values(&self) -> [Option<bool>; 6] {
        [
            self.merge,
            self.install,
            self.rebase,
            self.new_cards,
            self.design_escalation,
            self.blocked_dialogs,
        ]
    }

    /// Each switch from `self`, else from `defaults`, else off.
    pub fn over(&self, defaults: &SwitchValues) -> Switches {
        let on = |own: Option<bool>, default: Option<bool>| own.or(default).unwrap_or(false);
        Switches {
            merge: on(self.merge, defaults.merge),
            install: on(self.install, defaults.install),
            rebase: on(self.rebase, defaults.rebase),
            new_cards: on(self.new_cards, defaults.new_cards),
            design_escalation: on(self.design_escalation, defaults.design_escalation),
            blocked_dialogs: on(self.blocked_dialogs, defaults.blocked_dialogs),
        }
    }

    /// The review-round limit from `self`, else from `defaults`, else
    /// built-in, with which of the three it came from.
    fn review_rounds_over(&self, defaults: &SwitchValues) -> (u32, &'static str) {
        match (self.review_rounds, defaults.review_rounds) {
            (Some(rounds), _) => (rounds.get(), "herd file"),
            (None, Some(rounds)) => (rounds.get(), "lead_defaults"),
            (None, None) => (DEFAULT_REVIEW_ROUNDS, "built-in"),
        }
    }
}

/// What `loopherd feed` spawns (D14).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueFile {
    template: Option<String>,
    /// Each task's first word names one of these; set this or `template`.
    templates: Option<Vec<String>>,
    /// A shell command printing one task per line.
    source: String,
    /// A shell command printing one unready task per line.
    backlog: Option<String>,
    max: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    params: Vec<String>,
    name: Option<String>,
    branch: Option<String>,
    /// A detached worktree's slug pattern, instead of `branch`.
    worktree: Option<String>,
    prompt: Option<String>,
    prompt_file: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    /// A shell command that exits 0 once a member is finished, for `feed` to
    /// retire it. Placeholder values are substituted already shell-quoted.
    done: Option<String>,
    /// Like `done`, exiting 0 once a member's work is merged; a lead's
    /// watch wakes on it.
    merged: Option<String>,
    context_limit: Option<u64>,
    /// The human sits in its member's pane; only `add` starts one (D112).
    #[serde(default)]
    attended: bool,
}

/// [`Paths::from_env`]'s herds dir; `None` when it needs `$HOME` and that is unset.
pub fn herds_dir() -> Option<PathBuf> {
    let config = match (env_var("LOOPHERD_CONFIG_DIR"), env_var("XDG_CONFIG_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(xdg)) => PathBuf::from(xdg).join("loopherd"),
        (None, None) => PathBuf::from(env_var("HOME")?).join(".config/loopherd"),
    };
    Some(config.join("herds"))
}

fn env_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// How long [`Paths::lock_feed`] waits out another holder.
const FEED_LOCK_GRACE: Duration = Duration::from_secs(1);

/// Where herd files live, the home a leading `~` expands to, the lock
/// that keeps `feed` and `add` from filling a queue's slots at once, and
/// the paused herds' files.
#[derive(Debug, Clone)]
pub struct Paths {
    pub herds: PathBuf,
    pub home: PathBuf,
    pub feed_lock: PathBuf,
    pub paused: PathBuf,
}

impl Paths {
    /// loopherd's config dir, holding `herds/` and `config.toml`.
    pub fn config_dir(&self) -> &Path {
        self.herds.parent().unwrap_or(&self.herds)
    }

    /// loopherd's state dir, holding the ledger beside the feed lock.
    pub fn state_dir(&self) -> &Path {
        self.feed_lock.parent().unwrap_or(&self.feed_lock)
    }

    /// The state dir's `locks/`, which a sandboxed lead can't read, so it
    /// can't take a lock there and wedge the process that waits on it.
    pub fn locks_dir(&self) -> PathBuf {
        self.state_dir().join("locks")
    }

    /// `$LOOPHERD_CONFIG_DIR/herds`, else `$XDG_CONFIG_HOME/loopherd/herds`,
    /// defaulting to `~/.config/loopherd/herds`.
    pub fn from_env() -> Result<Paths> {
        let home = PathBuf::from(env_var("HOME").context("HOME is not set")?);
        Ok(Paths {
            herds: herds_dir().context("HOME is not set")?,
            home,
            feed_lock: FileLedger::default_path()?.with_file_name("feed.lock"),
            paused: crate::pause::dir()?,
        })
    }

    /// Takes the feed lock; fails if another run holds it past
    /// [`FEED_LOCK_GRACE`], which outlasts a lead's watch probing it.
    pub fn lock_feed(&self) -> Result<Lock> {
        let path = &self.feed_lock;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file =
            std::fs::File::create(path).with_context(|| format!("open {}", path.display()))?;
        let given_up = std::time::Instant::now() + FEED_LOCK_GRACE;
        loop {
            let attempt = file
                .try_clone()
                .with_context(|| format!("open {}", path.display()))?;
            if let Some(lock) =
                Lock::try_take(attempt).with_context(|| format!("lock {}", path.display()))?
            {
                return Ok(lock);
            }
            if std::time::Instant::now() >= given_up {
                bail!("feed, add or release already running; retry once it ends");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
impl Paths {
    /// Herd files, the feed lock and pauses under `root`; home is `/home/me`.
    pub fn under(root: &Path) -> Paths {
        Paths {
            herds: root.join("herds"),
            home: PathBuf::from("/home/me"),
            feed_lock: root.join("feed.lock"),
            paused: root.join("paused"),
        }
    }
}

pub struct AddRequest {
    pub herd: String,
    pub template: String,
    pub params: Vec<String>,
    /// Passed to claude after the herd's and the template's args.
    pub claude_args: Vec<String>,
    pub ready_timeout: Duration,
    /// The caller's herdr pane, recorded on an attended member to return to.
    pub return_pane: Option<String>,
    /// Whether an attended member's pane is focused once it is up and prompted.
    pub focus: bool,
    /// Asked only when the herd's `[worker] sandbox` is on.
    pub sandbox: Probe,
}

#[derive(Debug)]
pub struct Added {
    pub name: String,
    pub spawned: Spawned,
    /// Set when the member took the herd past its queue's `max`, or its
    /// pane couldn't be focused.
    pub warning: Option<String>,
}

/// Spawns a member of `req.herd`.
///
/// Into a herd with a `[queue]`, fails while `feed` holds the feed lock
/// past [`Paths::lock_feed`]'s 1s wait, and adds past the
/// queue's `max` with a warning: `add` is the human's override. An attended
/// member holds no slot, so skips both.
pub fn add(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &AddRequest,
    inherited_workspace: Option<&str>,
) -> Result<Added> {
    let spawn_req = resolve(paths, req)?;
    let attended = spawn_req
        .member_of
        .as_ref()
        .is_some_and(|m| m.attended.is_some());
    let max = match attended {
        true => None,
        false => load(&paths.herds, &req.herd)
            .context("herd")?
            .queue
            .map(|q| q.max as usize),
    };
    let _feed = max.map(|_| paths.lock_feed()).transpose()?;
    let held = match max {
        Some(_) => members(ledger, &req.herd)?,
        None => 0,
    };
    let (spawned, sandbox_warnings) = crate::worker_sandbox::spawn(
        herdr,
        ledger,
        paths,
        &spawn_req,
        inherited_workspace,
        &req.sandbox,
    )?;
    let past_max = max.filter(|&max| held >= max).map(|max| {
        format!(
            "{} now has {} members, past its queue max of {max}",
            req.herd,
            held + 1
        )
    });
    let warnings: Vec<String> = sandbox_warnings.into_iter().chain(past_max).collect();
    let mut warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    if attended
        && req.focus
        && !spawned.blocked
        && let Err(e) = herdr.focus(&spawned.pane_id)
    {
        warning = Some(format!("focus {}: {e:#}", spawned.pane_id));
    }
    Ok(Added {
        name: spawn_req.name,
        spawned,
        warning,
    })
}

/// How many unretired ledger entries belong to `herd`, not counting its
/// lead or attended members.
pub fn members(ledger: &dyn Ledger, herd: &str) -> Result<usize> {
    let entries = ledger.entries().context("ledger")?;
    Ok(entries
        .values()
        .filter(|e| holds_slot(e) && e.herd.as_deref() == Some(herd))
        .count())
}

/// Whether `herd` has a herd file in `dir` or any ledger record, retired included.
pub fn known(dir: &Path, ledger: &dyn Ledger, herd: &str) -> Result<bool> {
    if is_name(herd) && dir.join(format!("{herd}.toml")).is_file() {
        return Ok(true);
    }
    let entries = ledger.entries().context("ledger")?;
    Ok(entries.values().any(|e| e.herd.as_deref() == Some(herd)))
}

/// Whether `record` counts against its herd's queue `max`.
pub fn holds_slot(record: &AgentRecord) -> bool {
    !record.retired && !record.is_lead() && record.attended.is_none()
}

/// What `add` would do, one `key value` line each, then the prompt.
///
/// Makes only read-only herdr and git calls.
pub fn dry_run(
    herdr: &dyn Herdr,
    paths: &Paths,
    req: &AddRequest,
    inherited_workspace: Option<&str>,
) -> Result<String> {
    let spawn_req = resolve(paths, req)?;
    let (facts, prompt) = plan(herdr, &spawn_req, inherited_workspace)?;
    Ok([facts, vec!["prompt".into(), prompt]].concat().join("\n"))
}

/// What spawning `req` would do: `key value` lines, and the rendered prompt.
pub fn plan(
    herdr: &dyn Herdr,
    req: &SpawnRequest,
    inherited_workspace: Option<&str>,
) -> Result<(Vec<String>, String)> {
    let plan = preview(herdr, req, inherited_workspace)?;
    let worktree = plan.worktree.map(|p| p.display().to_string());
    let args: Vec<String> = req.claude_args.iter().map(|a| quote(a)).collect();
    let facts = vec![
        format!("name {}", req.name),
        format!("branch {}", req.branch().unwrap_or("none")),
        format!("worktree {}", worktree.as_deref().unwrap_or("none")),
        format!("workspace {}", plan.workspace_label),
        format!("args {}", args.join(" ")),
    ];
    Ok((facts, plan.prompt.unwrap_or_default()))
}

fn quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(char::is_whitespace) {
        format!("{arg:?}")
    } else {
        arg.to_string()
    }
}

/// A herd's `[queue]`, with what `feed` needs from the herd around it.
pub struct Queue {
    /// Each template the queue spawns from, with its param names in order.
    pub templates: BTreeMap<String, Vec<String>>,
    /// Whether each task's first word names its template, rather than the
    /// queue having one.
    pub routed: bool,
    /// The listed templates marked `attended`, which feed never spawns.
    pub attended: BTreeSet<String>,
    pub source: String,
    pub backlog: Option<String>,
    pub max: usize,
    pub repo: PathBuf,
    /// Each template's done check, for those that have one.
    pub done: BTreeMap<String, MemberCheck>,
    /// Each template's merged check, for those that have one.
    pub merged: BTreeMap<String, MemberCheck>,
    pub after_merge: Vec<String>,
}

pub fn queue(paths: &Paths, herd: &str) -> Result<Queue> {
    let file = load(&paths.herds, herd).context("herd")?;
    queue_in(&file, paths, herd)?.with_context(|| format!("herd: {herd} has no [queue]"))
}

/// What a lead's watch checks in `herd`: its members' checks, and its
/// queue if it has one.
pub fn lead_checks(paths: &Paths, herd: &str) -> Result<(MemberChecks, Option<Queue>)> {
    let file = load(&paths.herds, herd).context("herd")?;
    let checks = MemberChecks {
        repo: expand_home(&file.repo, &paths.home),
        done: checks_of(&file, "done", |t| t.done.as_deref())?,
        merged: checks_of(&file, "merged", |t| t.merged.as_deref())?,
    };
    Ok((checks, queue_in(&file, paths, herd)?))
}

/// Each template's `done` and `merged` checks, and the repo they run in.
pub struct MemberChecks {
    pub repo: PathBuf,
    pub done: BTreeMap<String, MemberCheck>,
    pub merged: BTreeMap<String, MemberCheck>,
}

fn queue_in(file: &HerdFile, paths: &Paths, herd: &str) -> Result<Option<Queue>> {
    let Some(queue) = &file.queue else {
        return Ok(None);
    };
    let mut templates = BTreeMap::new();
    let mut attended = BTreeSet::new();
    for name in queue.names() {
        let template = file
            .template
            .get(name)
            .with_context(|| format!("queue: {herd} has no template {name}"))?;
        templates.insert(name.clone(), template.params.clone());
        if template.attended {
            attended.insert(name.clone());
        }
    }
    Ok(Some(Queue {
        done: checks_of(file, "done", |t| t.done.as_deref())?,
        merged: checks_of(file, "merged", |t| t.merged.as_deref())?,
        after_merge: file.after_merge.clone(),
        templates,
        routed: queue.templates.is_some(),
        attended,
        source: queue.source.clone(),
        backlog: queue.backlog.clone(),
        max: queue.max as usize,
        repo: expand_home(&file.repo, &paths.home),
    }))
}

/// Each template's check named `field`, for those that have one.
fn checks_of(
    file: &HerdFile,
    field: &'static str,
    command: fn(&TemplateFile) -> Option<&str>,
) -> Result<BTreeMap<String, MemberCheck>> {
    let mut checks = BTreeMap::new();
    for (name, template) in &file.template {
        if let Some(check) = MemberCheck::of(template, field, command(template))
            .with_context(|| format!("template {name}"))?
        {
            checks.insert(name.clone(), check);
        }
    }
    Ok(checks)
}

/// A template's `done` or `merged` command, rendered per member like its prompt.
pub struct MemberCheck {
    /// The command as the herd file writes it, placeholders and all.
    text: String,
    command: Template,
    branch: Option<Template>,
}

impl MemberCheck {
    /// `command` is `template`'s field named `field`.
    fn of(
        template: &TemplateFile,
        field: &'static str,
        command: Option<&str>,
    ) -> Result<Option<MemberCheck>> {
        let Some(text) = command else {
            return Ok(None);
        };
        let Some(command) = pattern(command, &template.params, &BUILT_INS).context(field)? else {
            return Ok(None);
        };
        let branch = pattern(
            template.branch.as_deref(),
            &template.params,
            &["herd", "name"],
        )
        .context("branch")?;
        Ok(Some(MemberCheck {
            text: text.to_string(),
            command,
            branch,
        }))
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The command for `member`, whose `params` are its recorded values, each
    /// value shell-quoted since params come from the source's output.
    pub fn render(
        &self,
        member: &AgentRecord,
        params: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut values: HashMap<&str, &str> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        values.insert("herd", member.herd.as_deref().unwrap_or_default());
        values.insert("name", &member.name);
        let branch = match (&member.branch, &self.branch) {
            (Some(recorded), _) => recorded.clone(),
            (None, Some(pattern)) => pattern.render(&values)?,
            (None, None) => String::new(),
        };
        let worktree = member
            .worktree
            .as_ref()
            .unwrap_or(&member.repo)
            .display()
            .to_string();
        values.insert("branch", &branch);
        values.insert("worktree", &worktree);
        values.insert("pane", &member.pane_id);
        let quoted: HashMap<&str, String> = values
            .iter()
            .map(|(&k, v)| (k, crate::shell::quote(v)))
            .collect();
        self.command
            .render(&quoted.iter().map(|(&k, v)| (k, v.as_str())).collect())
    }
}

/// The spawn request for `req`, from the named herd file alone.
pub fn resolve(paths: &Paths, req: &AddRequest) -> Result<SpawnRequest> {
    let herd = load(&paths.herds, &req.herd).context("herd")?;
    let template = herd.template.get(&req.template).with_context(|| {
        let known: Vec<&str> = herd.template.keys().map(String::as_str).collect();
        format!(
            "template: {} has no template {} (it has: {})",
            req.herd,
            req.template,
            known.join(", ")
        )
    })?;
    let checked = check_template(&req.template, template, paths)?;
    if req.params.len() != template.params.len() {
        bail!(
            "template {} takes {} param{} ({}), got {}\n{USAGE}",
            req.template,
            template.params.len(),
            if template.params.len() == 1 { "" } else { "s" },
            template.params.join(", "),
            req.params.len()
        );
    }
    let params: Vec<(String, String)> = template
        .params
        .iter()
        .cloned()
        .zip(req.params.iter().cloned())
        .collect();
    for (key, value) in &params {
        // The default name joins every param.
        let reaches_a_path = checked.name.is_none()
            || [&checked.name, &checked.branch, &checked.worktree]
                .into_iter()
                .flatten()
                .any(|pattern| pattern.uses(key));
        if reaches_a_path && !is_plain_word(value) {
            bail!(
                "template {}: param {key} is {value:?}; a param in a name, branch or worktree \
                 must match [A-Za-z0-9][A-Za-z0-9._-]* without \"..\"",
                req.template
            );
        }
    }
    let mut values: HashMap<&str, &str> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    values.insert("herd", &req.herd);
    let attended = template.attended.then(|| Attended {
        return_pane: req.return_pane.clone(),
    });
    let shepherd_pane = attended.as_ref().map(|a| {
        let pane = a.return_pane.clone().unwrap_or_default();
        ("shepherd_pane".to_string(), pane)
    });

    let name = match &checked.name {
        Some(pattern) => pattern.render(&values)?,
        None => [req.template.as_str()]
            .into_iter()
            .chain(req.params.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("-"),
    };
    values.insert("name", &name);
    let checkout = match (&checked.branch, &checked.worktree) {
        (Some(pattern), _) => Some(Checkout::Branch(pattern.render(&values)?)),
        (None, Some(pattern)) => Some(Checkout::Detached(pattern.render(&values)?)),
        (None, None) => None,
    };

    Ok(SpawnRequest {
        repo: expand_home(&herd.repo, &paths.home),
        name,
        worktree: checkout,
        workspace: herd.workspace.clone(),
        prompt: Some(Prompt {
            source: format!("template {} {}", req.template, checked.prompt.source),
            text: checked.prompt.text,
        }),
        ready_timeout: req.ready_timeout,
        shell_timeout: SHELL_TIMEOUT,
        claude_args: [&herd.args[..], &template.args, &req.claude_args].concat(),
        member_of: Some(MemberOf {
            herd: req.herd.clone(),
            template: Some(req.template.clone()),
            params,
            values: shepherd_pane.into_iter().collect(),
            role: None,
            attended,
        }),
    })
}

const USAGE: &str = "usage: loopherd add <herd> <template> <param>…";

/// Safe as a path component and a ref component: no `/`, no `..`, and no
/// leading `.` or `-` that git or a shell could read as something else.
fn is_plain_word(value: &str) -> bool {
    value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A template's patterns and prompt, checked as far as they can be without param values.
struct CheckedTemplate {
    name: Option<Template>,
    branch: Option<Template>,
    worktree: Option<Template>,
    prompt: Prompt,
}

/// Every check `add` makes on a template before it knows the params' values,
/// shared with `herds`. Errors name the template and the offending field or file.
fn check_template(
    template_name: &str,
    template: &TemplateFile,
    paths: &Paths,
) -> Result<CheckedTemplate> {
    check_fields(template, paths).with_context(|| format!("template {template_name}"))
}

fn check_fields(template: &TemplateFile, paths: &Paths) -> Result<CheckedTemplate> {
    let name = pattern(template.name.as_deref(), &template.params, &["herd"]).context("name")?;
    let branch = pattern(
        template.branch.as_deref(),
        &template.params,
        &["herd", "name"],
    )
    .context("branch")?;
    let worktree = pattern(
        template.worktree.as_deref(),
        &template.params,
        &["herd", "name"],
    )
    .context("worktree")?;

    let prompt = read_prompt(&template.prompt, &template.prompt_file, paths)?;
    if prompt.text.trim().is_empty() {
        bail!("{} is empty", prompt.source);
    }
    let attended_keys: &[&str] = if template.attended {
        &["shepherd_pane"]
    } else {
        &[]
    };
    let prompt_keys = [&BUILT_INS[..], attended_keys].concat();
    pattern(Some(&prompt.text), &template.params, &prompt_keys).context(prompt.source.clone())?;
    pattern(template.done.as_deref(), &template.params, &BUILT_INS).context("done")?;
    pattern(template.merged.as_deref(), &template.params, &BUILT_INS).context("merged")?;
    Ok(CheckedTemplate {
        name,
        branch,
        worktree,
        prompt,
    })
}

/// A herd's `[lead]`, with what `loopherd lead` needs from the herd around it.
#[derive(Debug)]
pub struct LeadConfig {
    pub repo: PathBuf,
    pub workspace: Option<String>,
    pub prompt: Prompt,
    /// `[lead].args`, else the herd's `args`.
    pub args: Vec<String>,
    pub switches: Switches,
    pub review_rounds: u32,
}

/// Placeholders a lead's prompt may use.
pub const LEAD_KEYS: [&str; 5] = ["name", "herd", "worktree", "switches", "review_rounds"];

pub fn lead_config(paths: &Paths, herd: &str) -> Result<LeadConfig> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    let prompt = read_prompt(&lead.prompt, &lead.prompt_file, paths).context("lead")?;
    check_lead_prompt(&prompt).context("lead")?;
    let defaults = lead_defaults(paths)?;
    Ok(LeadConfig {
        repo: expand_home(&file.repo, &paths.home),
        workspace: file.workspace,
        prompt,
        switches: lead.switches().over(&defaults),
        review_rounds: lead.switches().review_rounds_over(&defaults).0,
        args: lead.args.unwrap_or(file.args),
    })
}

/// The context limit past which `member`'s Stop hook asks for a handoff:
/// its template's or `[lead]`'s `context_limit`, else `config.toml`'s.
pub fn context_limit(paths: &Paths, member: &AgentRecord) -> Result<u64> {
    let own = match (&member.herd, &member.template) {
        (Some(herd), _) if member.is_lead() => load(&paths.herds, herd)?
            .lead
            .and_then(|lead| lead.context_limit),
        (Some(herd), Some(template)) => load(&paths.herds, herd)?
            .template
            .get(template)
            .and_then(|t| t.context_limit),
        _ => None,
    };
    crate::context::effective_limit(own, &config_file(paths))
}

pub fn config_file(paths: &Paths) -> PathBuf {
    paths.herds.with_file_name("config.toml")
}

/// Whether `herd`'s file defines `template`.
pub fn has_template(paths: &Paths, herd: &str, template: &str) -> Result<bool> {
    Ok(load(&paths.herds, herd)?.template.contains_key(template))
}

/// The repo a herd's file names, with a leading `~` expanded, even while a
/// value in it is invalid (an unfilled queue source); a file that doesn't
/// parse is still an error.
pub fn repo(paths: &Paths, herd: &str) -> Result<PathBuf> {
    let file = file_of(&paths.herds, herd)?;
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let parsed: HerdFile = toml::from_str(&text).with_context(|| file.display().to_string())?;
    Ok(expand_home(&parsed.repo, &paths.home))
}

/// A herd's lead's switches and sandbox mode, without reading its prompt.
pub fn lead_sandbox(paths: &Paths, herd: &str) -> Result<(Switches, SandboxMode)> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    Ok((lead.switches().over(&lead_defaults(paths)?), lead.sandbox))
}

/// The args a herd's lead starts with before the command line's, and where
/// they're set: `[lead].args`, else the herd's `args`.
pub fn lead_args(paths: &Paths, herd: &str) -> Result<(Vec<String>, String)> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    Ok(match lead.args {
        Some(args) => (args, "[lead].args".into()),
        None => (file.args, format!("herd {herd}'s args")),
    })
}

/// The MCP servers a herd's `[lead]` names, which its lead alone gets.
pub fn lead_mcp_servers(paths: &Paths, herd: &str) -> Result<Vec<String>> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    Ok(lead.mcp_servers)
}

/// The Unix sockets a herd's `[lead]` opens to its lead; `None` is none.
pub fn lead_unix_sockets(paths: &Paths, herd: &str) -> Result<Option<UnixSockets>> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    Ok(lead.unix_sockets)
}

/// A herd's `[worker]` switches: sandbox and Unix sockets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Worker {
    pub sandbox: WorkerSandbox,
    pub unix_sockets: Option<UnixSockets>,
}

/// A herd's `[worker]`; off with no Unix sockets when the herd has none.
pub fn worker(paths: &Paths, herd: &str) -> Result<Worker> {
    let file = load(&paths.herds, herd).context("herd")?;
    let worker = file.worker.unwrap_or_default();
    Ok(Worker {
        sandbox: worker.sandbox,
        unix_sockets: worker.unix_sockets,
    })
}

/// A herd's lead's switches, without reading its prompt.
pub fn lead_switches(paths: &Paths, herd: &str) -> Result<Switches> {
    Ok(lead_sandbox(paths, herd)?.0)
}

/// How `herd`'s lead is kept from merging: `None` without a `[lead]`, or
/// with `merge` on, which ignores `merge_guard`.
pub fn merge_guard(paths: &Paths, herd: &str) -> Result<Option<MergeGuard>> {
    let file = load(&paths.herds, herd).context("herd")?;
    let Some(lead) = file.lead else {
        return Ok(None);
    };
    let merge = lead.switches().over(&lead_defaults(paths)?).merge;
    Ok((!merge).then_some(lead.merge_guard))
}

/// The repo whose merges the human hears of while `herd`'s lead runs: one
/// whose `[lead]` has `merge` off and guards it by hook; else `None`.
pub fn merge_alerts(paths: &Paths, herd: &str) -> Result<Option<PathBuf>> {
    Ok(match merge_guard(paths, herd)? {
        Some(MergeGuard::Hook) => Some(repo(paths, herd)?),
        Some(MergeGuard::Credentials) | None => None,
    })
}

/// What a herd's box in `loopherd view` shows from its herd file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// The `[queue]`'s `max`, when the herd has a queue.
    pub max: Option<usize>,
    /// The lead's switches, when the herd has a `[lead]`.
    pub switches: Option<Switches>,
    /// Why `config.toml`'s `[lead_defaults]` couldn't be read; every switch
    /// is then off, as `herds` shows and the lead's hook enforces.
    pub defaults_problem: Option<String>,
}

pub fn outline(paths: &Paths, herd: &str) -> Result<Outline> {
    let file = load(&paths.herds, herd)?;
    let (switches, defaults_problem) = match &file.lead {
        None => (None, None),
        Some(lead) => match lead_defaults(paths) {
            Ok(defaults) => (Some(lead.switches().over(&defaults)), None),
            Err(e) => (Some(Switches::default()), Some(format!("{e:#}"))),
        },
    };
    Ok(Outline {
        max: file.queue.map(|q| q.max as usize),
        switches,
        defaults_problem,
    })
}

fn lead_defaults(paths: &Paths) -> Result<SwitchValues> {
    let file = crate::config::read(&config_file(paths))?;
    Ok(file.lead_defaults.unwrap_or_default())
}

impl LeadFile {
    fn switches(&self) -> SwitchValues {
        SwitchValues {
            merge: self.merge,
            install: self.install,
            rebase: self.rebase,
            new_cards: self.new_cards,
            design_escalation: self.design_escalation,
            blocked_dialogs: self.blocked_dialogs,
            review_rounds: self.review_rounds,
        }
    }
}

fn check_lead_prompt(prompt: &Prompt) -> Result<()> {
    if prompt.text.trim().is_empty() {
        bail!("{} is empty", prompt.source);
    }
    pattern(Some(&prompt.text), &[], &LEAD_KEYS).context(prompt.source.clone())?;
    Ok(())
}

/// `prompt`, else `prompt_file` read relative to the herds directory.
fn read_prompt(
    prompt: &Option<String>,
    prompt_file: &Option<String>,
    paths: &Paths,
) -> Result<Prompt> {
    Ok(match (prompt, prompt_file) {
        (Some(text), _) => Prompt {
            source: "prompt".into(),
            text: text.clone(),
        },
        (None, Some(file)) => {
            let path = paths.herds.join(expand_home(file, &paths.home));
            Prompt {
                source: format!("prompt_file {}", path.display()),
                text: std::fs::read_to_string(&path)
                    .with_context(|| format!("prompt_file: read {}", path.display()))?,
            }
        }
        (None, None) => unreachable!("load rejects a prompt-less template or lead"),
    })
}

/// The warning for an enabled `[notify]` table in `config` that sends nothing,
/// shared by `herds` and `doctor`.
pub fn sends_nothing_warning(config: &Path) -> String {
    format!(
        "{}: [notify] is enabled with herdr = false, no command and no pane_label, \
         so nothing is sent",
        config.display()
    )
}

/// Whether `config` reads as TOML; a missing or unreadable file doesn't.
pub fn parses_as_toml(config: &Path) -> bool {
    std::fs::read_to_string(config).is_ok_and(|text| text.parse::<toml::Table>().is_ok())
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// Every herd file in `paths.herds`, sorted by herd name, with a usage line
/// per template. Exits 1 if `config.toml` or any herd file is invalid; warns,
/// without changing the exit, when panes are labelled but Claude's `settings`
/// lack the prompt hook that clears a label.
pub fn herds(paths: &Paths, settings: &Path) -> Result<(String, i32)> {
    let mut lines = Vec::new();
    let mut code = 0;
    let defaults = match crate::config::read(&config_file(paths)) {
        Ok(file) => Some(file.lead_defaults.unwrap_or_default()),
        Err(e) => {
            lines.push(format!("invalid {}", format!("{e:#}").trim_end()));
            code = 1;
            None
        }
    };
    let config = config_file(paths);
    let mut hook_warning_at = None;
    if parses_as_toml(&config) {
        match crate::notify::load(&config) {
            Err(e) => {
                lines.push(format!("invalid {}", format!("{e:#}").trim_end()));
                code = 1;
            }
            Ok(Some(notify)) if notify.sends_nothing() => {
                lines.push(format!("warning: {}", sends_nothing_warning(&config)))
            }
            Ok(Some(notify)) if notify.labels_panes() => hook_warning_at = Some(lines.len()),
            Ok(_) => {}
        }
    }
    let files = herd_files(&paths.herds)?;
    if files.is_empty() {
        lines.push(format!(
            "no herds: add a herd file at {}/<herd>.toml",
            paths.herds.display()
        ));
    }
    let mut valid = Vec::new();
    for file in &files {
        let herd = file.file_stem().unwrap_or_default().to_string_lossy();
        match checked(file, paths) {
            Ok(parsed) => {
                let reading = crate::pause::read(&paths.paused, &herd);
                let paused = match reading.pause() {
                    Some(pause) => format!("paused {}", pause.when()),
                    None => "paused no".into(),
                };
                lines.extend(describe(&herd, &parsed, &paused, defaults.as_ref()));
                lines.extend(reading.warning(&herd).map(|w| format!("  {w}")));
                valid.push(herd.into_owned());
            }
            Err(invalid) => {
                lines.push(invalid);
                code = 1;
            }
        }
    }
    if let Some(at) = hook_warning_at
        && let Some(warning) = prompt_hook_warning(&config, settings, &valid)
    {
        lines.insert(at, warning);
    }
    Ok((lines.join("\n"), code))
}

/// A warning when `settings` lacks loopherd's prompt hook, or can't be read,
/// without which a question's pane label stays until the pane's next session start.
fn prompt_hook_warning(config: &Path, settings: &Path, herds: &[String]) -> Option<String> {
    let problem = match crate::install_hook::prompt_hook_installed(settings) {
        Ok(true) => return None,
        Ok(false) => format!("loopherd's prompt hook is not in {}", settings.display()),
        Err(e) => format!(
            "loopherd's prompt hook can't be checked ({})",
            format!("{e:#}").trim_end()
        ),
    };
    let affected = match herds {
        [] => String::new(),
        [herd] => format!(" in herd {herd}"),
        _ => format!(" in herds {}", herds.join(", ")),
    };
    Some(format!(
        "warning: {}: [notify] pane_label is on but {problem}, \
         so a pane label{affected} stays until its pane's next session; \
         run scripts/install or loopherd install-hook, \
         or pass herds --settings <file> if you installed into another file",
        config.display(),
    ))
}

/// `herd`'s repo, with a leading `~` expanded, once its file passes the checks
/// `herds` runs; otherwise the line `herds` prints for it.
pub fn checked_repo(paths: &Paths, herd: &str) -> std::result::Result<PathBuf, String> {
    let file = file_of(&paths.herds, herd).map_err(|e| format!("{e:#}"))?;
    checked(&file, paths).map(|parsed| expand_home(&parsed.repo, &paths.home))
}

/// `file` read and checked, or `herds`' line calling it invalid.
fn checked(file: &Path, paths: &Paths) -> std::result::Result<HerdFile, String> {
    std::fs::read_to_string(file)
        .map_err(anyhow::Error::from)
        .and_then(|text| {
            let herd = file.file_stem().unwrap_or_default().to_string_lossy();
            check(&herd, &text, paths)
        })
        .map_err(|e| {
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            format!("invalid {name}: {}", format!("{e:#}").trim_end())
        })
}

/// `text` parsed as a herd file and checked as far as `add` and `lead` would
/// before knowing any params.
fn check(herd: &str, text: &str, paths: &Paths) -> Result<HerdFile> {
    let parsed = parse(text)?;
    for (name, template) in &parsed.template {
        check_template(name, template, paths)?;
    }
    if let Some(lead) = &parsed.lead {
        let prompt = read_prompt(&lead.prompt, &lead.prompt_file, paths);
        prompt.and_then(|p| check_lead_prompt(&p)).context("lead")?;
        check_mcp_servers(&lead.mcp_servers).context("lead")?;
        // A broken config.toml is reported on its own; judge the herd with merge off.
        let merge = lead
            .switches()
            .over(&lead_defaults(paths).unwrap_or_default())
            .merge;
        if !merge
            && lead.merge_guard == MergeGuard::Credentials
            && lead.unix_sockets == Some(UnixSockets::All)
        {
            bail!(
                "lead: can't start with merge_guard = \"credentials\": {}",
                credentials_with_sockets(herd)
            );
        }
    }
    Ok(parsed)
}

/// Why a credentials lead can't have every Unix socket open, and how to clear either switch.
pub fn credentials_with_sockets(herd: &str) -> String {
    format!(
        "unix_sockets = \"all\" opens your ssh agent, Git Credential Manager through WSL2 \
         interop and the Secret Service keyring to it, each of which acts as you; run loopherd \
         herd unset {herd} lead.unix_sockets, or loopherd herd set {herd} lead.merge_guard \
         '\"hook\"'"
    )
}

fn check_mcp_servers(servers: &[String]) -> Result<()> {
    for (at, server) in servers.iter().enumerate() {
        if server.trim().is_empty() {
            bail!("mcp_servers names a blank server");
        }
        if servers[..at].contains(server) {
            bail!("mcp_servers names {server:?} twice");
        }
    }
    Ok(())
}

/// The `*.toml` files in `dir` by herd name, skipping hidden ones; none if `dir` is missing.
fn herd_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        entries => entries.with_context(|| format!("read {}", dir.display()))?,
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        let hidden = path
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."));
        if !hidden && path.extension().is_some_and(|e| e == "toml") {
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_stem().cmp(&b.file_stem()));
    Ok(files)
}

/// `defaults` is `None` while `config.toml` is invalid.
fn describe(
    herd: &str,
    parsed: &HerdFile,
    paused: &str,
    defaults: Option<&SwitchValues>,
) -> Vec<String> {
    let workspace = parsed.workspace.as_deref().unwrap_or("(current)");
    let header = format!(
        "{herd}  repo {}  workspace {workspace}  {paused}",
        parsed.repo
    );
    let usages = parsed.template.iter().map(|(template, t)| {
        let params: String = t.params.iter().map(|p| format!(" <{p}>")).collect();
        let attended = if t.attended { "  (attended)" } else { "" };
        format!("  loopherd add {herd} {template}{params}{attended}")
    });
    let feed = parsed.queue.as_ref().map(|q| {
        let template = match &q.templates {
            Some(names) => format!("templates {}", names.join(", ")),
            None => format!("template {}", q.names().join(", ")),
        };
        format!("  loopherd feed {herd}  ({template}, max {})", q.max)
    });
    let backlog =
        parsed
            .queue
            .as_ref()
            .and_then(|q| q.backlog.as_ref())
            .map(|backlog| match backlog.trim().split_once('\n') {
                Some((first, _)) => format!("  backlog: {first} …"),
                None => format!("  backlog: {}", backlog.trim()),
            });
    let lead = parsed.lead.as_ref().into_iter().flat_map(|lead| {
        [
            format!("  loopherd lead {herd}"),
            format!("  switches: {}", switch_sources(&lead.switches(), defaults)),
            format!(
                "  review_rounds: {}",
                review_rounds_source(&lead.switches(), defaults)
            ),
            match lead.mcp_servers.is_empty() {
                true => "  mcp_servers: none".into(),
                false => format!("  mcp_servers: {}", lead.mcp_servers.join(", ")),
            },
            match lead.unix_sockets {
                Some(UnixSockets::All) => "  unix_sockets: all".into(),
                None => "  unix_sockets: none".into(),
            },
        ]
    });
    let workers = parsed.worker.as_ref().map(|worker| {
        let sockets = match worker.unix_sockets {
            Some(UnixSockets::All) => "all",
            None => "none",
        };
        format!(
            "  workers: sandbox {}, unix_sockets {sockets}",
            worker.sandbox.name()
        )
    });
    let after_merge = (!parsed.after_merge.is_empty())
        .then(|| format!("  after_merge: {}", parsed.after_merge.join("; ")));
    let unmerged = parsed
        .template
        .iter()
        .filter(|(_, t)| !parsed.after_merge.is_empty() && t.merged.is_none())
        .map(|(name, _)| {
            format!("  warning: template {name} has no merged check, so after_merge never runs for its members")
        });
    let queued_attended = parsed.queue.iter().flat_map(|q| q.names()).filter(|name| {
        parsed
            .template
            .get(name.as_str())
            .is_some_and(|t| t.attended)
    });
    let queued_attended = queued_attended.map(|name| {
        format!(
            "  warning: template {name} is attended, so feed never spawns it from [queue] templates"
        )
    });
    std::iter::once(header)
        .chain(usages)
        .chain(lead)
        .chain(workers)
        .chain(feed)
        .chain(backlog)
        .chain(after_merge)
        .chain(unmerged)
        .chain(queued_attended)
        .collect()
}

fn switch_sources(own: &SwitchValues, defaults: Option<&SwitchValues>) -> String {
    let Some(defaults) = defaults else {
        return "every switch off until config.toml is fixed".into();
    };
    let sources = own.values().into_iter().zip(defaults.values());
    SwitchValues::NAMES
        .iter()
        .zip(sources)
        .map(|(name, sources)| {
            let (on, source) = match sources {
                (Some(on), _) => (on, "herd file"),
                (None, Some(on)) => (on, "lead_defaults"),
                (None, None) => (false, "built-in"),
            };
            format!("{name} {} ({source})", on_off(on))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn review_rounds_source(own: &SwitchValues, defaults: Option<&SwitchValues>) -> String {
    let Some(defaults) = defaults else {
        return "unknown until config.toml is fixed".into();
    };
    let (rounds, source) = own.review_rounds_over(defaults);
    format!("{rounds} ({source})")
}

/// `text` parsed, failing on a placeholder that is neither a param nor in `built_ins`.
fn pattern(text: Option<&str>, params: &[String], built_ins: &[&str]) -> Result<Option<Template>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let pattern = Template::parse(text)?;
    pattern.check(
        params
            .iter()
            .map(String::as_str)
            .chain(built_ins.iter().copied()),
    )?;
    Ok(Some(pattern))
}

/// Placeholders every template gets, so no param may take their names.
const BUILT_INS: [&str; 5] = ["name", "herd", "branch", "worktree", "pane"];

/// Fails as `feed` does when `herd` has no herd file.
pub fn require(paths: &Paths, herd: &str) -> Result<()> {
    let file = file_of(&paths.herds, herd).context("herd")?;
    std::fs::metadata(&file)
        .with_context(|| format!("read {}", file.display()))
        .context("herd")?;
    Ok(())
}

/// The name of every herd with a herd file, sorted.
pub fn names(dir: &Path) -> Result<Vec<String>> {
    let files = herd_files(dir)?;
    let stems = files.iter().filter_map(|f| f.file_stem());
    Ok(stems.map(|s| s.to_string_lossy().into_owned()).collect())
}

/// Whether `herd` can name a herd file.
pub fn is_name(herd: &str) -> bool {
    !(herd.is_empty() || herd.starts_with('.') || herd.contains('/'))
}

fn file_of(dir: &Path, herd: &str) -> Result<PathBuf> {
    if !is_name(herd) {
        bail!("{herd:?} is not a herd name");
    }
    Ok(dir.join(format!("{herd}.toml")))
}

fn load(dir: &Path, herd: &str) -> Result<HerdFile> {
    let file = file_of(dir, herd)?;
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    parse(&text).with_context(|| file.display().to_string())
}

fn parse(text: &str) -> Result<HerdFile> {
    let parsed: HerdFile = toml::from_str(text)?;
    for (name, template) in &parsed.template {
        validate(template).with_context(|| format!("template {name}"))?;
    }
    if let Some(queue) = &parsed.queue {
        validate_queue(queue, &parsed.template).context("queue")?;
    }
    if let Some(lead) = &parsed.lead {
        one_prompt(&lead.prompt, &lead.prompt_file).context("lead")?;
        crate::context::check_tokens("context_limit", lead.context_limit).context("lead")?;
    }
    Ok(parsed)
}

impl QueueFile {
    fn names(&self) -> &[String] {
        match (&self.template, &self.templates) {
            (Some(one), _) => std::slice::from_ref(one),
            (None, Some(names)) => names,
            (None, None) => &[],
        }
    }
}

fn validate_queue(queue: &QueueFile, templates: &BTreeMap<String, TemplateFile>) -> Result<()> {
    match (&queue.template, &queue.templates) {
        (Some(_), Some(_)) => bail!("set one of template and templates, not both"),
        (None, None) => bail!("set one of template and templates"),
        (None, Some(names)) if names.is_empty() => bail!("templates is empty"),
        _ => {}
    }
    for name in queue.names() {
        if !templates.contains_key(name) {
            let known: Vec<&str> = templates.keys().map(String::as_str).collect();
            bail!("no template {name} (it has: {})", known.join(", "));
        }
    }
    if queue.max < 1 {
        bail!("max must be at least 1, got {}", queue.max);
    }
    if queue.source.trim().is_empty() {
        bail!("source is empty; set the shell command that prints the ready tasks");
    }
    if queue.backlog.as_ref().is_some_and(|b| b.trim().is_empty()) {
        bail!("backlog is empty; set the shell command that prints the unready tasks, or drop it");
    }
    Ok(())
}

fn one_prompt(prompt: &Option<String>, prompt_file: &Option<String>) -> Result<()> {
    match (prompt, prompt_file) {
        (Some(_), Some(_)) => bail!("set one of prompt and prompt_file, not both"),
        (None, None) => bail!("set one of prompt and prompt_file"),
        _ => Ok(()),
    }
}

fn validate(template: &TemplateFile) -> Result<()> {
    one_prompt(&template.prompt, &template.prompt_file)?;
    crate::context::check_tokens("context_limit", template.context_limit)?;
    if template.branch.is_some() && template.worktree.is_some() {
        bail!("set one of branch and worktree, not both");
    }
    for (i, param) in template.params.iter().enumerate() {
        if BUILT_INS.contains(&param.as_str()) {
            bail!("param {param} is a built-in placeholder");
        }
        if template.params[..i].contains(param) {
            bail!("param {param} appears twice");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;

    const PR_HERD: &str = r#"
repo = "/src/repo"
args = ["--permission-mode", "auto"]

[template.pr]
params = ["number"]
name = "zpr-{number}"
prompt = "You're {name} in {herd}. Run `gh pr checkout {number}`."
args = ["--model", "opus"]
"#;

    struct Config {
        dir: tempfile::TempDir,
    }

    impl Config {
        fn with(herds: &[(&str, &str)]) -> Config {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("herds")).unwrap();
            for (name, text) in herds {
                fs::write(dir.path().join(format!("herds/{name}.toml")), text).unwrap();
            }
            Config { dir }
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.dir.path().join("herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: self.dir.path().join("feed.lock"),
                paused: self.dir.path().join("paused"),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Claude's settings for this config; missing until a test writes it.
        fn settings(&self) -> PathBuf {
            self.dir.path().join("settings.json")
        }
    }

    fn request(herd: &str, template: &str, params: &[&str]) -> AddRequest {
        AddRequest {
            herd: herd.into(),
            template: template.into(),
            params: params.iter().map(|p| p.to_string()).collect(),
            claude_args: vec![],
            ready_timeout: Duration::from_secs(60),
            return_pane: None,
            focus: true,
            sandbox: || Ok(crate::sandbox::Sockets::Blocked),
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "my-app")])
    }

    #[test]
    fn adds_a_member_from_a_template_and_records_its_herd() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let Added { name, spawned, .. } = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1050"]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(name, "zpr-1050");
        assert_eq!(spawned.report(&name).1, 0);
        assert_eq!(
            ledger.0["zpr-1050"].params,
            Some([("number".to_string(), "1050".to_string())].into())
        );
        assert_eq!(herdr.tabs()[0].label, "zpr-1050");
        assert_eq!(herdr.tabs()[0].cwd, PathBuf::from("/src/repo"));
        assert_eq!(
            herdr.prompts(),
            vec![(
                "zpr-1050".into(),
                "You're zpr-1050 in prs. Run `gh pr checkout 1050`.".into()
            )]
        );
        let rec = &ledger.0["zpr-1050"];
        assert_eq!(rec.herd.as_deref(), Some("prs"));
        assert_eq!(rec.template.as_deref(), Some("pr"));
    }

    /// Herd `prs` in a git repo with an origin, whose template `pr` checks
    /// out `feature/existing`, with `[worker]` set to `worker`.
    fn worker_config(worker: &str) -> (Config, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = format!(
            "repo = {repo:?}\n[template.pr]\nparams = [\"number\"]\nname = \"zpr-{{number}}\"\n\
             branch = \"feature/existing\"\nprompt = \"go\"\nargs = [\"--model\", \"opus\"]\n\
             [worker]\n{worker}\n"
        );
        (Config::with(&[("prs", &herd)]), root)
    }

    fn unavailable() -> std::result::Result<crate::sandbox::Sockets, crate::sandbox::Unavailable> {
        Err(crate::sandbox::Unavailable {
            why: "bubblewrap isn't installed".into(),
            fix: "install bubblewrap".into(),
        })
    }

    #[test]
    fn add_starts_a_sandboxed_herds_worker_with_its_settings() {
        let (config, _repo) = worker_config("sandbox = \"require\"");
        let paths = config.paths();
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        add(
            &herdr,
            &mut ledger,
            &paths,
            &request("prs", "pr", &["1050"]),
            Some("w1"),
        )
        .unwrap();

        let settings = paths.config_dir().join("sandbox/zpr-1050.json");
        assert!(settings.is_file());
        let rec = &ledger.0["zpr-1050"];
        assert_eq!(
            rec.claude_args,
            [
                "--model",
                "opus",
                "--settings",
                &settings.display().to_string()
            ]
        );
        assert_eq!(rec.unsandboxed, None);
    }

    #[test]
    fn a_sandboxed_worker_whose_spawn_fails_leaves_no_settings_behind() {
        let (config, _repo) = worker_config("sandbox = \"require\"");
        let paths = config.paths();
        let mut herdr = herdr();
        herdr.startup = crate::herdr::fake::Startup::Fails;

        add(
            &herdr,
            &mut MemLedger::default(),
            &paths,
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            fs::read_dir(paths.config_dir().join("sandbox"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn add_refuses_a_required_worker_whose_sandbox_cant_start() {
        let (config, _repo) = worker_config("sandbox = \"require\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = AddRequest {
            sandbox: unavailable,
            ..request("prs", "pr", &["1050"])
        };

        let err = add(&herdr, &mut ledger, &config.paths(), &req, Some("w1")).unwrap_err();

        assert!(
            format!("{err:#}").contains("zpr-1050 can't start sandboxed"),
            "{err:#}"
        );
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn add_starts_a_preferred_worker_unsandboxed_flagged_and_warned() {
        let (config, _repo) = worker_config("sandbox = \"prefer\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = AddRequest {
            sandbox: unavailable,
            ..request("prs", "pr", &["1050"])
        };

        let added = add(&herdr, &mut ledger, &config.paths(), &req, Some("w1")).unwrap();

        let rec = &ledger.0["zpr-1050"];
        assert_eq!(
            rec.unsandboxed.as_deref(),
            Some("bubblewrap isn't installed")
        );
        assert!(!rec.claude_args.iter().any(|a| a == "--settings"));
        assert_eq!(
            added.warning.as_deref(),
            Some("zpr-1050 runs unsandboxed (bubblewrap isn't installed); install bubblewrap")
        );
    }

    fn queue_herd(max: i64) -> String {
        format!("{PR_HERD}\n[queue]\ntemplate = \"pr\"\nsource = \"true\"\nmax = {max}\n")
    }

    #[test]
    fn a_second_feed_lock_is_refused_until_the_first_is_dropped() {
        let config = Config::with(&[]);
        let paths = config.paths();
        let first = paths.lock_feed().unwrap();
        let err = paths.lock_feed().unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "feed, add or release already running; retry once it ends"
        );
        drop(first);
        paths.lock_feed().unwrap();
    }

    #[test]
    fn a_feed_lock_held_briefly_is_waited_for() {
        let config = Config::with(&[]);
        let paths = config.paths();
        let probe = paths.lock_feed().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(probe);
        });
        paths.lock_feed().unwrap();
        release.join().unwrap();
    }

    #[test]
    fn dropping_the_feed_lock_frees_it_while_a_spawning_thread_holds_a_copy() {
        let config = Config::with(&[]);
        let paths = config.paths();
        let first = paths.lock_feed().unwrap();
        // A child forked by another test thread holds this copy until it execs.
        let _inherited = first.try_clone().unwrap();
        drop(first);
        paths.lock_feed().unwrap();
    }

    #[test]
    fn add_into_a_queue_herd_is_refused_while_feed_runs() {
        let config = Config::with(&[("prs", &queue_herd(5))]);
        let _feed = config.paths().lock_feed().unwrap();
        let err = add_err(&config, &request("prs", "pr", &["1"]));
        assert_eq!(
            err,
            "feed, add or release already running; retry once it ends"
        );
    }

    #[test]
    fn add_into_a_herd_without_a_queue_ignores_a_running_feed() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let _feed = config.paths().lock_feed().unwrap();
        let (herdr, _, name) = add_ok(&config, &request("prs", "pr", &["1"]));
        assert_eq!(name, "zpr-1");
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn add_into_a_paused_herd_still_adds() {
        let config = Config::with(&[("prs", &queue_herd(5))]);
        let herds = crate::pause::Herds::One("prs");
        crate::pause::pause(&config.paths(), herds, None, std::time::SystemTime::now()).unwrap();
        let (herdr, _, name) = add_ok(&config, &request("prs", "pr", &["1"]));
        assert_eq!(name, "zpr-1");
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn add_past_a_queue_max_still_adds_with_a_warning() {
        let config = Config::with(&[("prs", &queue_herd(1))]);
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let mut add_pr = |number| {
            add(
                &herdr,
                &mut ledger,
                &config.paths(),
                &request("prs", "pr", &[number]),
                Some("w1"),
            )
            .unwrap()
        };

        assert_eq!(add_pr("1").warning, None);
        let past = add_pr("2");

        assert_eq!(past.name, "zpr-2");
        assert_eq!(
            past.warning.as_deref(),
            Some("prs now has 2 members, past its queue max of 1")
        );
        assert_eq!(herdr.tabs().len(), 2);
    }

    #[test]
    fn a_retired_member_frees_its_queue_slot_for_add() {
        let config = Config::with(&[("prs", &queue_herd(1))]);
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let paths = config.paths();
        add(
            &herdr,
            &mut ledger,
            &paths,
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap();
        ledger.0.get_mut("zpr-1").unwrap().retired = true;

        let added = add(
            &herdr,
            &mut ledger,
            &paths,
            &request("prs", "pr", &["2"]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(added.warning, None);
    }

    fn add_err(config: &Config, req: &AddRequest) -> String {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let err = add(&herdr, &mut ledger, &config.paths(), req, Some("w1")).unwrap_err();
        assert!(herdr.tabs().is_empty(), "{err:#}");
        assert!(ledger.0.is_empty(), "{err:#}");
        format!("{err:#}")
    }

    #[test]
    fn a_missing_herd_file_is_an_error_naming_its_path() {
        let config = Config::with(&[]);
        let err = add_err(&config, &request("nope", "pr", &["1"]));
        let path = config.path().join("herds/nope.toml");
        assert!(
            err.starts_with(&format!("herd: read {}: ", path.display())),
            "{err}"
        );
    }

    #[test]
    fn a_herd_name_cannot_reach_outside_the_herds_directory() {
        let config = Config::with(&[]);
        fs::write(config.path().join("evil.toml"), PR_HERD).unwrap();
        for herd in ["../evil", "", "a/b"] {
            let err = add_err(&config, &request(herd, "pr", &["1"]));
            assert!(err.starts_with("herd: "), "{herd:?}: {err}");
            assert!(err.contains("not a herd name"), "{herd:?}: {err}");
        }
    }

    #[test]
    fn an_invalid_herd_file_is_an_error() {
        let template = "[template.pr]\nparams = []\nprompt = \"go\"\n";
        for (text, expected) in [
            ("repo = ".to_string(), "TOML parse error"),
            (
                format!("repo = \"/r\"\ncolour = 1\n{template}"),
                "unknown field `colour`",
            ),
            (
                format!("repo = \"/r\"\n{template}model = \"x\"\n"),
                "unknown field `model`",
            ),
            (template.to_string(), "missing field `repo`"),
            (
                "repo = \"/r\"\n[template.pr]\nprompt = \"go\"\n".to_string(),
                "missing field `params`",
            ),
            (
                format!("repo = \"/r\"\n{template}prompt_file = \"p.md\"\n"),
                "template pr: set one of prompt and prompt_file, not both",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = []\n".to_string(),
                "template pr: set one of prompt and prompt_file",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = [\"n\", \"n\"]\nprompt = \"go\"\n"
                    .to_string(),
                "template pr: param n appears twice",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = [\"name\"]\nprompt = \"go\"\n".to_string(),
                "template pr: param name is a built-in placeholder",
            ),
            (
                format!("repo = \"/r\"\n{template}branch = \"b\"\nworktree = \"w\"\n"),
                "template pr: set one of branch and worktree, not both",
            ),
        ] {
            let config = Config::with(&[("h", &text)]);
            let err = add_err(&config, &request("h", "pr", &[]));
            assert!(err.starts_with("herd: "), "{text:?}: {err}");
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn an_unknown_template_is_an_error_listing_the_known_ones() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let err = add_err(&config, &request("prs", "issue", &["1"]));
        assert_eq!(err, "template: prs has no template issue (it has: pr)");
    }

    #[test]
    fn a_wrong_param_count_fails_with_usage() {
        let config = Config::with(&[("prs", PR_HERD)]);
        for params in [&[][..], &["1", "2"]] {
            let err = add_err(&config, &request("prs", "pr", params));
            assert_eq!(
                err,
                format!(
                    "template pr takes 1 param (number), got {}\n\
                     usage: loopherd add <herd> <template> <param>…",
                    params.len()
                )
            );
        }
    }

    #[test]
    fn a_param_that_reaches_a_name_branch_or_worktree_must_be_a_plain_word() {
        let herd = "repo = \"/r\"\n\
             [template.named]\nparams = [\"n\"]\nname = \"z-{n}\"\nprompt = \"go\"\n\
             [template.default]\nparams = [\"n\"]\nprompt = \"go\"\n\
             [template.branched]\nparams = [\"n\"]\nname = \"z\"\nbranch = \"fix/{n}\"\nprompt = \"go\"\n\
             [template.detached]\nparams = [\"n\"]\nname = \"z\"\nworktree = \"review-{n}\"\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        for template in ["named", "default", "branched", "detached"] {
            for value in ["..", "a/b", "../lead-h", "-x", ".hidden", "a b", ""] {
                let err = add_err(&config, &request("h", template, &[value]));
                assert_eq!(
                    err,
                    format!(
                        "template {template}: param n is {value:?}; a param in a name, branch or \
                         worktree must match [A-Za-z0-9][A-Za-z0-9._-]* without \"..\""
                    )
                );
            }
        }
    }

    #[test]
    fn a_param_only_the_prompt_uses_may_hold_any_text() {
        let herd = "repo = \"/r\"\n[template.ask]\nparams = [\"n\", \"q\"]\nname = \"z-{n}\"\n\
                    prompt = \"{q}\"\n";
        let config = Config::with(&[("h", herd)]);
        let (herdr, _, name) = add_ok(&config, &request("h", "ask", &["v1_x-3", "../a/b c"]));
        assert_eq!(name, "z-v1_x-3");
        assert_eq!(herdr.prompts()[0].1, "../a/b c");
    }

    fn add_ok(config: &Config, req: &AddRequest) -> (FakeHerdr, MemLedger, String) {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let Added { name, .. } =
            add(&herdr, &mut ledger, &config.paths(), req, Some("w1")).unwrap();
        (herdr, ledger, name)
    }

    const GRILL_HERD: &str = "repo = \"/r\"\n[template.grill]\nparams = [\"key\"]\n\
                              prompt = \"grill {key}, then focus {shepherd_pane}\"\nattended = true\n";

    fn attended_request(return_pane: Option<&str>, focus: bool) -> AddRequest {
        AddRequest {
            return_pane: return_pane.map(String::from),
            focus,
            ..request("h", "grill", &["lh-1"])
        }
    }

    #[test]
    fn add_of_an_attended_template_records_its_return_pane_and_focuses_the_new_pane() {
        let config = Config::with(&[("h", GRILL_HERD)]);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let added = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &attended_request(Some("w1:p9"), true),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(herdr.focused(), [added.spawned.pane_id]);
        assert_eq!(herdr.prompts()[0].1, "grill lh-1, then focus w1:p9");
        let record = &ledger.entries().unwrap()["grill-lh-1"];
        assert_eq!(
            record.attended,
            Some(Attended {
                return_pane: Some("w1:p9".into())
            })
        );
    }

    #[test]
    fn add_of_an_attended_template_outside_herdr_records_no_return_pane() {
        let config = Config::with(&[("h", GRILL_HERD)]);
        let (herdr, ledger, name) = add_ok(&config, &attended_request(None, true));

        assert_eq!(herdr.prompts()[0].1, "grill lh-1, then focus ");
        let record = &ledger.entries().unwrap()[&name];
        assert_eq!(record.attended, Some(Attended { return_pane: None }));
    }

    #[test]
    fn add_of_an_attended_template_leaves_focus_alone_with_no_focus_or_when_blocked() {
        let config = Config::with(&[("h", GRILL_HERD)]);
        let (unfocused, _, _) = add_ok(&config, &attended_request(Some("w1:p9"), false));
        assert_eq!(unfocused.focused(), Vec::<String>::new());

        let mut blocked = herdr();
        blocked.startup = crate::herdr::fake::Startup::BlockedForever;
        let mut ledger = MemLedger::default();
        let req = AddRequest {
            ready_timeout: Duration::ZERO,
            ..attended_request(Some("w1:p9"), true)
        };
        let added = add(&blocked, &mut ledger, &config.paths(), &req, Some("w1")).unwrap();
        assert!(added.spawned.blocked);
        assert_eq!(blocked.focused(), Vec::<String>::new());
    }

    #[test]
    fn add_of_a_plain_template_neither_focuses_nor_marks_it_attended() {
        let herd = "repo = \"/r\"\n[template.card]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        let req = AddRequest {
            return_pane: Some("w1:p9".into()),
            ..request("h", "card", &[])
        };
        let (herdr, ledger, name) = add_ok(&config, &req);

        assert_eq!(herdr.focused(), Vec::<String>::new());
        assert_eq!(ledger.entries().unwrap()[&name].attended, None);
    }

    #[test]
    fn only_an_attended_template_may_use_shepherd_pane() {
        let herd =
            "repo = \"/r\"\n[template.card]\nparams = []\nprompt = \"back to {shepherd_pane}\"\n";
        let config = Config::with(&[("h", herd)]);

        let err = add_err(&config, &request("h", "card", &[]));

        assert!(err.contains("unknown placeholder {shepherd_pane}"), "{err}");
    }

    #[test]
    fn the_default_name_is_the_template_then_the_params() {
        let herd = "repo = \"/r\"\n[template.pair]\nparams = [\"a\", \"b\"]\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        let (_, _, name) = add_ok(&config, &request("h", "pair", &["x", "y"]));
        assert_eq!(name, "pair-x-y");
    }

    #[test]
    fn the_branch_renders_params_herd_and_name_into_a_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = format!(
            "repo = {:?}\n[template.pr]\nparams = [\"n\"]\nname = \"z-{{n}}\"\n\
             branch = \"{{herd}}/{{name}}/{{n}}\"\nprompt = \"on {{branch}} in {{worktree}}\"\n",
            repo.display().to_string()
        );
        let config = Config::with(&[("prs", &herd)]);
        let (herdr, ledger, _) = add_ok(&config, &request("prs", "pr", &["7"]));
        let wt = root.path().join("proj/worktrees/prs-z-7-7");
        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(
            herdr.prompts()[0].1,
            format!("on prs/z-7/7 in {}", wt.display())
        );
        assert_eq!(ledger.0["z-7"].worktree, Some(wt));
    }

    fn detached_herd(repo: &Path) -> String {
        format!(
            "repo = {:?}\n[template.pr]\nparams = [\"n\"]\nname = \"z-{{n}}\"\n\
             worktree = \"{{herd}}-review-{{name}}\"\nprompt = \"on [{{branch}}] in {{worktree}}\"\n",
            repo.display().to_string()
        )
    }

    #[test]
    fn a_worktree_pattern_adds_a_member_in_a_detached_worktree_at_origins_default_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let config = Config::with(&[("prs", &detached_herd(&repo))]);
        let branches_before = crate::worktree::testing::branches(&repo);

        let (herdr, ledger, _) = add_ok(&config, &request("prs", "pr", &["7"]));

        let wt = root.path().join("proj/worktrees/prs-review-z-7");
        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(herdr.prompts()[0].1, format!("on [] in {}", wt.display()));
        assert_eq!(ledger.0["z-7"].worktree, Some(wt.clone()));
        assert_eq!(ledger.0["z-7"].branch.as_deref(), Some(""));
        assert_eq!(crate::worktree::testing::current_branch(&wt), "");
        assert_eq!(
            crate::worktree::testing::head(&wt),
            crate::worktree::testing::head_of(&repo, "origin/main")
        );
        assert_eq!(crate::worktree::testing::branches(&repo), branches_before);
    }

    #[test]
    fn a_detached_member_starts_at_origins_current_default_not_a_stale_origin_head() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        crate::worktree::testing::move_origin_default(root.path(), "staging");
        let config = Config::with(&[("prs", &detached_herd(&repo))]);

        add_ok(&config, &request("prs", "pr", &["7"]));

        let wt = root.path().join("proj/worktrees/prs-review-z-7");
        assert_eq!(
            crate::worktree::testing::head(&wt),
            crate::worktree::testing::head_of(&repo, "origin/staging")
        );
    }

    #[test]
    fn a_detached_member_reuses_an_existing_worktree_at_its_path() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let config = Config::with(&[("prs", &detached_herd(&repo))]);
        let (_, _, _) = add_ok(&config, &request("prs", "pr", &["7"]));
        let wt = root.path().join("proj/worktrees/prs-review-z-7");
        fs::write(wt.join("work.txt"), "keep").unwrap();

        let herdr = herdr();
        let mut ledger = MemLedger::default();
        add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["7"]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(fs::read_to_string(wt.join("work.txt")).unwrap(), "keep");
    }

    #[test]
    fn dry_run_of_a_detached_member_shows_no_branch_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let config = Config::with(&[("prs", &detached_herd(&repo))]);

        let report = dry_run(
            &herdr(),
            &config.paths(),
            &request("prs", "pr", &["7"]),
            Some("w1"),
        )
        .unwrap();

        let wt = root.path().join("proj/worktrees/prs-review-z-7");
        assert!(
            report.starts_with(&format!(
                "name z-7\nbranch none\nworktree {}\n",
                wt.display()
            )),
            "{report}"
        );
        assert!(report.ends_with(&format!("on [] in {}", wt.display())));
        assert!(!root.path().join("proj/worktrees").exists());
    }

    #[test]
    fn unknown_placeholders_fail_before_anything_is_created() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        for (name, branch, prompt, message) in [
            (
                "z-{nmber}",
                "b",
                "go",
                "template pr: name: unknown placeholder {nmber}",
            ),
            (
                "z-{pane}",
                "b",
                "go",
                "template pr: name: unknown placeholder {pane}",
            ),
            (
                "z",
                "b-{worktree}",
                "go",
                "template pr: branch: unknown placeholder {worktree}",
            ),
            (
                "z",
                "b",
                "hi {nmber}",
                "template pr: prompt: unknown placeholder {nmber}",
            ),
        ] {
            let herd = format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\"]\n\
                 name = {name:?}\nbranch = {branch:?}\nprompt = {prompt:?}\n",
                repo.display().to_string()
            );
            let config = Config::with(&[("prs", &herd)]);
            let err = add_err(&config, &request("prs", "pr", &["1"]));
            assert_eq!(err, message, "{herd}");
            assert!(!root.path().join("proj/worktrees").exists());
        }
    }

    #[test]
    fn a_leading_tilde_in_repo_is_the_home_directory() {
        let herd = "repo = \"~/src/repo\"\n[template.t]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        let (herdr, ledger, _) = add_ok(&config, &request("h", "t", &[]));
        assert_eq!(herdr.tabs()[0].cwd, PathBuf::from("/home/me/src/repo"));
        assert_eq!(ledger.0["t"].repo, PathBuf::from("/home/me/src/repo"));
    }

    #[test]
    fn prompt_file_is_read_relative_to_the_herd_file_and_rendered() {
        let herd = "repo = \"/r\"\n[template.t]\nparams = [\"n\"]\nprompt_file = \"t.md\"\n";
        let config = Config::with(&[("h", herd)]);
        fs::write(config.path().join("herds/t.md"), "{name} takes {n}\n").unwrap();
        let (herdr, _, _) = add_ok(&config, &request("h", "t", &["5"]));
        assert_eq!(herdr.prompts()[0].1, "t-5 takes 5\n");
    }

    #[test]
    fn a_missing_prompt_file_fails_before_anything_is_created() {
        let herd = "repo = \"/r\"\n[template.t]\nparams = []\nprompt_file = \"t.md\"\n";
        let config = Config::with(&[("h", herd)]);
        let err = add_err(&config, &request("h", "t", &[]));
        let path = config.path().join("herds/t.md");
        assert!(
            err.starts_with(&format!(
                "template t: prompt_file: read {}: ",
                path.display()
            )),
            "{err}"
        );
    }

    #[test]
    fn an_empty_prompt_fails_before_anything_is_created() {
        for prompt in ["prompt = \" \\n\"", "prompt_file = \"t.md\""] {
            let herd = format!("repo = \"/src/repo\"\n[template.t]\nparams = []\n{prompt}\n");
            let config = Config::with(&[("h", &herd)]);
            let path = config.path().join("herds/t.md");
            fs::write(&path, "\n").unwrap();
            // A clash on the name would surface if herdr were asked first.
            let herdr = herdr().with_agent("t");
            let mut ledger = MemLedger::default();
            let req = request("h", "t", &[]);
            let err = add(&herdr, &mut ledger, &config.paths(), &req, Some("w1")).unwrap_err();
            let source = if prompt.starts_with("prompt_file") {
                format!("prompt_file {}", path.display())
            } else {
                "prompt".into()
            };
            assert_eq!(format!("{err:#}"), format!("template t: {source} is empty"));
            assert!(herdr.tabs().is_empty());
            assert!(ledger.0.is_empty());
        }
    }

    #[test]
    fn claude_args_are_the_herds_then_the_templates_then_the_command_lines() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let req = AddRequest {
            claude_args: vec!["--verbose".into()],
            ..request("prs", "pr", &["1"])
        };
        let (herdr, ledger, _) = add_ok(&config, &req);
        let expected = ["--permission-mode", "auto", "--model", "opus", "--verbose"];
        assert_eq!(herdr.started()[0].args, expected);
        assert_eq!(ledger.0["zpr-1"].claude_args, expected);
    }

    #[test]
    fn the_workspace_is_the_herds_label_or_else_the_current_one() {
        let labeled = PR_HERD.replace(
            "args = [\"--permission",
            "workspace = \"my-app\"\nargs = [\"--permission",
        );
        let config = Config::with(&[("prs", PR_HERD), ("zag", &labeled)]);
        let (herdr, _, _) = add_ok(&config, &request("prs", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w1");
        let (herdr, ledger, _) = add_ok(&config, &request("zag", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w2");
        assert_eq!(ledger.0["zpr-1"].workspace_label.as_deref(), Some("my-app"));
    }

    #[test]
    fn a_workspace_label_matching_no_workspace_fails_before_anything_is_created() {
        let herd = PR_HERD.replace(
            "args = [\"--permission",
            "workspace = \"nope\"\nargs = [\"--permission",
        );
        let config = Config::with(&[("prs", &herd)]);
        let err = add_err(&config, &request("prs", "pr", &["1"]));
        assert_eq!(err, "workspace: no workspace labeled \"nope\"");
    }

    #[test]
    fn spawns_checks_still_apply_to_members() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let herdr = herdr().with_agent("zpr-1");
        let mut ledger = MemLedger::default();
        let err = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("already a live agent"),
            "{err:#}"
        );
        assert!(herdr.tabs().is_empty());

        let err = add_err(&config, &request("prs", "pr", &["Bad"]));
        assert!(err.starts_with("name: "), "{err}");
    }

    #[test]
    fn a_member_blocked_at_startup_exits_3_and_a_failed_start_closes_its_tab() {
        use crate::herdr::fake::Startup;
        let config = Config::with(&[("prs", PR_HERD)]);
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;
        let mut ledger = MemLedger::default();
        let Added { name, spawned, .. } = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap();
        assert_eq!(spawned.report(&name).1, 3);
        assert!(!ledger.0["zpr-1"].prompt_sent);

        let mut herdr = self::herdr();
        herdr.startup = Startup::Fails;
        let mut ledger = MemLedger::default();
        let err = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").starts_with("start: "), "{err:#}");
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn dry_run_prints_the_resolved_member_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = format!(
            "repo = {:?}\nworkspace = \"my-app\"\nargs = [\"--model\", \"opus\"]\n\
             [template.pr]\nparams = [\"n\"]\nname = \"z-{{n}}\"\nbranch = \"pr-{{n}}\"\n\
             prompt = \"{{name}} on {{branch}} in {{worktree}}, pane {{pane}}\"\n",
            repo.display().to_string()
        );
        let config = Config::with(&[("prs", &herd)]);
        let herdr = herdr();
        let req = AddRequest {
            claude_args: vec!["--add-dir".into(), "/tmp/a b".into()],
            ..request("prs", "pr", &["7"])
        };

        let report = dry_run(&herdr, &config.paths(), &req, Some("w1")).unwrap();

        let wt = root.path().join("proj/worktrees/pr-7");
        assert_eq!(
            report,
            format!(
                "name z-7\nbranch pr-7\nworktree {wt}\nworkspace my-app\n\
                 args --model opus --add-dir \"/tmp/a b\"\nprompt\n\
                 z-7 on pr-7 in {wt}, pane {{pane}}",
                wt = wt.display()
            )
        );
        assert!(herdr.tabs().is_empty());
        assert!(herdr.started().is_empty());
        assert!(herdr.prompts().is_empty());
        assert!(!root.path().join("proj/worktrees").exists());
    }

    #[test]
    fn dry_run_without_a_branch_runs_in_the_repo_and_still_checks_the_workspace() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let report = dry_run(
            &herdr(),
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap();
        assert!(
            report.starts_with("name zpr-1\nbranch none\nworktree none\nworkspace loopherd\n"),
            "{report}"
        );
        let err = dry_run(
            &herdr(),
            &config.paths(),
            &request("prs", "pr", &["1"]),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").starts_with("workspace: "), "{err:#}");
    }

    #[test]
    fn herds_lists_each_herd_with_a_usage_line_per_template_sorted_by_name() {
        let issues = "repo = \"~/src/z\"\nworkspace = \"my-app\"\n\
                      [template.issue]\nparams = [\"key\", \"slug\"]\nprompt = \"go\"\n\
                      [template.chat]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("zissues", issues), ("prs", PR_HERD)]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(
            report,
            "prs  repo /src/repo  workspace (current)  paused no\n\
             \x20 loopherd add prs pr <number>\n\
             zissues  repo ~/src/z  workspace my-app  paused no\n\
             \x20 loopherd add zissues chat\n\
             \x20 loopherd add zissues issue <key> <slug>"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_lists_a_herds_after_merge_and_warns_of_templates_with_no_merged_check() {
        let herd = "repo = \"/r\"\nafter_merge = [\"git pull --ff-only\", \"scripts/install\"]\n\
                    [template.card]\nparams = []\nprompt = \"go\"\nmerged = \"exit 0\"\n\
                    [template.chat]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("prs", herd)]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let lines: Vec<&str> = report.lines().skip(3).collect();
        assert_eq!(
            lines,
            [
                "  after_merge: git pull --ff-only; scripts/install",
                "  warning: template chat has no merged check, so after_merge never runs for its members",
            ]
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_marks_an_attended_template_and_warns_when_its_queue_lists_it() {
        let herd = "repo = \"/r\"\n\
                    [queue]\ntemplates = [\"card\", \"grill\"]\nsource = \"true\"\nmax = 2\n\
                    [template.card]\nparams = [\"key\"]\nprompt = \"go\"\n\
                    [template.grill]\nparams = [\"key\"]\nprompt = \"go\"\nattended = true\n";
        let config = Config::with(&[("prs", herd)]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let lines: Vec<&str> = report.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "  loopherd add prs card <key>",
                "  loopherd add prs grill <key>  (attended)",
                "  loopherd feed prs  (templates card, grill, max 2)",
                "  warning: template grill is attended, so feed never spawns it from [queue] templates",
            ]
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_shows_an_unreadable_pause_as_paused_with_its_time_unknown() {
        let config = Config::with(&[("prs", "repo = \"/r\"\n")]);
        fs::create_dir(config.path().join("paused")).unwrap();
        fs::write(config.path().join("paused/prs"), "").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(
            lines[0],
            "prs  repo /r  workspace (current)  paused (time unknown)"
        );
        let warning = "  warning: prs's pause is unreadable, counted as paused: parse ";
        assert!(lines[1].starts_with(warning), "{report}");
        assert_eq!(lines.len(), 2, "{report}");
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_sorts_by_herd_name_not_file_name() {
        let config = Config::with(&[("prs-old", PR_HERD), ("prs", PR_HERD)]);
        let (report, _) = herds(&config.paths(), &config.settings()).unwrap();
        let headers: Vec<&str> = report.lines().filter(|l| !l.starts_with(' ')).collect();
        assert_eq!(
            headers[0],
            "prs  repo /src/repo  workspace (current)  paused no"
        );
        assert!(headers[1].starts_with("prs-old  "), "{report}");
    }

    #[test]
    fn herds_reports_invalid_files_lists_the_rest_and_exits_1() {
        let config = Config::with(&[
            ("a-broken", "repo = "),
            ("prs", PR_HERD),
            ("z-bad", "repo = \"/r\"\n[template.t]\nparams = []\n"),
        ]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let lines: Vec<&str> = report.lines().collect();
        assert!(
            lines[0].starts_with("invalid a-broken.toml: TOML parse error"),
            "{report}"
        );
        assert!(!lines[0].contains(&config.path().display().to_string()));
        assert_eq!(
            lines[lines.len() - 3..],
            [
                "prs  repo /src/repo  workspace (current)  paused no",
                "  loopherd add prs pr <number>",
                "invalid z-bad.toml: template t: set one of prompt and prompt_file",
            ]
        );
        assert_eq!(code, 1);
    }

    fn herds_line(template: &str, files: &[(&str, &str)]) -> (String, i32) {
        let herd = format!("repo = \"/r\"\n[template.t]\nparams = [\"n\"]\n{template}\n");
        let config = Config::with(&[("h", &herd)]);
        for (name, text) in files {
            fs::write(config.path().join("herds").join(name), text).unwrap();
        }
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        let report = report.replace(&config.path().display().to_string(), "CONFIG");
        (report, code)
    }

    #[test]
    fn herds_reports_templates_add_would_reject() {
        for (template, files, message) in [
            (
                "name = \"z-{nmber}\"\nprompt = \"go\"",
                &[][..],
                "name: unknown placeholder {nmber}",
            ),
            (
                "name = \"z-{pane}\"\nprompt = \"go\"",
                &[],
                "name: unknown placeholder {pane}",
            ),
            (
                "branch = \"b-{worktree}\"\nprompt = \"go\"",
                &[],
                "branch: unknown placeholder {worktree}",
            ),
            (
                "worktree = \"w-{branch}\"\nprompt = \"go\"",
                &[],
                "worktree: unknown placeholder {branch}",
            ),
            (
                "branch = \"b\"\nworktree = \"w\"\nprompt = \"go\"",
                &[],
                "set one of branch and worktree, not both",
            ),
            (
                "branch = \"b-{n\"\nprompt = \"go\"",
                &[],
                "branch: unclosed placeholder {n",
            ),
            (
                "prompt = \"hi {nmber}\"",
                &[],
                "prompt: unknown placeholder {nmber}",
            ),
            (
                "prompt_file = \"t.md\"",
                &[("t.md", "{n} in {wrktree}\n")],
                "prompt_file CONFIG/herds/t.md: unknown placeholder {wrktree}",
            ),
            (
                "prompt_file = \"t.md\"",
                &[],
                "prompt_file: read CONFIG/herds/t.md: No such file or directory (os error 2)",
            ),
            ("prompt = \" \\n\"", &[], "prompt is empty"),
            (
                "prompt = \"go\"\ndone = \"gh pr view {nmber}\"",
                &[],
                "done: unknown placeholder {nmber}",
            ),
            (
                "prompt = \"go\"\nmerged = \"gh pr list --head {brnch}\"",
                &[],
                "merged: unknown placeholder {brnch}",
            ),
            (
                "prompt_file = \"t.md\"",
                &[("t.md", "\n")],
                "prompt_file CONFIG/herds/t.md is empty",
            ),
        ] {
            let (report, code) = herds_line(template, files);
            assert_eq!(report, format!("invalid h.toml: template t: {message}"));
            assert_eq!(code, 1);
        }
    }

    #[test]
    fn herds_accepts_every_placeholder_add_accepts() {
        let template = "name = \"z-{herd}-{n}\"\nbranch = \"{herd}/{name}/{n}\"\n\
                        prompt_file = \"t.md\"\n\
                        done = \"{name} {herd} {branch} {worktree} {pane} {n} {{x}}\"\n\
                        merged = \"{name} {branch} {worktree} {n}\"";
        let prompt = "{name} {herd} {branch} {worktree} {pane} {n} {{x}}";
        let (report, code) = herds_line(template, &[("t.md", prompt)]);
        assert_eq!(
            report,
            "h  repo /r  workspace (current)  paused no\n  loopherd add h t <n>"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_lists_a_detached_template_like_any_other() {
        let template = "name = \"z-{n}\"\nworktree = \"{herd}-review-{name}-{n}\"\n\
                        prompt = \"{branch} {worktree}\"";
        let (report, code) = herds_line(template, &[]);
        assert_eq!(
            report,
            "h  repo /r  workspace (current)  paused no\n  loopherd add h t <n>"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn a_worktree_pattern_with_an_unknown_placeholder_fails_before_anything_is_created() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = detached_herd(&repo).replace("{herd}-review", "{nmber}-review");
        let config = Config::with(&[("prs", &herd)]);
        let err = add_err(&config, &request("prs", "pr", &["1"]));
        assert_eq!(err, "template pr: worktree: unknown placeholder {nmber}");
        assert!(!root.path().join("proj/worktrees").exists());
    }

    #[test]
    fn herds_with_no_herd_files_hints_at_the_directory_and_exits_0() {
        let config = Config::with(&[]);
        fs::write(config.path().join("herds/notes.md"), "").unwrap();
        fs::write(config.path().join("herds/.draft.toml"), "repo = ").unwrap();
        let missing = Paths {
            herds: config.path().join("nope/herds"),
            ..config.paths()
        };
        for paths in [config.paths(), missing] {
            let (report, code) = herds(&paths, &config.settings()).unwrap();
            assert_eq!(
                report,
                format!(
                    "no herds: add a herd file at {}/<herd>.toml",
                    paths.herds.display()
                )
            );
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn herds_reports_an_invalid_config_toml_and_exits_1() {
        for herds_files in [&[("prs", PR_HERD)][..], &[]] {
            let config = Config::with(herds_files);
            let file = config.path().join("config.toml");
            fs::write(&file, "context_windw = 1000000\n").unwrap();

            let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

            let first = report.lines().next().unwrap();
            assert!(
                first.starts_with(&format!("invalid {}: ", file.display())),
                "{report}"
            );
            assert!(report.contains("context_windw"), "{report}");
            assert_eq!(code, 1, "{report}");
        }
        let config = Config::with(&[("prs", PR_HERD)]);
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(!report.contains("invalid"), "{report}");
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_checks_the_shepherds_self_restart_switch() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(&file, "[shepherd]\nself_restart = \"on\"\n").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert!(
            report.starts_with(&format!("invalid {}: ", file.display())),
            "{report}"
        );
        assert!(report.contains("self_restart"), "{report}");
        assert_eq!(code, 1, "{report}");

        fs::write(&file, "[shepherd]\nself_restart = true\n").unwrap();
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(!report.contains("invalid"), "{report}");
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_checks_the_herdr_plugin_table() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(&file, "[herdr_plugin]\nrevive_on_startup = 1\n").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert!(
            report.starts_with(&format!("invalid {}: ", file.display())),
            "{report}"
        );
        assert!(report.contains("revive_on_startup"), "{report}");
        assert_eq!(code, 1, "{report}");

        fs::write(&file, "[herdr_plugin]\nrevive_on_startup = true\n").unwrap();
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(!report.contains("invalid"), "{report}");
        assert_eq!(code, 0, "{report}");
    }

    #[test]
    fn herds_warns_of_an_enabled_notify_table_that_sends_nothing() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(&file, "[notify]\nenabled = true\nherdr = false\n").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert!(
            report.starts_with(&format!(
                "warning: {}: [notify] is enabled with herdr = false, no command and no pane_label",
                file.display()
            )),
            "{report}"
        );
        assert_eq!(code, 0, "{report}");
    }

    #[test]
    fn herds_warns_when_pane_label_is_on_without_the_prompt_hook_and_still_exits_0() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(&file, "[notify]\nenabled = true\npane_label = true\n").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let warning = report.lines().next().unwrap();
        assert!(
            warning.starts_with(&format!(
                "warning: {}: [notify] pane_label is on but loopherd's prompt hook is not in {}",
                file.display(),
                config.settings().display()
            )),
            "{report}"
        );
        assert!(warning.contains(" in herd prs "), "{report}");
        assert!(
            warning.contains(
                "run scripts/install or loopherd install-hook, or pass herds --settings <file>"
            ),
            "{report}"
        );
        assert!(report.contains("loopherd add prs"), "{report}");
        assert_eq!(code, 0, "{report}");
    }

    #[test]
    fn herds_names_only_valid_herds_in_the_prompt_hook_warning() {
        let config = Config::with(&[("a", PR_HERD), ("b", PR_HERD), ("bad", "repo = ")]);
        fs::write(
            config.path().join("config.toml"),
            "[notify]\nenabled = true\npane_label = true\n",
        )
        .unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let warning = report.lines().next().unwrap();
        assert!(warning.contains(" in herds a, b stays "), "{report}");
        assert!(report.contains("invalid bad.toml"), "{report}");
        assert_eq!(code, 1, "{report}");
    }

    #[test]
    fn herds_says_nothing_of_the_prompt_hook_once_it_is_installed_or_when_nothing_labels_panes() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        for notify in [
            "[notify]\nenabled = true\n",
            "[notify]\nenabled = false\npane_label = true\n",
        ] {
            fs::write(&file, notify).unwrap();
            let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
            assert!(!report.contains("prompt hook"), "{notify}: {report}");
            assert_eq!(code, 0, "{report}");
        }

        fs::write(&file, "[notify]\nenabled = true\npane_label = true\n").unwrap();
        crate::install_hook::install(&config.settings(), Path::new("/bin/loopherd"), UNIX_EPOCH)
            .unwrap();
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(!report.contains("warning"), "{report}");
        assert_eq!(code, 0, "{report}");
    }

    #[test]
    fn herds_warns_and_still_exits_0_when_claudes_settings_cannot_be_read() {
        let config = Config::with(&[("prs", PR_HERD)]);
        fs::write(
            config.path().join("config.toml"),
            "[notify]\nenabled = true\npane_label = true\n",
        )
        .unwrap();
        fs::write(config.settings(), "{ not json").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let warning = report.lines().next().unwrap();
        assert!(
            warning.contains(&format!(
                "[notify] pane_label is on but loopherd's prompt hook can't be checked (parse {}",
                config.settings().display()
            )),
            "{report}"
        );
        assert!(warning.contains(" in herd prs "), "{report}");
        assert!(
            warning.contains("run scripts/install or loopherd install-hook"),
            "{report}"
        );
        assert_eq!(code, 0, "{report}");
    }

    #[test]
    fn herds_takes_a_notify_table_that_only_labels_panes_and_refuses_a_non_bool_pane_label() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(
            &file,
            "[notify]\nenabled = true\nherdr = false\npane_label = true\n",
        )
        .unwrap();
        crate::install_hook::install(&config.settings(), Path::new("/bin/loopherd"), UNIX_EPOCH)
            .unwrap();
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(!report.contains("warning"), "{report}");
        assert_eq!(code, 0, "{report}");

        fs::write(&file, "[notify]\nenabled = true\npane_label = 1\n").unwrap();
        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
        assert!(
            report.starts_with(&format!("invalid {}: [notify]", file.display())),
            "{report}"
        );
        assert!(report.contains("pane_label"), "{report}");
        assert_eq!(code, 1, "{report}");
    }

    #[test]
    fn herds_reports_a_broken_notify_table_beside_a_context_error() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(&file, "context_window = 0\n[notify]\nkinds = [\"done\"]\n").unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(report.matches("invalid").count(), 2, "{report}");
        assert!(report.contains("[notify]"), "{report}");
        assert_eq!(code, 1, "{report}");
    }

    #[test]
    fn herds_reports_a_broken_notify_table_and_exits_1_while_context_limits_still_read() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let file = config.path().join("config.toml");
        fs::write(
            &file,
            "context_window = 300000\n[notify]\nkinds = [\"done\"]\n",
        )
        .unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        let first = report.lines().next().unwrap();
        assert!(
            first.starts_with(&format!("invalid {}: [notify]", file.display())),
            "{report}"
        );
        assert!(first.contains("done"), "{report}");
        assert_eq!(report.matches("invalid").count(), 1, "{report}");
        assert_eq!(code, 1, "{report}");
        assert_eq!(
            crate::context::effective_limit(None, &file).unwrap(),
            240_000
        );
    }

    #[test]
    fn herds_lists_a_queue_with_its_feed_usage() {
        let (report, code) = herds_line(
            "prompt = \"go\"\n[queue]\ntemplate = \"t\"\nsource = \"echo 1\"\nmax = 3",
            &[],
        );
        assert_eq!(
            report,
            "h  repo /r  workspace (current)  paused no\n\
             \x20 loopherd add h t <n>\n\
             \x20 loopherd feed h  (template t, max 3)"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_lists_a_queues_backlog_when_set() {
        let (report, code) = herds_line(
            "prompt = \"go\"\n[queue]\ntemplate = \"t\"\nsource = \"echo 1\"\n\
             backlog = \"cat unready\"\nmax = 3",
            &[],
        );
        assert_eq!(
            report,
            "h  repo /r  workspace (current)  paused no\n\
             \x20 loopherd add h t <n>\n\
             \x20 loopherd feed h  (template t, max 3)\n\
             \x20 backlog: cat unready"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_lists_a_multi_line_backlog_by_its_first_line() {
        let (report, _) = herds_line(
            "prompt = \"go\"\n[queue]\ntemplate = \"t\"\nsource = \"echo 1\"\n\
             backlog = \"\"\"\ncat unready |\n  sort\n\"\"\"\nmax = 3",
            &[],
        );
        assert!(report.ends_with("\n  backlog: cat unready | …"), "{report}");
    }

    #[test]
    fn herds_lists_a_routed_queue_with_its_templates() {
        let (report, code) = herds_line(
            "prompt = \"go\"\n[template.u]\nparams = []\nprompt = \"go\"\n\
             [queue]\ntemplates = [\"u\", \"t\"]\nsource = \"echo 1\"\nmax = 3",
            &[],
        );
        assert_eq!(
            report,
            "h  repo /r  workspace (current)  paused no\n\
             \x20 loopherd add h t <n>\n\
             \x20 loopherd add h u\n\
             \x20 loopherd feed h  (templates u, t, max 3)"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn a_leads_checks_fail_on_a_queue_setting_both_template_and_templates_or_neither() {
        for (queue, message) in [
            (
                "template = \"t\"\ntemplates = [\"t\"]\n",
                "queue: set one of template and templates, not both",
            ),
            ("", "queue: set one of template and templates"),
        ] {
            let herd = format!(
                "repo = \"/r\"\n[template.t]\nparams = []\nprompt = \"go\"\n\
                 [queue]\n{queue}source = \"true\"\nmax = 1\n"
            );
            let config = Config::with(&[("h", &herd)]);

            let err = lead_checks(&config.paths(), "h").err().unwrap();

            assert!(format!("{err:#}").ends_with(message), "{err:#}");
        }
    }

    #[test]
    fn herds_reports_an_invalid_queue() {
        for (queue, message) in [
            (
                "template = \"nope\"\nsource = \"echo 1\"\nmax = 1",
                "queue: no template nope (it has: t)",
            ),
            (
                "template = \"t\"\nsource = \"echo 1\"\nmax = 0",
                "queue: max must be at least 1, got 0",
            ),
            (
                "template = \"t\"\nsource = \"echo 1\"\nmax = -2",
                "queue: max must be at least 1, got -2",
            ),
            ("template = \"t\"\nmax = 1", "missing field `source`"),
            (
                "template = \"t\"\nsource = \" \\n\"\nmax = 1",
                "queue: source is empty",
            ),
            (
                "template = \"t\"\ntemplates = [\"t\"]\nsource = \"echo 1\"\nmax = 1",
                "queue: set one of template and templates, not both",
            ),
            (
                "source = \"echo 1\"\nmax = 1",
                "queue: set one of template and templates",
            ),
            (
                "templates = []\nsource = \"echo 1\"\nmax = 1",
                "queue: templates is empty",
            ),
            (
                "templates = [\"t\", \"nope\"]\nsource = \"echo 1\"\nmax = 1",
                "queue: no template nope (it has: t)",
            ),
            (
                "template = \"t\"\nsource = \"echo 1\"\nbacklog = 3\nmax = 1",
                "invalid type: integer `3`, expected a string",
            ),
            (
                "template = \"t\"\nsource = \"echo 1\"\nbacklog = \" \"\nmax = 1",
                "queue: backlog is empty",
            ),
        ] {
            let (report, code) = herds_line(&format!("prompt = \"go\"\n[queue]\n{queue}"), &[]);
            assert!(report.starts_with("invalid h.toml: "), "{report}");
            assert!(report.contains(message), "{queue}: {report}");
            assert_eq!(code, 1);
        }
    }

    const LEAD_HERD: &str = "repo = \"/r\"\nargs = [\"--model\", \"opus\"]\n\
                             [template.t]\nparams = []\nprompt = \"go\"\n";

    #[test]
    fn merge_guard_is_hook_by_default_and_ignored_while_merge_is_on() {
        let herd = |lead: &str| format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n{lead}");
        let config = Config::with(&[
            ("unset", &herd("")),
            ("hook", &herd("merge_guard = \"hook\"\n")),
            ("credentials", &herd("merge_guard = \"credentials\"\n")),
            (
                "merging",
                &herd("merge = true\nmerge_guard = \"credentials\"\n"),
            ),
            ("unknown", &herd("merge_guard = \"token\"\n")),
            ("leadless", LEAD_HERD),
        ]);
        let paths = config.paths();

        for name in ["unset", "hook"] {
            assert_eq!(merge_guard(&paths, name).unwrap(), Some(MergeGuard::Hook));
            assert_eq!(
                merge_alerts(&paths, name).unwrap(),
                Some(PathBuf::from("/r"))
            );
        }
        assert_eq!(
            merge_guard(&paths, "credentials").unwrap(),
            Some(MergeGuard::Credentials)
        );
        assert_eq!(merge_alerts(&paths, "credentials").unwrap(), None);
        for name in ["merging", "leadless"] {
            assert_eq!(merge_guard(&paths, name).unwrap(), None);
            assert_eq!(merge_alerts(&paths, name).unwrap(), None);
        }
        let err = format!("{:#}", merge_guard(&paths, "unknown").unwrap_err());
        assert!(err.contains("unknown variant `token`"), "{err}");
    }

    #[test]
    fn a_lead_table_parses_with_every_switch_off_by_default() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"lead {{herd}}\"\nmerge = true\n");
        let config = Config::with(&[("h", &herd)]);

        let lead = lead_config(&config.paths(), "h").unwrap();

        assert_eq!(lead.prompt.text, "lead {herd}");
        assert_eq!(lead.args, ["--model", "opus"]);
        assert_eq!(
            lead.switches.lines(),
            "merge on\ninstall off\nrebase off\nnew_cards off\n\
             design_escalation off\nblocked_dialogs off"
        );
    }

    #[test]
    fn lead_switches_reads_only_the_switches() {
        let herd = format!(
            "{LEAD_HERD}[lead]\nprompt_file = \"missing.md\"\nrebase = true\ninstall = true\n"
        );
        let config = Config::with(&[("h", &herd), ("no-lead", LEAD_HERD)]);
        let switches = lead_switches(&config.paths(), "h").unwrap();
        assert_eq!(
            (switches.merge, switches.install, switches.rebase),
            (false, true, true)
        );
        assert!(lead_switches(&config.paths(), "no-lead").is_err());
        assert!(lead_switches(&config.paths(), "absent").is_err());
    }

    #[test]
    fn a_switch_the_herd_file_leaves_unset_comes_from_lead_defaults() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nmerge = false\n");
        let config = Config::with(&[("h", &herd)]);
        fs::write(
            config.path().join("config.toml"),
            "[lead_defaults]\nmerge = true\ninstall = true\n",
        )
        .unwrap();

        let switches = lead_switches(&config.paths(), "h").unwrap();

        assert_eq!(
            (switches.merge, switches.install, switches.rebase),
            (false, true, false)
        );
        assert_eq!(
            lead_config(&config.paths(), "h").unwrap().switches,
            switches
        );
    }

    #[test]
    fn review_rounds_come_from_the_herd_file_else_lead_defaults_else_three() {
        let config = Config::with(&[
            (
                "own",
                &format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nreview_rounds = 5\n"),
            ),
            ("unset", &format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n")),
        ]);
        let rounds = |herd: &str| lead_config(&config.paths(), herd).unwrap().review_rounds;
        assert_eq!((rounds("own"), rounds("unset")), (5, 3));

        fs::write(
            config.path().join("config.toml"),
            "[lead_defaults]\nreview_rounds = 2\n",
        )
        .unwrap();
        assert_eq!((rounds("own"), rounds("unset")), (5, 2));
    }

    #[test]
    fn herds_shows_each_leads_review_rounds_and_where_they_came_from() {
        let lead = |rounds: &str| format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n{rounds}");
        let config = Config::with(&[("own", &lead("review_rounds = 5\n")), ("unset", &lead(""))]);
        let shown = |herd: &str| {
            let (report, code) = herds(&config.paths(), &config.settings()).unwrap();
            assert_eq!(code, 0, "{report}");
            let (_, rest) = report
                .split_once(&format!("  loopherd lead {herd}\n"))
                .unwrap();
            rest.lines()
                .find_map(|l| l.strip_prefix("  review_rounds: "))
                .unwrap()
                .to_string()
        };
        assert_eq!(shown("own"), "5 (herd file)");
        assert_eq!(shown("unset"), "3 (built-in)");

        fs::write(
            config.path().join("config.toml"),
            "[lead_defaults]\nreview_rounds = 2\n",
        )
        .unwrap();
        assert_eq!(shown("own"), "5 (herd file)");
        assert_eq!(shown("unset"), "2 (lead_defaults)");
    }

    #[test]
    fn herds_reports_review_rounds_that_are_not_a_positive_integer_as_invalid() {
        for bad in ["0", "-1", "2.5", "\"3\""] {
            let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nreview_rounds = {bad}\n");
            let config = Config::with(&[("h", &herd)]);

            let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

            assert_eq!(code, 1, "{bad}: {report}");
            assert!(report.contains("review_rounds"), "{bad}: {report}");

            let config = Config::with(&[("h", &format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n"))]);
            fs::write(
                config.path().join("config.toml"),
                format!("[lead_defaults]\nreview_rounds = {bad}\n"),
            )
            .unwrap();

            let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

            assert_eq!(code, 1, "{bad}: {report}");
            assert!(report.starts_with("invalid "), "{bad}: {report}");
            assert!(report.contains("review_rounds"), "{bad}: {report}");
        }
    }

    #[test]
    fn herds_shows_each_leads_switches_and_where_each_came_from() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nmerge = false\nrebase = true\n");
        let config = Config::with(&[("h", &herd), ("unled", LEAD_HERD)]);
        fs::write(
            config.path().join("config.toml"),
            "[lead_defaults]\nmerge = true\ninstall = true\n",
        )
        .unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 0, "{report}");
        assert!(
            report.contains(
                "  loopherd lead h\n  switches: merge off (herd file), install on (lead_defaults), \
                 rebase on (herd file), new_cards off (built-in), design_escalation off (built-in), \
                 blocked_dialogs off (built-in)\n"
            ),
            "{report}"
        );
        assert_eq!(report.matches("switches:").count(), 1, "{report}");
    }

    #[test]
    fn herds_shows_the_mcp_servers_each_lead_may_use_and_refuses_a_blank_or_repeated_name() {
        let herd = |servers: &str| {
            format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nmcp_servers = [{servers}]\n")
        };
        let config = Config::with(&[
            ("named", &herd("\"docs\", \"browser\"")),
            ("none", &format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n")),
            ("blank", &herd("\"\"")),
            ("twice", &herd("\"docs\", \"docs\"")),
        ]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 1, "{report}");
        assert!(
            report.contains("  mcp_servers: docs, browser\n"),
            "{report}"
        );
        assert!(report.contains("  mcp_servers: none\n"), "{report}");
        assert!(
            report.contains("lead: mcp_servers names a blank server"),
            "{report}"
        );
        assert!(
            report.contains("lead: mcp_servers names \"docs\" twice"),
            "{report}"
        );
    }

    #[test]
    fn herds_shows_which_unix_sockets_each_lead_may_use_and_refuses_any_but_all() {
        let lead = |line: &str| format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n{line}\n");
        let config = Config::with(&[
            ("all", &lead("unix_sockets = \"all\"")),
            ("none", &lead("")),
            ("some", &lead("unix_sockets = \"herdr\"")),
        ]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 1, "{report}");
        let shown = |herd: &str| {
            let at = report.find(&format!("{herd}  repo")).unwrap();
            report[at..]
                .lines()
                .find(|line| line.starts_with("  unix_sockets:"))
                .unwrap()
                .to_string()
        };
        assert_eq!(shown("all"), "  unix_sockets: all");
        assert_eq!(shown("none"), "  unix_sockets: none");
        assert!(report.contains("invalid some.toml"), "{report}");
        assert!(report.contains("unknown variant `herdr`"), "{report}");
    }

    #[test]
    fn herds_shows_each_herds_worker_sandbox_and_refuses_unknown_values() {
        let herd = |worker: &str| format!("{LEAD_HERD}[worker]\n{worker}\n");
        let config = Config::with(&[
            ("off", &herd("")),
            ("prefer", &herd("sandbox = \"prefer\"")),
            (
                "require",
                &herd("sandbox = \"require\"\nunix_sockets = \"all\""),
            ),
            ("bad-mode", &herd("sandbox = \"on\"")),
            ("bad-sockets", &herd("unix_sockets = \"herdr\"")),
            ("plain", LEAD_HERD),
        ]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 1, "{report}");
        let shown = |herd: &str| {
            let at = report.find(&format!("\n{herd}  repo")).unwrap() + 1;
            report[at..]
                .lines()
                .skip(1)
                .take_while(|line| line.starts_with("  "))
                .find(|line| line.starts_with("  workers:"))
                .map(str::to_string)
        };
        let workers = |line: &str| Some(format!("  workers: {line}"));
        assert_eq!(shown("off"), workers("sandbox off, unix_sockets none"));
        assert_eq!(
            shown("prefer"),
            workers("sandbox prefer, unix_sockets none")
        );
        assert_eq!(
            shown("require"),
            workers("sandbox require, unix_sockets all")
        );
        assert_eq!(shown("plain"), None);
        assert!(report.contains("invalid bad-mode.toml"), "{report}");
        assert!(report.contains("invalid bad-sockets.toml"), "{report}");
    }

    #[test]
    fn herds_calls_credentials_with_every_unix_socket_open_invalid() {
        let lead = |lines: &str| format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n{lines}\n");
        let config = Config::with(&[
            (
                "both",
                &lead("merge_guard = \"credentials\"\nunix_sockets = \"all\""),
            ),
            (
                "merging",
                &lead("merge = true\nmerge_guard = \"credentials\"\nunix_sockets = \"all\""),
            ),
            ("sockets", &lead("unix_sockets = \"all\"")),
        ]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 1, "{report}");
        assert!(
            report.contains(
                "invalid both.toml: lead: can't start with merge_guard = \"credentials\": \
                 unix_sockets = \"all\" opens your ssh agent, Git Credential Manager \
                 through WSL2 interop and the Secret Service keyring to it, each of which acts \
                 as you; run loopherd herd unset both lead.unix_sockets, or loopherd herd set \
                 both lead.merge_guard '\"hook\"'"
            ),
            "{report}"
        );
        assert_eq!(report.matches("invalid").count(), 1, "{report}");
    }

    #[test]
    fn herds_shows_every_switch_off_while_config_toml_is_broken() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nmerge = true\n");
        let config = Config::with(&[("h", &herd)]);
        fs::write(
            config.path().join("config.toml"),
            "[lead_defaults]\nmerj = true\n",
        )
        .unwrap();

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(code, 1, "{report}");
        assert!(report.starts_with("invalid "), "{report}");
        assert!(report.contains("merj"), "{report}");
        assert!(
            report.contains(
                "  loopherd lead h\n  switches: every switch off until config.toml is fixed\n  \
                 review_rounds: unknown until config.toml is fixed"
            ),
            "{report}"
        );
    }

    #[test]
    fn a_broken_config_toml_or_unknown_lead_default_fails_the_switches() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nmerge = true\n");
        let config = Config::with(&[("h", &herd)]);
        for (text, expected) in [
            ("[lead_defaults]\nmerj = true\n", "merj"),
            ("[lead_defaults]\nmerge = \"yes\"\n", "merge"),
            ("[lead_defaults", "config.toml"),
        ] {
            fs::write(config.path().join("config.toml"), text).unwrap();

            let err = format!("{:#}", lead_switches(&config.paths(), "h").unwrap_err());

            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn a_lead_tables_args_replace_the_herds() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\nargs = [\"--effort\", \"max\"]\n");
        let config = Config::with(&[("h", &herd)]);
        assert_eq!(
            lead_config(&config.paths(), "h").unwrap().args,
            ["--effort", "max"]
        );
    }

    #[test]
    fn a_lead_prompt_file_is_read_relative_to_the_herds_directory() {
        let herd = format!("{LEAD_HERD}[lead]\nprompt_file = \"lead.md\"\n");
        let config = Config::with(&[("h", &herd)]);
        fs::write(config.path().join("herds/lead.md"), "from file {name}").unwrap();
        assert_eq!(
            lead_config(&config.paths(), "h").unwrap().prompt.text,
            "from file {name}"
        );
    }

    #[test]
    fn an_invalid_lead_table_is_an_error_naming_the_problem() {
        for (lead, expected) in [
            (
                "prompt = \"go\"\nmerge_prs = true",
                "unknown field `merge_prs`",
            ),
            ("prompt = \"go\"\nmerge = \"yes\"", "invalid type"),
            (
                "prompt = \"go\"\nprompt_file = \"p.md\"",
                "lead: set one of prompt and prompt_file, not both",
            ),
            ("merge = true", "lead: set one of prompt and prompt_file"),
            (
                "prompt = \"{branch}\"",
                "lead: prompt: unknown placeholder {branch}",
            ),
        ] {
            let config = Config::with(&[("h", &format!("{LEAD_HERD}[lead]\n{lead}\n"))]);
            let err = format!("{:#}", lead_config(&config.paths(), "h").unwrap_err());
            assert!(err.contains(expected), "{lead}: {err}");
        }
    }

    #[test]
    fn a_herd_without_a_lead_table_has_no_lead() {
        let config = Config::with(&[("h", LEAD_HERD)]);
        let err = format!("{:#}", lead_config(&config.paths(), "h").unwrap_err());
        assert_eq!(err, "herd: h has no [lead]");
    }

    #[test]
    fn herds_lists_a_lead_and_reports_an_invalid_lead_prompt() {
        let ok = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n");
        let bad = format!("{LEAD_HERD}[lead]\nprompt = \"{{pane}}\"\n");
        let config = Config::with(&[("a", &ok), ("b", &bad)]);

        let (report, code) = herds(&config.paths(), &config.settings()).unwrap();

        assert_eq!(
            report,
            "a  repo /r  workspace (current)  paused no\n\
             \x20 loopherd add a t\n\
             \x20 loopherd lead a\n\
             \x20 switches: merge off (built-in), install off (built-in), rebase off (built-in), \
             new_cards off (built-in), design_escalation off (built-in), blocked_dialogs off (built-in)\n\
             \x20 review_rounds: 3 (built-in)\n\
             \x20 mcp_servers: none\n\
             \x20 unix_sockets: none\n\
             invalid b.toml: lead: prompt: unknown placeholder {pane}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn the_documented_loopherd_cards_lead_parses() {
        let table = include_str!("../docs/examples/loopherd-cards-lead.toml");
        let config = Config::with(&[("loopherd-cards", &format!("{LEAD_HERD}{table}"))]);
        fs::write(
            config.path().join("herds/loopherd-cards-lead.md"),
            include_str!("../docs/examples/loopherd-cards-lead.md"),
        )
        .unwrap();

        let lead = lead_config(&config.paths(), "loopherd-cards").unwrap();
        assert!(
            lead.prompt
                .text
                .contains("`loopherd --skill` in full, its Lead section")
        );
        assert!(!lead.prompt.text.contains("docs/agents/"));

        assert_eq!(lead.switches, Switches::default());
        assert_eq!(lead.args, ["--model", "opus"]);

        let merged = table
            .lines()
            .find_map(|line| line.strip_prefix("# merged = "))
            .expect("the example's merged line");
        let herd = format!(
            "repo = \"/r\"\n[template.card]\nparams = [\"n\"]\nprompt = \"go\"\n\
             branch = \"feat/{{n}}\"\nmerged = {merged}\n\
             [queue]\ntemplate = \"card\"\nsource = \"true\"\nmax = 1\n"
        );
        let config = Config::with(&[("cards", &herd)]);
        let (checks, _) = lead_checks(&config.paths(), "cards").unwrap();
        assert!(checks.merged.contains_key("card"));
    }

    #[test]
    fn every_switch_maps_to_its_own_line() {
        let herd = format!(
            "{LEAD_HERD}[lead]\nprompt = \"go\"\nmerge = true\ninstall = true\nrebase = true\n\
             new_cards = true\ndesign_escalation = true\nblocked_dialogs = true\n"
        );
        let config = Config::with(&[("h", &herd)]);
        assert_eq!(
            lead_config(&config.paths(), "h").unwrap().switches.lines(),
            "merge on\ninstall on\nrebase on\nnew_cards on\n\
             design_escalation on\nblocked_dialogs on"
        );
        for switch in [
            "install",
            "rebase",
            "new_cards",
            "design_escalation",
            "blocked_dialogs",
        ] {
            let herd = format!("{LEAD_HERD}[lead]\nprompt = \"go\"\n{switch} = true\n");
            let config = Config::with(&[("h", &herd)]);
            let lines = lead_config(&config.paths(), "h").unwrap().switches.lines();
            let on: Vec<&str> = lines.lines().filter(|l| l.ends_with(" on")).collect();
            assert_eq!(on, [format!("{switch} on")], "{switch}");
        }
    }

    fn record(herd: Option<&str>, template: Option<&str>, lead: bool) -> AgentRecord {
        AgentRecord {
            name: "m".into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            outcome: None,
            released_at: None,
            herd: herd.map(str::to_string),
            template: template.map(str::to_string),
            params: None,
            socket: None,
            process: None,
            role: lead.then_some(crate::ledger::Role::Lead),
            attended: None,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    const LIMITED_HERD: &str = r#"
repo = "/src/repo"

[template.pr]
params = ["number"]
prompt = "hi"
context_limit = 120000

[template.plain]
params = []
prompt = "hi"

[lead]
prompt = "lead"
context_limit = 90000
"#;

    #[test]
    fn a_members_context_limit_is_its_templates_or_its_leads() {
        let config = Config::with(&[("cards", LIMITED_HERD)]);
        let paths = config.paths();
        let limit = |herd, template, lead| context_limit(&paths, &record(herd, template, lead));
        assert_eq!(limit(Some("cards"), Some("pr"), false).unwrap(), 120_000);
        assert_eq!(limit(Some("cards"), None, true).unwrap(), 90_000);
    }

    #[test]
    fn without_its_own_limit_a_member_gets_config_tomls_default() {
        let config = Config::with(&[("cards", LIMITED_HERD)]);
        let paths = config.paths();
        let limit = |herd, template| context_limit(&paths, &record(herd, template, false));
        assert_eq!(limit(Some("cards"), Some("plain")).unwrap(), 160_000);
        assert_eq!(limit(None, None).unwrap(), 160_000);
        fs::write(
            config.path().join("config.toml"),
            "default_context_limit = 70000\n",
        )
        .unwrap();
        assert_eq!(limit(Some("cards"), Some("plain")).unwrap(), 70_000);
        assert_eq!(limit(None, None).unwrap(), 70_000);
        assert_eq!(limit(Some("cards"), Some("pr")).unwrap(), 120_000);
    }

    #[test]
    fn a_context_limit_under_the_minimum_is_an_invalid_herd_file() {
        for (herd, message) in [
            (
                "repo = \"/r\"\n[template.t]\nparams = []\nprompt = \"p\"\ncontext_limit = 0\n",
                "template t: context_limit must be at least 10000, got 0",
            ),
            (
                "repo = \"/r\"\n[lead]\nprompt = \"p\"\ncontext_limit = 500\n",
                "lead: context_limit must be at least 10000, got 500",
            ),
        ] {
            let config = Config::with(&[("cards", herd)]);
            let err = context_limit(&config.paths(), &record(Some("cards"), Some("t"), false))
                .unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.contains("cards.toml") && err.ends_with(message),
                "{err}"
            );
        }
    }

    #[test]
    fn a_missing_herd_file_fails_the_limit() {
        let config = Config::with(&[]);
        let err =
            context_limit(&config.paths(), &record(Some("gone"), Some("pr"), false)).unwrap_err();
        assert!(format!("{err:#}").contains("gone.toml"), "{err:#}");
    }
}
