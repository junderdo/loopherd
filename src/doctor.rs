//! `loopherd doctor`: whether this machine's loopherd install, herdr, config,
//! herd files and state are sound. It only reads, and names the command that
//! fixes each problem.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use anyhow::{Result, bail};

use crate::herd::MergeGuard;
use crate::herd::Paths;
use crate::herdr::{Herdr, Server};
use crate::ledger::AgentRecord;
use crate::ledger::{FileLedger, Ledger};
use crate::merge_guard::{DEFAULT_BRANCH_RULESET, FORCE_PUSH_RULESET};
use crate::pause::Reading;
use crate::sandbox::{SandboxMode, Sockets, Unavailable};
use crate::worker_sandbox::WorkerSandbox;
use serde_json::Value;

/// What the install checks read, gathered by the caller.
pub struct Install {
    pub settings: PathBuf,
    /// The settings file came from `--settings`, so each fix names it too.
    pub settings_given: bool,
    pub pane_label: bool,
    /// The `loopherd` a shell finds on PATH.
    pub on_path: Option<PathBuf>,
    pub running: PathBuf,
    /// Why Claude Code's sandbox can't start on this machine, if it can't.
    /// Whether it keeps Unix sockets closed when it does.
    pub sandbox: Result<Sockets, Unavailable>,
    /// WSL2 lets this machine run Windows programs.
    pub wsl_interop: bool,
    /// Where `install-hook` puts the skill for these settings; `None` without `HOME`.
    pub skill: Option<crate::skill::Layout>,
    /// The lead whose broker runs this doctor, as an `after_merge` of its feed.
    pub in_broker_of: Option<String>,
    /// How long a broker not yet on the loopherd on PATH gets to move onto it.
    pub broker_move_grace: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Failure,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

/// The oldest herdr loopherd is tested against.
pub const MIN_HERDR: &str = "0.9.5";

/// Where the config, herd and state checks read, gathered by the caller.
pub struct Setup {
    pub paths: Paths,
    pub ledger: FileLedger,
}

/// Every check, the herd, repo and state ones only for `herd` when given.
/// `setup` is `Err` when config and state can't be found; then they warn.
/// Fails when `herd` has no herd file.
pub fn checks(
    install: &Install,
    setup: &Result<Setup>,
    herd: Option<&str>,
    herdr: &dyn Herdr,
) -> Result<Vec<Check>> {
    if let (Some(herd), Ok(setup)) = (herd, setup) {
        crate::herd::require(&setup.paths, herd)?;
    }
    let mut checks = hook_checks(install);
    checks.extend(skill_check(install));
    checks.push(running_binary(install));
    checks.push(sandbox_check(install, setup));
    let (herdr_checks, reached) = herdr_checks(herdr);
    checks.extend(herdr_checks);
    match setup {
        Ok(setup) => {
            checks.extend(config_checks(&setup.paths, &install.settings));
            checks.extend(herd_checks(&setup.paths, herd));
            checks.extend(unix_socket_checks(install, &setup.paths, herd));
            checks.extend(state_checks(
                setup,
                herd,
                reached.map(|socket| (herdr, socket)),
                &BrokersOwn::of(install),
            ));
        }
        Err(e) if herd.is_some() => bail!("can't find herd files: {e:#}"),
        Err(e) => checks.push(Check {
            name: "config and state".into(),
            status: Status::Warning,
            detail: format!("not checked: {e:#}"),
            fix: Some("set HOME".into()),
        }),
    }
    Ok(checks)
}

/// Fails only while some herd has a lead to run in the sandbox, or, for a
/// filter that can't block Unix sockets, a lead it would refuse.
fn sandbox_check(install: &Install, setup: &Result<Setup>) -> Check {
    let (why, fix) = match &install.sandbox {
        Ok(Sockets::Blocked) => return ok("sandbox", "can start".into()),
        Ok(Sockets::Open(Unavailable { why, fix })) => {
            return sockets_check(why, fix, setup);
        }
        Err(Unavailable { why, fix }) => (why, fix),
    };
    let has_lead = |paths: &Paths| {
        crate::herd::names(&paths.herds).is_ok_and(|herds| {
            herds
                .iter()
                .any(|herd| crate::herd::lead_sandbox(paths, herd).is_ok())
        })
    };
    match setup {
        Ok(setup) if !has_lead(&setup.paths) => warning(
            "sandbox",
            format!("can't start: {why}; no herd has a lead to run in it yet"),
            fix.clone(),
        ),
        _ => failure("sandbox", format!("can't start: {why}"), fix.clone()),
    }
}

fn sockets_check(why: &str, fix: &str, setup: &Result<Setup>) -> Check {
    use crate::sandbox::WhenLoosened;
    let refused = |paths: &Paths| {
        crate::herd::names(&paths.herds).is_ok_and(|herds| {
            herds.iter().any(|herd| {
                crate::herd::lead_sandbox(paths, herd).is_ok()
                    && !crate::sandbox::sockets_open(paths, herd).unwrap_or(false)
                    && crate::sandbox::when_loosened(paths, herd)
                        .is_ok_and(|when| when != WhenLoosened::Warn)
            })
        })
    };
    let detail = format!("can't block Unix sockets: {why}");
    match setup {
        Ok(setup) if !refused(&setup.paths) => warning("sandbox", detail, fix.into()),
        _ => failure("sandbox", detail, fix.into()),
    }
}

/// A running lead without its sandbox, by `prefer` or from before it had one.
pub(crate) fn lead_sandbox_check(paths: &Paths, lead: &AgentRecord) -> Option<Check> {
    let started_with_it =
        crate::sandbox::launched_with_settings(paths, &lead.name, &lead.claude_args);
    let why = match &lead.unsandboxed {
        Some(why) => why.as_str(),
        None if started_with_it => {
            let why = lead.sockets_unblocked.as_deref()?;
            return Some(warning(
                &format!("lead {}", lead.name),
                crate::sandbox::sockets_unblocked_warning(&lead.name, why),
                format!(
                    "fix the sandbox check's cause, then quit its Claude and run loopherd \
                     revive --name {}",
                    lead.name
                ),
            ));
        }
        None => "it was started without its sandbox",
    };
    Some(warning(
        &format!("lead {}", lead.name),
        crate::sandbox::hook_only_warning(&lead.name, why),
        format!(
            "quit its Claude and run loopherd revive --name {}",
            lead.name
        ),
    ))
}

/// A running worker without the sandbox its herd's `[worker]` turns on.
fn worker_sandbox_check(paths: &Paths, worker: &AgentRecord) -> Option<Check> {
    let name = &worker.name;
    let (why, _, _) = match crate::worker_sandbox::running_unsandboxed(paths, worker) {
        Ok(running) => running?,
        Err(e) => {
            return Some(warning(
                &format!("worker {name}"),
                format!("its sandbox isn't checked: {e:#}"),
                "fix that, then run loopherd doctor again".into(),
            ));
        }
    };
    Some(warning(
        &format!("worker {name}"),
        format!("{name} runs unsandboxed ({why})"),
        format!("quit its Claude and run loopherd revive --name {name}"),
    ))
}

/// A lead whose launch record keeps no `PATH`, so its watch and broker run
/// under whatever `PATH` they inherit.
fn lead_path_check(paths: &Paths, lead: &str) -> Option<Check> {
    let kept = crate::sandbox::Lead::kept(paths, lead).ok()??;
    kept.path.is_none().then(|| {
        warning(
            &format!("lead {lead}"),
            format!(
                "{lead}'s launch record keeps no PATH, so its watch and broker run gh, git \
                 and its herd's checks from whatever PATH they inherit"
            ),
            format!("quit its Claude and run loopherd revive --name {lead}"),
        )
    })
}

fn config_checks(paths: &Paths, settings: &Path) -> Vec<Check> {
    let config = crate::herd::config_file(paths);
    let by_hand = format!("fix {} by hand", config.display());
    let mut checks = vec![];
    let file = match crate::config::read(&config) {
        Ok(file) => {
            checks.push(ok("config", config.display().to_string()));
            Some(file)
        }
        Err(e) => {
            checks.push(failure("config", message(&e), by_hand.clone()));
            None
        }
    };
    let self_restart = file
        .as_ref()
        .and_then(|f| f.shepherd.as_ref()?.self_restart)
        .unwrap_or(false);
    if self_restart {
        checks.extend(self_restart_checks(&config, settings));
    }
    if crate::herd::parses_as_toml(&config) {
        match crate::notify::load(&config) {
            Err(e) => checks.push(failure("notify", message(&e), by_hand)),
            Ok(Some(notify)) if notify.sends_nothing() => checks.push(Check {
                name: "notify".into(),
                status: Status::Warning,
                detail: crate::herd::sends_nothing_warning(&config),
                fix: Some(format!(
                    "turn on one of them, or [notify] enabled = false, {by_hand}"
                )),
            }),
            Ok(_) => {}
        }
    }
    let Some(file) = file else {
        return checks;
    };
    let unset = |name: &str, is_unset: bool| match is_unset {
        true => Check {
            name: name.into(),
            status: Status::Warning,
            detail: format!("unset in {}", config.display()),
            fix: Some("loopherd setup".into()),
        },
        false => ok(name, format!("set in {}", config.display())),
    };
    let shepherd = file.shepherd.as_ref().and_then(|s| s.context_limit);
    let limits_unset =
        file.context_window.is_none() && file.default_context_limit.is_none() && shepherd.is_none();
    checks.push(unset("lead defaults", file.lead_defaults.is_none()));
    checks.push(unset("context limits", limits_unset));
    checks
}

/// With `[shepherd] self_restart` on: a prompt can't cancel a self-restart
/// without the UserPromptSubmit hook, and a give-up goes unseen unless
/// `[notify]` sends it.
fn self_restart_checks(config: &Path, settings: &Path) -> Vec<Check> {
    let mut checks = vec![];
    if !crate::install_hook::prompt_hook_installed(settings).unwrap_or(false) {
        checks.push(warning(
            "self_restart",
            format!(
                "[shepherd] self_restart is on, but loopherd's UserPromptSubmit hook isn't in {}, \
                 so restart --self refuses",
                settings.display()
            ),
            "loopherd install-hook".into(),
        ));
    }
    let sends = crate::notify::load(config)
        .is_ok_and(|n| n.is_some_and(|n| n.sends(crate::notify::Kind::SelfRestart)));
    if !sends {
        checks.push(warning(
            "self_restart",
            "[shepherd] self_restart is on, but [notify] doesn't send self_restart, so a give-up \
             shows only on the shepherd's pane"
                .into(),
            format!(
                "set [notify] enabled = true with self_restart in its kinds, in {}",
                config.display()
            ),
        ));
    }
    checks
}

fn herd_checks(paths: &Paths, only: Option<&str>) -> Vec<Check> {
    let herds = match only {
        Some(herd) => vec![herd.to_string()],
        None => match crate::herd::names(&paths.herds) {
            Ok(names) => names,
            Err(e) => {
                let fix = format!("fix {} by hand", paths.herds.display());
                return vec![failure("herd files", message(&e), fix)];
            }
        },
    };
    let mut checks = vec![];
    for herd in herds {
        let file = paths.herds.join(format!("{herd}.toml"));
        let name = format!("herd {herd}");
        let repo = match crate::herd::checked_repo(paths, &herd) {
            Ok(repo) => repo,
            Err(invalid) => {
                let fix = format!("fix {} by hand", file.display());
                checks.push(failure(&name, invalid, fix));
                continue;
            }
        };
        checks.push(ok(&name, file.display().to_string()));
        let name = format!("herd {herd} repo");
        let set_repo = format!("loopherd herd set {herd} repo '\"<path>\"'");
        checks.push(match repo.exists() {
            false => failure(&name, format!("{} is missing", repo.display()), set_repo),
            true if !repo.is_dir() => failure(
                &name,
                format!("{} is not a directory", repo.display()),
                set_repo,
            ),
            // A worktree's `.git` is a file.
            true if !repo.join(".git").exists() => failure(
                &name,
                format!("{} is not a git repo", repo.display()),
                set_repo,
            ),
            true => ok(&name, repo.display().to_string()),
        });
        match crate::herd::merge_guard(paths, &herd) {
            Ok(Some(MergeGuard::Hook)) => checks.push(warning(
                &format!("herd {herd} merge_guard"),
                "hook: its lead holds your GitHub credentials, so its merges and force-pushes \
                 are stopped only for an honest lead"
                    .into(),
                format!(
                    "loopherd herd set {herd} lead.merge_guard '\"credentials\"', then \
                     loopherd merge-guard {herd}"
                ),
            )),
            Ok(Some(MergeGuard::Credentials)) => {
                let missing = crate::credentials::missing(paths, &herd)
                    .or_else(|| crate::credentials::ssh_missing(paths, &herd));
                if let Some(missing) = missing {
                    let fix = missing.fix.strip_prefix("run ").unwrap_or(&missing.fix);
                    checks.push(warning(
                        &format!("herd {herd} credentials"),
                        missing.what,
                        fix.into(),
                    ));
                }
            }
            _ => {}
        }
        if let Ok((_, SandboxMode::Prefer)) = crate::herd::lead_sandbox(paths, &herd) {
            checks.push(warning(
                &format!("herd {herd} sandbox"),
                "prefer: its lead starts without the sandbox, guarded by its tool hook alone, \
                 whenever the sandbox can't start"
                    .into(),
                format!("loopherd herd set {herd} lead.sandbox '\"require\"'"),
            ));
        }
        let credentials = matches!(
            crate::herd::merge_guard(paths, &herd),
            Ok(Some(MergeGuard::Credentials))
        );
        // A credentials herd short of `require` already failed as invalid.
        let prefer = matches!(crate::herd::worker(paths, &herd), Ok(w) if w.sandbox == WorkerSandbox::Prefer);
        if prefer && !credentials {
            checks.push(warning(
                &format!("herd {herd} workers"),
                "sandbox prefer: its workers start unsandboxed whenever their sandbox can't start"
                    .into(),
                format!("loopherd herd set {herd} worker.sandbox '\"require\"'"),
            ));
        }
        checks.extend(loosened_checks(paths, &herd));
        checks.extend(lead_arg_checks(paths, &herd));
        if let Ok(wanted) = crate::herd::lead_mcp_servers(paths, &herd) {
            let check = format!("herd {herd} mcp_servers");
            checks.extend(mcp_server_checks(
                paths, &check, &wanted, "[lead]", "its lead",
            ));
        }
        checks.extend(worker_guard_checks(paths, &herd));
    }
    checks
}

/// What would refuse or warn a sandboxed worker of `herd` before `feed`
/// starts one: its herd file's args and `[worker] mcp_servers`.
fn worker_guard_checks(paths: &Paths, herd: &str) -> Vec<Check> {
    let Ok(worker) = crate::herd::worker(paths, herd) else {
        return vec![];
    };
    if worker.sandbox == WorkerSandbox::Off {
        return vec![];
    }
    let check = format!("herd {herd} worker mcp_servers");
    let mut checks = mcp_server_checks(
        paths,
        &check,
        &worker.mcp_servers,
        "[worker]",
        "a sandboxed worker",
    );
    let Ok(found) = crate::worker_sandbox::configured_arg_findings(paths, herd) else {
        return checks;
    };
    let name = format!("herd {herd} worker args");
    let flags = found.flags.into_iter();
    checks.extend(flags.map(|(line, fix)| failure(&name, line, fix)));
    let disallowed = found.disallowed;
    let fix = format!(
        "drop it (a sandboxed worker takes only {})",
        crate::sandbox::ALLOWED_ARGS
    );
    checks.extend(disallowed.into_iter().map(|line| match worker.sandbox {
        WorkerSandbox::Require => failure(
            &name,
            line,
            format!(
                "{fix}, or loopherd herd set {herd} worker.sandbox '\"prefer\"' to start its \
                 workers with it as a warning"
            ),
        ),
        _ => warning(&name, line, fix.clone()),
    }));
    checks
}

/// Each herd whose `[lead] unix_sockets` reopens every Unix socket to its lead.
fn unix_socket_checks(install: &Install, paths: &Paths, only: Option<&str>) -> Vec<Check> {
    let herds = match only {
        Some(herd) => vec![herd.to_string()],
        None => crate::herd::names(&paths.herds).unwrap_or_default(),
    };
    let interop = match install.wsl_interop {
        true => {
            "; WSL2's Windows interop is enabled on this machine, so it can run Windows \
                 programs outside its sandbox"
        }
        false => "",
    };
    herds
        .into_iter()
        .filter(|herd| crate::sandbox::sockets_open(paths, herd).unwrap_or(false))
        .filter_map(|herd| {
            let lead = crate::lead::lead_name(&herd).ok()?;
            Some(warning(
                &format!("herd {herd} unix_sockets"),
                crate::sandbox::sockets_open_warning(&lead, &herd) + interop,
                format!("loopherd herd unset {herd} lead.unix_sockets"),
            ))
        })
        .collect()
}

/// Each name in `table`'s `mcp_servers` the human's user scope doesn't
/// configure, for which `whom` isn't started under either mode.
fn mcp_server_checks(
    paths: &Paths,
    check: &str,
    wanted: &[String],
    table: &str,
    whom: &str,
) -> Vec<Check> {
    if wanted.is_empty() {
        return vec![];
    }
    let file = crate::sandbox::user_mcp_file(paths);
    let configured = match crate::sandbox::user_mcp_servers(paths) {
        Ok(configured) => configured,
        Err(e) => {
            return vec![failure(
                check,
                format!("{e:#}, so {whom} isn't started"),
                format!("fix {}", file.display()),
            )];
        }
    };
    wanted
        .iter()
        .filter(|server| !configured.contains_key(*server))
        .map(|server| {
            failure(
                check,
                format!(
                    "{server:?} isn't in your user scope in {}, so {whom} isn't started",
                    file.display()
                ),
                format!(
                    "claude mcp add --scope user {server} …, or drop it from {table} mcp_servers"
                ),
            )
        })
        .collect()
}

/// What the human's own Claude settings loosen in `herd`'s lead's sandbox,
/// failing where that keeps the lead from starting.
fn loosened_checks(paths: &Paths, herd: &str) -> Vec<Check> {
    let loosened =
        crate::lead::lead_name(herd).and_then(|name| crate::sandbox::loosened(paths, &name, herd));
    // A herd whose lead's settings can't be built has that reported by its own check.
    let Ok(lines) = loosened else {
        return vec![];
    };
    finding_checks(paths, herd, "sandbox settings", lines, "remove it")
}

/// Each arg a herd's file passes its lead outside the allowlist, failing where
/// that keeps the lead from starting.
fn lead_arg_checks(paths: &Paths, herd: &str) -> Vec<Check> {
    let name = format!("herd {herd} lead args");
    let (Ok((args, shared)), Ok(lead)) = (
        crate::herd::lead_args(paths, herd),
        crate::lead::lead_name(herd),
    ) else {
        return vec![];
    };
    let fix = match shared {
        true => crate::sandbox::herd_args_fix(herd, "it"),
        false => "drop it".into(),
    };
    let herd_args = shared.then_some((herd, args.as_slice()));
    let mut checks: Vec<Check> = crate::sandbox::launch_flag_refusals(&lead, &args, herd_args)
        .into_iter()
        .map(|why| failure(&name, why, fix.clone()))
        .collect();
    let Ok(lines) = crate::sandbox::configured_disallowed_args(paths, herd) else {
        return checks;
    };
    let fix = format!("{fix} (a lead takes only {})", crate::sandbox::ALLOWED_ARGS);
    checks.extend(finding_checks(paths, herd, "lead args", lines, &fix));
    checks
}

fn finding_checks(
    paths: &Paths,
    herd: &str,
    what: &str,
    lines: Vec<String>,
    remove: &str,
) -> Vec<Check> {
    use crate::sandbox::WhenLoosened;
    let Ok(when) = crate::sandbox::when_loosened(paths, herd) else {
        return vec![];
    };
    let name = format!("herd {herd} {what}");
    lines
        .into_iter()
        .map(|line| match when {
            WhenLoosened::Warn => warning(&name, line, remove.into()),
            WhenLoosened::Refuse => failure(
                &name,
                line,
                format!(
                    "{remove}, or loopherd herd set {herd} lead.sandbox '\"prefer\"' to start \
                     its lead with it as a warning"
                ),
            ),
            WhenLoosened::RefuseForCredentials => failure(
                &name,
                line,
                format!(
                    "{remove}; a credentials lead never starts with it, so the other way is \
                     loopherd herd set {herd} lead.merge_guard '\"hook\"'"
                ),
            ),
        })
        .collect()
}

/// The loopherd rulesets on a herd's GitHub repo.
pub trait Rulesets {
    /// Each of [`DEFAULT_BRANCH_RULESET`] and [`FORCE_PUSH_RULESET`] on
    /// `repo`'s GitHub repo, as GitHub returns it.
    fn loopherd_rulesets(&self, repo: &Path) -> Result<Vec<Value>>;
}

struct RepoLead {
    herd: String,
    merge: bool,
    rebase: bool,
    guard: Option<MergeGuard>,
}

/// Each herd whose switches disagree with the loopherd rulesets on its
/// repo, looked up only for repos a credentials lead runs on, since only
/// `loopherd merge-guard` makes them.
pub fn ruleset_checks(paths: &Paths, only: Option<&str>, rules: &dyn Rulesets) -> Vec<Check> {
    let Ok(herds) = crate::herd::names(&paths.herds) else {
        return vec![];
    };
    let mut repos: Vec<(PathBuf, Vec<RepoLead>)> = vec![];
    for herd in herds {
        let (Ok(repo), Ok(Some(switches)), Ok(guard)) = (
            crate::herd::repo(paths, &herd),
            crate::herd::lead_switches(paths, &herd).map(Some),
            crate::herd::merge_guard(paths, &herd),
        ) else {
            continue;
        };
        let lead = RepoLead {
            herd,
            merge: switches.merge,
            rebase: switches.rebase,
            guard,
        };
        match repos
            .iter_mut()
            .find(|(r, _)| crate::worktree::same_dir(r, &repo))
        {
            Some((_, leads)) => leads.push(lead),
            None => repos.push((repo, vec![lead])),
        }
    }
    let mut checks = vec![];
    for (repo, leads) in repos {
        let guarded = leads
            .iter()
            .any(|l| l.guard == Some(MergeGuard::Credentials));
        if !guarded || only.is_some_and(|herd| leads.iter().all(|l| l.herd != herd)) {
            continue;
        }
        let found = match rules.loopherd_rulesets(&repo) {
            Ok(found) => found,
            Err(e) => {
                let names: Vec<&str> = leads.iter().map(|l| l.herd.as_str()).collect();
                let herds = match names.len() {
                    1 => "herd",
                    _ => "herds",
                };
                checks.push(warning(
                    &format!("{herds} {} rulesets", names.join(", ")),
                    format!("can't read the rulesets of {}: {e:#}", repo.display()),
                    "check gh can reach the repo, then loopherd doctor".into(),
                ));
                continue;
            }
        };
        checks.extend(
            disagreements(&repo, &leads, &found)
                .into_iter()
                .filter(|(herd, _)| only.is_none_or(|o| o == herd))
                .map(|(_, check)| check),
        );
    }
    checks
}

/// The shape of a ruleset loopherd compares, leaving out ids and the like.
fn ruleset_shape(ruleset: &Value) -> (Value, Value, Vec<String>, Vec<String>) {
    let mut rules: Vec<String> = ruleset["rules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["type"].as_str().map(String::from))
        .collect();
    rules.sort();
    let mut bypass: Vec<String> = ruleset["bypass_actors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| format!("{} {} {}", a["actor_type"], a["actor_id"], a["bypass_mode"]))
        .collect();
    bypass.sort();
    (
        ruleset["enforcement"].clone(),
        ruleset["conditions"]["ref_name"]["include"].clone(),
        rules,
        bypass,
    )
}

fn disagreements(repo: &Path, leads: &[RepoLead], found: &[Value]) -> Vec<(String, Check)> {
    let named = |name: &str| found.iter().find(|r| r["name"] == name);
    let shape = |r: &Value| ruleset_shape(r);
    let default_ok = named(DEFAULT_BRANCH_RULESET)
        .is_some_and(|r| shape(r) == shape(&crate::merge_guard::default_branch_ruleset()));
    let default_present = named(DEFAULT_BRANCH_RULESET).is_some();
    // Whether the force-push ruleset lets deploy keys through, when it's loopherd's.
    let force_bypass = named(FORCE_PUSH_RULESET).and_then(|r| {
        [false, true]
            .into_iter()
            .find(|&rebase| shape(r) == shape(&crate::merge_guard::force_push_ruleset(rebase)))
    });
    let force_present = named(FORCE_PUSH_RULESET).is_some();
    let shown = repo.display();
    let mut out = vec![];
    let mut push = |herd: &str, detail: String, fix: String| {
        out.push((
            herd.to_string(),
            warning(&format!("herd {herd} rulesets"), detail, fix),
        ));
    };
    let guarded: Vec<&RepoLead> = leads
        .iter()
        .filter(|l| l.guard == Some(MergeGuard::Credentials))
        .collect();
    for lead in leads {
        let herd = &lead.herd;
        let setup = format!("loopherd merge-guard {herd}");
        if lead.guard == Some(MergeGuard::Credentials) {
            if !default_ok {
                push(
                    herd,
                    format!(
                        "{shown} has no ruleset {DEFAULT_BRANCH_RULESET} as loopherd merge-guard \
                         makes it, so nothing on GitHub stops a push to the default branch"
                    ),
                    setup.clone(),
                );
            }
            match (force_bypass, lead.rebase) {
                (None, _) => push(
                    herd,
                    format!(
                        "{shown} has no ruleset {FORCE_PUSH_RULESET} as loopherd merge-guard \
                         makes it, so nothing on GitHub stops a force-push"
                    ),
                    setup.clone(),
                ),
                (Some(false), true) => push(
                    herd,
                    format!(
                        "rebase is on, but {FORCE_PUSH_RULESET} stops deploy keys too, so its \
                         lead can't force-push"
                    ),
                    setup.clone(),
                ),
                (Some(true), false) => push(
                    herd,
                    format!("rebase is off, but {FORCE_PUSH_RULESET} lets deploy keys force-push"),
                    setup.clone(),
                ),
                _ => {}
            }
            if let Some(other) = guarded
                .iter()
                .find(|o| o.herd < lead.herd && o.rebase != lead.rebase)
            {
                let on_off = |on: bool| if on { "on" } else { "off" };
                push(
                    herd,
                    format!(
                        "herds {} and {herd} both run on {shown} but need different force-push \
                         bypasses (rebase {} in {}, {} in {herd}), and one ruleset can't \
                         enforce both",
                        other.herd,
                        on_off(other.rebase),
                        other.herd,
                        on_off(lead.rebase)
                    ),
                    "give them the same rebase".into(),
                );
            }
            continue;
        }
        if lead.merge && default_present {
            push(
                herd,
                format!(
                    "its lead has merge on, but {DEFAULT_BRANCH_RULESET} on {shown} stops its \
                     merges"
                ),
                format!(
                    "loopherd herd set {herd} lead.merge false, or run herd {herd} on a repo of its own"
                ),
            );
        }
        if lead.rebase && force_present && !lead.merge {
            push(
                herd,
                format!(
                    "rebase is on, but {FORCE_PUSH_RULESET} on {shown} stops its force-pushes, \
                     made with your credentials"
                ),
                format!(
                    "loopherd herd set {herd} lead.rebase false, or run herd {herd} on a repo of its own"
                ),
            );
        }
    }
    out
}

/// The ledger, pause files and mailboxes, each lead's broker against
/// `binary`, and, when herdr was reached, each unretired member's pane on its socket.
fn state_checks(
    setup: &Setup,
    only: Option<&str>,
    reached: Option<(&dyn Herdr, Option<PathBuf>)>,
    binary: &BrokersOwn,
) -> Vec<Check> {
    let mut checks = pause_checks(&setup.paths, only);
    let entries = match setup.ledger.entries() {
        Ok(entries) => entries,
        Err(e) => {
            checks.insert(
                0,
                failure("ledger", message(&e), "fix the ledger by hand".into()),
            );
            return checks;
        }
    };
    checks.insert(0, ok("ledger", format!("{} entries", entries.len())));
    let members: Vec<_> = entries
        .into_values()
        .filter(|r| only.is_none_or(|herd| r.herd.as_deref() == Some(herd)))
        .collect();
    if let Ok(dir) = setup.ledger.reports_dir() {
        for member in &members {
            checks.extend(mailbox_check(&dir, &member.name));
        }
        for member in members.iter().filter(|r| !r.retired) {
            checks.extend(window_check(&dir, &member.name));
        }
    }
    checks.extend(
        members
            .iter()
            .filter(|r| !r.retired && r.is_lead())
            .filter_map(|lead| lead_sandbox_check(&setup.paths, lead)),
    );
    checks.extend(
        members
            .iter()
            .filter(|r| !r.retired && !r.may_be_lead())
            .filter_map(|worker| worker_sandbox_check(&setup.paths, worker)),
    );
    checks.extend(
        members
            .iter()
            .filter(|r| !r.retired && r.is_lead())
            .filter_map(|lead| lead_path_check(&setup.paths, &lead.name)),
    );
    // A lead known to be on another herdr has its broker under that herdr's
    // config dir; one whose herdr can't be told apart is checked, as revive starts it.
    let here = reached.as_ref().and_then(|(_, socket)| socket.as_deref());
    checks.extend(
        members
            .iter()
            .filter(|r| !r.retired && r.is_lead())
            .filter(|lead| here.is_none_or(|here| !elsewhere(Some(here), lead)))
            .filter_map(|lead| broker_check(&setup.paths, &lead.name, binary)),
    );
    if let Some((herdr, socket)) = reached {
        let unretired: Vec<_> = members.iter().filter(|r| !r.retired).collect();
        checks.extend(pane_checks(herdr, socket.as_deref(), &unretired));
    }
    checks
}

/// The loopherd a broker should run: the one on PATH, which `broker-restart`
/// and leads run, else this one.
struct BrokersOwn {
    path: PathBuf,
    on_path: bool,
    in_broker_of: Option<String>,
    move_grace: Duration,
}

impl BrokersOwn {
    fn of(install: &Install) -> BrokersOwn {
        let (path, on_path) = match install.on_path.as_ref().map(|p| p.canonicalize()) {
            Some(Ok(path)) => (path, true),
            _ => (canonical(&install.running), false),
        };
        BrokersOwn {
            path,
            on_path,
            in_broker_of: install.in_broker_of.clone(),
            move_grace: install.broker_move_grace,
        }
    }

    fn runs(&self, recorded: &crate::broker::Binary) -> bool {
        match crate::broker::FileId::of(&self.path) {
            Some(file) => recorded.file == file,
            None => self.is_at(&recorded.path),
        }
    }

    fn is_at(&self, path: &Path) -> bool {
        canonical(path) == self.path
    }
}

/// `path` with links resolved, or as given when it no longer exists.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl std::fmt::Display for BrokersOwn {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.on_path {
            true => write!(f, "the loopherd on PATH ({})", self.path.display()),
            false => write!(f, "this one ({})", self.path.display()),
        }
    }
}

/// The binary `lead`'s broker records, given a moment to move onto `own`
/// when that is the install on PATH, which brokers follow.
fn recorded_after_moving(
    paths: &Paths,
    lead: &str,
    own: &BrokersOwn,
) -> Option<crate::broker::Binary> {
    let given_up = std::time::Instant::now() + own.move_grace;
    loop {
        let recorded = crate::broker::recorded_binary(paths, lead);
        let moved = recorded.as_ref().is_some_and(|r| own.runs(r));
        if moved || !own.on_path || std::time::Instant::now() >= given_up {
            return recorded;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Fails while no broker serves `lead`; warns while one runs another build than `own`.
fn broker_check(paths: &Paths, lead: &str, own: &BrokersOwn) -> Option<Check> {
    let name = format!("broker {lead}");
    if !crate::broker::running(paths, lead) {
        return Some(failure(
            &name,
            format!("no broker is running for {lead}, so its loopherd commands fail"),
            format!("loopherd revive --name {lead}"),
        ));
    }
    if own.on_path && own.in_broker_of.as_deref() == Some(lead) {
        return Some(ok(
            &name,
            format!("{lead}'s broker runs this check, and moves onto {own} once it is answered"),
        ));
    }
    let runs = match recorded_after_moving(paths, lead, own) {
        Some(recorded) if own.runs(&recorded) => return None,
        Some(recorded) if own.is_at(&recorded.path) => format!(
            "an earlier build of {} (loopherd {})",
            own.path.display(),
            recorded.version
        ),
        Some(recorded) => format!(
            "loopherd {} at {}, not {own}",
            recorded.version,
            recorded.path.display()
        ),
        None => format!("a loopherd from before brokers recorded their build, not {own}"),
    };
    // A lead's install also runs doctor, and a lead is refused broker-restart.
    Some(warning(
        &name,
        format!("{lead}'s broker runs {runs}, so its lead's commands may fail"),
        format!("the shepherd runs loopherd broker-restart {lead}"),
    ))
}

fn pause_checks(paths: &Paths, only: Option<&str>) -> Vec<Check> {
    let dir = &paths.paused;
    let readings = match only {
        Some(herd) => vec![(herd.to_string(), crate::pause::read(dir, herd))],
        None => match crate::pause::all(dir) {
            Ok(readings) => readings,
            Err(e) => {
                let fix = format!("fix {} by hand", dir.display());
                return vec![failure("pause files", message(&e), fix)];
            }
        },
    };
    readings
        .into_iter()
        .filter_map(|(herd, reading)| match reading {
            Reading::Unreadable(e) => {
                // `pause` needs the herd file; `resume` deletes an orphaned pause too.
                let fix = match crate::herd::require(paths, &herd) {
                    Ok(()) => format!(
                        "loopherd pause {herd} to rewrite it, or loopherd resume {herd} to lift it"
                    ),
                    Err(_) => format!("loopherd resume {herd}"),
                };
                Some(failure(&format!("pause {herd}"), message(&e), fix))
            }
            _ => None,
        })
        .collect()
}

/// A failure when `name`'s mailbox can't be read or has a line that doesn't parse.
fn mailbox_check(dir: &Path, name: &str) -> Option<Check> {
    let (detail, fix) = match crate::report::history(dir, name) {
        Err(e) => (
            message(&e),
            format!("make {} readable", crate::report::shown_mailbox(dir, name)),
        ),
        Ok((_, warnings)) => {
            let detail = match warnings.as_slice() {
                [] => return None,
                [one] => one.clone(),
                [first, rest @ ..] => format!("{first} (and {} more bad lines)", rest.len()),
            };
            (detail, "fix the mailbox's bad lines by hand".into())
        }
    };
    Some(failure(&format!("mailbox {name}"), detail, fix))
}

/// The context window `name`'s last stop was measured against, and its source.
fn window_check(dir: &Path, name: &str) -> Option<Check> {
    let (entries, _) = crate::report::history(dir, name).ok()?;
    let window = entries.iter().rev().find_map(|e| match &e.body {
        crate::report::Body::Stop(stop) => Some(stop.window.as_ref()),
        _ => None,
    })??;
    Some(ok(&format!("context window {name}"), window.to_string()))
}

/// Whether `record` is on another herdr than the one at `here`. With herdr's
/// socket unknown, a recorded one can't be shown to be this herdr's.
fn elsewhere(here: Option<&Path>, record: &crate::ledger::AgentRecord) -> bool {
    match (here, &record.socket) {
        (Some(here), Some(there)) => !same_file(here, there),
        (None, Some(_)) => true,
        (_, None) => false,
    }
}

/// Each member on `here`'s herdr whose pane is gone warns; those on another
/// socket are counted, not checked.
fn pane_checks(
    herdr: &dyn Herdr,
    here: Option<&Path>,
    members: &[&crate::ledger::AgentRecord],
) -> Vec<Check> {
    if members.is_empty() {
        return vec![];
    }
    let agents = match herdr.agents_within(HERDR_TIMEOUT) {
        Ok(Some(agents)) => agents,
        Ok(None) => {
            let detail = format!(
                "herdr didn't list its agents within {}",
                humantime::format_duration(HERDR_TIMEOUT)
            );
            return vec![failure("members", detail, "restart herdr's server".into())];
        }
        Err(e) => {
            return vec![failure(
                "members",
                message(&e),
                "restart herdr's server".into(),
            )];
        }
    };
    let on = here.map_or("herdr's default socket".into(), |s| s.display().to_string());
    let (other, local): (Vec<_>, Vec<_>) =
        members.iter().copied().partition(|r| elsewhere(here, r));
    let mut checks: Vec<Check> = local
        .iter()
        .filter(|r| crate::matching::live_agent(r, &agents).is_none())
        .map(|r| Check {
            name: format!("member {}", r.name),
            status: Status::Warning,
            detail: format!("no live agent matches it (pane {}) on {on}", r.pane_id),
            fix: Some(format!(
                "loopherd revive --name {0}, or loopherd retire {0}",
                r.name
            )),
        })
        .collect();
    let live = local.len() - checks.len();
    if live > 0 {
        checks.push(ok("members", format!("{live} live on {on}")));
    }
    if !other.is_empty() {
        checks.push(ok(
            "members elsewhere",
            format!("{} on another herdr socket, not checked", other.len()),
        ));
    }
    checks
}

/// `e` with its causes, as the command that failed would print it.
fn message(e: &anyhow::Error) -> String {
    format!("{e:#}").trim_end().to_string()
}

fn hook_checks(install: &Install) -> Vec<Check> {
    let settings = install.settings.display();
    let stale_on_path = stale_on_path(install);
    let install_hook = install_hook(install);
    let hooks = match crate::install_hook::hooks(&install.settings) {
        Ok(hooks) => hooks,
        Err(e) => {
            return vec![failure(
                "settings",
                format!("{e:#}").trim_end().to_string(),
                format!("fix {settings} by hand, then {install_hook}"),
            )];
        }
    };
    let mut checks: Vec<Check> = hooks
        .iter()
        .map(|hook| {
            let name = format!("{} hook", hook.event);
            match (hook.binaries.is_empty(), hook.event) {
                (false, _) => ok(&name, format!("in {settings}")),
                (true, "UserPromptSubmit") if !install.pane_label => {
                    warning(&name, format!("not in {settings}"), install_hook.clone())
                }
                // Without them view and status list no running subagents; watch is unaffected.
                (true, "SubagentStart" | "SubagentStop") => {
                    warning(&name, format!("not in {settings}"), install_hook.clone())
                }
                (true, _) => failure(&name, format!("not in {settings}"), install_hook.clone()),
            }
        })
        .collect();
    let called: Vec<&PathBuf> = hooks.iter().flat_map(|h| &h.binaries).collect();
    checks.push(match &install.on_path {
        None => failure(
            "hook binary",
            "no loopherd on PATH".into(),
            format!("install loopherd on PATH, then {install_hook}"),
        ),
        // A relative path resolves against whatever cwd Claude runs the hook in.
        Some(on_path) => match called
            .iter()
            .find(|b| !b.is_absolute() || !same_file(b, on_path))
        {
            Some(other) => failure(
                "hook binary",
                format!(
                    "hooks call {}, not the loopherd on PATH ({})",
                    other.display(),
                    on_path.display()
                ),
                match stale_on_path {
                    Some(stale) => format!(
                        "put {} before {} on PATH, then {install_hook}",
                        dir_of(&install.running).display(),
                        dir_of(stale).display()
                    ),
                    None => install_hook,
                },
            ),
            None if called.is_empty() => ok("hook binary", "no hooks to check".into()),
            None => ok("hook binary", format!("hooks call {}", on_path.display())),
        },
    });
    checks
}

/// A loopherd first on PATH other than this one; by name it would point
/// the hooks back at itself.
fn stale_on_path(install: &Install) -> Option<&PathBuf> {
    install
        .on_path
        .as_ref()
        .filter(|on_path| !same_file(&install.running, on_path))
}

fn install_hook(install: &Install) -> String {
    let loopherd = match stale_on_path(install) {
        Some(_) => crate::shell::quote(&install.running.to_string_lossy()),
        None => "loopherd".to_string(),
    };
    match install.settings_given {
        true => format!(
            "{loopherd} install-hook --settings {}",
            crate::shell::quote(&install.settings.to_string_lossy())
        ),
        false => format!("{loopherd} install-hook"),
    }
}

/// Whether Claude's skills dir links to the skill `install-hook` writes.
fn skill_check(install: &Install) -> Option<Check> {
    use crate::skill::{Layout, Link};
    let layout = install.skill.as_ref()?;
    let Layout::Linked { real, link } = layout else {
        return None;
    };
    let at = link.join("loopherd");
    let skill = layout.skill();
    let fix = install_hook(install);
    let skill_dir = skill.parent().expect("the skill sits in a directory");
    if std::fs::symlink_metadata(skill_dir).is_ok_and(|m| m.is_symlink()) {
        return Some(warning(
            "skill",
            format!(
                "{} is a symlink, which install-hook won't write through",
                skill_dir.display()
            ),
            format!("move {} aside, then {fix}", skill_dir.display()),
        ));
    }
    Some(match crate::skill::link_state(real, link) {
        Link::Ours if at.join("SKILL.md").is_file() => ok(
            "skill",
            format!("{} links to {}", at.display(), skill.display()),
        ),
        Link::Ours => warning("skill", format!("no {}", skill.display()), fix),
        Link::Missing => warning("skill", format!("no {}", at.display()), fix),
        Link::OldCopy => warning("skill", format!("{} isn't a symlink", at.display()), fix),
        Link::Foreign(why) => warning(
            "skill",
            format!("{} {why}", at.display()),
            format!("move {} aside, then {fix}", at.display()),
        ),
    })
}

fn dir_of(binary: &Path) -> &Path {
    binary.parent().unwrap_or(binary)
}

fn running_binary(install: &Install) -> Check {
    let running = install.running.display();
    match &install.on_path {
        Some(on_path) if !same_file(&install.running, on_path) => warning(
            "running binary",
            format!(
                "this is {running}, not the loopherd on PATH ({})",
                on_path.display()
            ),
            "run the loopherd on PATH".into(),
        ),
        _ => ok("running binary", running.to_string()),
    }
}

/// How long herdr's status probe may take; a wedged server never answers.
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

/// The herdr checks, and the socket of the herdr they reached, if any.
fn herdr_checks(herdr: &dyn Herdr) -> (Vec<Check>, Option<Option<PathBuf>>) {
    let (checks, reached) = match herdr.server(HERDR_TIMEOUT) {
        Ok(Server::Running { socket, version }) => {
            let mut checks = running_checks(socket.clone(), version);
            checks.push(plugin_check(herdr));
            (checks, Some(socket))
        }
        not_running => (vec![unreached(not_running)], None),
    };
    (checks, reached)
}

fn on(socket: Option<PathBuf>) -> String {
    match socket {
        Some(socket) => socket.display().to_string(),
        None => "herdr's default socket".into(),
    }
}

fn unreached(server: Result<Server>) -> Check {
    match server {
        Ok(Server::Running { .. }) => unreachable!("a running server is reached"),
        Ok(Server::NotRunning { socket }) => {
            let detail = format!("no herdr server answers on {}", on(socket));
            failure("herdr", detail, "herdr".into())
        }
        Ok(Server::NoAnswer { socket }) => {
            let detail = format!(
                "herdr didn't answer on {} within {}",
                on(socket),
                humantime::format_duration(HERDR_TIMEOUT)
            );
            failure("herdr", detail, "restart herdr's server".into())
        }
        Ok(Server::Missing) => failure("herdr", "no herdr on PATH".into(), "install herdr".into()),
        Err(e) => failure("herdr", message(&e), "restart herdr's server".into()),
    }
}

fn running_checks(socket: Option<PathBuf>, version: Option<String>) -> Vec<Check> {
    let socket = on(socket);
    let upgrade = format!("upgrade herdr to {MIN_HERDR} or later and restart its server");
    let check = match version {
        None => failure(
            "herdr version",
            "herdr's server didn't report a version".into(),
            upgrade,
        ),
        Some(version) => match numbers(&version) {
            None => failure(
                "herdr version",
                format!("can't read herdr's version {version:?}"),
                upgrade,
            ),
            Some(v) if Some(v) < numbers(MIN_HERDR) => failure(
                "herdr version",
                format!("{version} is older than {MIN_HERDR}"),
                upgrade,
            ),
            Some(_) => ok("herdr version", version),
        },
    };
    vec![ok("herdr", format!("answers on {socket}")), check]
}

/// Only ever warns: loopherd works without its herdr plugin.
fn plugin_check(herdr: &dyn Herdr) -> Check {
    let ours = env!("CARGO_PKG_VERSION");
    let unavailable = "so its actions, popups and event hooks are unavailable";
    let plugin = match herdr.plugin("loopherd", HERDR_TIMEOUT) {
        Ok(Some(plugin)) => plugin,
        Ok(None) => {
            return warning(
                "herdr plugin",
                format!("loopherd's herdr plugin isn't installed, {unavailable}"),
                "herdr plugin install junderdo/loopherd/herdr-plugin".into(),
            );
        }
        Err(e) => {
            let detail = format!("can't list herdr's plugins: {}", message(&e));
            let fix = "run `herdr plugin list --plugin loopherd` to see why".into();
            return warning("herdr plugin", detail, fix);
        }
    };
    if !plugin.enabled {
        return warning(
            "herdr plugin",
            format!("loopherd's herdr plugin is disabled, {unavailable}"),
            "herdr plugin enable loopherd".into(),
        );
    }
    if plugin.version != ours {
        let newer = numbers(&plugin.version)
            .zip(numbers(ours))
            .is_some_and(|(plugin, ours)| plugin > ours);
        let fix = match &plugin.linked {
            _ if newer => format!("update loopherd to {}", plugin.version),
            Some(dir) => format!(
                "bring the linked plugin at {} to loopherd {ours}",
                dir.display()
            ),
            None => "herdr plugin update loopherd".into(),
        };
        let detail = format!("plugin {} differs from loopherd {ours}", plugin.version);
        return warning("herdr plugin", detail, fix);
    }
    let detail = match &plugin.linked {
        Some(dir) => format!("{ours}, enabled, linked from {}", dir.display()),
        None => format!("{ours}, enabled"),
    };
    ok("herdr plugin", detail)
}

/// How long each herd's `source` or `backlog` may run under `--trackers`.
pub const TRACKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs every herd's `source`, or only `only`'s, and `backlog` when set, from
/// the herd's repo, paused herds included. Reports each exit and task count,
/// never the tasks.
pub fn tracker_checks(
    paths: &crate::herd::Paths,
    only: Option<&str>,
    timeout: Duration,
) -> Vec<Check> {
    let names = match only {
        Some(herd) => Ok(vec![herd.to_string()]),
        None => crate::herd::names(&paths.herds),
    };
    let names = match names {
        Ok(names) => names,
        Err(e) => {
            let detail = format!("{e:#}").trim_end().to_string();
            return vec![failure("trackers", detail, "loopherd herds".into())];
        }
    };
    let mut checks = Vec::new();
    for herd in names {
        let queue = match crate::herd::lead_checks(paths, &herd) {
            Ok((_, Some(queue))) => queue,
            Ok((_, None)) => continue,
            Err(e) => {
                let detail = format!("{e:#}").trim_end().to_string();
                checks.push(failure(
                    &format!("{herd} trackers"),
                    detail,
                    "loopherd herds".into(),
                ));
                continue;
            }
        };
        // The herd checks already report a repo that isn't a directory.
        if !queue.repo.is_dir() {
            continue;
        }
        let commands = [
            ("source", Some(&queue.source)),
            ("backlog", queue.backlog.as_ref()),
        ];
        for (key, command) in commands {
            if let Some(command) = command {
                checks.push(tracker(&herd, key, command, &queue.repo, timeout));
            }
        }
    }
    checks
}

fn tracker(herd: &str, key: &str, command: &str, repo: &Path, timeout: Duration) -> Check {
    let name = format!("{herd} {key}");
    let shown = match command.trim().split_once('\n') {
        Some((first, _)) => format!("{first} …"),
        None => command.trim().to_string(),
    };
    let fix =
        format!("fix {herd}'s queue.{key}, e.g. loopherd herd set {herd} queue.{key} <command>");
    let ran = crate::shell::run_within(
        Command::new("sh").arg("-c").arg(command).current_dir(repo),
        timeout,
    );
    let output = match ran {
        Ok(Some(output)) => output,
        Ok(None) => {
            let after = humantime::format_duration(timeout);
            return failure(&name, format!("{shown:?} timed out after {after}"), fix);
        }
        Err(e) => {
            let detail = format!("{shown:?} didn't run in {}: {e:#}", repo.display());
            return failure(&name, detail, fix);
        }
    };
    let exit = match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => output.status.to_string(),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match stderr.trim().lines().last() {
            None => format!("{shown:?} failed ({exit})"),
            Some(last) => format!(
                "{shown:?} failed ({exit}): {}",
                crate::watch::printable(last)
            ),
        };
        return failure(&name, detail, fix);
    }
    let tasks = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count();
    ok(&name, format!("{exit}, {tasks} tasks"))
}

/// `major.minor.patch` as numbers, ignoring any pre-release or build suffix.
pub(crate) fn numbers(version: &str) -> Option<[u64; 3]> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(numbers)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn ok(name: &str, detail: String) -> Check {
    Check {
        name: name.into(),
        status: Status::Ok,
        detail,
        fix: None,
    }
}

fn warning(name: &str, detail: String, fix: String) -> Check {
    Check {
        status: Status::Warning,
        ..failure(name, detail, fix)
    }
}

fn failure(name: &str, detail: String, fix: String) -> Check {
    Check {
        name: name.into(),
        status: Status::Failure,
        detail,
        fix: Some(fix),
    }
}

fn count(checks: &[Check], status: Status) -> usize {
    checks.iter().filter(|c| c.status == status).count()
}

/// Problems with their fixes, one line each, or every check when `verbose`,
/// then the summary line.
pub fn text(checks: &[Check], verbose: bool) -> String {
    let mut out = String::new();
    for check in checks.iter().filter(|c| verbose || c.status != Status::Ok) {
        let status = match check.status {
            Status::Ok => "ok",
            Status::Warning => "warning",
            Status::Failure => "failure",
        };
        // A TOML error's caret diagram spans lines; one check stays one line.
        let detail: Vec<&str> = check
            .detail
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        out += &format!("{status} {}: {}", check.name, detail.join(" "));
        if let Some(fix) = &check.fix {
            out += &format!("; fix: {fix}");
        }
        out.push('\n');
    }
    out + &format!(
        "doctor: {} ok, {} warnings, {} failures\n",
        count(checks, Status::Ok),
        count(checks, Status::Warning),
        count(checks, Status::Failure),
    )
}

pub fn json(checks: &[Check]) -> String {
    serde_json::json!({
        "checks": checks,
        "ok": count(checks, Status::Ok),
        "warnings": count(checks, Status::Warning),
        "failures": count(checks, Status::Failure),
    })
    .to_string()
}

/// 0 when clean or only warnings, 1 on any failure.
pub fn exit_code(checks: &[Check]) -> i32 {
    match count(checks, Status::Failure) {
        0 => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Plugin;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::AgentRecord;
    use serde_json::json;
    use std::os::unix::fs::MetadataExt;

    const EVENTS: [(&str, &str); 7] = [
        ("Stop", "stop-hook"),
        ("StopFailure", "stop-hook"),
        ("SessionStart", "session-hook"),
        ("PreToolUse", "tool-hook"),
        ("UserPromptSubmit", "prompt-hook"),
        ("SubagentStart", "subagent-hook"),
        ("SubagentStop", "subagent-hook"),
    ];

    struct Machine {
        dir: tempfile::TempDir,
        install: Install,
        setup: Result<Setup>,
    }

    impl Machine {
        fn paths(&self) -> &Paths {
            &self.setup.as_ref().unwrap().paths
        }

        fn config(&self) -> PathBuf {
            crate::herd::config_file(self.paths())
        }

        fn herd_file(&self, herd: &str, text: &str) {
            std::fs::create_dir_all(&self.paths().herds).unwrap();
            std::fs::write(self.paths().herds.join(format!("{herd}.toml")), text).unwrap();
        }

        /// A herd file for `herd` whose repo is a fresh git repo.
        fn herd(&self, herd: &str) -> PathBuf {
            let repo = self.dir.path().join(format!("repos/{herd}"));
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            self.herd_file(herd, &format!("repo = {:?}\n", repo.to_str().unwrap()));
            repo
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn ledger(&self, records: &[AgentRecord]) {
            let mut ledger = FileLedger::new(self.state().join("ledger.json"));
            for record in records {
                ledger.record(record.clone()).unwrap();
            }
        }

        fn checks(&self, herd: Option<&str>, herdr: &FakeHerdr) -> Result<Vec<Check>> {
            checks(&self.install, &self.setup, herd, herdr)
        }
    }

    const CONFIG: &str = "context_window = 200000\n[lead_defaults]\nmerge = false\n";

    /// A settings file whose hooks for `events` call `exe`, with `exe` both
    /// on PATH and running.
    fn machine(events: &[&str]) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/loopherd");
        let settings = dir.path().join("settings.json");
        write_hooks(&settings, &exe, events);
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/config.toml"), CONFIG).unwrap();
        let skill = crate::skill::Layout::Linked {
            real: dir.path().join(".agents/skills"),
            link: dir.path().join("skills"),
        };
        crate::skill::install(&skill).unwrap();
        let setup = Setup {
            paths: Paths {
                herds: dir.path().join("config/herds"),
                home: dir.path().to_path_buf(),
                feed_lock: dir.path().join("state/feed.lock"),
                paused: dir.path().join("state/paused"),
            },
            ledger: FileLedger::new(dir.path().join("state/ledger.json")),
        };
        Machine {
            install: Install {
                settings,
                settings_given: false,
                pane_label: false,
                on_path: Some(exe.clone()),
                running: exe,
                sandbox: Ok(Sockets::Blocked),
                wsl_interop: false,
                skill: Some(skill.clone()),
                in_broker_of: None,
                broker_move_grace: Duration::ZERO,
            },
            setup: Ok(setup),
            dir,
        }
    }

    fn write_hooks(settings: &Path, exe: &Path, events: &[&str]) {
        let mut hooks = serde_json::Map::new();
        for (event, subcommand) in EVENTS.iter().filter(|(e, _)| events.contains(e)) {
            let command = crate::install_hook::hook_command(exe, subcommand).unwrap();
            hooks.insert(event.to_string(), group(event, &command));
        }
        let root = json!({ "hooks": hooks });
        std::fs::write(settings, serde_json::to_vec(&root).unwrap()).unwrap();
    }

    /// A settings group holding one hook running `command`, under Bash for PreToolUse.
    fn group(event: &str, command: &str) -> Value {
        let hooks = json!([{"type": "command", "command": command}]);
        match event {
            "PreToolUse" => json!([{"matcher": "Bash", "hooks": hooks}]),
            _ => json!([{ "hooks": hooks }]),
        }
    }

    const ALL: [&str; 7] = [
        "Stop",
        "StopFailure",
        "SessionStart",
        "PreToolUse",
        "UserPromptSubmit",
        "SubagentStart",
        "SubagentStop",
    ];

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[])
    }

    fn run(m: &Machine, herdr: &FakeHerdr) -> (String, i32) {
        run_on(m, None, herdr)
    }

    fn run_on(m: &Machine, herd: Option<&str>, herdr: &FakeHerdr) -> (String, i32) {
        let checks = m.checks(herd, herdr).unwrap();
        (text(&checks, false), exit_code(&checks))
    }

    #[test]
    fn a_sound_install_prints_only_the_summary_and_exits_0() {
        let m = machine(&ALL);
        assert_eq!(
            run(&m, &herdr()),
            ("doctor: 18 ok, 0 warnings, 0 failures\n".into(), 0)
        );
    }

    fn skill_link(m: &Machine) -> PathBuf {
        m.dir.path().join("skills/loopherd")
    }

    fn skill_warning(m: &Machine) -> String {
        let (out, code) = run(m, &herdr());
        assert_eq!(code, 0, "{out}");
        out.lines()
            .skip_while(|l| !l.contains("skill"))
            .take(2)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_missing_skill_link_warns_naming_install_hook() {
        let m = machine(&ALL);
        std::fs::remove_file(skill_link(&m)).unwrap();
        let warning = skill_warning(&m);
        assert!(
            warning.contains(&format!("no {}", skill_link(&m).display())),
            "{warning}"
        );
        assert!(warning.contains("loopherd install-hook"), "{warning}");
    }

    #[test]
    fn a_skill_dir_that_isnt_a_symlink_warns_naming_install_hook() {
        let m = machine(&ALL);
        std::fs::remove_file(skill_link(&m)).unwrap();
        std::fs::create_dir(skill_link(&m)).unwrap();
        std::fs::write(skill_link(&m).join("SKILL.md"), crate::skill::SKILL).unwrap();
        let warning = skill_warning(&m);
        assert!(warning.contains("isn't a symlink"), "{warning}");
        assert!(warning.contains("loopherd install-hook"), "{warning}");
    }

    #[test]
    fn a_dangling_skill_link_warns_even_when_its_target_reads_right() {
        let m = machine(&ALL);
        let real = m.dir.path().join("dotfiles");
        std::fs::create_dir_all(real.join("skills")).unwrap();
        std::fs::remove_file(skill_link(&m)).unwrap();
        std::fs::remove_dir(m.dir.path().join("skills")).unwrap();
        std::os::unix::fs::symlink(real.join("skills"), m.dir.path().join("skills")).unwrap();
        std::os::unix::fs::symlink("../.agents/skills/loopherd", skill_link(&m)).unwrap();
        assert!(!skill_link(&m).join("SKILL.md").exists());

        let warning = skill_warning(&m);

        assert!(warning.starts_with("warning skill:"), "{warning}");
        assert!(warning.contains("loopherd install-hook"), "{warning}");
    }

    #[test]
    fn a_symlinked_skill_dir_warns_to_move_it_aside() {
        let m = machine(&ALL);
        let real = m.dir.path().join(".agents/skills/loopherd");
        let mine = m.dir.path().join("dotfiles/loopherd");
        std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
        std::fs::rename(&real, &mine).unwrap();
        std::os::unix::fs::symlink(&mine, &real).unwrap();

        let warning = skill_warning(&m);

        assert!(warning.contains("is a symlink"), "{warning}");
        assert!(
            warning.contains(&format!(
                "move {} aside, then loopherd install-hook",
                real.display()
            )),
            "{warning}"
        );
    }

    #[test]
    fn a_skill_link_pointing_elsewhere_warns_to_move_it_aside() {
        let m = machine(&ALL);
        std::fs::remove_file(skill_link(&m)).unwrap();
        std::os::unix::fs::symlink("/elsewhere/loopherd", skill_link(&m)).unwrap();
        let warning = skill_warning(&m);
        assert!(
            warning.contains("points to /elsewhere/loopherd"),
            "{warning}"
        );
        assert!(
            warning.contains(&format!(
                "move {} aside, then loopherd install-hook",
                skill_link(&m).display()
            )),
            "{warning}"
        );
    }

    #[test]
    fn a_missing_watched_or_tool_hook_fails_naming_install_hook() {
        for missing in ["Stop", "StopFailure", "SessionStart", "PreToolUse"] {
            let events: Vec<&str> = ALL.into_iter().filter(|e| *e != missing).collect();
            let m = machine(&events);
            let (out, code) = run(&m, &herdr());
            assert_eq!(
                out,
                format!(
                    "failure {missing} hook: not in {}; fix: loopherd install-hook\n\
                     doctor: 17 ok, 0 warnings, 1 failures\n",
                    m.install.settings.display()
                ),
            );
            assert_eq!(code, 1);
        }
    }

    fn without(missing: &str) -> Vec<&'static str> {
        ALL.into_iter().filter(|e| *e != missing).collect()
    }

    #[test]
    fn a_missing_subagent_hook_warns_naming_install_hook() {
        for missing in ["SubagentStart", "SubagentStop"] {
            let m = machine(&without(missing));
            let (out, code) = run(&m, &herdr());
            assert_eq!(
                out,
                format!(
                    "warning {missing} hook: not in {}; fix: loopherd install-hook\n\
                     doctor: 17 ok, 1 warnings, 0 failures\n",
                    m.install.settings.display()
                ),
            );
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn a_missing_prompt_hook_warns_unless_pane_label_is_on() {
        let mut m = machine(&without("UserPromptSubmit"));
        let line = format!(
            "UserPromptSubmit hook: not in {}; fix: loopherd install-hook\n",
            m.install.settings.display()
        );
        assert_eq!(
            run(&m, &herdr()),
            (
                format!("warning {line}doctor: 17 ok, 1 warnings, 0 failures\n"),
                0
            )
        );
        m.install.pane_label = true;
        assert_eq!(
            run(&m, &herdr()),
            (
                format!("failure {line}doctor: 17 ok, 0 warnings, 1 failures\n"),
                1
            )
        );
    }

    #[test]
    fn a_settings_flag_is_repeated_in_the_fix() {
        let mut m = machine(&ALL[..3]);
        m.install.settings_given = true;
        let (out, _) = run(&m, &herdr());
        assert!(
            out.contains(&format!(
                "fix: loopherd install-hook --settings '{}'\n",
                m.install.settings.display()
            )),
            "{out}"
        );
    }

    #[test]
    fn a_missing_settings_file_fails_every_hook() {
        let mut m = machine(&ALL);
        m.install.settings = m.dir.path().join("absent.json");
        let (out, code) = run(&m, &herdr());
        assert!(
            out.ends_with("doctor: 11 ok, 3 warnings, 4 failures\n"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn unparseable_settings_fail_once_naming_the_file() {
        let m = machine(&ALL);
        std::fs::write(&m.install.settings, "{ not json").unwrap();
        let (out, code) = run(&m, &herdr());
        let first = out.lines().next().unwrap();
        assert!(
            first.starts_with(&format!(
                "failure settings: parse {}",
                m.install.settings.display()
            )),
            "{out}"
        );
        assert!(
            out.ends_with("doctor: 10 ok, 0 warnings, 1 failures\n"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn hooks_calling_another_binary_than_paths_fail() {
        let m = machine(&ALL);
        let old = m.dir.path().join("old/loopherd");
        write_hooks(&m.install.settings, &old, &ALL[..1]);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        let path_exe = m.install.on_path.clone().unwrap();
        let ours = |sub: &str| crate::install_hook::hook_command(&path_exe, sub).unwrap();
        for (event, sub) in &EVENTS[1..] {
            settings["hooks"][event] = group(event, &ours(sub));
        }
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "failure hook binary: hooks call {}, not the loopherd on PATH ({}); \
                 fix: loopherd install-hook\n\
                 doctor: 17 ok, 0 warnings, 1 failures\n",
                old.display(),
                path_exe.display()
            )
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_stale_loopherd_first_on_path_names_the_running_binary_in_the_fix() {
        let mut m = machine(&ALL);
        let stale = m.dir.path().join("cargo/bin/loopherd");
        m.install.on_path = Some(stale.clone());
        let running = m.install.running.clone();
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "failure hook binary: hooks call {running}, not the loopherd on PATH ({stale}); \
                 fix: put {running_dir} before {stale_dir} on PATH, then '{running}' install-hook\n\
                 warning running binary: this is {running}, not the loopherd on PATH ({stale}); \
                 fix: run the loopherd on PATH\n\
                 doctor: 16 ok, 1 warnings, 1 failures\n",
                running = running.display(),
                stale = stale.display(),
                running_dir = running.parent().unwrap().display(),
                stale_dir = stale.parent().unwrap().display(),
            )
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn no_loopherd_on_path_fails() {
        let mut m = machine(&ALL);
        m.install.on_path = None;
        let (out, code) = run(&m, &herdr());
        assert!(
            out.starts_with(
                "failure hook binary: no loopherd on PATH; \
                 fix: install loopherd on PATH, then loopherd install-hook\n"
            ),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn running_another_binary_than_paths_warns() {
        let mut m = machine(&ALL);
        let dev = m.dir.path().join("target/debug/loopherd");
        m.install.running = dev.clone();
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "warning running binary: this is {}, not the loopherd on PATH ({}); \
                 fix: run the loopherd on PATH\n\
                 doctor: 17 ok, 1 warnings, 0 failures\n",
                dev.display(),
                m.install.on_path.as_ref().unwrap().display()
            )
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn the_same_binary_through_a_symlink_is_paths() {
        let mut m = machine(&ALL);
        let real = m.install.running.clone();
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        let link = m.dir.path().join("loopherd");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        m.install.on_path = Some(link);
        assert_eq!(run(&m, &herdr()).1, 0);
        assert!(run(&m, &herdr()).0.starts_with("doctor: 18 ok"));
    }

    #[test]
    fn an_unreachable_herdr_fails_naming_its_socket() {
        let m = machine(&ALL);
        let herdr = herdr();
        herdr.set_unreachable(true);
        let (out, code) = run(&m, &herdr);
        assert_eq!(
            out,
            "failure herdr: no herdr server answers on /run/fake/herdr.sock; fix: herdr\n\
             doctor: 15 ok, 0 warnings, 1 failures\n"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_herdr_older_than_min_herdr_fails() {
        let m = machine(&ALL);
        for (version, ok) in [
            ("0.9.4", false),
            ("0.8.12", false),
            ("0.9.5", true),
            ("0.9.10", true),
            ("1.0.0-rc.1", true),
        ] {
            let mut herdr = herdr();
            herdr.version = version.into();
            let (out, code) = run(&m, &herdr);
            match ok {
                true => assert_eq!(code, 0, "{version}: {out}"),
                false => assert_eq!(
                    out,
                    format!(
                        "failure herdr version: {version} is older than {MIN_HERDR}; \
                         fix: upgrade herdr to {MIN_HERDR} or later and restart its server\n\
                         doctor: 17 ok, 0 warnings, 1 failures\n"
                    )
                ),
            }
        }
    }

    #[test]
    fn verbose_lists_every_check() {
        let m = machine(&ALL);
        let checks = checks(&m.install, &m.setup, None, &herdr()).unwrap();
        let exe = m.install.running.display();
        let dir = m.dir.path().display();
        let settings = m.install.settings.display();
        let config = m.config().display().to_string();
        assert_eq!(
            text(&checks, true),
            format!(
                "ok Stop hook: in {settings}\n\
                 ok StopFailure hook: in {settings}\n\
                 ok SessionStart hook: in {settings}\n\
                 ok PreToolUse hook: in {settings}\n\
                 ok UserPromptSubmit hook: in {settings}\n\
                 ok SubagentStart hook: in {settings}\n\
                 ok SubagentStop hook: in {settings}\n\
                 ok hook binary: hooks call {exe}\n\
                 ok skill: {dir}/skills/loopherd links to {dir}/.agents/skills/loopherd/SKILL.md\n\
                 ok running binary: {exe}\n\
                 ok sandbox: can start\n\
                 ok herdr: answers on /run/fake/herdr.sock\n\
                 ok herdr version: 0.9.5\n\
                 ok herdr plugin: {VERSION}, enabled\n\
                 ok config: {config}\n\
                 ok lead defaults: set in {config}\n\
                 ok context limits: set in {config}\n\
                 ok ledger: 0 entries\n\
                 doctor: 18 ok, 0 warnings, 0 failures\n"
            )
        );
    }

    #[test]
    fn json_gives_every_check_and_the_counts() {
        let m = machine(&without("UserPromptSubmit"));
        let checks = checks(&m.install, &m.setup, None, &herdr()).unwrap();
        let data: Value = serde_json::from_str(&json(&checks)).unwrap();
        assert_eq!(
            (&data["ok"], &data["warnings"], &data["failures"]),
            (&json!(17), &json!(1), &json!(0))
        );
        assert_eq!(data["checks"].as_array().unwrap().len(), 18);
        assert_eq!(
            data["checks"][4],
            json!({
                "name": "UserPromptSubmit hook",
                "status": "warning",
                "detail": format!("not in {}", m.install.settings.display()),
                "fix": "loopherd install-hook",
            })
        );
        assert_eq!(data["checks"][0]["fix"], Value::Null);
    }

    #[test]
    fn a_settings_path_with_spaces_and_quotes_is_quoted_in_the_fix() {
        let mut m = machine(&ALL[..3]);
        m.install.settings = m.dir.path().join("a dir/it's.json");
        m.install.settings_given = true;
        let (out, _) = run(&m, &herdr());
        let quoted = format!("'{}/a dir/it'\\''s.json'", m.dir.path().display());
        assert!(
            out.contains(&format!("fix: loopherd install-hook --settings {quoted}\n")),
            "{out}"
        );
    }

    #[test]
    fn a_tool_hook_under_another_matcher_than_bash_is_missing() {
        let m = machine(&ALL);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        settings["hooks"]["PreToolUse"][0]["matcher"] = json!("Edit");
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m, &herdr());
        assert!(out.starts_with("failure PreToolUse hook: not in "), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn the_users_own_hooks_are_neither_ours_nor_a_binary_to_check() {
        let m = machine(&ALL[1..]);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        let theirs = json!({"type": "command", "command": "'/usr/bin/notify-send' done"});
        settings["hooks"]["Stop"] = json!([{"hooks": [theirs]}]);
        settings["hooks"]["StopFailure"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(theirs);
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "failure Stop hook: not in {}; fix: loopherd install-hook\n\
                 doctor: 17 ok, 0 warnings, 1 failures\n",
                m.install.settings.display()
            )
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_hook_calling_a_relative_loopherd_fails() {
        let m = machine(&ALL);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        settings["hooks"]["Stop"][0]["hooks"][0]["command"] = json!("'loopherd' stop-hook || true");
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m, &herdr());
        assert!(
            out.starts_with("failure hook binary: hooks call loopherd, not the loopherd on PATH"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    const VERSION: &str = env!("CARGO_PKG_VERSION");

    fn plugin(enabled: bool, version: &str, linked: Option<&str>) -> Option<Plugin> {
        Some(Plugin {
            enabled,
            version: version.into(),
            linked: linked.map(PathBuf::from),
        })
    }

    fn with_plugin(plugin: Option<Plugin>, verbose: bool) -> (String, i32) {
        let m = machine(&ALL);
        let mut herdr = herdr();
        herdr.loopherd_plugin = plugin;
        let checks = m.checks(None, &herdr).unwrap();
        (text(&checks, verbose), exit_code(&checks))
    }

    #[test]
    fn a_missing_plugin_warns_naming_plugin_install_and_exits_0() {
        assert_eq!(
            with_plugin(None, false),
            (
                "warning herdr plugin: loopherd's herdr plugin isn't installed, so its \
                 actions, popups and event hooks are unavailable; \
                 fix: herdr plugin install junderdo/loopherd/herdr-plugin\n\
                 doctor: 17 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn a_disabled_plugin_warns_naming_plugin_enable() {
        assert_eq!(
            with_plugin(plugin(false, VERSION, None), false),
            (
                "warning herdr plugin: loopherd's herdr plugin is disabled, so its \
                 actions, popups and event hooks are unavailable; \
                 fix: herdr plugin enable loopherd\n\
                 doctor: 17 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn a_plugin_at_another_version_than_loopherd_warns() {
        let (out, code) = with_plugin(plugin(true, "0.0.1", None), false);
        assert_eq!(
            out,
            format!(
                "warning herdr plugin: plugin 0.0.1 differs from loopherd {VERSION}; \
                 fix: herdr plugin update loopherd\n\
                 doctor: 17 ok, 1 warnings, 0 failures\n"
            )
        );
        assert_eq!(code, 0);
        let (out, _) = with_plugin(plugin(true, "0.0.1", Some("/src/herdr-plugin")), false);
        assert!(
            out.starts_with(&format!(
                "warning herdr plugin: plugin 0.0.1 differs from loopherd {VERSION}; \
                 fix: bring the linked plugin at /src/herdr-plugin to loopherd {VERSION}\n"
            )),
            "{out}"
        );
    }

    #[test]
    fn a_plugin_newer_than_loopherd_names_updating_loopherd() {
        for linked in [None, Some("/src/herdr-plugin")] {
            let (out, code) = with_plugin(plugin(true, "999.0.0", linked), false);
            assert_eq!(
                out,
                format!(
                    "warning herdr plugin: plugin 999.0.0 differs from loopherd {VERSION}; \
                     fix: update loopherd to 999.0.0\n\
                     doctor: 17 ok, 1 warnings, 0 failures\n"
                )
            );
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn a_sound_plugin_is_listed_only_with_verbose() {
        let (out, _) = with_plugin(plugin(true, VERSION, None), true);
        assert!(
            out.contains(&format!("ok herdr plugin: {VERSION}, enabled\n")),
            "{out}"
        );
        let (out, _) = with_plugin(plugin(true, VERSION, Some("/src/herdr-plugin")), true);
        assert!(
            out.contains(&format!(
                "ok herdr plugin: {VERSION}, enabled, linked from /src/herdr-plugin\n"
            )),
            "{out}"
        );
    }

    fn herdr_failure(server: Server) -> String {
        let m = machine(&ALL);
        let mut herdr = herdr();
        herdr.server = Some(server);
        let (out, code) = run(&m, &herdr);
        assert_eq!(code, 1, "{out}");
        out.lines().next().unwrap().to_string()
    }

    #[test]
    fn a_herdr_that_does_not_answer_in_time_fails_naming_the_deadline() {
        let socket = Some(PathBuf::from("/run/wedged.sock"));
        assert_eq!(
            herdr_failure(Server::NoAnswer { socket }),
            "failure herdr: herdr didn't answer on /run/wedged.sock within 5s; \
             fix: restart herdr's server"
        );
        assert_eq!(
            herdr_failure(Server::NoAnswer { socket: None }),
            "failure herdr: herdr didn't answer on herdr's default socket within 5s; \
             fix: restart herdr's server"
        );
    }

    #[test]
    fn a_missing_herdr_or_one_without_a_version_fails_saying_which() {
        assert_eq!(
            herdr_failure(Server::Missing),
            "failure herdr: no herdr on PATH; fix: install herdr"
        );
        let socket = Some(PathBuf::from("/run/s.sock"));
        assert_eq!(
            herdr_failure(Server::Running {
                socket,
                version: None
            }),
            "failure herdr version: herdr's server didn't report a version; \
             fix: upgrade herdr to 0.9.5 or later and restart its server"
        );
    }

    fn problems(m: &Machine, herdr: &FakeHerdr) -> Vec<String> {
        problems_on(m, None, herdr)
    }

    /// Each warning and failure line, without the summary.
    fn problems_on(m: &Machine, herd: Option<&str>, herdr: &FakeHerdr) -> Vec<String> {
        let (out, _) = run_on(m, herd, herdr);
        out.lines()
            .filter(|l| !l.starts_with("doctor: "))
            .map(str::to_string)
            .collect()
    }

    fn member(name: &str, herd: &str, pane: &str, socket: Option<&str>) -> AgentRecord {
        serde_json::from_value(json!({
            "name": name, "pane_id": pane, "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd, "socket": socket,
        }))
        .unwrap()
    }

    #[test]
    fn a_config_that_does_not_load_fails_naming_the_file_and_error() {
        let m = machine(&ALL);
        let config = m.config();
        for (text, error) in [
            ("context_window = [", "TOML parse error"),
            ("colour = 1\n", "unknown field `colour`"),
            (
                "context_window = 5\n",
                "context_window must be at least 10000, got 5",
            ),
        ] {
            std::fs::write(&config, text).unwrap();
            let (out, code) = run(&m, &herdr());
            assert!(
                out.starts_with(&format!("failure config: {}: ", config.display())),
                "{out}"
            );
            assert!(out.contains(error), "{out}");
            let fix = format!("; fix: fix {} by hand\ndoctor: ", config.display());
            assert!(out.contains(&fix), "{out}");
            assert_eq!(code, 1);
        }
    }

    #[test]
    fn a_notify_table_that_does_not_load_fails_naming_the_file() {
        let m = machine(&ALL);
        let config = m.config();
        std::fs::write(
            &config,
            format!("{CONFIG}[notify]\nenabled = true\nloud = 1\n"),
        )
        .unwrap();
        let (out, code) = run(&m, &herdr());
        let line = out.lines().next().unwrap();
        assert!(
            line.starts_with(&format!("failure notify: {}: [notify]: ", config.display())),
            "{out}"
        );
        assert!(line.contains("unknown field `loud`"), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_missing_config_is_fine_but_its_unset_settings_warn_naming_setup() {
        let m = machine(&ALL);
        std::fs::remove_file(m.config()).unwrap();
        let config = m.config().display().to_string();
        assert_eq!(
            run(&m, &herdr()),
            (
                format!(
                    "warning lead defaults: unset in {config}; fix: loopherd setup\n\
                     warning context limits: unset in {config}; fix: loopherd setup\n\
                     doctor: 16 ok, 2 warnings, 0 failures\n"
                ),
                0
            )
        );
    }

    #[test]
    fn any_one_limit_set_counts_as_set() {
        let m = machine(&ALL);
        for limit in [
            "default_context_limit = 100000",
            "shepherd.context_limit = 100000",
        ] {
            std::fs::write(
                m.config(),
                format!("{limit}\n[lead_defaults]\nmerge = true\n"),
            )
            .unwrap();
            assert_eq!(problems(&m, &herdr()), Vec::<String>::new());
        }
    }

    #[test]
    fn an_invalid_herd_file_fails_with_the_line_herds_gives() {
        let m = machine(&ALL);
        m.herd("good");
        m.herd_file("bad", "repo = \"/r\"\ncolour = 1\n");
        let (herds, _) = crate::herd::herds(m.paths(), &m.install.settings).unwrap();
        let file = m.paths().herds.join("bad.toml");
        let checks = m.checks(None, &herdr()).unwrap();
        let bad = checks.iter().find(|c| c.name == "herd bad").unwrap();
        assert_eq!(bad.status, Status::Failure);
        assert!(
            bad.detail.starts_with("invalid bad.toml: "),
            "{}",
            bad.detail
        );
        assert!(
            bad.detail.contains("unknown field `colour`"),
            "{}",
            bad.detail
        );
        assert!(bad.detail.contains('\n'), "a TOML error spans lines");
        assert!(herds.contains(&format!("{}\n", bad.detail)), "{herds}");
        let (out, code) = run(&m, &herdr());
        let fix = format!("; fix: fix {} by hand", file.display());
        let line = out.lines().next().unwrap();
        assert!(
            line.starts_with("failure herd bad: invalid bad.toml: TOML parse error"),
            "{out}"
        );
        assert!(line.ends_with(&fix), "{out}");
        assert_eq!(out.lines().count(), 2, "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_credentials_herd_with_every_unix_socket_open_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n\
                 unix_sockets = \"all\"\n",
                repo.to_str().unwrap()
            ),
        );

        let checks = m.checks(None, &herdr()).unwrap();

        let herd = checks.iter().find(|c| c.name == "herd h").unwrap();
        assert_eq!(herd.status, Status::Failure);
        assert!(
            herd.detail.starts_with(
                "invalid h.toml: lead: can't start with merge_guard = \"credentials\": \
                 unix_sockets = \"all\" opens"
            ),
            "{}",
            herd.detail
        );
    }

    fn without_bubblewrap(m: &mut Machine) {
        m.install.sandbox = Err(Unavailable {
            why: "bubblewrap not installed".into(),
            fix: "install bubblewrap".into(),
        });
    }

    #[test]
    fn a_sandbox_that_cant_start_fails_naming_why_and_the_fix_while_a_herd_has_a_lead() {
        let mut m = machine(&ALL);
        without_bubblewrap(&mut m);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\n",
                repo.to_str().unwrap()
            ),
        );
        assert_eq!(
            problems(&m, &herdr()),
            ["failure sandbox: can't start: bubblewrap not installed; fix: install bubblewrap"]
        );
    }

    #[test]
    fn a_sandbox_that_cant_block_unix_sockets_fails_for_a_lead_that_would_be_refused() {
        let mut m = machine(&ALL);
        m.install.sandbox = Ok(Sockets::Open(Unavailable {
            why: "Claude Code finds no seccomp filter to block them with".into(),
            fix: "npm install -g @anthropic-ai/sandbox-runtime".into(),
        }));
        let repo = m.herd("h");
        let lead = |line: &str| {
            format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\n{line}\n",
                repo.to_str().unwrap()
            )
        };
        let check = "sandbox: can't block Unix sockets: Claude Code finds no seccomp filter to \
                     block them with";
        let fix = "; fix: npm install -g @anthropic-ai/sandbox-runtime";

        m.herd_file("h", &lead(""));
        assert_eq!(problems(&m, &herdr()), [format!("failure {check}{fix}")]);

        m.herd_file("h", &lead("sandbox = \"prefer\""));
        let found = problems(&m, &herdr());
        assert!(
            found.contains(&format!("warning {check}{fix}")),
            "{found:?}"
        );

        m.herd_file("h", &lead("unix_sockets = \"all\""));
        let found = problems(&m, &herdr());
        assert!(
            found.contains(&format!("warning {check}{fix}")),
            "{found:?}"
        );
    }

    #[test]
    fn a_running_lead_whose_sandbox_cant_block_unix_sockets_is_named() {
        let m = machine(&ALL);
        let mut lead = member("lead-h", "h", "w1:p2", None);
        lead.role = Some(crate::ledger::Role::Lead);
        lead.claude_args = vec![
            "--settings".into(),
            crate::sandbox::settings_file(m.paths(), "lead-h")
                .display()
                .to_string(),
        ];
        lead.sockets_unblocked = Some("Claude Code finds no seccomp filter".into());
        m.ledger(&[lead]);
        let found: Vec<_> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.starts_with("warning lead "))
            .collect();
        assert_eq!(
            found,
            [
                "warning lead lead-h: lead-h's sandbox can't block Unix sockets (Claude Code finds \
              no seccomp filter), so herdr, your ssh agent and WSL2's Windows interop stay open \
              to it; fix: fix the sandbox check's cause, then quit its Claude and run loopherd \
              revive --name lead-h"
            ]
        );
    }

    #[test]
    fn a_sandbox_that_cant_start_only_warns_while_no_herd_has_a_lead() {
        let mut m = machine(&ALL);
        without_bubblewrap(&mut m);
        let repo = m.herd("h");
        m.herd_file("h", &format!("repo = {:?}\n", repo.to_str().unwrap()));
        assert_eq!(
            problems(&m, &herdr()),
            ["warning sandbox: can't start: bubblewrap not installed; \
                 no herd has a lead to run in it yet; fix: install bubblewrap"]
        );
    }

    #[test]
    fn a_running_lead_without_its_sandbox_is_named_with_how_to_resume_it_in_one() {
        let m = machine(&ALL);
        let mut old = member("lead-old", "h", "w1:p1", None);
        old.role = Some(crate::ledger::Role::Lead);
        let mut hook_only = member("lead-pre", "h", "w1:p2", None);
        hook_only.role = Some(crate::ledger::Role::Lead);
        hook_only.unsandboxed = Some("bubblewrap not installed".into());
        let settings = crate::sandbox::settings_file(m.paths(), "lead-ok");
        let mut sandboxed = member("lead-ok", "h", "w1:p3", None);
        sandboxed.role = Some(crate::ledger::Role::Lead);
        sandboxed.claude_args = vec!["--settings".into(), settings.display().to_string()];
        m.ledger(&[old, hook_only, sandboxed]);

        let found: Vec<_> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.starts_with("warning lead "))
            .collect();

        assert_eq!(
            found,
            [
                "warning lead lead-old: lead-old runs without its sandbox (it was started \
                 without its sandbox), guarded by its tool hook alone; fix: quit its Claude and \
                 run loopherd revive --name lead-old",
                "warning lead lead-pre: lead-pre runs without its sandbox (bubblewrap not \
                 installed), guarded by its tool hook alone; fix: quit its Claude and run \
                 loopherd revive --name lead-pre",
            ]
        );
    }

    #[test]
    fn a_lead_whose_launch_record_keeps_no_path_warns_naming_its_revive() {
        let m = machine(&ALL);
        let sandbox = m.paths().config_dir().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let mut leads = vec![];
        for (name, record) in [
            ("lead-old", r#"{"herd": "h", "args": []}"#),
            (
                "lead-new",
                r#"{"herd": "h", "args": [], "path": "/usr/bin:/bin"}"#,
            ),
        ] {
            std::fs::write(sandbox.join(format!("{name}.lead.json")), record).unwrap();
            let mut lead = member(name, "h", "w1:p1", None);
            lead.role = Some(crate::ledger::Role::Lead);
            leads.push(lead);
        }
        m.ledger(&leads);

        let found: Vec<_> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.contains("keeps no PATH"))
            .collect();

        assert_eq!(
            found,
            [
                "warning lead lead-old: lead-old's launch record keeps no PATH, so its watch and \
              broker run gh, git and its herd's checks from whatever PATH they inherit; fix: \
              quit its Claude and run loopherd revive --name lead-old"
            ]
        );
    }

    #[test]
    fn a_broker_on_another_build_than_paths_warns_naming_the_shepherds_restart() {
        let mut m = machine(&ALL);
        let on_path = m.dir.path().join("bin/loopherd");
        let running = m.dir.path().join("target/release/loopherd");
        for exe in [&on_path, &running] {
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(exe, "loopherd").unwrap();
        }
        m.install.on_path = Some(on_path.clone());
        m.install.running = running;
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            ),
        );
        let kept = crate::sandbox::Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };
        let mut leads = vec![];
        for name in ["lead-new", "lead-rebuilt", "lead-other", "lead-old"] {
            crate::sandbox::prepare(m.paths(), name, &kept, || {
                Ok(crate::sandbox::Sockets::Blocked)
            })
            .unwrap();
            let mut lead = member(name, "h", "w1:p1", None);
            lead.role = Some(crate::ledger::Role::Lead);
            let settings = crate::sandbox::settings_file(m.paths(), name);
            lead.claude_args = vec!["--settings".into(), settings.display().to_string()];
            leads.push(lead);
        }
        m.ledger(&leads);
        let mut held = vec![];
        let mut lock = |name: &str, record: String| {
            let lock = m
                .paths()
                .config_dir()
                .join("broker")
                .join(format!("{name}.lock"));
            std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock)
                .unwrap();
            held.push(crate::lock::Lock::try_take(file).unwrap().unwrap());
            std::fs::write(&lock, format!("300\nmarks\n{record}")).unwrap();
        };
        let version = env!("LOOPHERD_VERSION");
        let meta = std::fs::metadata(&on_path).unwrap();
        let (dev, ino) = (meta.dev(), meta.ino());
        let linked_bin = m.dir.path().join("linked-bin");
        std::os::unix::fs::symlink(on_path.parent().unwrap(), &linked_bin).unwrap();
        let linked = linked_bin.join("loopherd");
        lock(
            "lead-new",
            format!(
                "version {version}\nbinary {}\nfile {dev} {ino}\n",
                on_path.display()
            ),
        );
        lock(
            "lead-rebuilt",
            format!(
                "version {version}\nbinary {}\nfile {dev} {}\n",
                linked.display(),
                ino + 1
            ),
        );
        let on_path = on_path.canonicalize().unwrap();
        let on_path = on_path.display();
        lock(
            "lead-other",
            "version 0.0.1 (abc1234)\nbinary /old/loopherd\nfile 1 2\n".into(),
        );
        lock("lead-old", format!("version {version}\nbinary {on_path}\n"));

        let brokers: Vec<String> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| is_broker_check(p))
            .collect();

        let fix = |lead: &str| format!("fix: the shepherd runs loopherd broker-restart {lead}");
        assert_eq!(
            brokers,
            [
                format!(
                    "warning broker lead-old: lead-old's broker runs a loopherd from before \
                     brokers recorded their build, not the loopherd on PATH ({on_path}), so its \
                     lead's commands may fail; {}",
                    fix("lead-old")
                ),
                format!(
                    "warning broker lead-other: lead-other's broker runs loopherd 0.0.1 \
                     (abc1234) at /old/loopherd, not the loopherd on PATH ({on_path}), so its \
                     lead's commands may fail; {}",
                    fix("lead-other")
                ),
                format!(
                    "warning broker lead-rebuilt: lead-rebuilt's broker runs an earlier build of \
                     {on_path} (loopherd {version}), so its lead's commands may fail; {}",
                    fix("lead-rebuilt")
                ),
            ]
        );
    }

    /// lead-h of herd h, with a broker lock held and recording a rebuild of
    /// the loopherd on PATH, returned with the lock's path and its record
    /// once it runs that loopherd.
    fn lead_on_an_earlier_build(m: &mut Machine) -> (crate::lock::Lock, PathBuf, String) {
        let on_path = m.dir.path().join("bin/loopherd");
        std::fs::create_dir_all(on_path.parent().unwrap()).unwrap();
        std::fs::write(&on_path, "loopherd").unwrap();
        m.install.on_path = Some(on_path.clone());
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            ),
        );
        let kept = crate::sandbox::Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };
        crate::sandbox::prepare(m.paths(), "lead-h", &kept, || {
            Ok(crate::sandbox::Sockets::Blocked)
        })
        .unwrap();
        let mut lead = member("lead-h", "h", "w1:p1", None);
        lead.role = Some(crate::ledger::Role::Lead);
        let settings = crate::sandbox::settings_file(m.paths(), "lead-h");
        lead.claude_args = vec!["--settings".into(), settings.display().to_string()];
        m.ledger(&[lead]);
        let lock = m.paths().config_dir().join("broker/lead-h.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let held = crate::lock::Lock::try_take(std::fs::File::create(&lock).unwrap())
            .unwrap()
            .unwrap();
        let meta = std::fs::metadata(&on_path).unwrap();
        let record = |ino: u64| {
            format!(
                "300\nmarks\nversion {}\nbinary {}\nfile {} {ino}\n",
                env!("LOOPHERD_VERSION"),
                on_path.display(),
                meta.dev()
            )
        };
        std::fs::write(&lock, record(meta.ino() + 1)).unwrap();
        (held, lock, record(meta.ino()))
    }

    /// Matched on the check's name, as a temp dir's path may itself say "broker".
    fn is_broker_check(problem: &str) -> bool {
        problem
            .split(':')
            .next()
            .is_some_and(|name| name.contains(" broker "))
    }

    fn broker_problems(m: &Machine) -> Vec<String> {
        problems(m, &herdr())
            .into_iter()
            .filter(|p| is_broker_check(p))
            .collect()
    }

    #[test]
    fn a_broker_that_moves_onto_the_install_within_the_grace_draws_no_warning() {
        let mut m = machine(&ALL);
        let (_held, lock, moved) = lead_on_an_earlier_build(&mut m);
        m.install.broker_move_grace = Duration::from_secs(5);
        let moving = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::write(&lock, moved).unwrap();
        });

        let brokers = broker_problems(&m);

        moving.join().unwrap();
        assert_eq!(brokers, Vec::<String>::new());
    }

    #[test]
    fn a_broker_that_stays_on_an_earlier_build_past_the_grace_is_warned_about() {
        let mut m = machine(&ALL);
        let (_held, _, _) = lead_on_an_earlier_build(&mut m);
        m.install.broker_move_grace = Duration::from_millis(100);

        let brokers = broker_problems(&m);

        assert_eq!(brokers.len(), 1, "{brokers:?}");
        assert!(
            brokers[0].ends_with("fix: the shepherd runs loopherd broker-restart lead-h"),
            "{}",
            brokers[0]
        );
    }

    #[test]
    fn doctor_run_inside_a_leads_broker_says_that_broker_moves_once_answered() {
        let mut m = machine(&ALL);
        let (_held, _, _) = lead_on_an_earlier_build(&mut m);
        m.install.in_broker_of = Some("lead-h".into());

        let out = text(&m.checks(None, &herdr()).unwrap(), true);

        assert!(broker_problems(&m).is_empty());
        assert!(
            out.contains("lead-h's broker runs this check, and moves onto the loopherd on PATH"),
            "{out}"
        );
    }

    #[test]
    fn with_no_loopherd_on_path_a_broker_is_checked_against_the_running_binary() {
        let mut m = machine(&ALL);
        let running = m.dir.path().join("target/release/loopherd");
        std::fs::create_dir_all(running.parent().unwrap()).unwrap();
        std::fs::write(&running, "loopherd").unwrap();
        m.install.on_path = None;
        m.install.running = running.clone();
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            ),
        );
        let kept = crate::sandbox::Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };
        crate::sandbox::prepare(m.paths(), "lead-h", &kept, || {
            Ok(crate::sandbox::Sockets::Blocked)
        })
        .unwrap();
        let mut lead = member("lead-h", "h", "w1:p1", None);
        lead.role = Some(crate::ledger::Role::Lead);
        let settings = crate::sandbox::settings_file(m.paths(), "lead-h");
        lead.claude_args = vec!["--settings".into(), settings.display().to_string()];
        m.ledger(&[lead]);
        let lock = m.paths().config_dir().join("broker/lead-h.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&lock).unwrap();
        let _held = crate::lock::Lock::try_take(file).unwrap().unwrap();
        std::fs::write(
            &lock,
            "300\nmarks\nversion 0.0.1 (abc1234)\nbinary /old/loopherd\nfile 1 2\n",
        )
        .unwrap();

        let brokers: Vec<String> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| is_broker_check(p))
            .collect();

        assert_eq!(
            brokers,
            [format!(
                "warning broker lead-h: lead-h's broker runs loopherd 0.0.1 (abc1234) at \
                 /old/loopherd, not this one ({}), so its lead's commands may fail; \
                 fix: the shepherd runs loopherd broker-restart lead-h",
                running.canonicalize().unwrap().display()
            )]
        );
    }

    #[test]
    fn an_unretired_lead_on_this_herdr_with_no_broker_fails_naming_revive() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            ),
        );
        let kept = crate::sandbox::Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };
        crate::sandbox::prepare(m.paths(), "lead-up", &kept, || {
            Ok(crate::sandbox::Sockets::Blocked)
        })
        .unwrap();
        let settings = crate::sandbox::settings_file(m.paths(), "lead-up");
        let lead = |name: &str| {
            let mut lead = member(name, "h", "w1:p1", None);
            lead.role = Some(crate::ledger::Role::Lead);
            lead.claude_args = vec!["--settings".into(), settings.display().to_string()];
            lead
        };
        let mut retired = lead("lead-old");
        retired.retired = true;
        let mut elsewhere = lead("lead-dev");
        elsewhere.socket = Some("/run/dev.sock".into());
        m.ledger(&[lead("lead-down"), lead("lead-up"), retired, elsewhere]);
        let paths = m.paths().clone();
        let ledger = FileLedger::new(m.state().join("ledger.json"));
        let serving = std::thread::spawn(move || {
            let herdr = herdr();
            let host = crate::broker::Host {
                herdr: &herdr,
                procs: &crate::process::ProcFs,
                claude_dir: None,
                process: crate::ledger::Process { pid: 0, start: 0 },
                live: |session| session.to_string(),
                spawn_waiter: |_, _| Ok(()),
                prs: &crate::github::fake::FakePrs(Ok(crate::github::PrLookup::None)),
                follow: None,
            };
            let poll = Duration::from_millis(10);
            crate::broker::serve(&host, &ledger, &paths, "lead-up", poll, None)
        });
        let given_up = std::time::Instant::now() + Duration::from_secs(5);
        while !crate::broker::running(m.paths(), "lead-up") {
            assert!(!serving.is_finished(), "{:?}", serving.join());
            assert!(
                std::time::Instant::now() < given_up,
                "the broker never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let brokers = |socket: Option<&str>| -> Vec<String> {
            let mut herdr = herdr();
            herdr.socket = socket.map(PathBuf::from);
            problems(&m, &herdr)
                .into_iter()
                .filter(|p| p.starts_with("failure broker"))
                .collect()
        };
        let down = |name: &str| {
            format!(
                "failure broker {name}: no broker is running for {name}, so its \
                 loopherd commands fail; fix: loopherd revive --name {name}"
            )
        };

        assert_eq!(brokers(Some("/run/live.sock")), [down("lead-down")]);
        assert_eq!(brokers(None), [down("lead-dev"), down("lead-down")]);
        let mut ledger = FileLedger::new(m.state().join("ledger.json"));
        ledger
            .update("lead-up", &mut |r| {
                r.retired = true;
                Ok(())
            })
            .unwrap();
        serving.join().unwrap().unwrap();
    }

    #[test]
    fn a_herd_whose_workers_run_on_prefer_is_named_and_one_on_off_or_require_isnt() {
        let m = machine(&ALL);
        for (herd, worker) in [("off", "off"), ("pre", "prefer"), ("req", "require")] {
            let repo = m.herd(herd);
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
                     [worker]\nsandbox = \"{worker}\"\n",
                    repo.to_str().unwrap()
                ),
            );
        }

        assert_eq!(
            problems(&m, &herdr()),
            [
                "warning herd pre workers: sandbox prefer: its workers start unsandboxed \
                 whenever their sandbox can't start; fix: loopherd herd set pre worker.sandbox \
                 '\"require\"'"
            ]
        );
    }

    #[test]
    fn a_credentials_herd_whose_workers_sandbox_isnt_required_fails() {
        let m = machine(&ALL);
        for (herd, lines) in [
            ("off", "[worker]\nsandbox = \"off\"\n"),
            ("unset", ""),
            ("pre", "[worker]\nsandbox = \"prefer\"\n"),
            ("req", "[worker]\nsandbox = \"require\"\n"),
        ] {
            let repo = m.herd(herd);
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
                     [lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n{lines}",
                    repo.to_str().unwrap()
                ),
            );
        }

        let checks = m.checks(None, &herdr()).unwrap();

        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.status == Status::Failure && c.detail.contains("[worker] sandbox is"))
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(failed, ["herd off", "herd pre", "herd unset"]);
        assert!(!checks.iter().any(|c| c.name.ends_with(" workers")));
    }

    #[test]
    fn a_running_worker_without_its_herds_sandbox_is_named_and_one_in_an_off_herd_isnt() {
        let m = machine(&ALL);
        for (herd, worker) in [("h", "require"), ("plain", "off")] {
            let repo = m.herd(herd);
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
                     [worker]\nsandbox = \"{worker}\"\n",
                    repo.to_str().unwrap()
                ),
            );
        }
        let old = member("w-old", "h", "w1:p1", None);
        let mut flagged = member("w-pre", "h", "w1:p2", None);
        flagged.unsandboxed = Some("bubblewrap not installed".into());
        let settings = crate::sandbox::settings_file(m.paths(), "w-ok");
        let mut sandboxed = member("w-ok", "h", "w1:p3", None);
        sandboxed.claude_args = vec!["--settings".into(), settings.display().to_string()];
        let off = member("w-off", "plain", "w1:p4", None);
        m.ledger(&[old, flagged, sandboxed, off]);

        let found: Vec<_> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.starts_with("warning worker "))
            .collect();

        assert_eq!(
            found,
            [
                "warning worker w-old: w-old runs unsandboxed (it was started without its \
                 sandbox); fix: quit its Claude and run loopherd revive --name w-old",
                "warning worker w-pre: w-pre runs unsandboxed (bubblewrap not installed); fix: \
                 quit its Claude and run loopherd revive --name w-pre",
            ]
        );
    }

    #[test]
    fn a_running_sandboxed_worker_whose_herd_is_gone_is_named() {
        let m = machine(&ALL);
        let dir = m.paths().config_dir().join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("w-gone.member.json"),
            r#"{"herd": "gone", "template": "t", "args": []}"#,
        )
        .unwrap();
        m.ledger(&[member("w-gone", "gone", "w1:p1", None)]);

        let found: Vec<_> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.starts_with("warning worker "))
            .collect();

        assert_eq!(
            found,
            [
                "warning worker w-gone: its sandbox isn't checked: w-gone was started in herd \
                 gone's worker sandbox, and herd gone is gone; restore its herd file, or run \
                 loopherd retire w-gone; fix: fix that, then run loopherd doctor again"
            ]
        );
    }

    #[test]
    fn a_herd_whose_lead_runs_on_prefer_is_named() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            ),
        );
        assert_eq!(
            problems(&m, &herdr()),
            [
                "warning herd h sandbox: prefer: its lead starts without the sandbox, \
              guarded by its tool hook alone, whenever the sandbox can't start; \
              fix: loopherd herd set h lead.sandbox '\"require\"'"
            ]
        );
    }

    #[test]
    fn a_herd_whose_lead_may_use_every_unix_socket_is_warned_of_and_of_windows_interop() {
        let mut m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\nunix_sockets = \"all\"\n",
                repo.to_str().unwrap()
            ),
        );
        let opened = "warning herd h unix_sockets: lead-h's sandbox lets it use any Unix socket, \
                      as herd h's [lead] unix_sockets = \"all\" says, which reopens the herdr \
                      socket, your ssh agent and WSL2's Windows interop to it";
        let fix = "; fix: loopherd herd unset h lead.unix_sockets";
        assert_eq!(problems(&m, &herdr()), [format!("{opened}{fix}")]);

        m.install.wsl_interop = true;
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "{opened}; WSL2's Windows interop is enabled on this machine, so it can run \
                 Windows programs outside its sandbox{fix}"
            )]
        );
    }

    /// Writes the human's `~/.claude/settings.json`, returning its path.
    fn user_settings(m: &Machine, text: &str) -> PathBuf {
        let file = m.paths().home.join(".claude/settings.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        file
    }

    #[test]
    fn user_settings_that_loosen_a_leads_sandbox_fail_under_require_and_warn_under_prefer() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        let lead = |mode: &str| {
            format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\nsandbox = \"{mode}\"\n",
                repo.to_str().unwrap()
            )
        };
        let file = user_settings(&m, r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);
        let file = file.display();

        m.herd_file("h", &lead("require"));
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h sandbox settings: {file}: sandbox.excludedCommands runs \
                 \"docker *\" outside it; fix: remove it, or loopherd herd set h lead.sandbox \
                 '\"prefer\"' to start its lead with it as a warning"
            )]
        );

        m.herd_file("h", &lead("prefer"));
        let problems = problems(&m, &herdr());
        assert!(
            problems.contains(&format!(
                "warning herd h sandbox settings: {file}: sandbox.excludedCommands runs \
                 \"docker *\" outside it; fix: remove it"
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn zero_byte_sandbox_placeholders_for_a_leads_settings_are_no_finding() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\n",
                repo.to_str().unwrap()
            ),
        );
        user_settings(&m, "");
        let claude = crate::worktree::worktree_path(&repo, "lead-h").join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        for name in ["settings.json", "settings.local.json"] {
            std::fs::write(claude.join(name), "").unwrap();
        }

        let problems = problems(&m, &herdr());

        assert!(
            !problems.iter().any(|p| p.contains("can't parse it")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herd_files_lead_arg_outside_the_allowlist_fails_under_require_and_warns_under_prefer() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        let lead = |mode: &str| {
            format!(
                "repo = {:?}\nargs = [\"--model\", \"opus\", \"--add-dir\", \"/srv\"]\n\
                 [lead]\nprompt = \"go\"\nmerge = true\nsandbox = \"{mode}\"\n",
                repo.to_str().unwrap()
            )
        };

        m.herd_file("h", &lead("require"));
        assert_eq!(
            problems(&m, &herdr()),
            [
                "failure herd h lead args: --add-dir /srv, from herd h's args; fix: give herd h \
              a [lead].args without it, since its workers take the herd's args too (a lead \
              takes only --model, --effort, --verbose and --permission-mode default, manual, auto \
              or plan), or loopherd herd set h lead.sandbox '\"prefer\"' to start its lead with it \
              as a warning"
            ]
        );

        m.herd_file("h", &lead("prefer"));
        let problems = problems(&m, &herdr());
        assert!(
            problems.contains(
                &"warning herd h lead args: --add-dir /srv, from herd h's args; fix: give herd \
                  h a [lead].args without it, since its workers take the herd's args too (a \
                  lead takes only --model, --effort, --verbose and --permission-mode default, manual, \
                  auto or plan)"
                    .to_string()
            ),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herd_files_lead_settings_flag_fails_even_under_prefer() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\nsandbox = \"prefer\"\n\
                 args = [\"--settings\", \"x.json\"]\n",
                repo.to_str().unwrap()
            ),
        );

        let problems = problems(&m, &herdr());

        assert!(
            problems.iter().any(|p| p.starts_with(
                "failure herd h lead args: lead-h's args pass --settings, which its sandbox's \
                 settings file takes;"
            )),
            "{problems:?}"
        );
        assert!(
            !problems
                .iter()
                .any(|p| p.starts_with("warning herd h lead args")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_launch_flag_in_the_herds_args_points_to_lead_args_not_dropping_it() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\nargs = [\"--settings\", \"x.json\"]\n\
                 [lead]\nprompt = \"go\"\nmerge = true\n",
                repo.to_str().unwrap()
            ),
        );

        let problems = problems(&m, &herdr());

        let fixes = "give herd h a [lead].args without it, since its workers take the herd's \
                     args too, and put those settings in ~/.claude/settings.json; fix: give herd \
                     h a [lead].args without it, since its workers take the herd's args too";
        assert!(
            problems.iter().any(|p| p.starts_with(
                "failure herd h lead args: lead-h's args pass --settings, which its sandbox's \
                 settings file takes;"
            ) && p.ends_with(fixes)),
            "{problems:?}"
        );
    }

    #[test]
    fn every_disallowed_lead_arg_is_reported_beside_each_launch_flag() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\n\
                 args = [\"--settings\", \"x.json\", \"--strict-mcp-config\", \
                 \"--add-dir\", \"/srv\", \"--verbose\", \"--frobnicate\"]\n",
                repo.to_str().unwrap()
            ),
        );

        let problems: Vec<String> = problems(&m, &herdr())
            .into_iter()
            .filter(|p| p.starts_with("failure herd h lead args"))
            .collect();

        let starts = [
            "failure herd h lead args: lead-h's args pass --settings,",
            "failure herd h lead args: lead-h's args pass --strict-mcp-config,",
            "failure herd h lead args: --add-dir /srv, from [lead].args; fix: drop it (",
            "failure herd h lead args: --frobnicate, from [lead].args; fix: drop it (",
        ];
        assert_eq!(problems.len(), starts.len(), "{problems:?}");
        for (problem, start) in problems.iter().zip(starts) {
            assert!(problem.starts_with(start), "{problems:?}");
        }
    }

    #[test]
    fn user_settings_that_loosen_a_leads_sandbox_show_even_while_its_credentials_are_missing() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n\
                 [worker]\nsandbox = \"require\"\n",
                repo.to_str().unwrap()
            ),
        );
        let file = user_settings(&m, r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);

        let problems = problems(&m, &herdr());

        assert!(
            problems.contains(&format!(
                "failure herd h sandbox settings: {}: sandbox.excludedCommands runs \
                 \"docker *\" outside it; fix: remove it; a credentials lead never starts with \
                 it, so the other way is loopherd herd set h lead.merge_guard '\"hook\"'",
                file.display()
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn a_leads_mcp_server_missing_from_the_user_scope_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge = true\n\
                 mcp_servers = [\"playwright\", \"trello\"]\n",
                repo.to_str().unwrap()
            ),
        );
        let file = m.paths().home.join(".claude.json");
        std::fs::write(
            &file,
            r#"{"mcpServers": {"playwright": {"command": "npx"}}}"#,
        )
        .unwrap();

        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h mcp_servers: \"trello\" isn't in your user scope in {}, so its \
                 lead isn't started; fix: claude mcp add --scope user trello …, or drop it \
                 from [lead] mcp_servers",
                file.display()
            )]
        );
    }

    #[test]
    fn a_sandboxed_workers_mcp_server_missing_from_the_user_scope_fails_and_an_off_herds_isnt() {
        let m = machine(&ALL);
        for (herd, worker) in [("h", "require"), ("plain", "off")] {
            let repo = m.herd(herd);
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n\
                     [worker]\nsandbox = \"{worker}\"\nmcp_servers = [\"trello\"]\n",
                    repo.to_str().unwrap()
                ),
            );
        }
        let file = m.paths().home.join(".claude.json");

        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h worker mcp_servers: \"trello\" isn't in your user scope in {}, \
                 so a sandboxed worker isn't started; fix: claude mcp add --scope user trello \
                 …, or drop it from [worker] mcp_servers",
                file.display()
            )]
        );
    }

    #[test]
    fn a_sandboxed_workers_args_outside_the_allowlist_fail_under_require_and_warn_under_prefer() {
        let m = machine(&ALL);
        for (herd, worker) in [("req", "require"), ("pre", "prefer"), ("plain", "off")] {
            let repo = m.herd(herd);
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\nargs = [\"--add-dir\", \"/a\"]\n[template.t]\nparams = []\n\
                     prompt = \"go\"\nargs = [\"--settings\", \"s.json\"]\n\
                     [worker]\nsandbox = \"{worker}\"\n",
                    repo.to_str().unwrap()
                ),
            );
        }

        let problems = problems(&m, &herdr());

        let allowed = crate::sandbox::ALLOWED_ARGS;
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.contains("worker args"))
                .cloned()
                .collect::<Vec<_>>(),
            [
                "failure herd pre worker args: --settings, from template t's args, which a \
                 sandboxed worker's launch takes; fix: drop it, and put those settings in \
                 ~/.claude/settings.json"
                    .to_string(),
                format!(
                    "warning herd pre worker args: --add-dir /a, from herd pre's args; fix: drop \
                     it (a sandboxed worker takes only {allowed})"
                ),
                "failure herd req worker args: --settings, from template t's args, which a \
                 sandboxed worker's launch takes; fix: drop it, and put those settings in \
                 ~/.claude/settings.json"
                    .to_string(),
                format!(
                    "failure herd req worker args: --add-dir /a, from herd req's args; fix: drop \
                     it (a sandboxed worker takes only {allowed}), or loopherd herd set req \
                     worker.sandbox '\"prefer\"' to start its workers with it as a warning"
                ),
            ]
        );
    }

    #[test]
    fn a_herd_whose_lead_has_merge_off_on_hook_is_warned() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\n",
                repo.to_str().unwrap()
            ),
        );
        assert_eq!(
            problems(&m, &herdr()),
            [
                "warning herd h merge_guard: hook: its lead holds your GitHub credentials, so \
                 its merges and force-pushes are stopped only for an honest lead; \
                 fix: loopherd herd set h lead.merge_guard '\"credentials\"', then loopherd \
                 merge-guard h"
            ]
        );
    }

    #[test]
    fn a_credentials_lead_missing_its_key_token_or_ssh_host_is_warned() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        m.herd_file(
            "h",
            &format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"\n\
                 [worker]\nsandbox = \"require\"\n",
                repo.to_str().unwrap()
            ),
        );
        let dir = m.dir.path().join("config/credentials");
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "warning herd h credentials: no deploy key at {}; fix: loopherd merge-guard h",
                dir.join("h.key").display()
            )]
        );

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("h.key"), "key").unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "warning herd h credentials: no token at {}; fix: loopherd merge-guard h for how \
                 to create and store it",
                dir.join("h.token").display()
            )]
        );

        std::fs::write(dir.join("h.token"), "github_pat_1").unwrap();
        let human = m.dir.path().join(".ssh/config");
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "warning herd h credentials: {} doesn't include loopherd's ssh config before its \
                 first Host; fix: add Include ~/config/ssh/config as its first line",
                human.display()
            )]
        );

        std::fs::create_dir_all(human.parent().unwrap()).unwrap();
        std::fs::write(&human, "Host *\n  User me\nInclude ~/config/ssh/config\n").unwrap();
        assert!(problems(&m, &herdr())[0].contains("doesn't include"));

        std::fs::write(&human, "host=*\n  User me\nInclude ~/config/ssh/config\n").unwrap();
        assert!(problems(&m, &herdr())[0].contains("doesn't include"));

        let ours = m.dir.path().join("config/ssh/config");
        std::fs::write(
            &human,
            format!(
                "# mine\ninclude ~/.ssh/conf.d/* \"{}\"\nHost *\n",
                ours.display()
            ),
        )
        .unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "warning herd h credentials: no current Host loopherd-h in {}; fix: loopherd \
                 merge-guard h",
                m.dir.path().join("config/ssh/config").display()
            )]
        );

        crate::credentials::write_host_block(m.paths(), "h").unwrap();
        assert_eq!(problems(&m, &herdr()), Vec::<String>::new());
    }

    /// The loopherd rulesets each repo has, or why they couldn't be read.
    struct FakeRulesets(Vec<(PathBuf, std::result::Result<Vec<Value>, String>)>);

    impl Rulesets for FakeRulesets {
        fn loopherd_rulesets(&self, repo: &Path) -> Result<Vec<Value>> {
            match self.0.iter().find(|(r, _)| r == repo) {
                Some((_, Ok(found))) => Ok(found.clone()),
                Some((_, Err(e))) => anyhow::bail!("{e}"),
                None => panic!("looked up {}", repo.display()),
            }
        }
    }

    /// Herds named by `leads` (herd, `[lead]` lines) all on one repo.
    fn on_one_repo(leads: &[(&str, &str)]) -> (Machine, PathBuf) {
        let m = machine(&ALL);
        let repo = m.dir.path().join("repos/shared");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        for (herd, lead) in leads {
            m.herd_file(
                herd,
                &format!(
                    "repo = {:?}\n[lead]\nprompt = \"go\"\n{lead}\n",
                    repo.to_str().unwrap()
                ),
            );
        }
        (m, repo)
    }

    fn ruleset_lines(m: &Machine, only: Option<&str>, rules: &FakeRulesets) -> Vec<String> {
        ruleset_checks(m.paths(), only, rules)
            .iter()
            .map(|c| {
                format!(
                    "{} {}: {}; fix: {}",
                    serde_json::to_value(c.status).unwrap().as_str().unwrap(),
                    c.name,
                    c.detail,
                    c.fix.as_deref().unwrap_or("-")
                )
            })
            .collect()
    }

    const CREDENTIALS: &str = "merge_guard = \"credentials\"";

    #[test]
    fn rulesets_are_read_only_for_repos_a_credentials_lead_runs_on() {
        let (m, _) = on_one_repo(&[("h", ""), ("m", "merge = true")]);

        assert_eq!(
            ruleset_lines(&m, None, &FakeRulesets(vec![])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_credentials_lead_whose_rulesets_match_its_switches_passes() {
        for (rebase, force) in [("", false), ("rebase = true", true)] {
            let (m, repo) = on_one_repo(&[("h", &format!("{CREDENTIALS}\n{rebase}"))]);
            let found = vec![
                crate::merge_guard::default_branch_ruleset(),
                crate::merge_guard::force_push_ruleset(force),
            ];

            assert_eq!(
                ruleset_lines(&m, None, &FakeRulesets(vec![(repo, Ok(found))])),
                Vec::<String>::new(),
                "{rebase}"
            );
        }
    }

    #[test]
    fn a_credentials_lead_missing_a_ruleset_or_with_the_wrong_bypass_is_warned() {
        let (m, repo) = on_one_repo(&[("h", &format!("{CREDENTIALS}\nrebase = true"))]);
        let mut changed = crate::merge_guard::default_branch_ruleset();
        changed["rules"] = json!([{"type": "deletion"}]);
        let found = vec![changed, crate::merge_guard::force_push_ruleset(false)];

        assert_eq!(
            ruleset_lines(&m, None, &FakeRulesets(vec![(repo.clone(), Ok(found))])),
            [
                format!(
                    "warning herd h rulesets: {} has no ruleset loopherd: default branch as \
                     loopherd merge-guard makes it, so nothing on GitHub stops a push to the \
                     default branch; fix: loopherd merge-guard h",
                    repo.display()
                ),
                "warning herd h rulesets: rebase is on, but loopherd: no force-push stops \
                 deploy keys too, so its lead can't force-push; fix: loopherd merge-guard h"
                    .to_string(),
            ]
        );

        let (m, repo) = on_one_repo(&[("h", CREDENTIALS)]);
        let found = vec![
            crate::merge_guard::default_branch_ruleset(),
            crate::merge_guard::force_push_ruleset(true),
        ];
        assert_eq!(
            ruleset_lines(&m, None, &FakeRulesets(vec![(repo, Ok(found))])),
            [
                "warning herd h rulesets: rebase is off, but loopherd: no force-push lets deploy \
              keys force-push; fix: loopherd merge-guard h"
            ]
        );
    }

    #[test]
    fn herds_on_one_repo_that_need_different_rulesets_are_warned() {
        let (m, repo) = on_one_repo(&[
            ("a", CREDENTIALS),
            ("b", &format!("{CREDENTIALS}\nrebase = true")),
            ("c", "merge = true"),
            ("d", "rebase = true"),
        ]);
        let found = vec![
            crate::merge_guard::default_branch_ruleset(),
            crate::merge_guard::force_push_ruleset(false),
        ];
        let rules = FakeRulesets(vec![(repo.clone(), Ok(found))]);
        let shared = repo.display();

        assert_eq!(
            ruleset_lines(&m, None, &rules),
            [
                "warning herd b rulesets: rebase is on, but loopherd: no force-push stops \
                 deploy keys too, so its lead can't force-push; fix: loopherd merge-guard b"
                    .to_string(),
                format!(
                    "warning herd b rulesets: herds a and b both run on {shared} but need \
                     different force-push bypasses (rebase off in a, on in b), and one ruleset \
                     can't enforce both; fix: give them the same rebase"
                ),
                format!(
                    "warning herd c rulesets: its lead has merge on, but loopherd: default branch \
                     on {shared} stops its merges; fix: loopherd herd set c lead.merge false, or \
                     run herd c on a repo of its own"
                ),
                format!(
                    "warning herd d rulesets: rebase is on, but loopherd: no force-push on \
                     {shared} stops its force-pushes, made with your credentials; fix: \
                     loopherd herd set d lead.rebase false, or run herd d on a repo of its own"
                ),
            ]
        );
        assert_eq!(
            ruleset_lines(&m, Some("c"), &rules),
            [format!(
                "warning herd c rulesets: its lead has merge on, but loopherd: default branch \
                 on {shared} stops its merges; fix: loopherd herd set c lead.merge false, or \
                 run herd c on a repo of its own"
            )]
        );
    }

    #[test]
    fn rulesets_that_cant_be_read_are_warned_once_per_repo() {
        let (m, repo) = on_one_repo(&[("a", CREDENTIALS), ("b", CREDENTIALS)]);
        let rules = FakeRulesets(vec![(repo.clone(), Err("gh api: HTTP 404".into()))]);

        assert_eq!(
            ruleset_lines(&m, None, &rules),
            [format!(
                "warning herds a, b rulesets: can't read the rulesets of {}: gh api: HTTP 404; \
                 fix: check gh can reach the repo, then loopherd doctor",
                repo.display()
            )]
        );
    }

    #[test]
    fn a_herd_file_is_checked_without_running_its_commands() {
        let m = machine(&ALL);
        let repo = m.herd("q");
        let ran = m.dir.path().join("ran");
        m.herd_file(
            "q",
            &format!(
                "repo = {:?}\n[template.w]\nparams = []\nprompt = \"go\"\n[queue]\nmax = 1\ntemplate = \"w\"\nsource = \"touch {}\"\nbacklog = \"touch {}\"\n",
                repo.to_str().unwrap(),
                ran.display(),
                ran.display()
            ),
        );
        let (_, code) = run(&m, &herdr());
        assert!(!ran.exists());
        assert_eq!(code, 0, "{:?}", problems(&m, &herdr()));
    }

    #[test]
    fn a_herd_repo_that_is_missing_or_not_a_git_repo_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::remove_dir(repo.join(".git")).unwrap();
        let fix = "fix: loopherd herd set h repo '\"<path>\"'";
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is not a git repo; {fix}",
                repo.display()
            )]
        );
        std::fs::remove_dir(&repo).unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is missing; {fix}",
                repo.display()
            )]
        );
    }

    #[test]
    fn a_repo_given_from_home_is_expanded() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::write(m.paths().herds.join("h.toml"), "repo = \"~/repos/h\"\n").unwrap();
        let checks = m.checks(None, &herdr()).unwrap();
        let check = checks.iter().find(|c| c.name == "herd h repo").unwrap();
        assert_eq!(
            (check.status, &check.detail),
            (Status::Ok, &repo.display().to_string())
        );
    }

    #[test]
    fn an_unparseable_ledger_fails_naming_it() {
        let m = machine(&ALL);
        std::fs::create_dir_all(m.state()).unwrap();
        std::fs::write(m.state().join("ledger.json"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        let ledger = m.state().join("ledger.json");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure ledger: parse ledger {}: ",
                ledger.display()
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unparseable_pause_file_fails_naming_it() {
        let m = machine(&ALL);
        m.herd("h");
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("h"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure pause h: parse {}: ",
                paused.join("h").display()
            )),
            "{problems:?}"
        );
        assert!(
            problems[0]
                .ends_with("fix: loopherd pause h to rewrite it, or loopherd resume h to lift it"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_mailbox_with_a_bad_line_fails_naming_it() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("a"), None, "w1:p1");
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let reports = m.state().join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("a.jsonl"), "nope\nnope either\n").unwrap();
        let problems = problems(&m, &herdr);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let mailbox = reports.join("a.jsonl");
        assert!(
            problems[0].starts_with(&format!(
                "failure mailbox a: skipped a bad line in {}: ",
                mailbox.display()
            )),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("(and 1 more bad lines)"),
            "{problems:?}"
        );
    }

    #[test]
    fn verbose_shows_each_open_members_window_and_its_source() {
        use crate::context::{Window, WindowFrom};
        use crate::report::{Body, Entry, Stop};
        let m = machine(&ALL);
        let herdr = herdr()
            .with_live_agent(Some("a"), None, "w1:p1")
            .with_live_agent(Some("b"), None, "w1:p2");
        let mut retired = member("old", "h", "w1:p3", None);
        retired.retired = true;
        m.ledger(&[
            member("a", "h", "w1:p1", None),
            member("b", "h", "w1:p2", None),
            retired,
        ]);
        let reports = m.state().join("reports");
        let stop = |window| {
            let stop = Stop {
                background: crate::report::Tally::Known(Default::default()),
                crons: crate::report::Tally::Known(0),
                error: None,
                session: None,
                context: None,
                limit: None,
                window,
            };
            Entry::new(std::time::SystemTime::now(), Body::Stop(stop))
        };
        let opus = Window {
            tokens: 1_000_000,
            from: WindowFrom::Model("claude-opus-5-5".into()),
        };
        let unknown = Window {
            tokens: 200_000,
            from: WindowFrom::UnknownModel("claude-mystery-9".into()),
        };
        crate::report::append(&reports, "a", &stop(Some(unknown))).unwrap();
        crate::report::append(&reports, "a", &stop(Some(opus.clone()))).unwrap();
        crate::report::append(&reports, "old", &stop(Some(opus))).unwrap();
        let checks = checks(&m.install, &m.setup, None, &herdr).unwrap();
        let shown: Vec<_> = text(&checks, true)
            .lines()
            .filter(|l| l.contains("context window"))
            .map(String::from)
            .collect();
        assert_eq!(
            shown,
            ["ok context window a: 1000000 from its model claude-opus-5-5"]
        );
    }

    #[test]
    fn a_member_whose_pane_is_gone_warns_naming_revive_or_retire() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("live"), None, "w1:p1");
        let mut retired = member("old", "h", "w1:p3", None);
        retired.retired = true;
        m.ledger(&[
            member("live", "h", "w1:p1", None),
            member("gone", "h", "w1:p2", None),
            retired,
        ]);
        assert_eq!(
            run(&m, &herdr),
            (
                "warning member gone: no live agent matches it (pane w1:p2) on /run/fake/herdr.sock; \
                 fix: loopherd revive --name gone, or loopherd retire gone\n\
                 doctor: 19 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn members_on_another_socket_are_counted_in_one_line_not_checked() {
        let m = machine(&ALL);
        m.ledger(&[
            member("here", "h", "w1:p1", Some("/run/fake/herdr.sock")),
            member("dev1", "h", "w1:p2", Some("/run/dev.sock")),
            member("dev2", "h", "w1:p3", Some("/run/dev.sock")),
        ]);
        let checks = m.checks(None, &herdr()).unwrap();
        let lines = text(&checks, true);
        assert!(
            lines.contains("ok members elsewhere: 2 on another herdr socket, not checked\n"),
            "{lines}"
        );
        assert!(!lines.contains("member dev"), "{lines}");
        assert!(lines.contains("warning member here: "), "{lines}");
    }

    #[test]
    fn strays_are_not_reported() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("stray"), None, "w1:p9");
        assert_eq!(run(&m, &herdr).0, "doctor: 18 ok, 0 warnings, 0 failures\n");
    }

    #[test]
    fn an_unreached_herdr_skips_the_pane_checks() {
        let m = machine(&ALL);
        m.ledger(&[member("gone", "h", "w1:p2", None)]);
        let herdr = herdr();
        herdr.set_unreachable(true);
        assert_eq!(
            problems(&m, &herdr),
            ["failure herdr: no herdr server answers on /run/fake/herdr.sock; fix: herdr"]
        );
    }

    #[test]
    fn a_herd_limits_herd_file_repo_and_state_checks_to_it() {
        let m = machine(&ALL);
        m.herd("h");
        m.herd_file("other", "colour = 1\n");
        m.ledger(&[
            member("a", "h", "w1:p1", None),
            member("b", "other", "w1:p2", None),
        ]);
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("other"), "{ nope").unwrap();
        std::fs::write(m.config(), "colour = 1\n").unwrap();
        let names = |herd| -> Vec<String> {
            problems_on(&m, Some(herd), &herdr())
                .iter()
                .filter(|p| p.starts_with("failure ") || p.starts_with("warning "))
                .map(|p| p.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(names("h"), ["failure config", "warning member a"]);
        std::fs::write(m.config(), CONFIG).unwrap();
        assert_eq!(
            names("other"),
            [
                "failure herd other",
                "failure pause other",
                "warning member b"
            ]
        );
    }

    #[test]
    fn an_unknown_herd_is_an_error() {
        let m = machine(&ALL);
        m.herd("h");
        let err = m.checks(Some("nope"), &herdr()).unwrap_err();
        assert!(format!("{err:#}").contains("nope.toml"), "{err:#}");
    }

    #[test]
    fn config_and_state_that_cannot_be_found_warn_and_the_rest_still_runs() {
        let mut m = machine(&ALL);
        m.setup = Err(anyhow::anyhow!("HOME is not set"));
        assert_eq!(
            run(&m, &herdr()),
            (
                "warning config and state: not checked: HOME is not set; \
                 fix: set HOME\n\
                 doctor: 14 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
        assert!(m.checks(Some("h"), &herdr()).is_err());
    }

    #[test]
    fn an_unparseable_pause_of_a_herd_without_a_file_names_only_resume() {
        let m = machine(&ALL);
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("ghost"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        assert!(
            problems[0].ends_with("; fix: loopherd resume ghost"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herd_repo_that_is_a_file_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::write(&repo, "").unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is not a directory; fix: loopherd herd set h repo '\"<path>\"'",
                repo.display()
            )]
        );
    }

    #[test]
    fn with_herdrs_socket_unknown_members_recorded_on_a_socket_are_counted_not_checked() {
        let m = machine(&ALL);
        m.ledger(&[member("dev", "h", "w1:p2", Some("/run/dev.sock"))]);
        let mut herdr = herdr();
        herdr.server = Some(Server::Running {
            socket: None,
            version: Some(MIN_HERDR.into()),
        });
        let checks = m.checks(None, &herdr).unwrap();
        let lines = text(&checks, true);
        assert!(
            lines.contains("ok members elsewhere: 1 on another herdr socket, not checked\n"),
            "{lines}"
        );
        assert!(!lines.contains("warning member"), "{lines}");
    }

    #[test]
    fn a_notify_table_that_sends_nothing_warns_as_herds_does() {
        let m = machine(&ALL);
        let config = m.config();
        std::fs::write(
            &config,
            format!("{CONFIG}[notify]\nenabled = true\nherdr = false\n"),
        )
        .unwrap();
        let problems = problems(&m, &herdr());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "warning notify: {}: [notify] is enabled with herdr = false, no command and no pane_label, so nothing is sent; ",
                config.display()
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn self_restart_warns_without_the_prompt_hook_or_a_notify_that_sends_it() {
        let m = machine(&ALL);
        let config = m.config();
        let notify = "[notify]\nenabled = true\n";
        std::fs::write(
            &config,
            format!("{CONFIG}[shepherd]\nself_restart = true\n{notify}"),
        )
        .unwrap();
        assert_eq!(problems(&m, &herdr()), Vec::<String>::new());

        std::fs::write(
            &config,
            format!("{CONFIG}[shepherd]\nself_restart = true\n"),
        )
        .unwrap();
        let found = problems(&m, &herdr());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with(
                "warning self_restart: [shepherd] self_restart is on, but [notify] doesn't send self_restart"
            ),
            "{found:?}"
        );

        let m = machine(&ALL[..4]);
        std::fs::write(
            m.config(),
            format!("{CONFIG}[shepherd]\nself_restart = true\n{notify}"),
        )
        .unwrap();
        let found = problems(&m, &herdr());
        assert!(
            found.iter().any(|p| p.starts_with(
                "warning self_restart: [shepherd] self_restart is on, but loopherd's UserPromptSubmit hook isn't in"
            ) && p.ends_with("; fix: loopherd install-hook")),
            "{found:?}"
        );

        let m = machine(&ALL[..4]);
        std::fs::write(m.config(), format!("{CONFIG}{notify}")).unwrap();
        let found = problems(&m, &herdr());
        assert!(
            !found.iter().any(|p| p.contains("self_restart")),
            "{found:?}"
        );
    }

    #[test]
    fn a_herds_dir_that_cannot_be_read_fails() {
        let m = machine(&ALL);
        std::fs::write(&m.paths().herds, "").unwrap();
        let problems = problems(&m, &herdr());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure herd files: read {}: ",
                m.paths().herds.display()
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn a_pause_dir_that_cannot_be_read_fails() {
        let m = machine(&ALL);
        std::fs::create_dir_all(m.state()).unwrap();
        std::fs::write(&m.paths().paused, "").unwrap();
        let problems = problems(&m, &herdr());
        let paused = m.paths().paused.display();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!("failure pause files: read {paused}: ")),
            "{problems:?}"
        );
        assert!(
            problems[0].ends_with(&format!("; fix: fix {paused} by hand")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_mailbox_that_cannot_be_read_fails_asking_for_a_readable_file() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("a"), None, "w1:p1");
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let mailbox = m.state().join("reports/a/mailbox.jsonl");
        std::fs::create_dir_all(&mailbox).unwrap();
        let problems = problems(&m, &herdr);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("failure mailbox a: "),
            "{problems:?}"
        );
        assert!(
            problems[0].ends_with(&format!("; fix: make {} readable", mailbox.display())),
            "{problems:?}"
        );
    }

    #[test]
    fn a_mailbox_with_one_bad_line_names_it_without_a_count() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("a"), None, "w1:p1");
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let reports = m.state().join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("a.jsonl"), "nope\n").unwrap();
        let problems = problems(&m, &herdr);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure mailbox a: skipped a bad line in {}: ",
                reports.join("a.jsonl").display()
            )),
            "{problems:?}"
        );
        assert!(!problems[0].contains("more bad lines"), "{problems:?}");
        assert!(
            problems[0].ends_with("; fix: fix the mailbox's bad lines by hand"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herdr_whose_agent_list_fails_fails_the_members_check() {
        let m = machine(&ALL);
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let herdr = herdr();
        herdr.set_malformed(true);
        let problems = problems(&m, &herdr);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("failure members: "), "{problems:?}");
        assert!(problems[0].contains("agent list"), "{problems:?}");
        assert!(
            problems[0].ends_with("; fix: restart herdr's server"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herdr_that_wedges_listing_agents_fails_after_the_deadline() {
        let m = machine(&ALL);
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let mut herdr = herdr();
        let (_held, gate) = std::sync::mpsc::channel::<()>();
        herdr.agents_gate = Some(gate.into());
        let started = std::time::Instant::now();
        let problems = problems(&m, &herdr);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            problems,
            [
                "failure members: herdr didn't list its agents within 5s; fix: restart herdr's server"
            ]
        );
    }

    #[test]
    fn an_unparseable_ledger_still_reports_an_unparseable_pause() {
        let m = machine(&ALL);
        m.herd("h");
        std::fs::create_dir_all(&m.paths().paused).unwrap();
        std::fs::write(m.paths().paused.join("h"), "{ nope").unwrap();
        std::fs::write(m.state().join("ledger.json"), "{ nope").unwrap();
        let names: Vec<String> = problems(&m, &herdr())
            .iter()
            .map(|p| p.split(':').next().unwrap().to_string())
            .collect();
        assert_eq!(names, ["failure ledger", "failure pause h"]);
    }

    struct Herds {
        dir: tempfile::TempDir,
    }

    impl Herds {
        /// Herd files whose `[queue]` holds `queue`'s lines, each herd's repo a temp dir.
        fn with(herds: &[(&str, &str)]) -> Herds {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("herds")).unwrap();
            std::fs::create_dir_all(dir.path().join("repo")).unwrap();
            for (name, queue) in herds {
                let text = format!(
                    "repo = {:?}\n[template.t]\nparams = [\"n\"]\nprompt = \"go\"\n{queue}\n",
                    dir.path().join("repo")
                );
                std::fs::write(dir.path().join(format!("herds/{name}.toml")), text).unwrap();
            }
            Herds { dir }
        }

        fn paths(&self) -> crate::herd::Paths {
            crate::herd::Paths {
                herds: self.dir.path().join("herds"),
                home: self.dir.path().to_path_buf(),
                feed_lock: self.dir.path().join("feed.lock"),
                paused: self.dir.path().join("paused"),
            }
        }

        fn run(&self, timeout: Duration) -> (String, i32) {
            self.run_on(None, timeout)
        }

        fn run_on(&self, herd: Option<&str>, timeout: Duration) -> (String, i32) {
            let checks = tracker_checks(&self.paths(), herd, timeout);
            (text(&checks, true), exit_code(&checks))
        }
    }

    fn queue(source: &str, backlog: Option<&str>) -> String {
        let backlog = backlog.map_or(String::new(), |b| format!("backlog = {b:?}\n"));
        format!("[queue]\ntemplate = \"t\"\nsource = {source:?}\n{backlog}max = 1")
    }

    #[test]
    fn a_herd_narrows_the_trackers_run_to_that_herd() {
        let ran = tempfile::tempdir().unwrap();
        let touch = |name: &str| format!("touch {}", ran.path().join(name).display());
        let herds = Herds::with(&[
            ("a", &queue(&touch("a-source"), Some(&touch("a-backlog")))),
            ("b", &queue(&touch("b-source"), Some(&touch("b-backlog")))),
        ]);
        assert_eq!(
            herds.run_on(Some("b"), TRACKER_TIMEOUT),
            (
                "ok b source: exit 0, 0 tasks\n\
                 ok b backlog: exit 0, 0 tasks\n\
                 doctor: 2 ok, 0 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
        assert!(!ran.path().join("a-source").exists());
        assert!(!ran.path().join("a-backlog").exists());
        assert!(ran.path().join("b-backlog").exists());
    }

    #[test]
    fn trackers_report_each_exit_and_task_count_but_never_a_task() {
        let herds = Herds::with(&[
            (
                "a",
                &queue("printf 'LH-1\\n\\nLH-2\\n# note\\n'", Some("echo LH-3")),
            ),
            ("b", &queue("true", None)),
        ]);
        assert_eq!(
            herds.run(TRACKER_TIMEOUT),
            (
                "ok a source: exit 0, 2 tasks\n\
                 ok a backlog: exit 0, 1 tasks\n\
                 ok b source: exit 0, 0 tasks\n\
                 doctor: 3 ok, 0 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn trackers_run_in_the_herds_repo() {
        let herds = Herds::with(&[("a", &queue("test -d ../herds && ls", None))]);
        std::fs::write(herds.dir.path().join("repo/task"), "").unwrap();
        assert_eq!(
            herds.run(TRACKER_TIMEOUT).0,
            "ok a source: exit 0, 1 tasks\ndoctor: 1 ok, 0 warnings, 0 failures\n"
        );
    }

    #[test]
    fn a_failing_tracker_fails_naming_the_herd_and_command() {
        let herds = Herds::with(&[(
            "a",
            &queue("echo LH-1; echo nope >&2; exit 3", Some("exit 4")),
        )]);
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert_eq!(
            out,
            "failure a source: \"echo LH-1; echo nope >&2; exit 3\" failed (exit 3): nope; \
             fix: fix a's queue.source, e.g. loopherd herd set a queue.source <command>\n\
             failure a backlog: \"exit 4\" failed (exit 4); \
             fix: fix a's queue.backlog, e.g. loopherd herd set a queue.backlog <command>\n\
             doctor: 0 ok, 0 warnings, 2 failures\n"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_tracker_outliving_its_timeout_fails() {
        let herds = Herds::with(&[("a", &queue("sleep 5", None))]);
        let (out, code) = herds.run(Duration::from_millis(200));
        assert!(
            out.starts_with("failure a source: \"sleep 5\" timed out after 200ms; "),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_paused_herd_runs_and_a_herd_without_a_queue_has_nothing_to_run() {
        let herds = Herds::with(&[("a", &queue("echo x", None)), ("b", "")]);
        let paths = herds.paths();
        std::fs::create_dir_all(&paths.paused).unwrap();
        std::fs::write(
            paths.paused.join("a"),
            r#"{"since":"2026-01-01T00:00:00Z","reason":null}"#,
        )
        .unwrap();
        assert!(matches!(
            crate::pause::read(&paths.paused, "a"),
            crate::pause::Reading::Paused(_)
        ));
        assert_eq!(
            herds.run(TRACKER_TIMEOUT).0,
            "ok a source: exit 0, 1 tasks\ndoctor: 1 ok, 0 warnings, 0 failures\n"
        );
    }

    #[test]
    fn a_herd_file_that_does_not_load_fails_its_trackers() {
        let herds = Herds::with(&[("a", &queue("echo x", None))]);
        std::fs::write(herds.dir.path().join("herds/a.toml"), "repo = ").unwrap();
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert!(out.starts_with("failure a trackers: herd: "), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_failing_trackers_stderr_shows_only_its_last_line_printable_and_cut() {
        let long = "x".repeat(300);
        let source = format!("printf 'first\\n\\033[31mred\\033[0m {long}\\n' >&2; exit 1");
        let herds = Herds::with(&[("a", &queue(&source, None))]);
        let (out, _) = herds.run(TRACKER_TIMEOUT);
        let (_, stderr) = out.split_once("failed (exit 1): ").unwrap();
        let (stderr, _) = stderr.split_once("; fix: ").unwrap();
        assert_eq!(stderr, format!("red {}…", "x".repeat(196)));
    }

    #[test]
    fn a_missing_repo_reports_nothing_and_runs_nothing() {
        let herds = Herds::with(&[("a", &queue("echo x", Some("echo y")))]);
        std::fs::remove_dir(herds.dir.path().join("repo")).unwrap();
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert_eq!(out, "doctor: 0 ok, 0 warnings, 0 failures\n");
        assert_eq!(code, 0);
    }
}
