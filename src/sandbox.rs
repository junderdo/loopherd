//! The Claude Code sandbox a lead runs in, written from its switches (D83, D89).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herd::{self, Paths};

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

/// Says why the sandbox can't start on this machine, if it can't.
pub type Probe = fn() -> Result<(), String>;

/// The machine's answer: macOS has its sandbox built in; Linux and WSL2 need
/// bubblewrap and socat on PATH, and bubblewrap able to make a sandbox.
pub fn probe() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err("Claude Code's sandbox runs only on macOS, Linux and WSL2".into());
    }
    let missing: Vec<&str> = [("bubblewrap", "bwrap"), ("socat", "socat")]
        .into_iter()
        .filter(|(_, program)| crate::on_path(program).is_none())
        .map(|(package, _)| package)
        .collect();
    if !missing.is_empty() {
        return Err(format!("{} not installed", missing.join(" and ")));
    }
    // Catches what PATH can't: WSL1, or user namespaces blocked by AppArmor or sysctl.
    let tried = std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
        .args(["--unshare-pid", "true"])
        .output();
    match tried {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr.lines().last().unwrap_or("no message").trim();
            Err(format!("bubblewrap can't make a sandbox here: {last}"))
        }
        Err(e) => Err(format!("bubblewrap won't run: {e}")),
    }
}

/// The lead whose sandbox is prepared.
pub struct Lead<'a> {
    pub name: &'a str,
    pub herd: &'a str,
    pub repo: &'a Path,
    pub worktree: &'a Path,
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

    /// The warning a hook-only start carries, if this is one.
    pub fn warning(&self, name: &str) -> Option<String> {
        match self {
            Launch::Sandboxed { .. } => None,
            Launch::HookOnly { why } => Some(format!(
                "{name} runs without its sandbox ({why}), guarded by its tool hook alone"
            )),
        }
    }
}

/// Writes `lead`'s settings from its herd's `[lead]` into loopherd's config
/// dir, which the settings keep it from writing. Fails under `require` when
/// `probe` says the sandbox can't start.
pub fn prepare(paths: &Paths, lead: &Lead, probe: Probe) -> Result<Launch> {
    let (switches, mode) = herd::lead_sandbox(paths, lead.herd)?;
    if let Err(why) = probe() {
        return match mode {
            SandboxMode::Prefer => Ok(Launch::HookOnly { why }),
            SandboxMode::Require => Err(refusal(
                lead,
                &format!("its sandbox can't start: {why}; install what it needs"),
            )),
        };
    }
    let config = paths.config_dir();
    let settings = settings(paths, lead, switches.install);
    let file = settings_file(paths, lead.name);
    std::fs::create_dir_all(config.join("sandbox"))
        .with_context(|| format!("create {}", config.join("sandbox").display()))?;
    let text = serde_json::to_string_pretty(&settings)? + "\n";
    std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
    Ok(Launch::Sandboxed { settings: file })
}

/// Checks a running lead was launched with its settings, as `claude_args`
/// record: under `require` an unsandboxed one is refused, under `prefer` it
/// gets the warning a hook-only start carries.
pub fn check_running(paths: &Paths, lead: &Lead, claude_args: &[String]) -> Result<Option<String>> {
    let (_, mode) = herd::lead_sandbox(paths, lead.herd)?;
    let file = settings_file(paths, lead.name).display().to_string();
    let sandboxed = claude_args
        .windows(2)
        .any(|pair| pair[0] == "--settings" && pair[1] == file);
    if sandboxed {
        return Ok(None);
    }
    let why = "it was started without its sandbox";
    match mode {
        SandboxMode::Prefer => Ok(Launch::HookOnly { why: why.into() }.warning(lead.name)),
        SandboxMode::Require => Err(refusal(
            lead,
            &format!(
                "{why}; retire it and start it again with loopherd lead {}",
                lead.herd
            ),
        )),
    }
}

fn refusal(lead: &Lead, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{} can't run sandboxed: {why}, or set [lead] sandbox = \"prefer\" in herd {} \
         to run it guarded by its tool hook alone",
        lead.name,
        lead.herd
    )
}

fn settings_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.json"))
}

fn settings(paths: &Paths, lead: &Lead, install: bool) -> Value {
    let config = paths.config_dir();
    let claude = paths.home.join(".claude");
    // Members' worktrees sit beside the lead's; feed and retire make and remove them.
    let worktrees = lead.worktree.parent().unwrap_or(lead.worktree);
    let mut allow_write = vec![worktrees, lead.repo, paths.state_dir()];
    let local_bin = paths.home.join(".local/bin");
    if install {
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
    if install {
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
    json!({
        "sandbox": sandbox,
        "permissions": {
            "denyInsteadOfPrompting": true,
            "allow": allow,
            "deny": [edit_rule(config), edit_rule(&claude)],
        },
    })
}

/// A deny for the file tools; `//` marks the path absolute, not relative to the settings file.
fn edit_rule(dir: &Path) -> String {
    format!("Edit(/{}/**)", dir.display())
}
