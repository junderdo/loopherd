//! Herd files (D7, D9) and `loopherd add`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, FileLedger, Ledger};
use crate::lock::Lock;
use crate::spawn::{
    Checkout, MemberOf, Prompt, SHELL_TIMEOUT, SpawnRequest, Spawned, preview, spawn,
};
use crate::template::Template;

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
}

/// A herd's lead member (D17).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeadFile {
    prompt: Option<String>,
    prompt_file: Option<String>,
    args: Option<Vec<String>>,
    #[serde(default)]
    merge: bool,
    #[serde(default)]
    install: bool,
    #[serde(default)]
    rebase: bool,
    #[serde(default)]
    new_cards: bool,
    #[serde(default)]
    design_escalation: bool,
    #[serde(default)]
    blocked_dialogs: bool,
    context_limit: Option<u64>,
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

impl Switches {
    /// One `<switch> on|off` line each, for the lead's `{switches}`.
    pub fn lines(&self) -> String {
        [
            ("merge", self.merge),
            ("install", self.install),
            ("rebase", self.rebase),
            ("new_cards", self.new_cards),
            ("design_escalation", self.design_escalation),
            ("blocked_dialogs", self.blocked_dialogs),
        ]
        .map(|(name, on)| format!("{name} {}", if on { "on" } else { "off" }))
        .join("\n")
    }
}

/// What `loopherd feed` spawns (D14).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueFile {
    template: String,
    /// A shell command printing one task per line.
    source: String,
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
}

/// How long [`Paths::lock_feed`] waits out another holder.
const FEED_LOCK_GRACE: Duration = Duration::from_secs(1);

/// Where herd files live, the home a leading `~` expands to, and the lock
/// that keeps `feed` and `add` from filling a queue's slots at once.
pub struct Paths {
    pub herds: PathBuf,
    pub home: PathBuf,
    pub feed_lock: PathBuf,
}

impl Paths {
    /// `$LOOPHERD_CONFIG_DIR/herds`, else `$XDG_CONFIG_HOME/loopherd/herds`,
    /// defaulting to `~/.config/loopherd/herds`.
    pub fn from_env() -> Result<Paths> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let home = PathBuf::from(var("HOME").context("HOME is not set")?);
        let config = match (var("LOOPHERD_CONFIG_DIR"), var("XDG_CONFIG_HOME")) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(xdg)) => PathBuf::from(xdg).join("loopherd"),
            (None, None) => home.join(".config/loopherd"),
        };
        Ok(Paths {
            herds: config.join("herds"),
            home,
            feed_lock: FileLedger::default_path()?.with_file_name("feed.lock"),
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
                bail!("feed already running");
            }
            std::thread::sleep(Duration::from_millis(20));
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
}

#[derive(Debug)]
pub struct Added {
    pub name: String,
    pub spawned: Spawned,
    /// Set when the member took the herd past its queue's `max`.
    pub warning: Option<String>,
}

/// Spawns a member of `req.herd`.
///
/// Into a herd with a `[queue]`, fails while `feed` holds the feed lock
/// past [`Paths::lock_feed`]'s 1s wait, and adds past the
/// queue's `max` with a warning: `add` is the human's override.
pub fn add(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &AddRequest,
    inherited_workspace: Option<&str>,
) -> Result<Added> {
    let spawn_req = resolve(paths, req)?;
    let max = load(&paths.herds, &req.herd)
        .context("herd")?
        .queue
        .map(|q| q.max as usize);
    let _feed = max.map(|_| paths.lock_feed()).transpose()?;
    let held = match max {
        Some(_) => members(ledger, &req.herd)?,
        None => 0,
    };
    let spawned = spawn(herdr, ledger, &spawn_req, inherited_workspace)?;
    let warning = max.filter(|&max| held >= max).map(|max| {
        format!(
            "{} now has {} members, past its queue max of {max}",
            req.herd,
            held + 1
        )
    });
    Ok(Added {
        name: spawn_req.name,
        spawned,
        warning,
    })
}

/// How many unretired ledger entries belong to `herd`, not counting its lead.
pub fn members(ledger: &dyn Ledger, herd: &str) -> Result<usize> {
    let entries = ledger.entries().context("ledger")?;
    Ok(entries
        .values()
        .filter(|e| !e.retired && !e.is_lead() && e.herd.as_deref() == Some(herd))
        .count())
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
    pub template: String,
    pub source: String,
    pub max: usize,
    pub repo: PathBuf,
    /// The template's param names, in order.
    pub params: Vec<String>,
    /// Each template's done check, for those that have one.
    pub done: BTreeMap<String, MemberCheck>,
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
    let params = file
        .template
        .get(&queue.template)
        .with_context(|| format!("queue: {herd} has no template {}", queue.template))?
        .params
        .clone();
    Ok(Some(Queue {
        done: checks_of(file, "done", |t| t.done.as_deref())?,
        params,
        template: queue.template.clone(),
        source: queue.source.clone(),
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
        let Some(command) = pattern(command, &template.params, &BUILT_INS).context(field)? else {
            return Ok(None);
        };
        let branch = pattern(
            template.branch.as_deref(),
            &template.params,
            &["herd", "name"],
        )
        .context("branch")?;
        Ok(Some(MemberCheck { command, branch }))
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
    let mut values: HashMap<&str, &str> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    values.insert("herd", &req.herd);

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
            values: vec![],
            role: None,
        }),
    })
}

const USAGE: &str = "usage: loopherd add <herd> <template> <param>…";

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
    pattern(Some(&prompt.text), &template.params, &BUILT_INS).context(prompt.source.clone())?;
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
}

/// Placeholders a lead's prompt may use.
pub const LEAD_KEYS: [&str; 4] = ["name", "herd", "worktree", "switches"];

pub fn lead_config(paths: &Paths, herd: &str) -> Result<LeadConfig> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    let prompt = read_prompt(&lead.prompt, &lead.prompt_file, paths).context("lead")?;
    check_lead_prompt(&prompt).context("lead")?;
    Ok(LeadConfig {
        repo: expand_home(&file.repo, &paths.home),
        workspace: file.workspace,
        prompt,
        switches: lead.switches(),
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
    let config = paths.herds.with_file_name("config.toml");
    crate::context::effective_limit(own, &config)
}

/// A herd's `[lead]` switches, without reading its prompt.
pub fn lead_switches(paths: &Paths, herd: &str) -> Result<Switches> {
    let file = load(&paths.herds, herd).context("herd")?;
    let lead = file
        .lead
        .with_context(|| format!("herd: {herd} has no [lead]"))?;
    Ok(lead.switches())
}

impl LeadFile {
    fn switches(&self) -> Switches {
        Switches {
            merge: self.merge,
            install: self.install,
            rebase: self.rebase,
            new_cards: self.new_cards,
            design_escalation: self.design_escalation,
            blocked_dialogs: self.blocked_dialogs,
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

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// Every herd file in `paths.herds`, sorted by herd name, with a usage line
/// per template. Exits 1 if any file is invalid.
pub fn herds(paths: &Paths) -> Result<(String, i32)> {
    let files = herd_files(&paths.herds)?;
    if files.is_empty() {
        let hint = format!(
            "no herds: add a herd file at {}/<herd>.toml",
            paths.herds.display()
        );
        return Ok((hint, 0));
    }
    let mut lines = Vec::new();
    let mut code = 0;
    for file in &files {
        let herd = file.file_stem().unwrap_or_default().to_string_lossy();
        match std::fs::read_to_string(file)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse(&text))
            .and_then(|parsed| {
                for (name, template) in &parsed.template {
                    check_template(name, template, paths)?;
                }
                if let Some(lead) = &parsed.lead {
                    let prompt = read_prompt(&lead.prompt, &lead.prompt_file, paths);
                    prompt.and_then(|p| check_lead_prompt(&p)).context("lead")?;
                }
                Ok(parsed)
            }) {
            Ok(parsed) => lines.extend(describe(&herd, &parsed)),
            Err(e) => {
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                lines.push(format!("invalid {name}: {}", format!("{e:#}").trim_end()));
                code = 1;
            }
        }
    }
    Ok((lines.join("\n"), code))
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

fn describe(herd: &str, parsed: &HerdFile) -> Vec<String> {
    let workspace = parsed.workspace.as_deref().unwrap_or("(current)");
    let header = format!("{herd}  repo {}  workspace {workspace}", parsed.repo);
    let usages = parsed.template.iter().map(|(template, t)| {
        let params: String = t.params.iter().map(|p| format!(" <{p}>")).collect();
        format!("  loopherd add {herd} {template}{params}")
    });
    let feed = parsed.queue.as_ref().map(|q| {
        format!(
            "  loopherd feed {herd}  (template {}, max {})",
            q.template, q.max
        )
    });
    let lead = parsed
        .lead
        .as_ref()
        .map(|_| format!("  loopherd lead {herd}"));
    std::iter::once(header)
        .chain(usages)
        .chain(lead)
        .chain(feed)
        .collect()
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

fn load(dir: &Path, herd: &str) -> Result<HerdFile> {
    if herd.is_empty() || herd.starts_with('.') || herd.contains('/') {
        bail!("{herd:?} is not a herd name");
    }
    let file = dir.join(format!("{herd}.toml"));
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

fn validate_queue(queue: &QueueFile, templates: &BTreeMap<String, TemplateFile>) -> Result<()> {
    if !templates.contains_key(&queue.template) {
        let known: Vec<&str> = templates.keys().map(String::as_str).collect();
        bail!(
            "no template {} (it has: {})",
            queue.template,
            known.join(", ")
        );
    }
    if queue.max < 1 {
        bail!("max must be at least 1, got {}", queue.max);
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
    use std::time::Duration;

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
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn request(herd: &str, template: &str, params: &[&str]) -> AddRequest {
        AddRequest {
            herd: herd.into(),
            template: template.into(),
            params: params.iter().map(|p| p.to_string()).collect(),
            claude_args: vec![],
            ready_timeout: Duration::from_secs(60),
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "zagops")])
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

    fn queue_herd(max: i64) -> String {
        format!("{PR_HERD}\n[queue]\ntemplate = \"pr\"\nsource = \"true\"\nmax = {max}\n")
    }

    #[test]
    fn a_second_feed_lock_is_refused_until_the_first_is_dropped() {
        let config = Config::with(&[]);
        let paths = config.paths();
        let first = paths.lock_feed().unwrap();
        let err = paths.lock_feed().unwrap_err();
        assert_eq!(format!("{err:#}"), "feed already running");
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
        assert_eq!(err, "feed already running");
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

    fn add_ok(config: &Config, req: &AddRequest) -> (FakeHerdr, MemLedger, String) {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let Added { name, .. } =
            add(&herdr, &mut ledger, &config.paths(), req, Some("w1")).unwrap();
        (herdr, ledger, name)
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
            "workspace = \"zagops\"\nargs = [\"--permission",
        );
        let config = Config::with(&[("prs", PR_HERD), ("zag", &labeled)]);
        let (herdr, _, _) = add_ok(&config, &request("prs", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w1");
        let (herdr, ledger, _) = add_ok(&config, &request("zag", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w2");
        assert_eq!(ledger.0["zpr-1"].workspace_label.as_deref(), Some("zagops"));
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

        let err = add_err(&config, &request("prs", "pr", &["Bad/Name"]));
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
            "repo = {:?}\nworkspace = \"zagops\"\nargs = [\"--model\", \"opus\"]\n\
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
                "name z-7\nbranch pr-7\nworktree {wt}\nworkspace zagops\n\
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
        let issues = "repo = \"~/src/z\"\nworkspace = \"zagops\"\n\
                      [template.issue]\nparams = [\"key\", \"slug\"]\nprompt = \"go\"\n\
                      [template.chat]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("zissues", issues), ("prs", PR_HERD)]);

        let (report, code) = herds(&config.paths()).unwrap();

        assert_eq!(
            report,
            "prs  repo /src/repo  workspace (current)\n\
             \x20 loopherd add prs pr <number>\n\
             zissues  repo ~/src/z  workspace zagops\n\
             \x20 loopherd add zissues chat\n\
             \x20 loopherd add zissues issue <key> <slug>"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_sorts_by_herd_name_not_file_name() {
        let config = Config::with(&[("prs-old", PR_HERD), ("prs", PR_HERD)]);
        let (report, _) = herds(&config.paths()).unwrap();
        let headers: Vec<&str> = report.lines().filter(|l| !l.starts_with(' ')).collect();
        assert_eq!(headers[0], "prs  repo /src/repo  workspace (current)");
        assert!(headers[1].starts_with("prs-old  "), "{report}");
    }

    #[test]
    fn herds_reports_invalid_files_lists_the_rest_and_exits_1() {
        let config = Config::with(&[
            ("a-broken", "repo = "),
            ("prs", PR_HERD),
            ("z-bad", "repo = \"/r\"\n[template.t]\nparams = []\n"),
        ]);

        let (report, code) = herds(&config.paths()).unwrap();

        let lines: Vec<&str> = report.lines().collect();
        assert!(
            lines[0].starts_with("invalid a-broken.toml: TOML parse error"),
            "{report}"
        );
        assert!(!lines[0].contains(&config.path().display().to_string()));
        assert_eq!(
            lines[lines.len() - 3..],
            [
                "prs  repo /src/repo  workspace (current)",
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
        let (report, code) = herds(&config.paths()).unwrap();
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
            "h  repo /r  workspace (current)\n  loopherd add h t <n>"
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
            "h  repo /r  workspace (current)\n  loopherd add h t <n>"
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
            let (report, code) = herds(&paths).unwrap();
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
    fn herds_lists_a_queue_with_its_feed_usage() {
        let (report, code) = herds_line(
            "prompt = \"go\"\n[queue]\ntemplate = \"t\"\nsource = \"echo 1\"\nmax = 3",
            &[],
        );
        assert_eq!(
            report,
            "h  repo /r  workspace (current)\n\
             \x20 loopherd add h t <n>\n\
             \x20 loopherd feed h  (template t, max 3)"
        );
        assert_eq!(code, 0);
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

        let (report, code) = herds(&config.paths()).unwrap();

        assert_eq!(
            report,
            "a  repo /r  workspace (current)\n\
             \x20 loopherd add a t\n\
             \x20 loopherd lead a\n\
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
            herd: herd.map(str::to_string),
            template: template.map(str::to_string),
            params: None,
            socket: None,
            process: None,
            role: lead.then_some(crate::ledger::Role::Lead),
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
