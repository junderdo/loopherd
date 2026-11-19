//! The Claude Code sandbox a lead runs in, written from its switches (D83, D89).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::herd::{self, Paths};
use crate::ledger::AgentRecord;
use crate::process::Procs;
use crate::worktree::{same_dir, worktree_path};

/// `[lead] sandbox`: what happens to a lead whose sandbox can't start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// The lead isn't started.
    #[default]
    Require,
    /// The lead starts guarded by its tool hook alone.
    Prefer,
}

/// Why the sandbox can't start on this machine, and what would let it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub why: String,
    pub fix: String,
}

pub type Probe = fn() -> Result<(), Unavailable>;

/// Older Claude Code prompts where these settings deny, or lets a repo's
/// settings loosen the sandbox.
const MIN_CLAUDE: [u64; 3] = [2, 1, 319];

/// The machine's answer: Claude Code new enough for these settings, and on
/// Linux and WSL2, bubblewrap and socat on PATH with bubblewrap able to make
/// a sandbox. macOS has its sandbox built in.
///
/// Claude Code has no way to run its own check short of starting a session,
/// so this tries what that check needs.
pub fn probe() -> Result<(), Unavailable> {
    let version = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map_err(|e| Unavailable {
            why: format!("claude won't run: {e}"),
            fix: "install Claude Code".into(),
        })?;
    claude_new_enough(&String::from_utf8_lossy(&version.stdout))?;
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err(Unavailable {
            why: "Claude Code's sandbox runs only on macOS, Linux and WSL2".into(),
            fix: "run leads on one of those".into(),
        });
    }
    let missing: Vec<&str> = [("bubblewrap", "bwrap"), ("socat", "socat")]
        .into_iter()
        .filter(|(_, program)| crate::on_path(program).is_none())
        .map(|(package, _)| package)
        .collect();
    if !missing.is_empty() {
        let missing = missing.join(" and ");
        return Err(Unavailable {
            why: format!("{missing} not installed"),
            fix: format!("install {missing}"),
        });
    }
    // Catches what PATH can't: WSL1, or user namespaces blocked by AppArmor or sysctl.
    let tried = std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
        .args(["--unshare-pid", "true"])
        .output();
    let fix = "run under WSL2, not WSL1, and let bubblewrap make user namespaces \
               (kernel.unprivileged_userns_clone, or an AppArmor profile for bwrap)";
    match tried {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr.lines().last().unwrap_or("no message").trim();
            Err(Unavailable {
                why: format!("bubblewrap can't make a sandbox here: {last}"),
                fix: fix.into(),
            })
        }
        Err(e) => Err(Unavailable {
            why: format!("bubblewrap won't run: {e}"),
            fix: fix.into(),
        }),
    }
}

/// Checks `claude --version`'s output against [`MIN_CLAUDE`].
fn claude_new_enough(version: &str) -> Result<(), Unavailable> {
    let min = MIN_CLAUDE.map(|n| n.to_string()).join(".");
    let given = version.split_whitespace().next().unwrap_or_default();
    match crate::doctor::numbers(given) {
        Some(numbers) if numbers >= MIN_CLAUDE => Ok(()),
        _ => Err(Unavailable {
            why: format!("Claude Code {given:?} is older than {min}, or not a version"),
            fix: format!("update Claude Code to {min} or later"),
        }),
    }
}

/// What loopherd keeps of a lead in its config dir, where the lead can't
/// write: its herd, and the args its Claude starts with besides its settings.
/// `revive` trusts this over the ledger, which the lead can write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    pub herd: String,
    pub args: Vec<String>,
}

impl Lead {
    /// The record `prepare` kept for lead `name`, if any.
    pub fn kept(paths: &Paths, name: &str) -> Result<Option<Lead>> {
        let file = lead_file(paths, name);
        match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .with_context(|| format!("parse {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }
}

/// Every lead kept in loopherd's config dir, by name.
fn kept_leads(paths: &Paths) -> Result<Vec<(String, Lead)>> {
    let dir = paths.config_dir().join("sandbox");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut leads = vec![];
    for entry in entries {
        let file = entry
            .with_context(|| format!("read {}", dir.display()))?
            .file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".lead.json")) else {
            continue;
        };
        if let Some(lead) = Lead::kept(paths, name)? {
            leads.push((name.to_string(), lead));
        }
    }
    Ok(leads)
}

/// The lead whose sandboxed Claude started process `me`: an ancestor run with
/// `--settings` naming that lead's settings file. A lead can't change its own
/// Claude's command line, as it can its ledger entry.
pub fn calling_lead(paths: &Paths, procs: &dyn Procs, me: u32) -> Option<String> {
    let dir = paths.config_dir().join("sandbox");
    let mut pid = procs.parent(me);
    while let Some(p) = pid.filter(|&p| p > 1) {
        let found = procs.cmdline(p).and_then(|args| {
            args.windows(2)
                .filter(|pair| pair[0] == "--settings")
                .find_map(|pair| lead_of_settings(&dir, Path::new(&pair[1])))
        });
        if found.is_some() {
            return found;
        }
        pid = procs.parent(p);
    }
    None
}

fn lead_of_settings(dir: &Path, file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?.strip_suffix(".json")?;
    let is_settings = file.parent() == Some(dir) && !name.ends_with(".lead");
    is_settings.then(|| name.to_string())
}

/// Checks a ledger entry, which any lead can write, before loopherd runs git
/// outside the sandbox on its repo and worktree.
pub struct Guard {
    paths: Paths,
    /// The lead whose sandboxed Claude runs this loopherd, if one does.
    caller: Option<String>,
}

impl Guard {
    pub fn new(paths: Paths, caller: Option<String>) -> Guard {
        Guard { paths, caller }
    }

    /// Why loopherd won't run git on `record`'s repo and worktree, if it
    /// won't. A herd's member must be where its herd file puts members: its
    /// repo the herd's, its worktree one beside the herd's other worktrees,
    /// and no lead's, whose `.git` the lead can write. A caller lead may act
    /// only on its own herd's members.
    pub fn refusal(&self, record: &AgentRecord) -> Result<Option<String>> {
        let name = &record.name;
        if let Some(caller) = &self.caller {
            let own = Lead::kept(&self.paths, caller)?.map(|lead| lead.herd);
            if own.is_none() || record.herd != own {
                return Ok(Some(format!(
                    "{name} isn't a member of {caller}'s herd, and {caller} runs this loopherd"
                )));
            }
        }
        let Some(herd) = &record.herd else {
            return Ok(None);
        };
        let repo = herd::repo(&self.paths, herd)?;
        if !same_dir(&record.repo, &repo) {
            return Ok(Some(format!(
                "{name}'s ledger entry names repo {}, not herd {herd}'s {}",
                record.repo.display(),
                repo.display()
            )));
        }
        let Some(worktree) = &record.worktree else {
            return Ok(None);
        };
        let beside = worktree_path(&repo, "x");
        let in_place = worktree.file_name().is_some()
            && worktree
                .parent()
                .zip(beside.parent())
                .is_some_and(|(a, b)| same_dir(a, b));
        if !in_place {
            return Ok(Some(format!(
                "{name}'s ledger entry names worktree {}, which isn't beside herd {herd}'s \
                 worktrees in {}",
                worktree.display(),
                beside.parent().unwrap_or(&repo).display()
            )));
        }
        for (lead, kept) in kept_leads(&self.paths)? {
            if lead == *name {
                continue;
            }
            let Ok(lead_repo) = herd::repo(&self.paths, &kept.herd) else {
                continue;
            };
            if same_dir(worktree, &worktree_path(&lead_repo, &lead)) {
                return Ok(Some(format!(
                    "{name}'s ledger entry names lead {lead}'s worktree {}",
                    worktree.display()
                )));
            }
        }
        Ok(None)
    }
}

/// How a lead's Claude is started.
pub enum Launch {
    Sandboxed {
        settings: PathBuf,
    },
    /// `prefer`, with why the sandbox can't start.
    HookOnly {
        why: String,
    },
}

impl Launch {
    pub fn args(&self) -> Vec<String> {
        match self {
            Launch::Sandboxed { settings } => {
                vec!["--settings".into(), settings.display().to_string()]
            }
            Launch::HookOnly { .. } => vec![],
        }
    }

    /// Why the lead runs without its sandbox, if it does.
    pub fn hook_only(&self) -> Option<&str> {
        match self {
            Launch::Sandboxed { .. } => None,
            Launch::HookOnly { why } => Some(why),
        }
    }

    /// The warning a hook-only start carries, if this is one.
    pub fn warning(&self, name: &str) -> Option<String> {
        self.hook_only().map(|why| hook_only_warning(name, why))
    }
}

pub fn hook_only_warning(name: &str, why: &str) -> String {
    format!("{name} runs without its sandbox ({why}), guarded by its tool hook alone")
}

/// Keeps `lead` in loopherd's config dir and writes its settings there from
/// its herd's `[lead]`; the settings keep it from writing either. Fails under
/// `require` when `probe` says the sandbox can't start, and for args that
/// pass their own `--settings`, which would displace or double the lead's.
pub fn prepare(paths: &Paths, name: &str, lead: &Lead, probe: Probe) -> Result<Launch> {
    if lead
        .args
        .iter()
        .any(|a| a == "--settings" || a.starts_with("--settings="))
    {
        bail!(
            "{name}'s args pass --settings, which its sandbox's settings file takes; \
             drop it from [lead].args, the herd's args and the command line, and put those \
             settings in ~/.claude/settings.json"
        );
    }
    let (_, mode) = herd::lead_sandbox(paths, &lead.herd)?;
    let dir = paths.config_dir().join("sandbox");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = lead_file(paths, name);
    std::fs::write(&file, serde_json::to_string_pretty(lead)? + "\n")
        .with_context(|| format!("write {}", file.display()))?;
    if let Err(Unavailable { why, fix }) = probe() {
        return match mode {
            SandboxMode::Prefer => Ok(Launch::HookOnly { why }),
            SandboxMode::Require => Err(refusal(
                name,
                &lead.herd,
                &format!("its sandbox can't start: {why}; {fix}"),
            )),
        };
    }
    let file = settings_file(paths, name);
    let text = serde_json::to_string_pretty(&settings(paths, name, &lead.herd)?)? + "\n";
    std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
    Ok(Launch::Sandboxed { settings: file })
}

/// How a running lead stands against its sandbox.
#[derive(Debug, PartialEq, Eq)]
pub enum Running {
    Sandboxed,
    /// Sandboxed, by settings its herd's switches have since changed.
    Stale(String),
    /// Started without its settings, under `prefer`.
    HookOnly(String),
}

/// Checks a running lead was launched with its settings, as `claude_args`
/// record, and that they match its herd's current switches. Under `require`
/// one launched without them is refused.
pub fn check_running(
    paths: &Paths,
    name: &str,
    herd: &str,
    claude_args: &[String],
) -> Result<Running> {
    let (_, mode) = herd::lead_sandbox(paths, herd)?;
    let file = settings_file(paths, name);
    let shown = file.display().to_string();
    let resume = format!("quit its Claude and run loopherd revive --name {name}");
    if !claude_args
        .windows(2)
        .any(|pair| pair[0] == "--settings" && pair[1] == shown)
    {
        let why = "it was started without its sandbox";
        return match mode {
            SandboxMode::Prefer => Ok(Running::HookOnly(why.into())),
            SandboxMode::Require => Err(refusal(
                name,
                herd,
                &format!("{why}; {resume} to resume it sandboxed"),
            )),
        };
    }
    let current = settings(paths, name, herd)?;
    let on_disk = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if on_disk.as_ref() == Some(&current) {
        return Ok(Running::Sandboxed);
    }
    Ok(Running::Stale(format!(
        "{name}'s sandbox was set before herd {herd}'s current switches, \
         which reach it only when its Claude starts again; {resume}"
    )))
}

fn refusal(name: &str, herd: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{name} can't run sandboxed: {why}, or set [lead] sandbox = \"prefer\" in herd {herd} \
         to run it guarded by its tool hook alone"
    )
}

/// The settings file a lead's Claude is started with.
pub fn settings_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.json"))
}

fn lead_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.lead.json"))
}

/// The loopherd commands a lead runs that make or remove its members'
/// worktrees, beside its own and so outside what it may write.
const WORKTREE_COMMANDS: [&str; 5] = [
    "loopherd feed",
    "loopherd feed *",
    "loopherd add *",
    "loopherd retire *",
    "loopherd release *",
];

/// Lead `name`'s settings, from its herd file alone: its worktree is where
/// `lead` puts it in the herd's repo, whatever the ledger says.
fn settings(paths: &Paths, name: &str, herd: &str) -> Result<Value> {
    let (switches, _) = herd::lead_sandbox(paths, herd)?;
    let repo = herd::repo(paths, herd)?;
    let config = paths.config_dir();
    let claude = paths.home.join(".claude");
    let worktree = worktree_path(&repo, name);
    let mut allow_write = vec![worktree.as_path(), paths.state_dir()];
    let local_bin = paths.home.join(".local/bin");
    if switches.install {
        allow_write.push(&local_bin);
    }
    let mut sandbox = json!({
        "enabled": true,
        "failIfUnavailable": true,
        "allowUnsandboxedCommands": false,
        // Linux can't narrow this to herdr's socket: its seccomp filter can't see socket paths.
        "network": {"allowAllUnixSockets": true},
        "filesystem": {
            "allowWrite": allow_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "denyWrite": [config.display().to_string(), claude.display().to_string()],
        },
    });
    // The sandbox ends every process a command leaves behind, the self-restart's waiter too.
    let mut excluded = vec!["loopherd restart --self"];
    excluded.extend(WORKTREE_COMMANDS);
    if switches.install {
        // ~/.claude is a protected path no allowWrite opens, and the installer writes there.
        excluded.extend([
            "scripts/install",
            "scripts/install *",
            "./scripts/install",
            "./scripts/install *",
        ]);
    }
    sandbox["excludedCommands"] = json!(excluded);
    let mut allow = vec!["WebFetch(domain:*)".to_string()];
    allow.extend(excluded.iter().map(|command| format!("Bash({command})")));
    Ok(json!({
        "sandbox": sandbox,
        "permissions": {
            "denyInsteadOfPrompting": true,
            "allow": allow,
            "deny": [edit_rule(config), edit_rule(&claude)],
        },
    }))
}

/// A deny for the file tools; `//` marks the path absolute, not relative to the settings file.
fn edit_rule(dir: &Path) -> String {
    format!("Edit(/{}/**)", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calling_lead_is_the_one_whose_settings_an_ancestor_claude_was_started_with() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let settings = settings_file(&paths, "lead-h").display().to_string();
        let mut procs = crate::process::FakeProcs::pane();
        procs.set_cmdline(30, &["claude", "--settings", &settings]);
        procs.add(40, "/bin/bash", 30, 400);
        procs.add(50, "/usr/bin/loopherd", 40, 500);

        assert_eq!(calling_lead(&paths, &procs, 50), Some("lead-h".into()));

        let elsewhere = root.path().join("lead-h.json").display().to_string();
        procs.set_cmdline(30, &["claude", "--settings", &elsewhere]);
        assert_eq!(calling_lead(&paths, &procs, 50), None);
    }

    #[test]
    fn claude_at_or_past_the_floor_passes_and_older_says_to_update() {
        assert_eq!(claude_new_enough("2.1.319 (Claude Code)\n"), Ok(()));
        assert_eq!(claude_new_enough("2.2.0 (Claude Code)\n"), Ok(()));
        assert_eq!(
            claude_new_enough("2.1.318 (Claude Code)\n"),
            Err(Unavailable {
                why: "Claude Code \"2.1.318\" is older than 2.1.319, or not a version".into(),
                fix: "update Claude Code to 2.1.319 or later".into(),
            })
        );
        assert!(claude_new_enough("").is_err());
    }
}
