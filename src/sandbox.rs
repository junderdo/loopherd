//! The Claude Code sandbox a lead runs in, written from its switches (D83, D89).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::herd::{self, MergeGuard, Paths};
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

/// `[lead] unix_sockets`: the Unix sockets a lead may use besides none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnixSockets {
    All,
}

/// Why the sandbox can't start on this machine, and what would let it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub why: String,
    pub fix: String,
}

/// Whether a sandbox that starts keeps Unix sockets closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sockets {
    Blocked,
    /// Its seccomp filter is missing or lets a connect through.
    Open(Unavailable),
}

pub type Probe = fn() -> Result<Sockets, Unavailable>;

/// Older Claude Code prompts where these settings deny, or lets a repo's
/// settings loosen the sandbox.
const MIN_CLAUDE: [u64; 3] = [2, 1, 319];

/// The machine's answer: Claude Code new enough for these settings, and on
/// Linux and WSL2, bubblewrap and socat on PATH with bubblewrap able to make
/// a sandbox, and whether its seccomp filter blocks a Unix-socket connect.
/// macOS has its sandbox built in.
///
/// Claude Code has no way to run its own check short of starting a session,
/// so this tries what that check needs.
pub fn probe() -> Result<Sockets, Unavailable> {
    let version = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map_err(|e| Unavailable {
            why: format!("claude won't run: {e}"),
            fix: "install Claude Code".into(),
        })?;
    claude_new_enough(&String::from_utf8_lossy(&version.stdout))?;
    if cfg!(target_os = "macos") {
        return Ok(Sockets::Blocked);
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
        Ok(out) if out.status.success() => Ok(socket_filter()),
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

/// How the human gives Claude Code a seccomp filter where it has none.
pub const FILTER_FIX: &str = "npm install -g @anthropic-ai/sandbox-runtime";

/// What one run of socat under a candidate seccomp filter showed.
#[derive(Debug, PartialEq, Eq)]
enum Tried {
    Blocked,
    ConnectedThrough,
    /// The filter failed to set itself up, socat unrun: its own error line.
    FailedSetup(String),
    /// Not a filter at all, such as a Claude Code that doesn't dispatch on `ARGV0`.
    NotAFilter,
}

/// Only socat's own denial counts as blocked: the filter's setup errors
/// (`apply-seccomp: …`) also end in `Operation not permitted`.
fn tried(succeeded: bool, stderr: &str) -> Tried {
    if let Some(line) = stderr
        .lines()
        .find(|line| line.trim_start().starts_with("apply-seccomp:"))
    {
        return Tried::FailedSetup(line.trim().into());
    }
    let socat = |said: &str| {
        stderr
            .lines()
            .any(|line| line.contains("socat[") && line.contains(said))
    };
    if succeeded && socat("successfully connected") {
        Tried::ConnectedThrough
    } else if !succeeded && socat("Operation not permitted") {
        Tried::Blocked
    } else {
        Tried::NotAFilter
    }
}

/// Tries a Unix-socket connect under each seccomp filter Claude Code's
/// sandbox could apply, with socat, which the sandbox needs anyway.
fn socket_filter() -> Sockets {
    let open = |why: String, fix: String| Sockets::Open(Unavailable { why, fix });
    let tmp_fix = || "set TMPDIR to a writable dir on a local filesystem".to_string();
    let Ok(dir) = tempfile::tempdir() else {
        return open("no temp dir to try its seccomp filter in".into(), tmp_fix());
    };
    let socket = dir.path().join("probe.sock");
    let Ok(_listener) = std::os::unix::net::UnixListener::bind(&socket) else {
        return open(
            "no Unix socket to try its seccomp filter on".into(),
            tmp_fix(),
        );
    };
    let mut failed = None;
    for mut filter in seccomp_filters() {
        let child = filter
            .args(["socat", "-d", "-d", "-u", "OPEN:/dev/null"])
            .arg(format!("UNIX-CONNECT:{}", socket.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let result = match child.ok().and_then(|child| finish(child, 10)) {
            Some(out) => tried(out.status.success(), &String::from_utf8_lossy(&out.stderr)),
            None => Tried::NotAFilter,
        };
        match result {
            Tried::Blocked => return Sockets::Blocked,
            Tried::ConnectedThrough => {
                return open(
                    "its seccomp filter let a Unix-socket connect through".into(),
                    format!("update Claude Code, or {FILTER_FIX}"),
                );
            }
            Tried::FailedSetup(line) => {
                failed.get_or_insert(line);
            }
            Tried::NotAFilter => {}
        }
    }
    match failed {
        None => open(
            "Claude Code finds no seccomp filter to block them with".into(),
            FILTER_FIX.into(),
        ),
        Some(line) => open(
            format!("its seccomp filter can't set itself up: {line}"),
            "let it make user namespaces, as bubblewrap does \
             (kernel.unprivileged_userns_clone, or an AppArmor profile for claude)"
                .into(),
        ),
    }
}

/// Commands that run a program under a seccomp filter Claude Code could
/// use: its native binary's own, chosen by `ARGV0`, which a version
/// manager's shim passes on, then an npm install's helper where Claude Code
/// looks for it.
fn seccomp_filters() -> Vec<std::process::Command> {
    let Some(claude) = crate::on_path("claude") else {
        return vec![];
    };
    let mut native = std::process::Command::new(&claude);
    native.env("ARGV0", "apply-seccomp");
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return vec![native],
    };
    let helper = Path::new("vendor/seccomp").join(arch).join("apply-seccomp");
    let package = claude
        .canonicalize()
        .ok()
        .and_then(|real| real.parent().map(Path::to_path_buf));
    let global = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
        .filter(|root| root.is_absolute())
        .map(|root| root.join("@anthropic-ai/sandbox-runtime"));
    let near = package
        .into_iter()
        .flat_map(|dir| [dir.clone(), dir.join("../.."), dir.join("..")]);
    std::iter::once(native)
        .chain(
            near.chain(global)
                .map(|dir| dir.join(&helper))
                .filter(|path| path.is_file())
                .map(std::process::Command::new),
        )
        .collect()
}

/// `child`'s output, or `None` once it has run `secs` seconds, when it's killed.
fn finish(mut child: std::process::Child, secs: u64) -> Option<std::process::Output> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while child.try_wait().ok()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().ok()
}

/// Whether WSL2's binfmt_misc entry for Windows programs is enabled here.
pub fn wsl_interop() -> bool {
    std::fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop")
        .is_ok_and(|text| text.lines().next() == Some("enabled"))
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

fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// The worktree of every lead kept in loopherd's config dir, by name. A kept
/// record or herd file that can't be read is skipped: one bad file mustn't
/// stop every retire.
fn lead_worktrees(paths: &Paths) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(paths.config_dir().join("sandbox")) else {
        return vec![];
    };
    entries
        .filter_map(|entry| {
            let file = entry.ok()?.file_name();
            let name = file.to_str()?.strip_suffix(".lead.json")?.to_string();
            let lead = Lead::kept(paths, &name).ok()??;
            let repo = herd::repo(paths, &lead.herd).ok()?;
            let worktree = worktree_path(&repo, &name);
            Some((name, worktree))
        })
        .collect()
}

/// The lead whose sandboxed Claude started process `me`: an ancestor run with
/// `--settings` naming that lead's settings file. A lead can't change its own
/// Claude's command line, as it can its ledger entry. Errs when an ancestor
/// runs a lead from another config dir, which this loopherd can't vouch for.
pub fn calling_lead(paths: &Paths, procs: &dyn Procs, me: u32) -> Result<Option<String>> {
    let dir = paths.config_dir().join("sandbox");
    let mut pid = procs.parent(me);
    while let Some(p) = pid.filter(|&p| p > 1) {
        let settings = procs.cmdline(p).into_iter().flat_map(|args| {
            args.windows(2)
                .filter(|pair| pair[0] == "--settings")
                .map(|pair| PathBuf::from(&pair[1]))
                .collect::<Vec<_>>()
        });
        for file in settings {
            let Some((at, name)) = lead_of_settings(&file) else {
                continue;
            };
            if at == dir {
                return Ok(Some(name));
            }
            if at.join(format!("{name}.lead.json")).is_file() {
                bail!(
                    "refused: an ancestor Claude runs lead {name} from {}, not from this \
                     loopherd's config dir {}",
                    at.display(),
                    paths.config_dir().display()
                );
            }
        }
        pid = procs.parent(p);
    }
    Ok(None)
}

/// The first `PATH` entry that would let a lead choose which `loopherd` runs:
/// empty or relative (resolved from its cwd), or inside a dir it can write.
fn plantable_path_entry(path: Option<&std::ffi::OsStr>, writable: &[&Path]) -> Option<String> {
    std::env::split_paths(path?).find_map(|dir| {
        if dir.as_os_str().is_empty() {
            Some("an empty entry".to_string())
        } else if dir.is_relative() {
            Some(format!("the relative entry {}", dir.display()))
        } else {
            let inside = writable.iter().find(|w| dir.starts_with(w))?;
            Some(format!("{}, inside {}", dir.display(), inside.display()))
        }
    })
}

/// The sandbox dir and lead name a lead's settings file `file` names.
fn lead_of_settings(file: &Path) -> Option<(PathBuf, String)> {
    let name = file.file_name()?.to_str()?.strip_suffix(".json")?;
    let at = file.parent()?;
    let is_settings =
        at.file_name()? == "sandbox" && !name.ends_with(".lead") && !name.ends_with(".mcp");
    is_settings.then(|| (at.to_path_buf(), name.to_string()))
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

    /// Why the lead running this loopherd, if one does, may not act on
    /// `record` at all: it isn't a member of the lead's own herd.
    pub fn caller_refusal(&self, record: &AgentRecord) -> Result<Option<String>> {
        let Some(caller) = &self.caller else {
            return Ok(None);
        };
        let own = Lead::kept(&self.paths, caller)?.map(|lead| lead.herd);
        Ok((own.is_none() || record.herd != own).then(|| {
            format!(
                "{} isn't a member of {caller}'s herd, and {caller} runs this loopherd",
                record.name
            )
        }))
    }

    /// Why loopherd won't run git on `record`'s repo, its worktree, or the
    /// worktree its branch would be given, if it won't. A herd's member must
    /// be in its herd's repo, and when a lead runs this, its worktree must sit
    /// beside the herd's other worktrees. An entry whose herd file is gone is
    /// the human's to act on, as a hand-spawned one. No entry may name another lead's
    /// worktree, or a repo in a lead's worktree or the state dir, where a lead
    /// could have written its git config.
    pub fn path_refusal(&self, record: &AgentRecord) -> Result<Option<String>> {
        if let Some(why) = self.caller_refusal(record)? {
            return Ok(Some(why));
        }
        let name = &record.name;
        let leads = lead_worktrees(&self.paths);
        let writable = leads
            .iter()
            .map(|(_, worktree)| worktree.as_path())
            .chain([self.paths.state_dir()]);
        let inside = |path: &Path, dir: &Path| {
            let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            real(path).starts_with(real(dir))
        };
        if let Some(dir) = writable.into_iter().find(|dir| inside(&record.repo, dir)) {
            return Ok(Some(format!(
                "{name}'s ledger entry names repo {}, inside {}, which a lead can write",
                record.repo.display(),
                dir.display()
            )));
        }
        let branch_path = record
            .branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(|b| worktree_path(&record.repo, b));
        for path in record.worktree.iter().chain(&branch_path) {
            if let Some((lead, _)) = leads
                .iter()
                .find(|(lead, worktree)| lead != name && same_dir(path, worktree))
            {
                return Ok(Some(format!(
                    "{name}'s ledger entry names lead {lead}'s worktree {}",
                    path.display()
                )));
            }
        }
        let Some(herd) = &record.herd else {
            return Ok(None);
        };
        let repo = match herd::repo(&self.paths, herd) {
            Err(err) if !self.for_lead() && is_not_found(&err) => return Ok(None),
            found => found.with_context(|| {
                format!("herd {herd}'s file names no repo to check {name}'s ledger entry against")
            })?,
        };
        if !same_dir(&record.repo, &repo) {
            return Ok(Some(format!(
                "{name}'s ledger entry names repo {}, not herd {herd}'s {}",
                record.repo.display(),
                repo.display()
            )));
        }
        // ensure_worktree reuses a branch's worktree wherever the human made it.
        if !self.for_lead() {
            return Ok(None);
        }
        let beside = worktree_path(&repo, "x");
        let in_place = |worktree: &Path| {
            worktree.file_name().is_some()
                && worktree
                    .parent()
                    .zip(beside.parent())
                    .is_some_and(|(a, b)| same_dir(a, b))
        };
        if let Some(worktree) = record.worktree.as_deref().filter(|w| !in_place(w)) {
            return Ok(Some(format!(
                "{name}'s ledger entry names worktree {}, which isn't beside herd {herd}'s \
                 worktrees in {}",
                worktree.display(),
                beside.parent().unwrap_or(&repo).display()
            )));
        }
        Ok(None)
    }

    /// Whether a lead runs this loopherd.
    pub fn for_lead(&self) -> bool {
        self.caller.is_some()
    }
}

/// How a lead's Claude is started.
pub enum Launch {
    Sandboxed {
        settings: PathBuf,
        /// What loosens it: the human's own settings under `prefer`, or its herd's `unix_sockets`.
        warnings: Vec<String>,
        /// Why it can't block Unix sockets, started so under `prefer`.
        sockets_unblocked: Option<String>,
        mcp: PathBuf,
    },
    /// `prefer`, with why the sandbox can't start, and the args that may loosen it.
    HookOnly {
        why: String,
        warnings: Vec<String>,
        mcp: PathBuf,
    },
}

impl Launch {
    pub fn args(&self) -> Vec<String> {
        let (settings, mcp) = match self {
            Launch::Sandboxed { settings, mcp, .. } => (Some(settings), mcp),
            Launch::HookOnly { mcp, .. } => (None, mcp),
        };
        let mut args: Vec<String> = settings
            .map(|file| vec!["--settings".into(), file.display().to_string()])
            .unwrap_or_default();
        args.extend([
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            mcp.display().to_string(),
        ]);
        args
    }

    /// Why the lead runs without its sandbox, if it does.
    pub fn hook_only(&self) -> Option<&str> {
        match self {
            Launch::Sandboxed { .. } => None,
            Launch::HookOnly { why, .. } => Some(why),
        }
    }

    /// Why the lead's sandbox can't block Unix sockets, if it can't.
    pub fn sockets_unblocked(&self) -> Option<&str> {
        match self {
            Launch::Sandboxed {
                sockets_unblocked, ..
            } => sockets_unblocked.as_deref(),
            Launch::HookOnly { .. } => None,
        }
    }

    /// The warnings the start carries: why it's hook-only, or what loosens its sandbox.
    pub fn warnings(&self, name: &str) -> Vec<String> {
        match self {
            Launch::Sandboxed { warnings, .. } => warnings.clone(),
            Launch::HookOnly { why, warnings, .. } => {
                [vec![hook_only_warning(name, why)], warnings.clone()].concat()
            }
        }
    }
}

/// Whether lead `name`'s herd opts it into every Unix socket.
pub fn sockets_open(paths: &Paths, herd: &str) -> Result<bool> {
    Ok(herd::lead_unix_sockets(paths, herd)? == Some(UnixSockets::All))
}

pub fn sockets_open_warning(name: &str, herd: &str) -> String {
    format!(
        "{name}'s sandbox lets it use any Unix socket, as herd {herd}'s [lead] unix_sockets = \
         \"all\" says, which reopens the herdr socket, your ssh agent and WSL2's Windows \
         interop to it"
    )
}

pub fn sockets_unblocked_warning(name: &str, why: &str) -> String {
    format!(
        "{name}'s sandbox can't block Unix sockets ({why}), so herdr, your ssh agent and \
         WSL2's Windows interop stay open to it"
    )
}

pub fn hook_only_warning(name: &str, why: &str) -> String {
    format!("{name} runs without its sandbox ({why}), guarded by its tool hook alone")
}

/// Keeps `lead` in loopherd's config dir and writes its settings there from
/// its herd's `[lead]`; the settings keep it from writing either. Fails under
/// `require` when `probe` says the sandbox can't start, and for args that
/// pass their own `--settings`, which would displace or double the lead's.
pub fn prepare(paths: &Paths, name: &str, lead: &Lead, probe: Probe) -> Result<Launch> {
    let passes = |flag: &str| {
        lead.args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let drop = "drop it from [lead].args, the herd's args and the command line";
    if passes("--settings") {
        bail!(
            "{name}'s args pass --settings, which its sandbox's settings file takes; \
             {drop}, and put those settings in ~/.claude/settings.json"
        );
    }
    for flag in ["--mcp-config", "--strict-mcp-config"] {
        if passes(flag) {
            bail!(
                "{name}'s args pass {flag}, which its launch takes; {drop}, and name the \
                 servers it may use in [lead] mcp_servers"
            );
        }
    }
    let disallowed = disallowed_args(paths, &lead.herd, &lead.args)?;
    if let Some(refusal) = disallowed_refusal(paths, name, &lead.herd, &disallowed)? {
        return Err(refusal);
    }
    let arg_warnings: Vec<String> = disallowed
        .iter()
        .map(|line| disallowed_warning(name, line))
        .collect();
    let (_, mode) = herd::lead_sandbox(paths, &lead.herd)?;
    let dir = paths.config_dir().join("sandbox");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = lead_file(paths, name);
    std::fs::write(&file, serde_json::to_string_pretty(lead)? + "\n")
        .with_context(|| format!("write {}", file.display()))?;
    let mcp = dir.join(format!("{name}.mcp.json"));
    let text = serde_json::to_string_pretty(&mcp_config(paths, name, &lead.herd)?)? + "\n";
    write_private(&mcp, &text).with_context(|| format!("write {}", mcp.display()))?;
    let by_credentials = herd::merge_guard(paths, &lead.herd)? == Some(MergeGuard::Credentials);
    let sockets = probe();
    if let Err(Unavailable { why, fix }) = sockets.clone() {
        if by_credentials {
            return Err(credentials_refusal(
                name,
                &format!(
                    "its sandbox can't start ({why}), and without it the lead could read your \
                     GitHub credentials; {fix}, or set [lead] merge_guard = \"hook\" in herd {}",
                    lead.herd
                ),
            ));
        }
        return match mode {
            SandboxMode::Prefer => Ok(Launch::HookOnly {
                why,
                warnings: arg_warnings,
                mcp,
            }),
            SandboxMode::Require => Err(refusal(
                name,
                &lead.herd,
                &format!("its sandbox can't start: {why}; {fix}"),
            )),
        };
    }
    let mut sockets_unblocked = None;
    if let Ok(Sockets::Open(Unavailable { why, fix })) = sockets
        && !sockets_open(paths, &lead.herd)?
    {
        let herd = &lead.herd;
        match when_loosened(paths, herd)? {
            WhenLoosened::Warn => sockets_unblocked = Some((why, fix)),
            WhenLoosened::Refuse => bail!(
                "{name} can't run sandboxed: its sandbox can't block Unix sockets: {why}; \
                 {fix}, or set [lead] sandbox = \"prefer\" in herd {herd} to start it with \
                 them open"
            ),
            WhenLoosened::RefuseForCredentials => {
                return Err(credentials_refusal(
                    name,
                    &format!(
                        "its sandbox can't block Unix sockets ({why}), so it could use your ssh \
                         agent; {fix}, or set [lead] merge_guard = \"hook\" in herd {herd}"
                    ),
                ));
            }
        }
    }
    let worktree = worktree_path(&herd::repo(paths, &lead.herd)?, name);
    let writable = [worktree.as_path(), paths.state_dir()];
    if let Some(entry) = plantable_path_entry(std::env::var_os("PATH").as_deref(), &writable) {
        bail!(
            "{name} can't start: PATH has {entry}, where it could plant the loopherd its \
             watch runs outside its sandbox; drop that entry from PATH"
        );
    }
    // Before its settings, which name what it makes.
    create_state(paths, name)?;
    let own = settings(paths, name, &lead.herd)?;
    let loosened = loosenings(&paths.home, &worktree, &own);
    if let Some(refusal) = loosened_refusal(paths, name, &lead.herd, &loosened)? {
        return Err(refusal);
    }
    let file = settings_file(paths, name);
    let text = serde_json::to_string_pretty(&own)? + "\n";
    write_private(&file, &text).with_context(|| format!("write {}", file.display()))?;
    let mut warnings: Vec<String> = loosened
        .iter()
        .map(|line| loosened_warning(name, line))
        .chain(arg_warnings)
        .collect();
    if sockets_open(paths, &lead.herd)? {
        warnings.push(sockets_open_warning(name, &lead.herd));
    }
    if let Some((why, fix)) = &sockets_unblocked {
        warnings.push(format!("{}; {fix}", sockets_unblocked_warning(name, why)));
    }
    Ok(Launch::Sandboxed {
        settings: file,
        warnings,
        sockets_unblocked: sockets_unblocked.map(|(why, _)| why),
        mcp,
    })
}

/// The only MCP servers lead `name` gets, by `--strict-mcp-config`: those its
/// herd's `[lead] mcp_servers` names, from the human's user scope in
/// `~/.claude.json`. Local and project scope are the repo's, which members
/// and the lead can write.
fn mcp_config(paths: &Paths, name: &str, herd: &str) -> Result<Value> {
    let wanted = herd::lead_mcp_servers(paths, herd)?;
    let mut servers = serde_json::Map::new();
    if wanted.is_empty() {
        return Ok(json!({"mcpServers": servers}));
    }
    let configured = user_mcp_servers(paths)?;
    for server in wanted {
        let Some(found) = configured.get(&server) else {
            bail!(
                "{name} can't start: herd {herd}'s [lead] mcp_servers names {server:?}, which \
                 {} doesn't configure in your user scope; add it with claude mcp add --scope \
                 user, or drop it from mcp_servers (local and project scopes, claude.ai \
                 connectors and plugins' servers can't be given to a lead)",
                user_mcp_file(paths).display()
            );
        };
        servers.insert(server, found.clone());
    }
    Ok(json!({"mcpServers": servers}))
}

pub fn user_mcp_file(paths: &Paths) -> PathBuf {
    paths.home.join(".claude.json")
}

/// The MCP servers the human's user scope configures, by name; none when
/// `~/.claude.json` is missing.
pub fn user_mcp_servers(paths: &Paths) -> Result<serde_json::Map<String, Value>> {
    let file = user_mcp_file(paths);
    let config: Value = match std::fs::read_to_string(&file) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    Ok(match &config["mcpServers"] {
        Value::Object(servers) => servers.clone(),
        _ => serde_json::Map::new(),
    })
}

/// What the human's own Claude settings loosen in lead `name`'s sandbox, one
/// line per key, each naming its file.
/// A lead whose own settings can't be built is judged against none, so a
/// failure to build them doesn't hide these.
pub fn loosened(paths: &Paths, name: &str, herd: &str) -> Result<Vec<String>> {
    let own = settings(paths, name, herd).unwrap_or_else(|_| json!({}));
    let worktree = worktree_path(&herd::repo(paths, herd)?, name);
    Ok(loosenings(&paths.home, &worktree, &own))
}

/// What a loosened sandbox does to a lead's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenLoosened {
    /// `prefer`: it starts, warned.
    Warn,
    /// `require`: it doesn't start.
    Refuse,
    /// `merge_guard = "credentials"`, whose token a loosened sandbox could expose, under either mode.
    RefuseForCredentials,
}

pub fn when_loosened(paths: &Paths, herd: &str) -> Result<WhenLoosened> {
    let (_, mode) = herd::lead_sandbox(paths, herd)?;
    Ok(match (herd::merge_guard(paths, herd)?, mode) {
        (Some(MergeGuard::Credentials), _) => WhenLoosened::RefuseForCredentials,
        (_, SandboxMode::Require) => WhenLoosened::Refuse,
        (_, SandboxMode::Prefer) => WhenLoosened::Warn,
    })
}

fn loosened_warning(name: &str, line: &str) -> String {
    format!("{name}'s sandbox is loosened by {line}")
}

/// The refusal for a loosened sandbox, or `None` where it starts warned.
fn loosened_refusal(
    paths: &Paths,
    name: &str,
    herd: &str,
    loosened: &[String],
) -> Result<Option<anyhow::Error>> {
    if loosened.is_empty() {
        return Ok(None);
    }
    let why = format!(
        "your Claude settings loosen its sandbox: {}; remove those",
        loosened.join("; ")
    );
    refusal_by_mode(paths, name, herd, &why)
}

/// The refusal for a lead whose args aren't all allowlisted, or `None` where it starts warned.
fn disallowed_refusal(
    paths: &Paths,
    name: &str,
    herd: &str,
    disallowed: &[String],
) -> Result<Option<anyhow::Error>> {
    if disallowed.is_empty() {
        return Ok(None);
    }
    let why = format!(
        "its args may loosen its sandbox or permissions: {}; a lead takes only {ALLOWED_ARGS}, \
         so drop those",
        disallowed.join("; ")
    );
    refusal_by_mode(paths, name, herd, &why)
}

fn disallowed_warning(name: &str, line: &str) -> String {
    format!("{name} starts with an arg that may loosen its sandbox or permissions: {line}")
}

pub const ALLOWED_ARGS: &str =
    "--model, --effort, --verbose and --permission-mode default, auto or plan";

/// Each of a lead's `args`, without loopherd's own launch flags, that the
/// allowlist doesn't cover, with its values and where it was set: the
/// herd file's args they start with, else the command line. D100 says why
/// it's an allowlist.
fn disallowed_args(paths: &Paths, herd: &str, args: &[String]) -> Result<Vec<String>> {
    let (configured, source) = herd::lead_args(paths, herd)?;
    let sources = match args.strip_prefix(&configured[..]) {
        Some(rest) => vec![(&configured[..], source), (rest, "the command line".into())],
        None => vec![(args, "its recorded args".into())],
    };
    Ok(sources
        .into_iter()
        .flat_map(|(args, source)| {
            outside_allowlist(args)
                .into_iter()
                .map(move |arg| format!("{arg}, from {source}"))
        })
        .collect())
}

/// What herd `herd`'s file passes its lead outside the allowlist, one line each.
pub fn configured_disallowed_args(paths: &Paths, herd: &str) -> Result<Vec<String>> {
    let (configured, _) = herd::lead_args(paths, herd)?;
    disallowed_args(paths, herd, &configured)
}

/// Each arg the allowlist doesn't cover, with the values that follow a flag.
fn outside_allowlist(args: &[String]) -> Vec<String> {
    let mut found = vec![];
    let mut at = 0;
    while at < args.len() {
        if let Some(taken) = allowed_len(&args[at..]) {
            at += taken;
            continue;
        }
        let mut shown = vec![args[at].as_str()];
        at += 1;
        if shown[0].starts_with('-') && !shown[0].contains('=') {
            while let Some(value) = args.get(at).filter(|a| !a.starts_with('-')) {
                shown.push(value);
                at += 1;
            }
        }
        found.push(shown.join(" "));
    }
    found
}

/// How many of `args` an allowlisted form at their start takes, if one does.
fn allowed_len(args: &[String]) -> Option<usize> {
    let (flag, inline) = match args[0].split_once('=') {
        Some((flag, value)) => (flag, Some(value)),
        None => (args[0].as_str(), None),
    };
    let fits = |value: &str| match flag {
        "--model" | "--effort" => !value.is_empty() && !value.starts_with('-'),
        "--permission-mode" => ["default", "auto", "plan"].contains(&value),
        _ => false,
    };
    match (flag, inline) {
        ("--verbose", None) => Some(1),
        (_, Some(value)) => fits(value).then_some(1),
        (_, None) => args.get(1).filter(|value| fits(value)).map(|_| 2),
    }
}

/// `why` refuses a lead under `require` or `merge_guard = "credentials"`; `None` under `prefer`.
fn refusal_by_mode(
    paths: &Paths,
    name: &str,
    herd: &str,
    why: &str,
) -> Result<Option<anyhow::Error>> {
    Ok(match when_loosened(paths, herd)? {
        WhenLoosened::Warn => None,
        WhenLoosened::Refuse => Some(anyhow::anyhow!(
            "{name} can't run sandboxed: {why}, or set [lead] sandbox = \"prefer\" in herd \
             {herd} to start it with them as warnings"
        )),
        WhenLoosened::RefuseForCredentials => Some(credentials_refusal(
            name,
            &format!("{why}, or set [lead] merge_guard = \"hook\" in herd {herd}"),
        )),
    })
}

/// Keys that loosen a sandbox when set to anything but `false`.
const LOOSENING_SWITCHES: [(&str, &str); 9] = [
    (
        "/sandbox/filesystem/disabled",
        "turns its filesystem rules off",
    ),
    ("/sandbox/enableWeakerNestedSandbox", "weakens it"),
    ("/sandbox/allowNestedUserNamespaces", "weakens it"),
    ("/sandbox/enableWeakerNetworkIsolation", "weakens it"),
    ("/sandbox/allowAppleEvents", "lets it drive other apps"),
    (
        "/sandbox/network/httpProxyPort",
        "hands its network to your proxy, past its own checks on local addresses",
    ),
    (
        "/sandbox/network/socksProxyPort",
        "hands its network to your proxy, past its own checks on local addresses",
    ),
    (
        "/sandbox/network/allowLocalBinding",
        "lets it bind local ports on macOS",
    ),
    (
        "/sandbox/network/allowMachLookup",
        "opens macOS services to it",
    ),
];

/// The tools whose `permissions.allow` rules open writes; the file tools run
/// outside the sandbox.
const WRITING_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// What the human's Claude settings add to a lead's sandbox, whose own
/// settings are `own`: one line per loosening entry, naming its file. Of the
/// worktree's project and local settings only the permissions count, since
/// `--settings` locks their sandbox keys out. D100 says which keys reach a
/// lead, and why.
fn loosenings(home: &Path, worktree: &Path, own: &Value) -> Vec<String> {
    let own_writes: Vec<PathBuf> = entries(&own["sandbox"]["filesystem"]["allowWrite"])
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let user_file = home.join(".claude/settings.json");
    let mut found = vec![];
    if let Some(user) = read_settings(&user_file, &mut found) {
        found.extend(sandbox_loosenings(
            &user,
            &user_file,
            home,
            own,
            &own_writes,
        ));
        let rules_root = home.join(".claude");
        found.extend(permission_loosenings(
            &user,
            &user_file,
            &rules_root,
            worktree,
            home,
            &own_writes,
        ));
    }
    for name in ["settings.json", "settings.local.json"] {
        let file = worktree.join(".claude").join(name);
        if let Some(project) = read_settings(&file, &mut found) {
            found.extend(permission_loosenings(
                &project,
                &file,
                worktree,
                worktree,
                home,
                &own_writes,
            ));
        }
    }
    found
}

/// A settings file's contents; `None` when it's missing or empty, or, noted
/// in `found`, when it can't be read or parsed. Claude's sandbox leaves a
/// zero-byte placeholder for a protected settings file that didn't exist.
fn read_settings(file: &Path, found: &mut Vec<String>) -> Option<Value> {
    let shown = file.display();
    match std::fs::read_to_string(file) {
        Ok(text) if text.is_empty() => None,
        Ok(text) => match serde_json::from_str(&text) {
            Ok(settings) => Some(settings),
            Err(e) => {
                found.push(format!("{shown}: can't parse it: {e}"));
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            found.push(format!("{shown}: can't read it: {e}"));
            None
        }
    }
}

/// Only an exact match with a path the lead may write is no loosening:
/// beneath one, the lead could plant a symlink before it exists.
fn opens_writes(path: Option<PathBuf>, own_writes: &[PathBuf]) -> bool {
    !path.is_some_and(|path| own_writes.contains(&path))
}

fn sandbox_loosenings(
    user: &Value,
    file: &Path,
    home: &Path,
    own: &Value,
    own_writes: &[PathBuf],
) -> Vec<String> {
    let shown = file.display();
    let mut found = vec![];
    let own_commands = entries(&own["sandbox"]["excludedCommands"]);
    for command in entries(&user["sandbox"]["excludedCommands"]) {
        if !own_commands.contains(&command) {
            found.push(format!(
                "{shown}: sandbox.excludedCommands runs {command:?} outside it"
            ));
        }
    }
    for path in entries(&user["sandbox"]["filesystem"]["allowWrite"]) {
        if opens_writes(absolute(&path, home), own_writes) {
            found.push(format!(
                "{shown}: sandbox.filesystem.allowWrite opens {path} to writes"
            ));
        }
    }
    // allowRead carves into denyRead. Credentials to hide make any of it suspect;
    // otherwise only a carve into what is denied counts.
    let credentials = !own["sandbox"]["filesystem"]["allowRead"].is_null();
    let own_reads = entries(&own["sandbox"]["filesystem"]["allowRead"]);
    let denied: Vec<PathBuf> = entries(&own["sandbox"]["filesystem"]["denyRead"])
        .iter()
        .filter_map(|path| absolute(path, home))
        .collect();
    for path in entries(&user["sandbox"]["filesystem"]["allowRead"]) {
        let opens = absolute(&path, home).is_none_or(|path| {
            denied
                .iter()
                .any(|deny| deny.starts_with(&path) || path.starts_with(deny))
        });
        if !own_reads.contains(&path) && (credentials || opens) {
            found.push(format!(
                "{shown}: sandbox.filesystem.allowRead opens {path} to reads"
            ));
        }
    }
    for (pointer, what) in LOOSENING_SWITCHES {
        let set = user
            .pointer(pointer)
            .is_some_and(|v| v != &Value::Bool(false));
        if set && own.pointer(pointer).is_none() {
            let key = pointer.trim_start_matches('/').replace('/', ".");
            found.push(format!("{shown}: {key} {what}"));
        }
    }
    found
}

/// What a settings file's permissions open to the file tools, which run
/// outside the sandbox. `rules_root` is where its `/path` rules start.
fn permission_loosenings(
    settings: &Value,
    file: &Path,
    rules_root: &Path,
    worktree: &Path,
    home: &Path,
    own_writes: &[PathBuf],
) -> Vec<String> {
    let shown = file.display();
    let mut found = vec![];
    for path in entries(&settings["permissions"]["additionalDirectories"]) {
        if opens_writes(absolute(&path, home), own_writes) {
            found.push(format!(
                "{shown}: permissions.additionalDirectories opens {path} to writes"
            ));
        }
    }
    for rule in entries(&settings["permissions"]["allow"]) {
        let (tool, spec) = match rule.split_once('(') {
            Some((tool, rest)) => match rest.strip_suffix(')') {
                Some(spec) => (tool, spec),
                None => continue,
            },
            None => (rule.as_str(), "*"),
        };
        if !WRITING_TOOLS.contains(&tool) {
            continue;
        }
        let everywhere = matches!(spec, "*" | "**");
        // Only Edit's rules take paths; another tool's rule with one matches nothing.
        if !everywhere && tool != "Edit" {
            continue;
        }
        let confined = !everywhere
            && rule_dir(spec, rules_root, worktree, home)
                .is_some_and(|dir| own_writes.iter().any(|own| dir.starts_with(own)));
        if !confined {
            found.push(format!("{shown}: permissions.allow opens {rule} to writes"));
        }
    }
    if settings["permissions"]["defaultMode"] == "bypassPermissions" {
        found.push(format!(
            "{shown}: permissions.defaultMode bypassPermissions lets its file tools write \
             anywhere"
        ));
    }
    found
}

/// The directory an `Edit(…)` rule's matches all lie beneath: its path up to
/// the first wildcard. `//` starts at `/`, `~/` at home, `/` at `rules_root`,
/// and a bare path at the lead's working directory, its worktree. `None` when
/// it climbs with `..`.
fn rule_dir(spec: &str, rules_root: &Path, worktree: &Path, home: &Path) -> Option<PathBuf> {
    let (base, rest) = if let Some(rest) = spec.strip_prefix("//") {
        (Path::new("/"), rest)
    } else if let Some(rest) = spec.strip_prefix("~/") {
        (home, rest)
    } else if let Some(rest) = spec.strip_prefix('/') {
        (rules_root, rest)
    } else {
        (worktree, spec)
    };
    let literal: PathBuf = rest
        .split('/')
        .take_while(|part| !part.contains(['*', '?', '[']))
        .collect();
    let dir = base.join(literal);
    let climbs = dir
        .components()
        .any(|c| c == std::path::Component::ParentDir);
    (!climbs).then_some(dir)
}

/// A settings list's entries; anything else that's set is one entry, as written.
fn entries(value: &Value) -> Vec<String> {
    match value {
        Value::Null => vec![],
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map_or_else(|| item.to_string(), String::from))
            .collect(),
        other => vec![other.to_string()],
    }
}

/// A sandbox path as an absolute one, or `None` when it's relative or climbs with `..`.
fn absolute(path: &str, home: &Path) -> Option<PathBuf> {
    let path = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(
            path.strip_prefix('/')
                .filter(|p| p.starts_with('/'))
                .unwrap_or(path),
        ),
    };
    let climbs = path
        .components()
        .any(|c| c == std::path::Component::ParentDir);
    (path.is_absolute() && !climbs).then_some(path)
}

/// A credentials lead's settings hold its token, so only the human reads them.
fn write_private(file: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut open = std::fs::OpenOptions::new();
    let created = open
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    created.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(&mut &created, text.as_bytes())
}

fn credentials_refusal(name: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!("{name} can't start with merge_guard = \"credentials\": {why}")
}

/// How a running lead stands against its sandbox.
#[derive(Debug, PartialEq, Eq)]
pub enum Running {
    /// With warnings: what loosens its sandbox under `prefer`, and whether its
    /// herd's switches have changed since its settings were written.
    Sandboxed(Vec<String>),
    /// Started without its settings, under `prefer`: why, and other warnings.
    HookOnly(String, Vec<String>),
}

/// Whether `claude_args`, as the ledger records a lead's launch, pass lead
/// `name` its sandbox's settings file.
pub fn launched_with_settings(paths: &Paths, name: &str, claude_args: &[String]) -> bool {
    let file = settings_file(paths, name).display().to_string();
    claude_args
        .windows(2)
        .any(|pair| pair[0] == "--settings" && pair[1] == file)
}

/// `args` without `--settings` and the MCP flags, which loopherd adds to each lead's launch.
pub fn without_launch_flags(args: &[String]) -> Vec<String> {
    let mut kept = vec![];
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--settings" => {
                args.next();
            }
            // It takes every value up to the next flag.
            "--mcp-config" => while args.next_if(|a| !a.starts_with('-')).is_some() {},
            "--strict-mcp-config" => {}
            _ if arg.starts_with("--settings=") || arg.starts_with("--mcp-config=") => {}
            _ => kept.push(arg.clone()),
        }
    }
    kept
}

/// Checks a running lead was launched with its settings, as `claude_args`
/// record, that they match its herd's current switches, and what the human's
/// own settings loosen, and that its args are allowlisted. Under `require` one
/// launched without them, loosened, or with other args is refused.
pub fn check_running(
    paths: &Paths,
    name: &str,
    herd: &str,
    claude_args: &[String],
) -> Result<Running> {
    let (_, mode) = herd::lead_sandbox(paths, herd)?;
    let file = settings_file(paths, name);
    let resume = format!("quit its Claude and run loopherd revive --name {name}");
    let args = match Lead::kept(paths, name)? {
        Some(kept) => kept.args,
        None => without_launch_flags(claude_args),
    };
    let disallowed = disallowed_args(paths, herd, &args)?;
    if let Some(refusal) = disallowed_refusal(paths, name, herd, &disallowed)? {
        return Err(refusal);
    }
    let mut warnings: Vec<String> = disallowed
        .iter()
        .map(|line| disallowed_warning(name, line))
        .collect();
    if !claude_args.iter().any(|a| a == "--strict-mcp-config") {
        warnings.push(format!(
            "{name} was started with every MCP server you configure, not only its herd's \
             [lead] mcp_servers; {resume}"
        ));
    }
    if !launched_with_settings(paths, name, claude_args) {
        let why = "it was started without its sandbox";
        if herd::merge_guard(paths, herd)? == Some(MergeGuard::Credentials) {
            return Err(credentials_refusal(
                name,
                &format!(
                    "{why}, so it can read your GitHub credentials; {resume} to resume it \
                     sandboxed"
                ),
            ));
        }
        return match mode {
            SandboxMode::Prefer => Ok(Running::HookOnly(why.into(), warnings)),
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
    // Judged against what the running Claude started with, which may predate the switches.
    let worktree = worktree_path(&herd::repo(paths, herd)?, name);
    let loosened = loosenings(&paths.home, &worktree, on_disk.as_ref().unwrap_or(&current));
    if let Some(refusal) = loosened_refusal(paths, name, herd, &loosened)? {
        return Err(refusal);
    }
    warnings.extend(loosened.iter().map(|line| loosened_warning(name, line)));
    let switched = on_disk.as_ref().map(|s| switch_settings(s, paths));
    if switched != Some(switch_settings(&current, paths)) {
        warnings.push(format!(
            "{name}'s sandbox was set before herd {herd}'s current switches, \
             which reach it only when its Claude starts again; {resume}"
        ));
    }
    Ok(Running::Sandboxed(warnings))
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

/// The state dir's directories a lead writes from its sandbox: `reports/`
/// for its mailbox and its watch's files, `handoffs/` for its handoff. The
/// rest stays read-only to it as a directory, since a deny on one file holds
/// only until something outside the sandbox renames over that file.
const WRITABLE_STATE: [&str; 2] = ["reports", "handoffs"];

/// The shepherd's watch files in `reports/`. Written in place, never
/// replaced, so a deny on each file holds.
const SHEPHERDS_WATCH: [&str; 2] = ["reports/watch.lock", "reports/watch.fired"];

/// What in the state dir is the human's or the shepherd's, denied by name
/// to the file tools, which the sandbox doesn't cover. `true` marks a
/// directory.
const KEPT_FROM_FILE_TOOLS: [(&str, bool); 5] = [
    ("paused", true),
    ("shepherd.json", false),
    ("shepherd.json.lock", false),
    ("ledger.json", false),
    ("ledger.lock", false),
];

/// Makes what lead `name`'s sandbox names in the state dir. An `allowWrite`
/// entry for a missing path has no effect on Linux, and a `denyWrite` entry
/// gets a placeholder that would sit in the way of the first real write.
pub fn create_state(paths: &Paths, name: &str) -> Result<()> {
    let state = paths.state_dir();
    let requests = crate::broker::requests_dir(paths, name);
    // Every herd's lead folder, so a lead started later sits in one a literal deny names.
    let leads = herd::names(&paths.herds)?
        .into_iter()
        .filter_map(|herd| crate::lead::lead_name(&herd).ok())
        .map(|lead| state.join("broker").join(lead));
    for dir in WRITABLE_STATE
        .map(|dir| state.join(dir))
        .into_iter()
        .chain([requests, paths.locks_dir()])
        .chain(leads)
    {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    for file in SHEPHERDS_WATCH {
        let file = state.join(file);
        // Left as it is when it exists: the sandbox's placeholder for it is read-only.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                return Err(e).with_context(|| format!("create {}", file.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Lead `name`'s settings, from its herd file alone: its worktree is where
/// `lead` puts it in the herd's repo, whatever the ledger says.
fn settings(paths: &Paths, name: &str, herd: &str) -> Result<Value> {
    let (switches, _) = herd::lead_sandbox(paths, herd)?;
    let repo = herd::repo(paths, herd)?;
    let config = paths.config_dir();
    let claude = paths.home.join(".claude");
    let worktree = worktree_path(&repo, name);
    let state = paths.state_dir();
    let mut allow_write = vec![worktree];
    allow_write.extend(WRITABLE_STATE.map(|dir| state.join(dir)));
    // Only its own: a broker takes every request in its folder as its lead's.
    allow_write.push(crate::broker::requests_dir(paths, name));
    if switches.install {
        allow_write.push(paths.home.join(".local/bin"));
    }
    let shepherds_watch = SHEPHERDS_WATCH.map(|file| state.join(file));
    let mut deny_write = vec![config.to_path_buf(), claude.clone()];
    deny_write.extend(shepherds_watch.iter().cloned());
    let mut deny = vec![edit_rule(config), edit_rule(&claude)];
    deny.extend(KEPT_FROM_FILE_TOOLS.map(|(path, dir)| match dir {
        true => edit_rule(&state.join(path)),
        false => file_rule(&state.join(path)),
    }));
    deny.extend(shepherds_watch.iter().map(|file| file_rule(file)));
    if cfg!(target_os = "linux") {
        // The Linux sandbox skips glob denies, so this one reaches only the file tools.
        deny.push(file_rule(&state.join("broker/*")));
    }
    deny.extend(
        others_in_broker(paths, name)?
            .iter()
            .map(|(path, dir)| match dir {
                true => edit_rule(path),
                false => file_rule(path),
            }),
    );
    let mut sandbox = json!({
        "enabled": true,
        "failIfUnavailable": true,
        "allowUnsandboxedCommands": false,
        // Set either way, so the human's own settings can't open them.
        "network": {"allowAllUnixSockets": sockets_open(paths, herd)?},
        "filesystem": {
            "allowWrite": allow_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "denyWrite": deny_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            // On Linux a file it can only read is still one it can flock.
            "denyRead": [paths.locks_dir().display().to_string()],
        },
    });
    // loopherd's own commands that write past the sandbox run in the lead's broker (D98).
    // A sandboxed watch would hold the sandbox's .git/config.lock placeholder for its whole run (D104).
    let mut excluded: Vec<&str> = vec!["loopherd watch", "loopherd watch *"];
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
    let mut env = None;
    if herd::merge_guard(paths, herd)? == Some(MergeGuard::Credentials) {
        if switches.install {
            return Err(credentials_refusal(
                name,
                &format!(
                    "install = true runs scripts/install outside the sandbox, from a worktree \
                     the lead can edit; set [lead] install = false, or merge_guard = \"hook\", \
                     in herd {herd}"
                ),
            ));
        }
        env = Some(
            crate::credentials::lead_env(paths, herd)
                .map_err(|e| credentials_refusal(name, &format!("{e:#}")))?,
        );
        let human =
            crate::credentials::human_credentials(&paths.home, &|name| std::env::var_os(name));
        // The file tools run outside the sandbox, so each path is denied to both.
        let mut deny_read: Vec<String> = human
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        deny_read.push(paths.locks_dir().display().to_string());
        sandbox["filesystem"]["denyRead"] = json!(deny_read);
        sandbox["filesystem"]["allowRead"] = json!(crate::credentials::ssh_readable(&paths.home));
        deny.extend(human.iter().map(|(path, is_dir)| {
            let under = if *is_dir { "/**" } else { "" };
            format!("Read(/{}{under})", path.display())
        }));
    }
    let mut settings = json!({
        "sandbox": sandbox,
        "permissions": {
            "denyInsteadOfPrompting": true,
            "allow": allow,
            "deny": deny,
        },
    });
    if let Some(env) = env {
        settings["env"] = Value::Object(env);
    }
    Ok(settings)
}

/// What in `<state>/broker/` isn't lead `name`'s `requests/`, each named, as
/// the file tools' denies. An `Edit` deny is also a sandbox write deny, which
/// beats `allowWrite`, so one on all of `broker/` would close `requests/`.
/// On Linux a glob deny covers the rest; elsewhere the sandbox would apply it.
fn others_in_broker(paths: &Paths, name: &str) -> Result<Vec<(PathBuf, bool)>> {
    let broker = paths.state_dir().join("broker");
    let own = broker.join(name);
    let mut found = vec![];
    for (dir, skip) in [(&broker, name), (&own, "requests")] {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("list {}", dir.display()))?;
            if entry.file_name() != skip {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                found.push((entry.path(), is_dir));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// `settings` without the denies naming paths in `<state>/broker/` one by
/// one, which change as leads and herds come and go, not with the switches.
fn switch_settings(settings: &Value, paths: &Paths) -> Value {
    let broker = format!("Edit(/{}/", paths.state_dir().join("broker").display());
    let named = |rule: &str| {
        rule.strip_prefix(&broker)
            .is_some_and(|rest| !rest.trim_end_matches("/**)").contains('*'))
    };
    let mut settings = settings.clone();
    if let Some(deny) = settings["permissions"]["deny"].as_array_mut() {
        deny.retain(|rule| !rule.as_str().is_some_and(named));
    }
    settings
}

/// A deny for the file tools; `//` marks the path absolute, not relative to the settings file.
fn edit_rule(dir: &Path) -> String {
    format!("Edit(/{}/**)", dir.display())
}

fn file_rule(file: &Path) -> String {
    format!("Edit(/{})", file.display())
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

        assert_eq!(
            calling_lead(&paths, &procs, 50).unwrap(),
            Some("lead-h".into())
        );

        let elsewhere = root.path().join("lead-h.json").display().to_string();
        procs.set_cmdline(30, &["claude", "--settings", &elsewhere]);
        assert_eq!(calling_lead(&paths, &procs, 50).unwrap(), None);
    }

    #[test]
    fn a_path_entry_a_lead_could_plant_loopherd_in_is_named() {
        let worktree = Path::new("/repo-worktrees/lead-h");
        let state = Path::new("/state/loopherd");
        let named = |path: &str| plantable_path_entry(Some(path.as_ref()), &[worktree, state]);

        assert_eq!(named("/usr/bin:/home/h/.local/bin"), None);
        assert_eq!(named("/usr/bin::/bin").as_deref(), Some("an empty entry"));
        assert_eq!(
            named("/usr/bin:node_modules/.bin").as_deref(),
            Some("the relative entry node_modules/.bin")
        );
        assert_eq!(
            named("/repo-worktrees/lead-h/bin:/usr/bin").as_deref(),
            Some("/repo-worktrees/lead-h/bin, inside /repo-worktrees/lead-h")
        );
        assert_eq!(named("/repo-worktrees/lead-hx/bin"), None);
    }

    #[test]
    fn a_lead_run_from_another_config_dir_is_refused_not_missed() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(&root.path().join("ours"));
        let theirs = root.path().join("theirs/sandbox");
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(theirs.join("lead-h.lead.json"), "{}").unwrap();
        let settings = theirs.join("lead-h.json").display().to_string();
        let mut procs = crate::process::FakeProcs::pane();
        procs.set_cmdline(30, &["claude", "--settings", &settings]);
        procs.add(50, "/usr/bin/loopherd", 30, 500);

        let err = calling_lead(&paths, &procs, 50).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!(
                "refused: an ancestor Claude runs lead lead-h from {}, not from this \
                 loopherd's config dir {}",
                theirs.display(),
                paths.config_dir().display()
            )
        );
    }

    #[test]
    fn only_socats_own_denial_counts_as_a_blocked_socket() {
        let denied = "2026/11/20 09:40:00 socat[7] E socket(1, 1, 0): Operation not permitted\n";
        assert_eq!(tried(false, denied), Tried::Blocked);
        assert_eq!(
            tried(
                false,
                "apply-seccomp: unshare(CLONE_NEWUSER): Operation not permitted\n"
            ),
            Tried::FailedSetup(
                "apply-seccomp: unshare(CLONE_NEWUSER): Operation not permitted".into()
            )
        );
        let connected = "2026/11/20 09:40:00 socat[7] N successfully connected from local \
                         address AF=1\n";
        assert_eq!(tried(true, connected), Tried::ConnectedThrough);
        assert_eq!(tried(true, "2.1.329 (Claude Code)\n"), Tried::NotAFilter);
        assert_eq!(
            tried(false, "error: unknown option '-u'\n"),
            Tried::NotAFilter
        );
        assert_eq!(
            tried(false, "mise ERROR no tasks defined\n"),
            Tried::NotAFilter
        );
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
