//! The Claude Code sandbox a herd's workers run in, opt-in by `[worker] sandbox` (D107).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handoff;
use crate::herd::{self, MergeGuard, Paths};
use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger};
use crate::sandbox::{
    ALLOWED_ARGS, Sockets, Unavailable, UnixSockets, confine_credentials, disallowed_warning,
    edit_rule, entries_but, file_rule, launch_flag_refusal, launch_flags_in, loosened_warning,
    loosenings, mcp_file, outside_allowlist, without_launch_flags, write_private,
};
use crate::spawn::{Checkout, MemberOf, SpawnRequest, Spawned};

/// `[worker] sandbox`: whether a herd's workers start sandboxed, and what
/// happens to one whose sandbox can't start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerSandbox {
    #[default]
    Off,
    /// The worker isn't started.
    Require,
    /// The worker starts unsandboxed, flagged in the ledger.
    Prefer,
}

impl WorkerSandbox {
    pub fn name(self) -> &'static str {
        match self {
            WorkerSandbox::Off => "off",
            WorkerSandbox::Require => "require",
            WorkerSandbox::Prefer => "prefer",
        }
    }
}

/// What loopherd keeps of a sandboxed worker in its config dir, where the
/// worker can't write. `revive` trusts this over the ledger, which a worker
/// started unsandboxed under `prefer` could have rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub herd: String,
    pub template: String,
    pub args: Vec<String>,
    /// The worktree its sandbox lets it write.
    #[serde(default)]
    pub worktree: Option<PathBuf>,
}

impl Member {
    /// The record `prepare` kept for worker `name`, if any.
    pub fn kept(paths: &Paths, name: &str) -> Result<Option<Member>> {
        let file = member_file(paths, name);
        match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .with_context(|| format!("parse {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }
}

/// The member `revive` resumes worker `record` as: the one `prepare` kept, or,
/// for one started while its herd's `[worker]` sandbox was off, one from its
/// ledger record once the sandbox is on. `None` resumes it as recorded.
pub fn to_resume(paths: &Paths, record: &AgentRecord) -> Result<Option<Member>> {
    // A worker's launch flags are loopherd's, added again at each start.
    if let Some(member) = Member::kept(paths, &record.name)? {
        let args = without_launch_flags(&member.args);
        return Ok(Some(Member { args, ..member }));
    }
    let (Some(herd), Some(template)) = (&record.herd, &record.template) else {
        return Ok(None);
    };
    let on = herd::worker(paths, herd).is_ok_and(|w| w.sandbox != WorkerSandbox::Off);
    Ok((on && !record.may_be_lead()).then(|| Member {
        herd: herd.clone(),
        template: template.clone(),
        args: without_launch_flags(&record.claude_args),
        worktree: record.worktree.clone(),
    }))
}

pub fn member_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.member.json"))
}

/// How a worker's Claude is started. Either way its herd's sandbox is on,
/// it gets only the MCP servers in `mcp`, and `warnings` are what `prefer`
/// let through.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Its herd leaves `[worker] sandbox` off.
    Off,
    Sandboxed {
        settings: PathBuf,
        mcp: PathBuf,
        warnings: Vec<String>,
    },
    /// `prefer`, with why the sandbox can't start and what would let it.
    Unsandboxed {
        why: String,
        fix: String,
        mcp: PathBuf,
        warnings: Vec<String>,
    },
}

impl Launch {
    /// The args loopherd adds to the worker's Claude.
    pub fn args(&self) -> Vec<String> {
        let mcp_lock = |mcp: &Path| {
            vec![
                "--strict-mcp-config".to_string(),
                "--mcp-config".into(),
                mcp.display().to_string(),
            ]
        };
        match self {
            Launch::Off => vec![],
            Launch::Unsandboxed { mcp, .. } => mcp_lock(mcp),
            Launch::Sandboxed { settings, mcp, .. } => [
                vec!["--settings".into(), settings.display().to_string()],
                mcp_lock(mcp),
            ]
            .concat(),
        }
    }

    /// Why the worker runs without its sandbox, if it does.
    pub fn unsandboxed(&self) -> Option<&str> {
        match self {
            Launch::Unsandboxed { why, .. } => Some(why),
            _ => None,
        }
    }

    pub fn warnings(&self, name: &str) -> Vec<String> {
        match self {
            Launch::Off => vec![],
            Launch::Sandboxed { warnings, .. } => warnings.clone(),
            Launch::Unsandboxed {
                why, fix, warnings, ..
            } => [
                vec![format!("{name} runs unsandboxed ({why}); {fix}")],
                warnings.clone(),
            ]
            .concat(),
        }
    }
}

/// A worker its sandbox refused before anything was started for it.
#[derive(Debug)]
pub struct NotStarted(pub String);

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotStarted {}

/// Where the args a worker is judged by come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgsFrom {
    /// Its herd's, its template's, then the command line's, as `add` and `feed` start it.
    Fresh,
    /// Kept from when it was started, as `revive` finds it.
    Kept,
}

/// Keeps `member` and writes worker `name`'s settings and MCP config in
/// loopherd's config dir, from its herd's `[worker]`, when that turns the
/// sandbox on. Under `require`, a sandbox that can't start, or that the
/// human's settings or the worker's args would loosen, fails with
/// [`NotStarted`]; under `prefer` those are warnings. A passed launch flag or
/// an unknown MCP server fails under both.
pub fn prepare(
    paths: &Paths,
    name: &str,
    member: &Member,
    from: ArgsFrom,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
) -> Result<Launch> {
    let worker = herd_worker(paths, name, &member.herd)?;
    if worker.sandbox == WorkerSandbox::Off {
        return Ok(Launch::Off);
    }
    let herd = &member.herd;
    let require = worker.sandbox == WorkerSandbox::Require;
    let prefer_instead = prefer_instead(herd);
    let (disallowed, mcp_config) = judge_args_and_mcp(paths, name, member, &worker, from)?;
    // A branch checked out in the main checkout plans the main checkout itself.
    let repo = herd::repo(paths, herd)?;
    let worktree =
        (member.worktree.as_deref()).filter(|dir| !crate::worktree::same_dir(dir, &repo));
    // Why, as a refusal says it and as the ledger flags it, and the fix.
    let unavailable = match (worktree, probe()) {
        (None, _) => {
            let why = "it has no worktree of its own, and its sandbox won't open the main checkout";
            let fix = format!("give herd {herd}'s template {} a branch", member.template);
            Some((why.to_string(), why.to_string(), fix))
        }
        (_, Err(Unavailable { why, fix })) => {
            Some((format!("its sandbox can't start: {why}"), why, fix))
        }
        (_, Ok(Sockets::Open(Unavailable { why, fix }))) if worker.unix_sockets.is_none() => {
            let why = format!("its sandbox can't block Unix sockets: {why}");
            Some((why.clone(), why, fix))
        }
        (_, Ok(_)) => None,
    };
    let credentials = herd::merge_guard(paths, herd)? == Some(MergeGuard::Credentials);
    if credentials && worker.unix_sockets == Some(UnixSockets::All) {
        let why = herd::credentials_with_sockets(herd, "worker");
        return Err(NotStarted(format!(
            "{name} can't start with merge_guard = \"credentials\": {why}"
        ))
        .into());
    }
    let refused_otherwise = unsandboxed_fix(herd, credentials, worker.sandbox, "start");
    if let (Some((why, _, fix)), Some(otherwise)) = (&unavailable, refused_otherwise) {
        return Err(NotStarted(format!(
            "{name} can't start sandboxed: {why}; {fix}, or {otherwise}"
        ))
        .into());
    }
    let own = match (&unavailable, worktree) {
        (None, Some(worktree)) => {
            let own = settings(paths, name, herd, worktree)?;
            let loosened = loosenings(&paths.home, worktree, &own);
            if require && !loosened.is_empty() {
                return Err(NotStarted(format!(
                    "{name} can't start sandboxed: your Claude settings loosen its sandbox: {}; \
                     remove those, {prefer_instead}",
                    loosened.join("; ")
                ))
                .into());
            }
            Some((own, loosened))
        }
        _ => None,
    };
    let dir = paths.config_dir().join("sandbox");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let kept = member_file(paths, name);
    std::fs::write(&kept, serde_json::to_string_pretty(member)? + "\n")
        .with_context(|| format!("write {}", kept.display()))?;
    let mcp = mcp_file(paths, name);
    let text = serde_json::to_string_pretty(&mcp_config)? + "\n";
    write_private(&mcp, &text).with_context(|| format!("write {}", mcp.display()))?;
    let arg_warnings = disallowed.iter().map(|line| disallowed_warning(name, line));
    let Some((own, loosened)) = own else {
        let (_, why, fix) = unavailable.expect("no settings only when unavailable");
        let warnings = arg_warnings.collect();
        return Ok(Launch::Unsandboxed {
            why,
            fix,
            mcp,
            warnings,
        });
    };
    // An allowWrite entry for a missing path has no effect on Linux.
    for dir in own_dirs(paths, name) {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = settings_file(paths, name);
    let text = serde_json::to_string_pretty(&own)? + "\n";
    write_private(&file, &text).with_context(|| format!("write {}", file.display()))?;
    let warnings = (loosened.iter().map(|line| loosened_warning(name, line)))
        .chain(arg_warnings)
        .collect();
    Ok(Launch::Sandboxed {
        settings: file,
        mcp,
        warnings,
    })
}

/// The refusals `prepare` makes from worker `name`'s args and MCP servers
/// alone, before its worktree is read; else its disallowed args and MCP config.
fn judge_args_and_mcp(
    paths: &Paths,
    name: &str,
    member: &Member,
    worker: &herd::Worker,
    from: ArgsFrom,
) -> Result<(Vec<String>, Value)> {
    let herd = &member.herd;
    let require = worker.sandbox == WorkerSandbox::Require;
    let prefer_instead = prefer_instead(herd);
    let sources = arg_sources(paths, member, from)?;
    if let Some(why) = launch_flag_refusals(name, &sources).into_iter().next() {
        return Err(NotStarted(why).into());
    }
    let disallowed = disallowed(&sources);
    if require && !disallowed.is_empty() {
        let fix = match from {
            ArgsFrom::Fresh => "drop those".to_string(),
            ArgsFrom::Kept => format!(
                "retire it and add it again from herd {herd}'s template {}, without those",
                member.template
            ),
        };
        return Err(NotStarted(format!(
            "{name} can't start sandboxed: its args may loosen its sandbox or permissions: {}; \
             a sandboxed worker takes only {ALLOWED_ARGS}, so {fix}, {prefer_instead}",
            disallowed.join("; ")
        ))
        .into());
    }
    // An unreadable ~/.claude.json refuses this worker alone, so a feed goes on.
    let mcp_config = crate::sandbox::user_scope_mcp_config(paths, &worker.mcp_servers)
        .map_err(|e| NotStarted(format!("{name} can't start: its MCP servers: {e:#}")))?
        .map_err(|server| {
            NotStarted(crate::sandbox::unknown_mcp_server(
                paths, name, herd, "[worker]", &server, "a worker",
            ))
        })?;
    Ok((disallowed, mcp_config))
}

fn prefer_instead(herd: &str) -> String {
    format!("or set [worker] sandbox = \"prefer\" in herd {herd} to start it with them as warnings")
}

/// A worker's args split by where they were set, each part named.
fn arg_sources(
    paths: &Paths,
    member: &Member,
    from: ArgsFrom,
) -> Result<Vec<(Vec<String>, String)>> {
    let args = &member.args;
    if from == ArgsFrom::Kept {
        return Ok(vec![(args.clone(), "the args it was started with".into())]);
    }
    let (herd_args, template_args) = herd::member_args(paths, &member.herd, &member.template)?;
    let herd_end = herd_args.len().min(args.len());
    let template_end = (herd_end + template_args.len()).min(args.len());
    Ok(vec![
        (
            args[..herd_end].to_vec(),
            format!("herd {}'s args", member.herd),
        ),
        (
            args[herd_end..template_end].to_vec(),
            format!("template {}'s args", member.template),
        ),
        (args[template_end..].to_vec(), "the command line".into()),
    ])
}

/// Why each of loopherd's own launch flags in `sources` refuses worker `name`.
fn launch_flag_refusals(name: &str, sources: &[(Vec<String>, String)]) -> Vec<String> {
    sources
        .iter()
        .flat_map(|(args, source)| {
            launch_flags_in(args).into_iter().map(move |flag| {
                launch_flag_refusal(name, flag, &format!("drop it from {source}"), "[worker]")
            })
        })
        .collect()
}

/// Each arg in `sources` outside the allowlist, with its values and source.
fn disallowed(sources: &[(Vec<String>, String)]) -> Vec<String> {
    sources
        .iter()
        .flat_map(|(args, source)| {
            outside_allowlist(&without_launch_flags(args))
                .into_iter()
                .map(move |arg| format!("{arg}, from {source}"))
        })
        .collect()
}

/// What a sandboxed worker of herd `herd` would be refused or warned for in
/// its herd file's args, the herd's and each template's, one line each.
pub struct ArgFindings {
    /// loopherd's own launch flags, refused under both modes, each with its fix.
    pub flags: Vec<(String, String)>,
    /// Args outside the allowlist.
    pub disallowed: Vec<String>,
}

pub fn configured_arg_findings(paths: &Paths, herd: &str) -> Result<ArgFindings> {
    let sources = herd::args_by_source(paths, herd)?;
    let flags = sources
        .iter()
        .flat_map(|(args, source)| {
            launch_flags_in(args).into_iter().map(move |flag| {
                let fix = match flag {
                    "--settings" => "drop it, and put those settings in ~/.claude/settings.json",
                    _ => "drop it, and name the servers they may use in [worker] mcp_servers",
                };
                let line =
                    format!("{flag}, from {source}, which a sandboxed worker's launch takes");
                (line, fix.to_string())
            })
        })
        .collect();
    Ok(ArgFindings {
        flags,
        disallowed: disallowed(&sources),
    })
}

/// Spawns herd member `req`, in its herd's worker sandbox when `[worker]
/// sandbox` turns it on, and flags one started without it in the ledger.
/// Returns the start's warnings. A refusal by the sandbox is [`NotStarted`].
pub fn spawn(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &SpawnRequest,
    inherited_workspace: Option<&str>,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
) -> Result<(Spawned, Vec<String>)> {
    let recorded = |ledger: &dyn Ledger| -> Result<bool> {
        let entries = ledger.entries().context("ledger")?;
        Ok(entries.get(&req.name).is_some_and(|r| !r.retired))
    };
    let member = match &req.member_of {
        // A name in use fails spawn's own check, before its holder's files are touched.
        Some(MemberOf {
            herd,
            template: Some(template),
            ..
        }) if !recorded(ledger)? => Member {
            herd: herd.clone(),
            template: template.clone(),
            args: req.claude_args.clone(),
            worktree: req.planned_worktree()?,
        },
        _ => {
            let spawned = crate::spawn::spawn(herdr, ledger, req, inherited_workspace)?;
            return Ok((spawned, vec![]));
        }
    };
    // A branch checked out in the main checkout plans the main checkout itself.
    // A retired worker's Claude may still run on the files `prepare` rewrites.
    crate::spawn::ensure_name_free(herdr, &req.name).context("name")?;
    let fresh = checkout_before_prepare(herdr, paths, req, &member, inherited_workspace)?;
    let launch = match prepare(paths, &req.name, &member, ArgsFrom::Fresh, probe) {
        Ok(launch) => launch,
        // A retry would reuse the worktree and branch as they are, not as origin then has them.
        Err(e) => {
            return Err(match fresh.map(|f| f.remove(&req.repo)) {
                Some(Err(left)) => e.context(format!("{left:#}")),
                _ => e,
            });
        }
    };
    let mut req = req.clone();
    req.claude_args.extend(launch.args());
    let spawned = match crate::spawn::spawn(herdr, ledger, &req, inherited_workspace) {
        Ok(spawned) => spawned,
        Err(e) => {
            // Kept once anything records the worker, whose Claude may run.
            if launch != Launch::Off && !recorded(ledger).unwrap_or(true) {
                for file in [
                    settings_file(paths, &req.name),
                    mcp_file(paths, &req.name),
                    member_file(paths, &req.name),
                ] {
                    let _ = std::fs::remove_file(file);
                }
            }
            return Err(e);
        }
    };
    let mut warnings = launch.warnings(&req.name);
    if let Some(why) = launch.unsandboxed() {
        let flagged = ledger.update(&req.name, &mut |r| {
            r.unsandboxed = Some(why.to_string());
            Ok(())
        });
        if let (Err(e), Some(warning)) = (flagged, warnings.first_mut()) {
            *warning += &format!("; not flagged in the ledger: {e:#}");
        }
    }
    Ok((spawned, warnings))
}

/// A worktree `add` or `feed` just checked out, with the local branch it created.
struct Fresh {
    dir: PathBuf,
    branch: Option<String>,
}

impl Fresh {
    fn remove(self, repo: &Path) -> Result<()> {
        let removed = match &self.branch {
            Some(branch) => crate::worktree::remove_worktree_and_branch(repo, &self.dir, branch),
            None => crate::worktree::remove_worktree(repo, &self.dir),
        };
        removed.with_context(|| match &self.branch {
            Some(branch) => format!(
                "its new worktree {} or branch {branch} is left",
                self.dir.display()
            ),
            None => format!("its new worktree {} is left", self.dir.display()),
        })
    }
}

/// Checks out a sandboxed worker's worktree, after spawn's own refusals and
/// those of its args and MCP servers, so `prepare` reads its project settings.
fn checkout_before_prepare(
    herdr: &dyn Herdr,
    paths: &Paths,
    req: &SpawnRequest,
    member: &Member,
    inherited_workspace: Option<&str>,
) -> Result<Option<Fresh>> {
    let worker = herd::worker(paths, &member.herd).ok();
    let (Some(checkout), Some(planned), Some(worker)) = (&req.worktree, &member.worktree, worker)
    else {
        return Ok(None);
    };
    if worker.sandbox == WorkerSandbox::Off {
        return Ok(None);
    }
    crate::spawn::preview(herdr, req, inherited_workspace)?;
    judge_args_and_mcp(paths, &req.name, member, &worker, ArgsFrom::Fresh)?;
    let branch = match checkout {
        Checkout::Branch(branch) if !crate::worktree::has_local_branch(&req.repo, branch) => {
            Some(branch.clone())
        }
        _ => None,
    };
    let fresh = !planned.exists();
    checkout.ensure(&req.repo).context("worktree")?;
    Ok(fresh.then(|| Fresh {
        dir: planned.clone(),
        branch,
    }))
}

/// Why running worker `record` is without the sandbox its herd's `[worker]`
/// turns on, with that herd and its mode; `None` when it has it or needn't.
pub fn running_unsandboxed(
    paths: &Paths,
    record: &AgentRecord,
) -> Result<Option<(String, String, WorkerSandbox)>> {
    let name = &record.name;
    let (herd, mode) = match Member::kept(paths, name)? {
        Some(member) => {
            let mode = herd_worker(paths, name, &member.herd)?.sandbox;
            (member.herd, mode)
        }
        // A member loopherd never sandboxed, whose herd file may since have gone.
        None => match &record.herd {
            Some(herd) if !record.may_be_lead() => {
                let mode = herd::worker(paths, herd).map_or(WorkerSandbox::Off, |w| w.sandbox);
                (herd.clone(), mode)
            }
            _ => return Ok(None),
        },
    };
    if mode == WorkerSandbox::Off
        || crate::sandbox::launched_with_settings(paths, name, &record.claude_args)
    {
        return Ok(None);
    }
    let why = record
        .unsandboxed
        .clone()
        .unwrap_or_else(|| "it was started without its sandbox".into());
    Ok(Some((why, herd, mode)))
}

/// Checks running worker `record` against its herd's `[worker] sandbox`
/// without writing anything: its recorded args must pass it its settings,
/// its kept args must be allowlisted, and the human's settings mustn't loosen
/// its sandbox. Returns why it runs unsandboxed under `prefer`, and the
/// warnings; `require` refuses what `prepare` would.
pub fn check_running(paths: &Paths, record: &AgentRecord) -> Result<(Option<String>, Vec<String>)> {
    let name = &record.name;
    let resume = format!("quit its Claude and run loopherd revive --name {name}");
    let mcp_unlocked = !record
        .claude_args
        .iter()
        .any(|a| a == "--strict-mcp-config");
    let mcp_warning = mcp_unlocked.then(|| {
        format!(
            "{name} was started with every MCP server you configure, not only its herd's \
             [worker] mcp_servers; {resume}"
        )
    });
    if let Some((why, herd, mode)) = running_unsandboxed(paths, record)? {
        let credentials = herd::merge_guard(paths, &herd)? == Some(MergeGuard::Credentials);
        let Some(otherwise) = unsandboxed_fix(&herd, credentials, mode, "run") else {
            let warning = format!("{name} runs unsandboxed ({why})");
            return Ok((
                Some(why),
                [warning].into_iter().chain(mcp_warning).collect(),
            ));
        };
        bail!("{name} can't run sandboxed: {why}; {resume} to resume it sandboxed, or {otherwise}");
    }
    let Some(member) = Member::kept(paths, name)? else {
        return Ok((None, vec![]));
    };
    let worker = herd_worker(paths, name, &member.herd)?;
    if worker.sandbox == WorkerSandbox::Off {
        return Ok((None, vec![]));
    }
    let Some(worktree) = &member.worktree else {
        return Ok((None, mcp_warning.into_iter().collect()));
    };
    let herd = &member.herd;
    let sources = arg_sources(paths, &member, ArgsFrom::Kept)?;
    let disallowed = disallowed(&sources);
    // Judged against what the running Claude started with.
    let own = match std::fs::read_to_string(settings_file(paths, name)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| json!({})),
        Err(_) => settings(paths, name, herd, worktree)?,
    };
    let loosened = loosenings(&paths.home, worktree, &own);
    if worker.sandbox == WorkerSandbox::Require {
        let prefer = format!(
            "or set [worker] sandbox = \"prefer\" in herd {herd} to run it with them as warnings"
        );
        if !disallowed.is_empty() {
            bail!(
                "{name} can't run sandboxed: its args may loosen its sandbox or permissions: {}; \
                 a sandboxed worker takes only {ALLOWED_ARGS}, so retire it and add it again \
                 from herd {herd}'s template {}, without those, {prefer}",
                disallowed.join("; "),
                member.template
            );
        }
        if !loosened.is_empty() {
            bail!(
                "{name} can't run sandboxed: your Claude settings loosen its sandbox: {}; \
                 remove those, {prefer}",
                loosened.join("; ")
            );
        }
    }
    let warnings = (loosened.iter().map(|line| loosened_warning(name, line)))
        .chain(disallowed.iter().map(|line| disallowed_warning(name, line)))
        .chain(mcp_warning)
        .collect();
    Ok((None, warnings))
}

/// What the human sets in herd `herd` to `verb` a worker unsandboxed, when
/// its sandbox refuses one; `None` when `prefer` already does. A
/// credentials herd's worker would hold their gh, so it needs `hook` too.
fn unsandboxed_fix(
    herd: &str,
    credentials: bool,
    mode: WorkerSandbox,
    verb: &str,
) -> Option<String> {
    let switches = match (credentials, mode) {
        (true, WorkerSandbox::Require) => {
            "[lead] merge_guard = \"hook\" and [worker] sandbox = \"prefer\""
        }
        (true, _) => "[lead] merge_guard = \"hook\"",
        (false, WorkerSandbox::Require) => "[worker] sandbox = \"prefer\"",
        (false, _) => return None,
    };
    let with = if credentials {
        " with your GitHub credentials"
    } else {
        ""
    };
    Some(format!(
        "set {switches} in herd {herd} to {verb} it unsandboxed{with}"
    ))
}

/// Herd `herd`'s `[worker]`, which worker `name` was kept under.
fn herd_worker(paths: &Paths, name: &str, herd: &str) -> Result<herd::Worker> {
    herd::worker(paths, herd).map_err(|e| {
        let gone = e.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        });
        match gone {
            true => anyhow::anyhow!(
                "{name} was started in herd {herd}'s worker sandbox, and herd {herd} is gone; \
                 restore its herd file, or run loopherd retire {name}"
            ),
            false => e,
        }
    })
}

pub fn settings_file(paths: &Paths, name: &str) -> PathBuf {
    crate::sandbox::settings_file(paths, name)
}

/// Worker `name`'s handoff and mailbox directories, the state it may write.
fn own_dirs(paths: &Paths, name: &str) -> [PathBuf; 2] {
    let state = paths.state_dir();
    [handoff::dir(state, name), state.join("reports").join(name)]
}

fn settings(paths: &Paths, name: &str, herd: &str, worktree: &Path) -> Result<Value> {
    let worker = herd::worker(paths, herd)?;
    let repo = herd::repo(paths, herd)?;
    // A repo that isn't one yet fails the spawn's own git calls.
    let git = crate::worktree::common_dir(&repo).unwrap_or_else(|_| repo.join(".git"));
    let state = paths.state_dir();
    let config = paths.config_dir();
    let claude = paths.home.join(".claude");
    let own = own_dirs(paths, name);
    let allow_write = [worktree.to_path_buf(), own[0].clone(), own[1].clone()];
    let deny_write = [
        config.to_path_buf(),
        claude.clone(),
        git.join("hooks"),
        git.join("config"),
    ];
    let mut deny = vec![
        edit_rule(config),
        edit_rule(&claude),
        edit_rule(&git.join("hooks")),
        file_rule(&git.join("config")),
        edit_rule(&state.join("paused")),
        edit_rule(&state.join("broker")),
        edit_rule(&paths.locks_dir()),
    ];
    deny.extend(
        [
            "ledger.json",
            "ledger.lock",
            "shepherd.json",
            "shepherd.json.lock",
        ]
        .map(|file| file_rule(&state.join(file))),
    );
    // A glob over these would cover the worker's own directory too, and on
    // Linux a named deny whose path later goes missing gets the sandbox's
    // placeholder there; elsewhere the sandbox makes none.
    if !cfg!(target_os = "linux") {
        for dir in ["reports", "handoffs"].map(|dir| state.join(dir)) {
            let others = entries_but(&dir, name)?;
            deny.extend(others.iter().map(|(path, is_dir)| match is_dir {
                true => edit_rule(path),
                false => file_rule(path),
            }));
        }
    }
    let mut allow = vec!["WebFetch(domain:*)".to_string()];
    allow.extend(own.iter().map(|dir| edit_rule(dir)));
    let paths_of = |list: &[PathBuf]| -> Vec<String> {
        list.iter().map(|p| p.display().to_string()).collect()
    };
    let mut settings = json!({
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "allowUnsandboxedCommands": false,
            "network": {"allowAllUnixSockets": worker.unix_sockets == Some(UnixSockets::All)},
            "filesystem": {
                "allowWrite": paths_of(&allow_write),
                "denyWrite": paths_of(&deny_write),
                "denyRead": [paths.locks_dir().display().to_string()],
            },
        },
        "permissions": {
            "denyInsteadOfPrompting": true,
            // Its cwd, the worktree, and these are all its file tools may write, whatever its mode.
            "blockWritesOutsideWorkingDirectories": true,
            "additionalDirectories": paths_of(&own),
            "allow": allow,
            "deny": deny,
        },
    });
    if herd::merge_guard(paths, herd)? == Some(MergeGuard::Credentials) {
        confine_credentials(
            paths,
            name,
            herd,
            &mut settings,
            [],
            &crate::sandbox::THIS_MACHINE,
        )
        .map_err(|e| NotStarted(format!("{e:#}")))?;
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Herd {
        _root: tempfile::TempDir,
        paths: Paths,
        repo: PathBuf,
    }

    fn herd(worker: &str) -> Herd {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        std::fs::create_dir_all(&paths.herds).unwrap();
        let repo = root.path().join("repo");
        std::fs::write(
            paths.herds.join("h.toml"),
            format!(
                "repo = {repo:?}\n[template.t]\nparams = []\nprompt = \"go\"\n[worker]\n{worker}\n"
            ),
        )
        .unwrap();
        Herd {
            _root: root,
            paths,
            repo,
        }
    }

    fn member(worktree: Option<&Path>) -> Member {
        Member {
            herd: "h".into(),
            template: "t".into(),
            args: vec!["--model".into(), "opus".into()],
            worktree: worktree.map(Path::to_path_buf),
        }
    }

    fn blocked() -> Result<Sockets, Unavailable> {
        Ok(Sockets::Blocked)
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_required_worker_may_write_only_its_worktree_handoffs_and_mailbox() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let state = h.paths.state_dir().to_path_buf();
        let config = h.paths.config_dir().to_path_buf();

        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let file = config.join("sandbox/w1.json");
        assert_eq!(
            launch.args(),
            [
                "--settings".to_string(),
                file.display().to_string(),
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                config.join("sandbox/w1.mcp.json").display().to_string(),
            ]
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let fs = &settings["sandbox"]["filesystem"];
        assert_eq!(
            strings(&fs["allowWrite"]),
            [
                worktree.display().to_string(),
                state.join("handoffs/w1").display().to_string(),
                state.join("reports/w1").display().to_string(),
            ]
        );
        let deny_write = strings(&fs["denyWrite"]);
        for denied in [
            config.clone(),
            PathBuf::from("/home/me/.claude"),
            h.repo.join(".git/hooks"),
            h.repo.join(".git/config"),
        ] {
            assert!(
                deny_write.contains(&denied.display().to_string()),
                "{denied:?}: {deny_write:?}"
            );
        }
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(settings["sandbox"]["network"]["allowAllUnixSockets"], false);
        assert_eq!(settings["permissions"]["denyInsteadOfPrompting"], true);
    }

    fn read_json(file: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    #[test]
    fn a_workers_file_tools_are_denied_the_config_dir_git_hooks_and_the_state_dir_but_its_own() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let state = h.paths.state_dir().to_path_buf();

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let settings = read_json(&settings_file(&h.paths, "w1"));
        let deny = strings(&settings["permissions"]["deny"]);
        let config = h.paths.config_dir();
        let git = h.repo.join(".git");
        for rule in [
            format!("Edit(/{}/**)", config.display()),
            "Edit(//home/me/.claude/**)".to_string(),
            format!("Edit(/{}/**)", git.join("hooks").display()),
            format!("Edit(/{})", git.join("config").display()),
            format!("Edit(/{})", state.join("ledger.json").display()),
            format!("Edit(/{})", state.join("shepherd.json").display()),
            format!("Edit(/{}/**)", state.join("paused").display()),
            format!("Edit(/{}/**)", state.join("broker").display()),
            format!("Edit(/{}/**)", state.join("locks").display()),
        ] {
            assert!(deny.contains(&rule), "{rule}: {deny:?}");
        }
        assert!(
            !deny
                .iter()
                .any(|rule| rule.contains("/reports/w1") || rule.contains("/handoffs/w1")),
            "{deny:?}"
        );
        assert_eq!(
            strings(&settings["permissions"]["allow"]),
            [
                "WebFetch(domain:*)".to_string(),
                format!("Edit(/{}/**)", state.join("handoffs/w1").display()),
                format!("Edit(/{}/**)", state.join("reports/w1").display()),
            ]
        );
        assert!(state.join("handoffs/w1").is_dir());
        assert!(state.join("reports/w1").is_dir());
    }

    #[test]
    fn a_workers_file_tools_may_write_only_its_worktree_handoffs_and_mailbox_in_any_mode() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let state = h.paths.state_dir().to_path_buf();

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let permissions = &read_json(&settings_file(&h.paths, "w1"))["permissions"];
        assert_eq!(permissions["blockWritesOutsideWorkingDirectories"], true);
        assert_eq!(
            strings(&permissions["additionalDirectories"]),
            [
                state.join("handoffs/w1").display().to_string(),
                state.join("reports/w1").display().to_string(),
            ]
        );
    }

    #[test]
    fn a_worker_without_a_worktree_is_refused_under_require_and_unsandboxed_under_prefer() {
        let h = herd("sandbox = \"require\"");
        let err = prepare(&h.paths, "w1", &member(None), ArgsFrom::Fresh, &blocked).unwrap_err();
        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: it has no worktree of its own, and its sandbox won't \
             open the main checkout; give herd h's template t a branch, or set [worker] \
             sandbox = \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(&h.paths, "w1", &member(None), ArgsFrom::Fresh, &blocked).unwrap();
        assert_eq!(
            launch.unsandboxed(),
            Some("it has no worktree of its own, and its sandbox won't open the main checkout")
        );
        assert!(!settings_file(&h.paths, "w1").exists());
    }

    #[test]
    fn a_sandboxed_worker_is_kept_with_its_herd_template_and_args() {
        let h = herd("sandbox = \"prefer\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let file = h.paths.config_dir().join("sandbox/w1.member.json");
        assert_eq!(
            read_json(&file),
            json!({
                "herd": "h",
                "template": "t",
                "args": ["--model", "opus"],
                "worktree": worktree,
            })
        );
    }

    #[test]
    fn a_herd_with_the_worker_sandbox_off_writes_nothing() {
        for worker in ["", "sandbox = \"off\"\nunix_sockets = \"all\""] {
            let h = herd(worker);
            let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

            let launch = prepare(
                &h.paths,
                "w1",
                &member(Some(&worktree)),
                ArgsFrom::Fresh,
                &blocked,
            )
            .unwrap();

            assert_eq!(launch, Launch::Off);
            assert_eq!(launch.args(), Vec::<String>::new());
            assert!(!h.paths.config_dir().join("sandbox").exists(), "{worker}");
        }
    }

    #[test]
    fn unix_sockets_all_opens_every_socket_to_the_herds_workers() {
        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let settings = read_json(&settings_file(&h.paths, "w1"));
        assert_eq!(settings["sandbox"]["network"]["allowAllUnixSockets"], true);
    }

    fn unavailable() -> Result<Sockets, Unavailable> {
        Err(Unavailable {
            why: "bubblewrap isn't installed".into(),
            fix: "install bubblewrap".into(),
        })
    }

    fn sockets_open() -> Result<Sockets, Unavailable> {
        Ok(Sockets::Open(Unavailable {
            why: "no seccomp filter".into(),
            fix: "npm install -g @anthropic-ai/sandbox-runtime".into(),
        }))
    }

    #[test]
    fn require_refuses_a_worker_whose_sandbox_cant_start_and_writes_nothing() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        let err = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &unavailable,
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its sandbox can't start: bubblewrap isn't installed; \
             install bubblewrap, or set [worker] sandbox = \"prefer\" in herd h to start it \
             unsandboxed"
        );
        assert!(!h.paths.config_dir().join("sandbox").exists());
    }

    #[test]
    fn prefer_starts_a_worker_whose_sandbox_cant_start_unsandboxed_and_kept() {
        let h = herd("sandbox = \"prefer\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &unavailable,
        )
        .unwrap();

        let mcp = mcp_file(&h.paths, "w1");
        assert_eq!(
            launch,
            Launch::Unsandboxed {
                why: "bubblewrap isn't installed".into(),
                fix: "install bubblewrap".into(),
                mcp: mcp.clone(),
                warnings: vec![],
            }
        );
        assert_eq!(
            launch.args(),
            [
                "--strict-mcp-config",
                "--mcp-config",
                &mcp.display().to_string()
            ]
        );
        assert_eq!(
            launch.warnings("w1"),
            ["w1 runs unsandboxed (bubblewrap isn't installed); install bubblewrap"]
        );
        assert!(!settings_file(&h.paths, "w1").exists());
        assert!(member_file(&h.paths, "w1").is_file());
    }

    #[test]
    fn a_sandbox_that_cant_block_sockets_is_refused_under_require_and_warned_under_prefer() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let err = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &sockets_open,
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its sandbox can't block Unix sockets: no seccomp \
             filter; npm install -g @anthropic-ai/sandbox-runtime, or set [worker] sandbox = \
             \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &sockets_open,
        )
        .unwrap();
        assert_eq!(
            launch.unsandboxed(),
            Some("its sandbox can't block Unix sockets: no seccomp filter")
        );

        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &sockets_open,
        )
        .unwrap();
        assert_eq!(launch.args()[0], "--settings");
    }
    /// Herd `h` with herd args `herd_args`, template `t`'s `template_args`,
    /// `[worker]` `worker`, and a home of its own.
    fn herd_with_args(worker: &str, herd_args: &str, template_args: &str) -> Herd {
        let mut h = herd(worker);
        std::fs::write(
            h.paths.herds.join("h.toml"),
            format!(
                "repo = {:?}\nargs = [{herd_args}]\n[template.t]\nparams = []\nprompt = \"go\"\n\
                 args = [{template_args}]\n[worker]\n{worker}\n",
                h.repo
            ),
        )
        .unwrap();
        h.paths.home = h._root.path().join("home");
        std::fs::create_dir_all(h.paths.home.join(".claude")).unwrap();
        h
    }

    fn with_args(worktree: &Path, args: &[&str]) -> Member {
        Member {
            args: args.iter().map(|a| a.to_string()).collect(),
            ..member(Some(worktree))
        }
    }

    #[test]
    fn require_refuses_a_worker_the_users_or_worktrees_settings_loosen_naming_each_file_and_key() {
        let h = herd_with_args("sandbox = \"require\"", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let user = h.paths.home.join(".claude/settings.json");
        std::fs::write(&user, r#"{"sandbox": {"excludedCommands": ["docker"]}}"#).unwrap();
        let local = worktree.join(".claude/settings.local.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(
            &local,
            r#"{"permissions": {"defaultMode": "bypassPermissions"}}"#,
        )
        .unwrap();

        let err = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap_err();

        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            format!(
                "w1 can't start sandboxed: your Claude settings loosen its sandbox: {}: \
                 sandbox.excludedCommands runs \"docker\" outside it; {}: \
                 permissions.defaultMode bypassPermissions lets its file tools write anywhere; \
                 remove those, or set [worker] sandbox = \"prefer\" in herd h to start it with \
                 them as warnings",
                user.display(),
                local.display()
            )
        );
        assert!(!settings_file(&h.paths, "w1").exists());
        assert!(!member_file(&h.paths, "w1").exists());
    }

    #[test]
    fn prefer_starts_a_worker_the_users_settings_loosen_sandboxed_with_a_warning() {
        let h = herd_with_args("sandbox = \"prefer\"", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let user = h.paths.home.join(".claude/settings.json");
        std::fs::write(
            &user,
            r#"{"sandbox": {"filesystem": {"allowWrite": ["/srv"]}}}"#,
        )
        .unwrap();

        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        assert_eq!(launch.unsandboxed(), None);
        assert_eq!(launch.args()[0], "--settings");
        assert_eq!(
            launch.warnings("w1"),
            [format!(
                "w1's sandbox is loosened by {}: sandbox.filesystem.allowWrite opens /srv to writes",
                user.display()
            )]
        );
    }

    #[test]
    fn a_worker_gets_only_the_user_scope_mcp_servers_its_herd_names() {
        let h = herd_with_args("sandbox = \"require\"\nmcp_servers = [\"docs\"]", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        std::fs::write(
            h.paths.home.join(".claude.json"),
            r#"{"mcpServers": {"docs": {"command": "docs-mcp"}, "browser": {"command": "pw"}}}"#,
        )
        .unwrap();

        let launch = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let mcp = mcp_file(&h.paths, "w1");
        assert_eq!(
            launch.args()[2..],
            [
                "--strict-mcp-config",
                "--mcp-config",
                &mcp.display().to_string()
            ]
        );
        assert_eq!(
            read_json(&mcp),
            json!({"mcpServers": {"docs": {"command": "docs-mcp"}}})
        );
        let mode = std::fs::metadata(&mcp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_worker_whose_herd_names_no_mcp_servers_gets_none() {
        let h = herd_with_args("sandbox = \"prefer\"", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &unavailable,
        )
        .unwrap();

        assert_eq!(
            read_json(&mcp_file(&h.paths, "w1")),
            json!({"mcpServers": {}})
        );
    }

    #[test]
    fn an_mcp_server_the_user_scope_lacks_refuses_the_worker_under_prefer_too() {
        let h = herd_with_args("sandbox = \"prefer\"\nmcp_servers = [\"docs\"]", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        let err = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap_err();

        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            format!(
                "w1 can't start: herd h's [worker] mcp_servers names \"docs\", which {} doesn't \
                 configure in your user scope; add it with claude mcp add --scope user, or drop \
                 it from mcp_servers (local and project scopes, claude.ai connectors and \
                 plugins' servers can't be given to a worker)",
                h.paths.home.join(".claude.json").display()
            )
        );
        assert!(!h.paths.config_dir().join("sandbox").exists());
    }

    #[test]
    fn an_unreadable_user_scope_refuses_only_the_worker_that_needs_it() {
        let h = herd_with_args("sandbox = \"prefer\"\nmcp_servers = [\"docs\"]", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        std::fs::write(h.paths.home.join(".claude.json"), "{not json").unwrap();

        let err = prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree)),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap_err();

        assert!(err.downcast_ref::<NotStarted>().is_some(), "{err:#}");
        assert!(
            format!("{err:#}").starts_with("w1 can't start: its MCP servers: parse "),
            "{err:#}"
        );
    }

    #[test]
    fn require_refuses_args_outside_the_allowlist_naming_each_and_its_source() {
        let h = herd_with_args(
            "sandbox = \"require\"",
            r#""--add-dir", "/a""#,
            r#""--verbose""#,
        );
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let args = [
            "--add-dir",
            "/a",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
        ];

        let err = prepare(
            &h.paths,
            "w1",
            &with_args(&worktree, &args),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap_err();

        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its args may loosen its sandbox or permissions: \
             --add-dir /a, from herd h's args; --permission-mode bypassPermissions, from the \
             command line; a sandboxed worker takes only --model, --effort, --verbose and \
             --permission-mode default, manual, auto or plan, so drop those, or set [worker] \
             sandbox = \"prefer\" in herd h to start it with them as warnings"
        );
        assert!(!h.paths.config_dir().join("sandbox").exists());
    }

    #[test]
    fn prefer_starts_a_worker_with_args_outside_the_allowlist_warned() {
        let h = herd_with_args("sandbox = \"prefer\"", "", r#""--allowedTools", "Write""#);
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let args = ["--allowedTools", "Write"];

        let launch = prepare(
            &h.paths,
            "w1",
            &with_args(&worktree, &args),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        assert_eq!(launch.args()[0], "--settings");
        assert_eq!(
            launch.warnings("w1"),
            [
                "w1 starts with an arg that may loosen its sandbox or permissions: --allowedTools \
              Write, from template t's args"
            ]
        );
    }

    #[test]
    fn a_kept_worker_is_judged_by_the_args_it_was_started_with() {
        let h = herd_with_args("sandbox = \"require\"", "", "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let args = ["--dangerously-skip-permissions"];

        let err = prepare(
            &h.paths,
            "w1",
            &with_args(&worktree, &args),
            ArgsFrom::Kept,
            &blocked,
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its args may loosen its sandbox or permissions: \
             --dangerously-skip-permissions, from the args it was started with; a sandboxed \
             worker takes only --model, --effort, --verbose and --permission-mode default, \
             manual, auto or plan, so retire it and add it again from herd h's template t, \
             without those, or set [worker] sandbox = \"prefer\" in herd h to start it with \
             them as warnings"
        );
    }

    #[test]
    fn a_worker_passing_loopherds_own_launch_flags_is_refused_under_prefer_too() {
        let h = herd_with_args("sandbox = \"prefer\"", r#""--mcp-config", "x.json""#, "");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let args = ["--mcp-config", "x.json"];

        let err = prepare(
            &h.paths,
            "w1",
            &with_args(&worktree, &args),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap_err();

        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            "w1's args pass --mcp-config, which its launch takes; drop it from herd h's args, \
             and name the servers it may use in [worker] mcp_servers"
        );
    }

    #[test]
    fn a_herds_args_and_templates_args_are_judged_for_its_sandboxed_workers() {
        let h = herd_with_args(
            "sandbox = \"require\"",
            r#""--settings", "s.json", "--model", "opus""#,
            r#""--add-dir", "/a""#,
        );

        let found = configured_arg_findings(&h.paths, "h").unwrap();

        assert_eq!(
            found.flags,
            [(
                "--settings, from herd h's args, which a sandboxed worker's launch takes".into(),
                "drop it, and put those settings in ~/.claude/settings.json".into()
            )]
        );
        assert_eq!(found.disallowed, ["--add-dir /a, from template t's args"]);
    }

    /// Herd `h` on a GitHub repo, its lead guarded by credentials, with its key and token.
    fn credentials_herd(worker: &str) -> Herd {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        std::fs::create_dir_all(&paths.herds).unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let url = "https://github.com/acme/widgets.git";
        crate::worktree::testing::git(&repo, &["remote", "set-url", "origin", url]).unwrap();
        std::fs::write(
            paths.herds.join("h.toml"),
            format!(
                "repo = {repo:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
                 [lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n[worker]\n{worker}\n"
            ),
        )
        .unwrap();
        let credentials = paths.config_dir().join("credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        std::fs::write(credentials.join("h.key"), "a private key\n").unwrap();
        std::fs::write(credentials.join("h.token"), "github_pat_pr_only\n").unwrap();
        Herd {
            _root: root,
            paths,
            repo,
        }
    }

    fn worktree_of(h: &Herd) -> PathBuf {
        h.repo.parent().unwrap().join("worktrees/feat-x")
    }

    #[test]
    fn a_sandboxed_worker_in_a_credentials_herd_gets_the_herds_key_and_token_and_not_the_humans() {
        let h = credentials_herd("sandbox = \"require\"");

        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree_of(&h))),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();

        let settings = read_json(&settings_file(&h.paths, "w1"));
        let fetch = "url.git@loopherd-h:acme/widgets.git.insteadOf";
        let push = "url.git@loopherd-h:acme/widgets.git.pushInsteadOf";
        assert_eq!(
            settings["env"],
            json!({
                "GH_TOKEN": "github_pat_pr_only",
                "SSH_AUTH_SOCK": "",
                "GITHUB_TOKEN": "",
                "GH_ENTERPRISE_TOKEN": "",
                "GITHUB_ENTERPRISE_TOKEN": "",
                "GITHUB_PERSONAL_ACCESS_TOKEN": "",
                "GITHUB_PAT": "",
                "GH_PAT": "",
                "GITHUB_API_TOKEN": "",
                "HOMEBREW_GITHUB_API_TOKEN": "",
                "GIT_CONFIG_COUNT": "5",
                "GIT_CONFIG_KEY_0": "credential.helper",
                "GIT_CONFIG_VALUE_0": "",
                "GIT_CONFIG_KEY_1": fetch,
                "GIT_CONFIG_VALUE_1": "https://github.com/acme/widgets.git",
                "GIT_CONFIG_KEY_2": push,
                "GIT_CONFIG_VALUE_2": "https://github.com/acme/widgets.git",
                "GIT_CONFIG_KEY_3": fetch,
                "GIT_CONFIG_VALUE_3": "git@github.com:acme/widgets.git",
                "GIT_CONFIG_KEY_4": push,
                "GIT_CONFIG_VALUE_4": "git@github.com:acme/widgets.git",
            })
        );
        let config = h.paths.config_dir().display().to_string();
        let under = |path: &str| format!("{config}/{path}");
        let fs = &settings["sandbox"]["filesystem"];
        assert_eq!(
            fs["allowRead"],
            json!([
                "/home/me/.ssh/config",
                "/home/me/.ssh/known_hosts",
                under("herds"),
                under("config.toml"),
                under("trello.env"),
                under("ssh/config"),
                under("credentials/h.key"),
            ])
        );
        let deny_read = strings(&fs["denyRead"]);
        let hidden = [
            "/home/me/.ssh",
            "/home/me/.netrc",
            "/home/me/.config/gh",
            "/home/me/.git-credentials",
            "/home/me/.config/git/credentials",
            "/home/me/.cache/git/credential",
            "/home/me/.claude.json",
            "/home/me/.docker/config.json",
            "/home/me/.npmrc",
            "/home/me/.config/github-copilot",
            "/home/me/.claude/backups",
            &config,
        ];
        assert_eq!(deny_read[..12], hidden);
        let locks = h.paths.locks_dir().display().to_string();
        assert_eq!(deny_read.last(), Some(&locks));
        let deny = strings(&settings["permissions"]["deny"]);
        for rule in [
            "Read(//home/me/.ssh/**)",
            "Read(//home/me/.netrc)",
            "Read(//home/me/.config/gh/**)",
            "Read(//home/me/.git-credentials)",
            "Read(//home/me/.claude.json)",
            "Read(//home/me/.claude/backups/**)",
            &format!("Read(/{config}/**)"),
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule}: {deny:?}");
        }
    }

    #[test]
    fn a_credentials_herds_worker_without_its_herds_key_or_token_isnt_started_under_either_mode() {
        for mode in ["require", "prefer"] {
            for file in ["h.key", "h.token"] {
                let h = credentials_herd(&format!("sandbox = \"{mode}\""));
                let missing = h.paths.config_dir().join("credentials").join(file);
                std::fs::remove_file(&missing).unwrap();

                let err = prepare(
                    &h.paths,
                    "w1",
                    &member(Some(&worktree_of(&h))),
                    ArgsFrom::Fresh,
                    &blocked,
                )
                .unwrap_err();

                assert!(err.downcast_ref::<NotStarted>().is_some(), "{mode} {file}");
                let shown = format!("{err:#}");
                assert!(
                    shown.starts_with(&format!(
                        "w1 can't start with merge_guard = \"credentials\": no {} at {}",
                        if file == "h.key" {
                            "deploy key"
                        } else {
                            "token"
                        },
                        missing.display()
                    )),
                    "{shown}"
                );
                assert!(
                    !h.paths.config_dir().join("sandbox").exists(),
                    "{mode} {file}"
                );
            }
        }
    }

    #[test]
    fn a_credentials_herds_worker_that_would_run_unsandboxed_isnt_started_under_either_mode() {
        type Probe = fn() -> Result<Sockets, Unavailable>;
        let cases: [(&str, bool, Probe); 3] = [
            (
                "its sandbox can't start: bubblewrap isn't installed; install bubblewrap",
                true,
                unavailable,
            ),
            (
                "its sandbox can't block Unix sockets: no seccomp filter; npm install -g \
                 @anthropic-ai/sandbox-runtime",
                true,
                sockets_open,
            ),
            (
                "it has no worktree of its own, and its sandbox won't open the main checkout; \
                 give herd h's template t a branch",
                false,
                blocked,
            ),
        ];
        for (mode, switches) in [
            (
                "require",
                "[lead] merge_guard = \"hook\" and [worker] sandbox = \"prefer\"",
            ),
            ("prefer", "[lead] merge_guard = \"hook\""),
        ] {
            for (why, has_worktree, probe) in &cases {
                let h = credentials_herd(&format!("sandbox = \"{mode}\""));
                let worktree = has_worktree.then(|| worktree_of(&h));

                let err = prepare(
                    &h.paths,
                    "w1",
                    &member(worktree.as_deref()),
                    ArgsFrom::Fresh,
                    probe,
                )
                .unwrap_err();

                assert!(err.downcast_ref::<NotStarted>().is_some());
                assert_eq!(
                    format!("{err:#}"),
                    format!(
                        "w1 can't start sandboxed: {why}, or set {switches} in herd h to start \
                         it unsandboxed with your GitHub credentials"
                    ),
                    "{mode}"
                );
                assert!(!h.paths.config_dir().join("sandbox").exists(), "{mode}");
            }
        }
    }

    #[test]
    fn a_credentials_herds_worker_with_every_unix_socket_open_isnt_started_under_either_mode() {
        for mode in ["require", "prefer"] {
            let h = credentials_herd(&format!("sandbox = \"{mode}\"\nunix_sockets = \"all\""));

            let err = prepare(
                &h.paths,
                "w1",
                &member(Some(&worktree_of(&h))),
                ArgsFrom::Fresh,
                &blocked,
            )
            .unwrap_err();

            assert!(err.downcast_ref::<NotStarted>().is_some(), "{mode}");
            assert_eq!(
                format!("{err:#}"),
                "w1 can't start with merge_guard = \"credentials\": unix_sockets = \"all\" opens \
                 your ssh agent, Git Credential Manager through WSL2 interop and the Secret \
                 Service keyring to it, each of which acts as you; run loopherd herd unset h \
                 worker.unix_sockets, or loopherd herd set h lead.merge_guard '\"hook\"'",
                "{mode}"
            );
            assert!(!h.paths.config_dir().join("sandbox").exists(), "{mode}");
        }
    }

    /// Runs `script` in bash in `dir` with `env`, a fake `gh` that answers as
    /// GitHub does for the herd's PR-only token, and a fake `ssh` that serves
    /// any host from the bare repos under `remotes`, logging each host it's asked for.
    fn as_worker_shell(
        dir: &Path,
        env: &serde_json::Map<String, Value>,
        remotes: &Path,
        script: &str,
    ) -> std::process::Output {
        let bin = remotes.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fakes = [
            (
                "gh",
                "echo \"$1 $2 as ${GH_TOKEN:-gho_human}\" >> \"$GH_LOG\"\n\
                 case \"$1 $2\" in\n\
                 'auth token') echo gho_human ;;\n\
                 'pr create') echo https://github.com/acme/widgets/pull/1 ;;\n\
                 'pr merge') [ \"$GH_TOKEN\" = github_pat_pr_only ] || exit 0\n\
                   echo 'GraphQL: Resource not accessible by personal access token \
                 (mergePullRequest)' >&2; exit 1 ;;\n\
                 esac\n",
            ),
            (
                "ssh",
                "for arg; do case $arg in -*) ;; *) echo \"$arg\" >> \"$SSH_LOG\"; break ;; \
                 esac; done\ncd \"$REMOTES\" && eval \"${@: -1}\"\n",
            ),
        ];
        for (name, body) in fakes {
            let file = bin.join(name);
            std::fs::write(&file, format!("#!/bin/bash\n{body}")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        std::process::Command::new("bash")
            .args(["-c", script])
            .current_dir(dir)
            .env_clear()
            .env("PATH", path)
            .env("HOME", remotes)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_SSH_COMMAND", bin.join("ssh"))
            .env("SSH_LOG", remotes.join("ssh.log"))
            .env("GH_LOG", remotes.join("gh.log"))
            .env("REMOTES", remotes)
            .envs(env.iter().map(|(k, v)| (k, v.as_str().unwrap())))
            .output()
            .unwrap()
    }

    #[test]
    fn a_credentials_herds_worker_pushes_and_opens_prs_on_the_herds_key_and_token_but_cant_merge() {
        let h = credentials_herd("sandbox = \"require\"");
        prepare(
            &h.paths,
            "w1",
            &member(Some(&worktree_of(&h))),
            ArgsFrom::Fresh,
            &blocked,
        )
        .unwrap();
        let settings = read_json(&settings_file(&h.paths, "w1"));
        let env = settings["env"].as_object().unwrap();
        let remotes = h.repo.parent().unwrap().join("remotes");
        let bare = remotes.join("acme/widgets.git");
        crate::worktree::testing::git(
            h.repo.parent().unwrap(),
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        )
        .unwrap();
        // As init writes them for a credentials herd.
        let token = "GH_TOKEN=${GH_TOKEN:-$(gh auth token --user acme)}";
        let line = |command: &str| format!("{token} {command}");

        let pushed = as_worker_shell(
            &h.repo,
            env,
            &remotes,
            &line(
                "git -c credential.helper= -c credential.helper='!gh auth git-credential' \
                 push -q origin HEAD:refs/heads/feat-x",
            ),
        );
        let created = as_worker_shell(&h.repo, env, &remotes, &line("gh pr create --fill"));
        let merged = as_worker_shell(&h.repo, env, &remotes, &line("gh pr merge 1 --merge"));

        let shown = |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(pushed.status.success(), "{}", shown(&pushed));
        assert_eq!(
            std::fs::read_to_string(remotes.join("ssh.log")).unwrap(),
            "git@loopherd-h\n"
        );
        crate::worktree::testing::git(&bare, &["rev-parse", "--verify", "feat-x"]).unwrap();
        assert!(created.status.success(), "{}", shown(&created));
        assert_eq!(
            std::fs::read_to_string(remotes.join("gh.log")).unwrap(),
            "pr create as github_pat_pr_only\npr merge as github_pat_pr_only\n",
            "neither asks for the human's token"
        );
        assert!(!merged.status.success());
        assert_eq!(
            shown(&merged),
            "GraphQL: Resource not accessible by personal access token (mergePullRequest)\n"
        );
        let human = as_worker_shell(
            &h.repo,
            &Default::default(),
            &remotes,
            &line("gh pr merge 1 --merge"),
        );
        assert!(human.status.success(), "the human's own gh merges");
    }

    #[test]
    fn a_worker_in_a_hook_or_merging_herd_keeps_the_humans_credentials() {
        for lead in [
            "merge_guard = \"hook\"",
            "merge = true\nmerge_guard = \"credentials\"",
        ] {
            let h = herd("sandbox = \"require\"");
            let toml = h.paths.herds.join("h.toml");
            let text = std::fs::read_to_string(&toml).unwrap();
            std::fs::write(&toml, format!("{text}[lead]\nprompt = \"go\"\n{lead}\n")).unwrap();

            prepare(
                &h.paths,
                "w1",
                &member(Some(&worktree_of(&h))),
                ArgsFrom::Fresh,
                &blocked,
            )
            .unwrap();

            let settings = read_json(&settings_file(&h.paths, "w1"));
            assert_eq!(settings.get("env"), None, "{lead}");
            assert_eq!(settings["sandbox"]["filesystem"].get("allowRead"), None);
            assert!(
                !settings["permissions"]["deny"]
                    .to_string()
                    .contains("Read(")
            );
        }
    }
}
