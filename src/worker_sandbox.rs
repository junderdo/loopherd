//! The Claude Code sandbox a herd's workers run in, opt-in by `[worker] sandbox` (D107).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handoff;
use crate::herd::{self, Paths};
use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger};
use crate::sandbox::{
    Sockets, Unavailable, UnixSockets, edit_rule, entries_but, file_rule, write_private,
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
/// worker can't write. `revive` trusts this over the ledger, which it can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub herd: String,
    pub template: String,
    pub args: Vec<String>,
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
    worktree: Option<&Path>,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
) -> Result<Launch> {
    let worker = herd::worker(paths, &member.herd)?;
    if worker.sandbox == WorkerSandbox::Off {
        return Ok(Launch::Off);
    }
    let herd = &member.herd;
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
    if let (Some((why, _, fix)), WorkerSandbox::Require) = (&unavailable, worker.sandbox) {
        return Err(NotStarted(format!(
            "{name} can't start sandboxed: {why}; {fix}, or set [worker] sandbox = \"prefer\" \
             in herd {herd} to start it unsandboxed"
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
        },
        _ => {
            let spawned = crate::spawn::spawn(herdr, ledger, req, inherited_workspace)?;
            return Ok((spawned, vec![]));
        }
    };
    let worktree = req.planned_worktree()?;
    let launch = prepare(paths, &req.name, &member, worktree.as_deref(), probe)?;
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
            let mode = herd::worker(paths, &member.herd)?.sandbox;
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
    if mode == WorkerSandbox::Require {
        bail!(
            "{name} can't run sandboxed: {why}; quit its Claude and run loopherd revive --name \
             {name} to resume it sandboxed, or set [worker] sandbox = \"prefer\" in herd {herd} \
             to run it unsandboxed"
        );
    }
    let warning = format!("{name} runs unsandboxed ({why})");
    Ok((Some(why), vec![warning]))
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
    Ok(json!({
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
            "allow": allow,
            "deny": deny,
        },
    }))
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

    fn member() -> Member {
        Member {
            herd: "h".into(),
            template: "t".into(),
            args: vec!["--model".into(), "opus".into()],
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

        let launch = prepare(&h.paths, "w1", &member(), Some(&worktree), &blocked).unwrap();

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

    fn read_json(file: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    #[test]
    fn a_workers_file_tools_are_denied_the_config_dir_git_hooks_and_the_state_dir_but_its_own() {
        let h = herd("sandbox = \"require\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");
        let state = h.paths.state_dir().to_path_buf();

        prepare(&h.paths, "w1", &member(), Some(&worktree), &blocked).unwrap();

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
    fn a_worker_without_a_worktree_is_refused_under_require_and_unsandboxed_under_prefer() {
        let h = herd("sandbox = \"require\"");
        let err = prepare(&h.paths, "w1", &member(), None, &blocked).unwrap_err();
        assert!(err.downcast_ref::<NotStarted>().is_some());
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: it has no worktree of its own, and its sandbox won't \
             open the main checkout; give herd h's template t a branch, or set [worker] \
             sandbox = \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(&h.paths, "w1", &member(), None, &blocked).unwrap();
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

        prepare(&h.paths, "w1", &member(), Some(&worktree), &blocked).unwrap();

        let file = h.paths.config_dir().join("sandbox/w1.member.json");
        assert_eq!(
            read_json(&file),
            json!({"herd": "h", "template": "t", "args": ["--model", "opus"]})
        );
    }

    #[test]
    fn a_herd_with_the_worker_sandbox_off_writes_nothing() {
        for worker in ["", "sandbox = \"off\"\nunix_sockets = \"all\""] {
            let h = herd(worker);
            let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

            let launch = prepare(&h.paths, "w1", &member(), Some(&worktree), &blocked).unwrap();

            assert_eq!(launch, Launch::Off);
            assert_eq!(launch.args(), Vec::<String>::new());
            assert!(!h.paths.config_dir().join("sandbox").exists(), "{worker}");
        }
    }

    #[test]
    fn unix_sockets_all_opens_every_socket_to_the_herds_workers() {
        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let worktree = h.repo.parent().unwrap().join("worktrees/feat-x");

        prepare(&h.paths, "w1", &member(), Some(&worktree), &blocked).unwrap();

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

        let err = prepare(&h.paths, "w1", &member(), Some(&worktree), &unavailable).unwrap_err();

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

        let launch = prepare(&h.paths, "w1", &member(), Some(&worktree), &unavailable).unwrap();

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
        let err = prepare(&h.paths, "w1", &member(), Some(&worktree), &sockets_open).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "w1 can't start sandboxed: its sandbox can't block Unix sockets: no seccomp \
             filter; npm install -g @anthropic-ai/sandbox-runtime, or set [worker] sandbox = \
             \"prefer\" in herd h to start it unsandboxed"
        );

        let h = herd("sandbox = \"prefer\"");
        let launch = prepare(&h.paths, "w1", &member(), Some(&worktree), &sockets_open).unwrap();
        assert_eq!(
            launch,
            Launch::Unsandboxed {
                why: "its sandbox can't block Unix sockets: no seccomp filter".into(),
                fix: "npm install -g @anthropic-ai/sandbox-runtime".into(),
            }
        );

        let h = herd("sandbox = \"require\"\nunix_sockets = \"all\"");
        let launch = prepare(&h.paths, "w1", &member(), Some(&worktree), &sockets_open).unwrap();
        assert_eq!(launch.args()[0], "--settings");
    }
}
