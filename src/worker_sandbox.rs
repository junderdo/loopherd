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
    Sockets, Unavailable, UnixSockets, confine_credentials, edit_rule, entries_but, file_rule,
    write_private,
};
use crate::spawn::{MemberOf, SpawnRequest, Spawned};

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
    if let Some(member) = Member::kept(paths, &record.name)? {
        return Ok(Some(member));
    }
    let (Some(herd), Some(template)) = (&record.herd, &record.template) else {
        return Ok(None);
    };
    let on = herd::worker(paths, herd).is_ok_and(|w| w.sandbox != WorkerSandbox::Off);
    Ok((on && !record.may_be_lead()).then(|| Member {
        herd: herd.clone(),
        template: template.clone(),
        args: record.claude_args.clone(),
        worktree: record.worktree.clone(),
    }))
}

pub fn member_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.member.json"))
}

/// How a worker's Claude is started.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Its herd leaves `[worker] sandbox` off.
    Off,
    Sandboxed {
        settings: PathBuf,
    },
    /// `prefer`, with why the sandbox can't start and what would let it.
    Unsandboxed {
        why: String,
        fix: String,
    },
}

impl Launch {
    /// The args loopherd adds to the worker's Claude.
    pub fn args(&self) -> Vec<String> {
        match self {
            Launch::Off | Launch::Unsandboxed { .. } => vec![],
            Launch::Sandboxed { settings } => {
                vec!["--settings".into(), settings.display().to_string()]
            }
        }
    }
}

impl Launch {
    /// Why the worker runs without its sandbox, if it does.
    pub fn unsandboxed(&self) -> Option<&str> {
        match self {
            Launch::Unsandboxed { why, .. } => Some(why),
            _ => None,
        }
    }

    pub fn warnings(&self, name: &str) -> Vec<String> {
        match self {
            Launch::Unsandboxed { why, fix } => {
                vec![format!("{name} runs unsandboxed ({why}); {fix}")]
            }
            _ => vec![],
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

/// Keeps `member` and writes worker `name`'s settings in loopherd's config
/// dir, from its herd's `[worker]`, when that turns the sandbox on. Under
/// `require`, a sandbox that can't start fails with [`NotStarted`].
pub fn prepare(
    paths: &Paths,
    name: &str,
    member: &Member,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
) -> Result<Launch> {
    let worker = herd_worker(paths, name, &member.herd)?;
    if worker.sandbox == WorkerSandbox::Off {
        return Ok(Launch::Off);
    }
    let herd = &member.herd;
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
        (None, Some(worktree)) => Some(settings(paths, name, herd, worktree)?),
        _ => None,
    };
    let dir = paths.config_dir().join("sandbox");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let kept = member_file(paths, name);
    std::fs::write(&kept, serde_json::to_string_pretty(member)? + "\n")
        .with_context(|| format!("write {}", kept.display()))?;
    let Some(own) = own else {
        let (_, why, fix) = unavailable.expect("no settings only when unavailable");
        return Ok(Launch::Unsandboxed { why, fix });
    };
    // An allowWrite entry for a missing path has no effect on Linux.
    for dir in own_dirs(paths, name) {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = settings_file(paths, name);
    let text = serde_json::to_string_pretty(&own)? + "\n";
    write_private(&file, &text).with_context(|| format!("write {}", file.display()))?;
    Ok(Launch::Sandboxed { settings: file })
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
    let launch = prepare(paths, &req.name, &member, probe)?;
    let mut req = req.clone();
    req.claude_args.extend(launch.args());
    let spawned = match crate::spawn::spawn(herdr, ledger, &req, inherited_workspace) {
        Ok(spawned) => spawned,
        Err(e) => {
            // Kept once anything records the worker, whose Claude may run.
            if launch != Launch::Off && !recorded(ledger).unwrap_or(true) {
                for file in [
                    settings_file(paths, &req.name),
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
/// without writing anything: its recorded args must pass it its settings.
/// Returns why it runs unsandboxed under `prefer`, and the warnings;
/// `require` refuses one started without them.
pub fn check_running(paths: &Paths, record: &AgentRecord) -> Result<(Option<String>, Vec<String>)> {
    let name = &record.name;
    let Some((why, herd, mode)) = running_unsandboxed(paths, record)? else {
        return Ok((None, vec![]));
    };
    let credentials = herd::merge_guard(paths, &herd)? == Some(MergeGuard::Credentials);
    let Some(otherwise) = unsandboxed_fix(&herd, credentials, mode, "run") else {
        let warning = format!("{name} runs unsandboxed ({why})");
        return Ok((Some(why), vec![warning]));
    };
    bail!(
        "{name} can't run sandboxed: {why}; quit its Claude and run loopherd revive --name \
         {name} to resume it sandboxed, or {otherwise}"
    );
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
    // A clone's own git dir; a linked worktree's `.git` is a file naming the shared one.
    let own_git = worktree.join(".git");
    let git_dirs = std::iter::once(git).chain((!own_git.is_file()).then_some(own_git));
    let mut deny_write = vec![config.to_path_buf(), claude.clone()];
    let mut deny = vec![edit_rule(config), edit_rule(&claude)];
    for dir in git_dirs {
        deny_write.extend([dir.join("hooks"), dir.join("config")]);
        deny.extend([
            edit_rule(&dir.join("hooks")),
            file_rule(&dir.join("config")),
        ]);
    }
    deny.extend([
        edit_rule(&state.join("paused")),
        edit_rule(&state.join("broker")),
        edit_rule(&paths.locks_dir()),
    ]);
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
        confine_credentials(paths, name, herd, &mut settings, [])
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

        let launch = prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

        let file = config.join("sandbox/w1.json");
        assert_eq!(
            launch.args(),
            ["--settings".to_string(), file.display().to_string()]
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

    #[test]
    fn a_workers_own_clone_has_its_git_hooks_and_config_denied_but_a_linked_worktree_doesnt() {
        let h = herd("sandbox = \"require\"");
        let clone = h.repo.parent().unwrap().join("worktrees/feat-x");
        let linked = h.repo.parent().unwrap().join("worktrees/feat-y");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: elsewhere\n").unwrap();
        let denied = |worktree: &Path| {
            prepare(&h.paths, "w1", &member(Some(worktree)), &blocked).unwrap();
            let settings = read_json(&h.paths.config_dir().join("sandbox/w1.json"));
            let deny_write = strings(&settings["sandbox"]["filesystem"]["denyWrite"]);
            let deny = strings(&settings["permissions"]["deny"]);
            (deny_write, deny)
        };

        let (deny_write, deny) = denied(&clone);
        for own in ["hooks", "config"].map(|f| clone.join(".git").join(f).display().to_string()) {
            assert!(deny_write.contains(&own), "{own}: {deny_write:?}");
            assert!(
                deny.iter().any(|rule| rule.contains(&own)),
                "{own}: {deny:?}"
            );
        }
        let (deny_write, _) = denied(&linked);
        assert!(
            !deny_write
                .iter()
                .any(|p| p.starts_with(&linked.display().to_string()))
        );
    }

    fn read_json(file: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    #[test]
    fn a_workers_file_tools_are_denied_the_config_dir_git_hooks_and_the_state_dir_but_its_own() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let state = h.paths.state_dir().to_path_buf();

        prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

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

        prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

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
        let err = prepare(&h.paths, "w1", &member(None), &blocked).unwrap_err();
        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: it has no worktree of its own, and its sandbox won't \
             open the main checkout; give herd h's template t a branch, or set [worker] \
             sandbox = \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(&h.paths, "w1", &member(None), &blocked).unwrap();
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

        prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

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

            let launch = prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

            assert_eq!(launch, Launch::Off);
            assert_eq!(launch.args(), Vec::<String>::new());
            assert!(!h.paths.config_dir().join("sandbox").exists(), "{worker}");
        }
    }

    #[test]
    fn unix_sockets_all_opens_every_socket_to_the_herds_workers() {
        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        prepare(&h.paths, "w1", &member(Some(&worktree)), &blocked).unwrap();

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

        let err = prepare(&h.paths, "w1", &member(Some(&worktree)), &unavailable).unwrap_err();

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

        let launch = prepare(&h.paths, "w1", &member(Some(&worktree)), &unavailable).unwrap();

        assert_eq!(
            launch,
            Launch::Unsandboxed {
                why: "bubblewrap isn't installed".into(),
                fix: "install bubblewrap".into(),
            }
        );
        assert_eq!(launch.args(), Vec::<String>::new());
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
        let err = prepare(&h.paths, "w1", &member(Some(&worktree)), &sockets_open).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its sandbox can't block Unix sockets: no seccomp \
             filter; npm install -g @anthropic-ai/sandbox-runtime, or set [worker] sandbox = \
             \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(&h.paths, "w1", &member(Some(&worktree)), &sockets_open).unwrap();
        assert_eq!(
            launch,
            Launch::Unsandboxed {
                why: "its sandbox can't block Unix sockets: no seccomp filter".into(),
                fix: "npm install -g @anthropic-ai/sandbox-runtime".into(),
            }
        );

        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let launch = prepare(&h.paths, "w1", &member(Some(&worktree)), &sockets_open).unwrap();
        assert_eq!(launch.args()[0], "--settings");
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

        prepare(&h.paths, "w1", &member(Some(&worktree_of(&h))), &blocked).unwrap();

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
            &config,
        ];
        assert_eq!(deny_read[..7], hidden);
        let locks = h.paths.locks_dir().display().to_string();
        assert_eq!(deny_read.last(), Some(&locks));
        let deny = strings(&settings["permissions"]["deny"]);
        for rule in [
            "Read(//home/me/.ssh/**)",
            "Read(//home/me/.netrc)",
            "Read(//home/me/.config/gh/**)",
            "Read(//home/me/.git-credentials)",
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

                let err =
                    prepare(&h.paths, "w1", &member(Some(&worktree_of(&h))), &blocked).unwrap_err();

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

                let err = prepare(&h.paths, "w1", &member(worktree.as_deref()), probe).unwrap_err();

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

            let err =
                prepare(&h.paths, "w1", &member(Some(&worktree_of(&h))), &blocked).unwrap_err();

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
        prepare(&h.paths, "w1", &member(Some(&worktree_of(&h))), &blocked).unwrap();
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

            prepare(&h.paths, "w1", &member(Some(&worktree_of(&h))), &blocked).unwrap();

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
