//! The Claude Code sandbox a lead runs in, written from its switches (D83, D89).

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handoff;
use crate::herd::{self, MergeGuard, Paths};
use crate::ledger::{AgentRecord, Ledger};
use crate::process::Procs;
use crate::spawn::SpawnRequest;
use crate::worker_sandbox::WorkerSandbox;
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
    let version = output_within(
        std::process::Command::new("claude").arg("--version"),
        PROBE_SECS,
    )
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
    let tried = output_within(
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
            .args(["--unshare-pid", "true"]),
        PROBE_SECS,
    );
    let fix = "run under WSL2, not WSL1, and let bubblewrap make user namespaces \
               (kernel.unprivileged_userns_clone, or an AppArmor profile for bwrap)";
    match tried {
        Ok(out) if out.status.success() => Ok(socket_filter(seccomp_filters())),
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

/// How long each command the probe runs gets before it's killed.
const PROBE_SECS: u64 = 10;

/// A command that runs a program under a seccomp filter, and the program
/// that sets the filter up, as a fix names it.
struct Filter {
    program: &'static str,
    command: std::process::Command,
}

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

/// Tries a Unix-socket connect under each of `filters` in turn, with socat,
/// which the sandbox needs anyway, until one answers.
fn socket_filter(filters: impl Iterator<Item = Filter>) -> Sockets {
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
    for Filter {
        program,
        mut command,
    } in filters
    {
        command
            .args(["socat", "-d", "-d", "-u", "OPEN:/dev/null"])
            .arg(format!("UNIX-CONNECT:{}", socket.display()));
        let result = match output_within(&mut command, PROBE_SECS) {
            Ok(out) => tried(out.status.success(), &String::from_utf8_lossy(&out.stderr)),
            Err(_) => Tried::NotAFilter,
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
                failed.get_or_insert((line, program));
            }
            Tried::NotAFilter => {}
        }
    }
    match failed {
        None => open(
            "Claude Code finds no seccomp filter to block them with".into(),
            FILTER_FIX.into(),
        ),
        Some((line, program)) => open(
            format!("its seccomp filter can't set itself up: {line}"),
            format!(
                "let it make user namespaces, as bubblewrap does \
                 (kernel.unprivileged_userns_clone, or an AppArmor profile for {program})"
            ),
        ),
    }
}

/// The seccomp filters Claude Code could use, looked up lazily: its native
/// binary's own, chosen by `ARGV0`, which a version manager's shim passes on,
/// then an npm install's helper where Claude Code looks for it.
fn seccomp_filters() -> impl Iterator<Item = Filter> {
    let claude = crate::on_path("claude");
    let native = claude.clone().map(|claude| {
        let mut command = std::process::Command::new(claude);
        command.env("ARGV0", "apply-seccomp");
        Filter {
            program: "claude",
            command,
        }
    });
    let helpers = std::iter::once_with(move || npm_helpers(claude.as_deref())).flatten();
    native.into_iter().chain(helpers)
}

/// npm's `apply-seccomp` beside the `claude` on PATH, or in the global sandbox-runtime.
fn npm_helpers(claude: Option<&Path>) -> Vec<Filter> {
    let Some(claude) = claude else {
        return vec![];
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return vec![],
    };
    let helper = Path::new("vendor/seccomp").join(arch).join("apply-seccomp");
    let package = claude
        .canonicalize()
        .ok()
        .and_then(|real| real.parent().map(Path::to_path_buf));
    let global = output_within(
        std::process::Command::new("npm").args(["root", "-g"]),
        PROBE_SECS,
    )
    .ok()
    .filter(|out| out.status.success())
    .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    .filter(|root| root.is_absolute())
    .map(|root| root.join("@anthropic-ai/sandbox-runtime"));
    package
        .into_iter()
        .flat_map(|dir| [dir.clone(), dir.join("../.."), dir.join("..")])
        .chain(global)
        .map(|dir| dir.join(&helper))
        .filter(|path| path.is_file())
        .map(|path| Filter {
            program: "apply-seccomp",
            command: std::process::Command::new(path),
        })
        .collect()
}

/// `command`'s output, with no stdin, run in its own process group; once it,
/// or a descendant holding its output, has run `secs` seconds the group is
/// killed and this errs `TimedOut`.
fn output_within(
    command: &mut std::process::Command,
    secs: u64,
) -> std::io::Result<std::process::Output> {
    crate::shell::run_within(command, std::time::Duration::from_secs(secs))
        .map_err(std::io::Error::other)?
        .ok_or_else(|| {
            std::io::Error::new(ErrorKind::TimedOut, format!("still running after {secs}s"))
        })
}

/// Whether one of WSL2's binfmt_misc entries for Windows programs is enabled here.
pub fn wsl_interop() -> bool {
    wsl_interop_in(Path::new("/proc/sys/fs/binfmt_misc"))
}

/// Newer WSL registers interop under suffixed names such as `WSLInterop-late`.
fn wsl_interop_in(binfmt: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(binfmt) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("WSLInterop")
            && std::fs::read_to_string(entry.path())
                .is_ok_and(|text| text.lines().next() == Some("enabled"))
    })
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
/// write: its herd, the args its Claude starts with besides its settings, and
/// the `PATH` its unsandboxed watch and broker run under. `revive` trusts this
/// over the ledger, which the lead can write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    pub herd: String,
    pub args: Vec<String>,
    /// The human's `PATH` as `lead` or `revive` last started it, less every
    /// entry the lead could plant a binary in. [`prepare`] sets it, whatever
    /// the caller passes; `None` in a record from before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
            // A sandboxed worker's settings sit beside the leads'.
            if at.join(format!("{name}.member.json")).is_file() {
                continue;
            }
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

/// `path` less every entry a lead could plant a binary in: those
/// [`plantable_path_entry`] names, and any in the temp dir or under a
/// world-writable dir, such as its sandbox's temp dir. `None` when unset or not UTF-8.
fn recorded_path(path: Option<&std::ffi::OsStr>, writable: &[&Path]) -> Option<String> {
    let temp = std::env::temp_dir();
    let kept: Vec<&str> = path?
        .to_str()?
        .split(':')
        .filter(|entry| {
            let dir = Path::new(entry);
            plantable_path_entry(Some(dir.as_os_str()), writable).is_none()
                && !dir.starts_with(&temp)
                && !under_world_writable(dir)
        })
        .collect();
    Some(kept.join(":"))
}

/// Whether `dir`, or where it resolves to, sits in a dir anyone may write.
fn under_world_writable(dir: &Path) -> bool {
    let resolved = dir.canonicalize().ok();
    [Some(dir), resolved.as_deref()]
        .into_iter()
        .flatten()
        .flat_map(Path::ancestors)
        .any(|d| {
            d.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o002 != 0)
        })
}

/// The sandbox dir and lead name a lead's settings file `file` names.
fn lead_of_settings(file: &Path) -> Option<(PathBuf, String)> {
    let name = file.file_name()?.to_str()?.strip_suffix(".json")?;
    let at = file.parent()?;
    let is_settings = at.file_name()? == "sandbox"
        && ![".lead", ".mcp", ".member"]
            .iter()
            .any(|suffix| name.ends_with(suffix));
    is_settings.then(|| (at.to_path_buf(), name.to_string()))
}

/// Where a member's git would run, and how refusals name it.
struct Placement<'a> {
    subject: String,
    name: &'a str,
    /// The lead whose own worktree this may be: a lead's own ledger entry.
    own_lead: Option<&'a str>,
    herd: Option<&'a str>,
    repo: &'a Path,
    worktree: Option<&'a Path>,
    branch: Option<&'a str>,
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

    pub fn state_dir(&self) -> &Path {
        self.paths.state_dir()
    }

    /// Removes a retired lead's MCP config, and its settings once `ended`
    /// says its Claude has, so a running Claude keeps its sandbox; a
    /// sandboxed worker's settings, MCP config and kept record, once ended. A lead caller
    /// removes no lead's files: it could have forged the entry. Returns a
    /// line per file removed or failed.
    pub fn forget_launch(&self, name: &str, ended: bool) -> Vec<String> {
        // A lead can't write the config dir, so a member record there is no forgery of its.
        let member = crate::worker_sandbox::member_file(&self.paths, name);
        if member.is_file() {
            let files = [
                settings_file(&self.paths, name),
                mcp_file(&self.paths, name),
                member,
            ];
            return match ended {
                true => removed(files),
                false => vec![],
            };
        }
        if self.caller.is_some() {
            return vec![];
        }
        // A name like `lead-x.lead` would otherwise map to lead-x's files.
        if !lead_file(&self.paths, name).is_file() {
            return vec![];
        }
        let mcp = mcp_file(&self.paths, name);
        let settings = ended.then(|| settings_file(&self.paths, name));
        removed(std::iter::once(mcp).chain(settings))
    }

    /// Why the lead running this loopherd, if one does, may not act on
    /// `record` at all: it isn't a member of the lead's own herd.
    pub fn caller_refusal(&self, record: &AgentRecord) -> Result<Option<String>> {
        self.herd_refusal(&record.name, record.herd.as_deref())
    }

    fn herd_refusal(&self, name: &str, herd: Option<&str>) -> Result<Option<String>> {
        let Some(caller) = &self.caller else {
            return Ok(None);
        };
        let own = Lead::kept(&self.paths, caller)?.map(|lead| lead.herd);
        Ok((own.is_none() || herd != own.as_deref()).then(|| {
            format!("{name} isn't a member of {caller}'s herd, and {caller} runs this loopherd")
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
        self.placement_refusal(&Placement {
            subject: format!("{}'s ledger entry", record.name),
            name: &record.name,
            own_lead: Some(&record.name),
            herd: record.herd.as_deref(),
            repo: &record.repo,
            worktree: record.worktree.as_deref(),
            branch: record.branch.as_deref(),
        })
    }

    /// [`Guard::path_refusal`] for the member `req` would spawn, at the
    /// worktree it would get, which may be an existing one for its branch.
    /// A lead may reuse an existing worktree only when an entry of its herd
    /// in `ledger` records it.
    pub fn spawn_refusal(&self, req: &SpawnRequest, ledger: &dyn Ledger) -> Result<Option<String>> {
        let placed = |worktree| Placement {
            subject: format!("adding {}", req.name),
            name: &req.name,
            own_lead: None,
            herd: req.member_of.as_ref().map(|m| m.herd.as_str()),
            repo: &req.repo,
            worktree,
            branch: req.branch(),
        };
        // Finding an existing worktree runs git, so the repo is checked first.
        if let Some(why) = self.placement_refusal(&placed(None))? {
            return Ok(Some(why));
        }
        let worktree = req.planned_worktree()?;
        if let Some(why) = self.placement_refusal(&placed(worktree.as_deref()))? {
            return Ok(Some(why));
        }
        let reused = worktree.filter(|w| self.for_lead() && w.exists());
        let (Some(worktree), Some(herd)) = (reused, &req.member_of) else {
            return Ok(None);
        };
        let entries = ledger.entries()?;
        let mut at_worktree = entries.values().filter(|entry| {
            entry
                .worktree
                .as_deref()
                .is_some_and(|w| same_dir(w, &worktree))
        });
        let own = |entry: &&AgentRecord| entry.herd.as_deref() == Some(herd.herd.as_str());
        if let Some(holder) = at_worktree.clone().find(|e| !e.retired && !own(e)) {
            let of = holder
                .herd
                .as_deref()
                .map_or("no herd's".into(), |h| format!("herd {h}'s"));
            return Ok(Some(format!(
                "adding {} would reuse worktree {}, which {of} {} holds",
                req.name,
                worktree.display(),
                holder.name
            )));
        }
        Ok((!at_worktree.any(|e| own(&e))).then(|| {
            format!(
                "adding {} would reuse worktree {}, which no ledger entry of herd {} records",
                req.name,
                worktree.display(),
                herd.herd
            )
        }))
    }

    fn placement_refusal(&self, placed: &Placement) -> Result<Option<String>> {
        let Placement {
            subject,
            name,
            own_lead,
            herd,
            repo: member_repo,
            worktree,
            branch,
        } = placed;
        if let Some(why) = self.herd_refusal(name, *herd)? {
            return Ok(Some(why));
        }
        let leads = lead_worktrees(&self.paths);
        let writable = leads
            .iter()
            .map(|(_, worktree)| worktree.as_path())
            .chain([self.paths.state_dir()]);
        let inside = |path: &Path, dir: &Path| {
            let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            real(path).starts_with(real(dir))
        };
        if let Some(dir) = writable.into_iter().find(|dir| inside(member_repo, dir)) {
            return Ok(Some(format!(
                "{subject} names repo {}, inside {}, which a lead can write",
                member_repo.display(),
                dir.display()
            )));
        }
        let branch_path = branch
            .filter(|b| !b.is_empty())
            .map(|b| worktree_path(member_repo, b));
        for path in worktree.iter().copied().chain(branch_path.as_deref()) {
            if let Some((lead, _)) = leads.iter().find(|(lead, worktree)| {
                Some(lead.as_str()) != *own_lead && same_dir(path, worktree)
            }) {
                return Ok(Some(format!(
                    "{subject} names lead {lead}'s worktree {}",
                    path.display()
                )));
            }
        }
        let Some(herd) = herd else {
            return Ok(None);
        };
        let repo = match herd::repo(&self.paths, herd) {
            Err(err) if !self.for_lead() && is_not_found(&err) => return Ok(None),
            found => found.with_context(|| {
                format!("herd {herd}'s file names no repo to check {subject} against")
            })?,
        };
        if !same_dir(member_repo, &repo) {
            return Ok(Some(format!(
                "{subject} names repo {}, not herd {herd}'s {}",
                member_repo.display(),
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
        if let Some(worktree) = worktree.filter(|w| !in_place(w)) {
            return Ok(Some(format!(
                "{subject} names worktree {}, which isn't beside herd {herd}'s \
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

/// Removes each of `files`, a line per file removed or failed.
fn removed(files: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    files
        .into_iter()
        .filter_map(|file| match std::fs::remove_file(&file) {
            Ok(()) => Some(format!("removed {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!("warning: kept {}: {e}", file.display())),
        })
        .collect()
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
/// `lead`'s args are fresh from its herd file and the command line.
pub fn prepare(paths: &Paths, name: &str, lead: &Lead, probe: Probe) -> Result<Launch> {
    start(
        paths,
        name,
        lead,
        &probe,
        ArgsFrom::Fresh,
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`prepare`] for a lead resuming with the args it was started with.
/// `probe` is asked at most once, and only for a lead that gets that far.
pub fn prepare_kept(
    paths: &Paths,
    name: &str,
    lead: &Lead,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
) -> Result<Launch> {
    start(
        paths,
        name,
        lead,
        probe,
        ArgsFrom::Kept,
        std::env::var_os("PATH").as_deref(),
    )
}

/// Where the args a lead is judged by come from.
#[derive(Clone, Copy)]
enum ArgsFrom {
    /// Its herd file's, then the command line's, as `lead` starts it.
    Fresh,
    /// Kept from when it was started, as `revive` and `restart` find it.
    Kept,
}

/// Why `args` pass each flag loopherd's launch of lead `name` takes, refused
/// under every mode. `herd_args` is herd and args where the lead takes its
/// herd's shared args, which its workers take too.
pub fn launch_flag_refusals(
    name: &str,
    args: &[String],
    herd_args: Option<(&str, &[String])>,
) -> Vec<String> {
    let drop = |flag: &str| match herd_args {
        Some((herd, shared)) if launch_flags_in(shared).contains(&flag) => {
            let command_line = &args[shared.len().min(args.len())..];
            match launch_flags_in(command_line).contains(&flag) {
                true => format!(
                    "{}, and drop it from the command line",
                    herd_args_fix(herd, "it")
                ),
                false => herd_args_fix(herd, "it"),
            }
        }
        _ => "drop it from [lead].args, the herd's args and the command line".into(),
    };
    launch_flags_in(args)
        .into_iter()
        .map(|flag| launch_flag_refusal(name, flag, &drop(flag), "[lead]"))
        .collect()
}

/// Which of loopherd's own launch flags `args` pass.
pub fn launch_flags_in(args: &[String]) -> Vec<&'static str> {
    ["--settings", "--mcp-config", "--strict-mcp-config"]
        .into_iter()
        .filter(|flag| {
            args.iter()
                .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
        })
        .collect()
}

/// Why member `name` can't pass `flag`, with how to `drop` it; `table` is
/// the herd file table whose `mcp_servers` names its servers.
pub fn launch_flag_refusal(name: &str, flag: &str, drop: &str, table: &str) -> String {
    match flag {
        "--settings" => format!(
            "{name}'s args pass --settings, which its sandbox's settings file takes; {drop}, \
             and put those settings in ~/.claude/settings.json"
        ),
        _ => format!(
            "{name}'s args pass {flag}, which its launch takes; {drop}, and name the servers \
             it may use in {table} mcp_servers"
        ),
    }
}

/// The warnings `lead`'s fresh args start it with, failing where they'd
/// refuse its start, as `lead --dry-run` checks them.
pub fn fresh_arg_warnings(paths: &Paths, name: &str, lead: &Lead) -> Result<Vec<String>> {
    arg_warnings(paths, name, lead, ArgsFrom::Fresh)
}

fn arg_warnings(paths: &Paths, name: &str, lead: &Lead, from: ArgsFrom) -> Result<Vec<String>> {
    let configured = match from {
        ArgsFrom::Fresh => Some(herd::lead_args(paths, &lead.herd)?),
        ArgsFrom::Kept => None,
    };
    let herd_args = match &configured {
        Some((args, true)) => Some((lead.herd.as_str(), args.as_slice())),
        _ => None,
    };
    if let Some(why) = launch_flag_refusals(name, &lead.args, herd_args)
        .into_iter()
        .next()
    {
        bail!(why);
    }
    let configured = configured
        .as_ref()
        .map(|(args, shared)| (args.as_slice(), *shared));
    let disallowed = disallowed_args(&lead.herd, &lead.args, configured);
    if let Some(refusal) = disallowed_refusal(paths, name, &lead.herd, &disallowed, from)? {
        return Err(refusal);
    }
    Ok(disallowed
        .iter()
        .map(|arg| disallowed_warning(name, &arg.line))
        .collect())
}

fn start(
    paths: &Paths,
    name: &str,
    lead: &Lead,
    probe: &dyn Fn() -> Result<Sockets, Unavailable>,
    from: ArgsFrom,
    path: Option<&std::ffi::OsStr>,
) -> Result<Launch> {
    let arg_warnings = arg_warnings(paths, name, lead, from)?;
    let (_, mode) = herd::lead_sandbox(paths, &lead.herd)?;
    let dir = paths.config_dir().join("sandbox");
    let mcp = mcp_file(paths, name);
    let mcp_text = serde_json::to_string_pretty(&mcp_config(paths, name, &lead.herd)?)? + "\n";
    let keep_launch = || -> Result<()> {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let worktree = worktree_path(&herd::repo(paths, &lead.herd)?, name);
        let kept = Lead {
            path: recorded_path(path, &[worktree.as_path(), paths.state_dir()]),
            ..lead.clone()
        };
        let file = lead_file(paths, name);
        std::fs::write(&file, serde_json::to_string_pretty(&kept)? + "\n")
            .with_context(|| format!("write {}", file.display()))?;
        write_private(&mcp, &mcp_text).with_context(|| format!("write {}", mcp.display()))
    };
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
            SandboxMode::Prefer => keep_launch().map(|()| Launch::HookOnly {
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
    if let Some(entry) = plantable_path_entry(path, &writable) {
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
    keep_launch()?;
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
    user_scope_mcp_config(paths, &wanted)?.map_err(|server| {
        anyhow::anyhow!(unknown_mcp_server(
            paths, name, herd, "[lead]", &server, "a lead"
        ))
    })
}

/// An `--mcp-config` holding only the `wanted` servers, from the human's
/// user scope; `Err` names the first one it doesn't configure.
pub fn user_scope_mcp_config(
    paths: &Paths,
    wanted: &[String],
) -> Result<std::result::Result<Value, String>> {
    let mut servers = serde_json::Map::new();
    if wanted.is_empty() {
        return Ok(Ok(json!({"mcpServers": servers})));
    }
    let configured = user_mcp_servers(paths)?;
    for server in wanted {
        let Some(found) = configured.get(server) else {
            return Ok(Err(server.clone()));
        };
        servers.insert(server.clone(), found.clone());
    }
    Ok(Ok(json!({"mcpServers": servers})))
}

/// Why member `name` can't start with `server`, which herd `herd`'s `table`
/// names and the human's user scope doesn't configure.
pub fn unknown_mcp_server(
    paths: &Paths,
    name: &str,
    herd: &str,
    table: &str,
    server: &str,
    whom: &str,
) -> String {
    format!(
        "{name} can't start: herd {herd}'s {table} mcp_servers names {server:?}, which {} \
         doesn't configure in your user scope; add it with claude mcp add --scope user, or \
         drop it from mcp_servers (local and project scopes, claude.ai connectors and \
         plugins' servers can't be given to {whom})",
        user_mcp_file(paths).display()
    )
}

/// The `--mcp-config` file a sandboxed member's Claude is started with.
pub fn mcp_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .config_dir()
        .join("sandbox")
        .join(format!("{name}.mcp.json"))
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

pub fn loosened_warning(name: &str, line: &str) -> String {
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
    disallowed: &[Disallowed],
    from: ArgsFrom,
) -> Result<Option<anyhow::Error>> {
    if disallowed.is_empty() {
        return Ok(None);
    }
    let from_source = |source| disallowed.iter().any(|arg| arg.source == source);
    let fix = match from {
        ArgsFrom::Fresh if from_source(Source::HerdArgs) => {
            match from_source(Source::CommandLine) {
                true => format!(
                    "{}, and drop those from the command line",
                    herd_args_fix(herd, "those from its args")
                ),
                false => herd_args_fix(herd, "those"),
            }
        }
        ArgsFrom::Fresh => "drop those".to_string(),
        ArgsFrom::Kept => {
            format!("retire it and start it again with loopherd lead {herd}, without those")
        }
    };
    let why = format!(
        "its args may loosen its sandbox or permissions: {}; a lead takes only {ALLOWED_ARGS}, \
         so {fix}",
        disallowed
            .iter()
            .map(|arg| arg.line.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    );
    refusal_by_mode(paths, name, herd, &why)
}

/// How to keep `args` from a lead whose herd's args also start its workers.
pub fn herd_args_fix(herd: &str, args: &str) -> String {
    format!(
        "give herd {herd} a [lead].args without {args}, since its workers take the herd's args too"
    )
}

pub fn disallowed_warning(name: &str, line: &str) -> String {
    format!("{name} starts with an arg that may loosen its sandbox or permissions: {line}")
}

pub const ALLOWED_ARGS: &str =
    "--model, --effort, --verbose and --permission-mode default, manual, auto or plan";

/// An arg outside the allowlist, shown with its values and where it was set.
struct Disallowed {
    line: String,
    source: Source,
}

/// Where a lead's arg was set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The herd's `args`, which its workers take too.
    HerdArgs,
    LeadArgs,
    CommandLine,
    /// Kept from its start, the source not recorded.
    Kept,
}

/// Each of a lead's `args`, without loopherd's own launch flags, that the
/// allowlist doesn't cover. `configured` is what `args` start with from the
/// herd file and whether those are the herd's shared args; `None` means
/// they were kept from its start. D100 says why it's an allowlist.
fn disallowed_args(
    herd: &str,
    args: &[String],
    configured: Option<(&[String], bool)>,
) -> Vec<Disallowed> {
    let sources = match configured {
        None => vec![(
            args,
            "the args it was started with".to_string(),
            Source::Kept,
        )],
        Some((configured, shared)) => {
            let rest = &args[configured.len().min(args.len())..];
            let (shown, source) = match shared {
                true => (format!("herd {herd}'s args"), Source::HerdArgs),
                false => ("[lead].args".into(), Source::LeadArgs),
            };
            vec![
                (&args[..args.len() - rest.len()], shown, source),
                (rest, "the command line".into(), Source::CommandLine),
            ]
        }
    };
    sources
        .into_iter()
        .flat_map(|(args, shown, source)| {
            outside_allowlist(args)
                .into_iter()
                .map(move |arg| Disallowed {
                    line: format!("{arg}, from {shown}"),
                    source,
                })
        })
        .collect()
}

/// What herd `herd`'s file passes its lead outside the allowlist, one line
/// each, but for loopherd's own launch flags, which [`launch_flag_refusals`] names.
pub fn configured_disallowed_args(paths: &Paths, herd: &str) -> Result<Vec<String>> {
    let (configured, shared) = herd::lead_args(paths, herd)?;
    let configured = without_launch_flags(&configured);
    let disallowed = disallowed_args(herd, &configured, Some((&configured, shared)));
    Ok(disallowed.into_iter().map(|arg| arg.line).collect())
}

/// Each arg the allowlist doesn't cover, with the values that follow a flag.
pub fn outside_allowlist(args: &[String]) -> Vec<String> {
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
        "--permission-mode" => ["default", "manual", "auto", "plan"].contains(&value),
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

/// Keys that loosen a sandbox when set to anything but `false` or an empty list.
const LOOSENING_SWITCHES: [(&str, &str); 10] = [
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
    (
        "/sandbox/network/allowUnixSockets",
        "reopens those socket paths on macOS",
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
pub fn loosenings(home: &Path, worktree: &Path, own: &Value) -> Vec<String> {
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
    // otherwise only a carve at or beneath a denied path, or a glob that could match
    // one, counts: over an ancestor Claude keeps the narrower deny. A relative entry
    // resolves against ~/.claude.
    let credentials = !own["sandbox"]["filesystem"]["allowRead"].is_null();
    let own_reads = entries(&own["sandbox"]["filesystem"]["allowRead"]);
    let denied: Vec<PathBuf> = entries(&own["sandbox"]["filesystem"]["denyRead"])
        .iter()
        .filter_map(|path| absolute(path, home))
        .collect();
    for path in entries(&user["sandbox"]["filesystem"]["allowRead"]) {
        let relative = !path.starts_with('/') && !path.starts_with('~');
        let resolved = if relative {
            absolute(&home.join(".claude").join(&path).to_string_lossy(), home)
        } else {
            absolute(&path, home)
        };
        let opens = match resolved {
            Some(resolved) => {
                let glob = |part: &str| part.contains(['*', '?', '[']);
                let globbed = resolved.iter().any(|part| glob(&part.to_string_lossy()));
                let literal: PathBuf = resolved
                    .iter()
                    .take_while(|part| !glob(&part.to_string_lossy()))
                    .collect();
                denied
                    .iter()
                    .any(|deny| literal.starts_with(deny) || globbed && deny.starts_with(&literal))
            }
            // A `..` or a `~user` could land anywhere.
            None => true,
        };
        if !own_reads.contains(&path) && (credentials || opens) {
            found.push(format!(
                "{shown}: sandbox.filesystem.allowRead opens {path} to reads"
            ));
        }
    }
    for (pointer, what) in LOOSENING_SWITCHES {
        let set = user.pointer(pointer).is_some_and(|v| {
            v != &Value::Bool(false) && v.as_array().is_none_or(|v| !v.is_empty())
        });
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
/// any segment is `..`, even past a wildcard.
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
    if rest.split('/').any(|part| part == "..") {
        return None;
    }
    let literal: PathBuf = rest
        .split('/')
        .take_while(|part| !part.contains(['*', '?', '[']))
        .collect();
    Some(base.join(literal))
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
pub fn write_private(file: &Path, text: &str) -> std::io::Result<()> {
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
/// launched without them, loosened, with other args, or with Unix sockets
/// open, as `sockets_unblocked` records, is refused.
pub fn check_running(
    paths: &Paths,
    name: &str,
    herd: &str,
    claude_args: &[String],
    sockets_unblocked: Option<&str>,
) -> Result<Running> {
    let (_, mode) = herd::lead_sandbox(paths, herd)?;
    let file = settings_file(paths, name);
    let resume = format!("quit its Claude and run loopherd revive --name {name}");
    let args = match Lead::kept(paths, name)? {
        Some(kept) => without_launch_flags(&kept.args),
        None => without_launch_flags(claude_args),
    };
    let disallowed = disallowed_args(herd, &args, None);
    if let Some(refusal) = disallowed_refusal(paths, name, herd, &disallowed, ArgsFrom::Kept)? {
        return Err(refusal);
    }
    let mut warnings: Vec<String> = disallowed
        .iter()
        .map(|arg| disallowed_warning(name, &arg.line))
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
    if let Some(why) = sockets_unblocked
        && !sockets_open(paths, herd)?
    {
        let started =
            format!("it was started with a sandbox that can't block Unix sockets ({why})");
        let revive = format!("{resume} to resume it with them blocked");
        match when_loosened(paths, herd)? {
            WhenLoosened::Warn => warnings.push(format!(
                "{}; fix the sandbox check's cause, then {revive}",
                sockets_unblocked_warning(name, why)
            )),
            WhenLoosened::Refuse => bail!(
                "{name} can't run sandboxed: {started}; {revive}, or set [lead] sandbox = \
                 \"prefer\" in herd {herd} to run it with them open"
            ),
            WhenLoosened::RefuseForCredentials => {
                return Err(credentials_refusal(
                    name,
                    &format!(
                        "{started}, so it could use your ssh agent; {revive}, or set [lead] \
                         merge_guard = \"hook\" in herd {herd}"
                    ),
                ));
            }
        }
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

/// What in the state dir is the human's or the shepherd's, denied by name
/// to the file tools, which the sandbox doesn't cover. `true` marks a
/// directory. Every mailbox and watch file is in `reports/`, which the
/// lead's report and watch write from outside its sandbox.
const KEPT_FROM_FILE_TOOLS: [(&str, bool); 6] = [
    ("reports", true),
    ("paused", true),
    ("shepherd.json", false),
    ("shepherd.json.lock", false),
    ("ledger.json", false),
    ("ledger.lock", false),
];

/// Makes what lead `name`'s sandbox names in the state dir. An `allowWrite`
/// entry for a missing path has no effect on Linux, and a deny on a missing
/// path gets a placeholder that would sit in the way of the first real write.
pub fn create_state(paths: &Paths, name: &str) -> Result<()> {
    let state = paths.state_dir();
    let requests = crate::broker::requests_dir(paths, name);
    // Every herd's lead and the shepherd, so one started later sits in a folder a literal deny names.
    let leads: Vec<String> = herd::names(&paths.herds)?
        .into_iter()
        .filter_map(|herd| crate::lead::lead_name(&herd).ok())
        .collect();
    let broker = state.join("broker");
    let handoffs = leads
        .iter()
        .map(String::as_str)
        .chain([name, crate::context::SHEPHERD])
        .map(|agent| handoff::dir(state, agent));
    for dir in [state.to_path_buf(), paths.locks_dir()]
        .into_iter()
        .chain(handoffs)
    {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let own = [broker.clone(), broker.join(name), requests.clone()];
    for dir in &own {
        create_private_dir(dir).with_context(|| format!("create {}", requests.display()))?;
    }
    for dir in leads.iter().map(|lead| broker.join(lead)) {
        create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    Ok(())
}

/// Makes `dir` with mode 0700, as the broker makes its own, and tightens it
/// to 0700 if it is already our directory: older installs made these 0755.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists && dir.is_dir() => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    if meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } {
        // The umask can strip owner bits from a new folder too.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Lead `name`'s settings, from its herd file alone: its worktree is where
/// `lead` puts it in the herd's repo, whatever the ledger says.
fn settings(paths: &Paths, name: &str, herd: &str) -> Result<Value> {
    settings_on(paths, name, herd, &THIS_MACHINE)
}

/// The machine a credentials member is confined on. Unit tests run under a
/// fake home, so the runner's real XDG dirs and git config stay out.
pub const THIS_MACHINE: crate::credentials::Machine<'static> = crate::credentials::Machine {
    env: &|name| (!cfg!(test)).then(|| std::env::var_os(name)).flatten(),
    mounts: &|| std::fs::read("/proc/mounts"),
    credential_helpers: &|home| match cfg!(test) {
        true => String::new(),
        false => crate::credentials::git_credential_helpers(home),
    },
};

fn settings_on(
    paths: &Paths,
    name: &str,
    herd: &str,
    machine: &crate::credentials::Machine,
) -> Result<Value> {
    let (switches, _) = herd::lead_sandbox(paths, herd)?;
    let repo = herd::repo(paths, herd)?;
    let config = paths.config_dir();
    let claude = paths.home.join(".claude");
    let worktree = worktree_path(&repo, name);
    let state = paths.state_dir();
    // Only its own: a broker takes every request in its folder as its lead's.
    let mut allow_write = vec![
        worktree,
        handoff::dir(state, name),
        crate::broker::requests_dir(paths, name),
    ];
    if switches.install {
        allow_write.push(paths.home.join(".local/bin"));
    }
    let deny_write = [config.to_path_buf(), claude.clone()];
    let mut deny = vec![edit_rule(config), edit_rule(&claude)];
    deny.extend(KEPT_FROM_FILE_TOOLS.map(|(path, dir)| match dir {
        true => edit_rule(&state.join(path)),
        false => file_rule(&state.join(path)),
    }));
    if cfg!(target_os = "linux") {
        // The Linux sandbox skips glob denies, so these reach only the file tools.
        deny.push(file_rule(&state.join("broker/*")));
        deny.push(file_rule(&state.join("handoffs/*")));
    } else {
        // The shepherd's resume still reads this, which may not exist to be listed below.
        deny.push(file_rule(&handoff::flat(state, crate::context::SHEPHERD)));
    }
    // On Linux the globs cover the file tools, and a named path that later goes
    // missing would get the sandbox's empty placeholder, which may read as a handoff.
    let others = match cfg!(target_os = "linux") {
        true => vec![],
        false => [
            others_in_broker(paths, name)?,
            others_in_handoffs(paths, name)?,
        ]
        .concat(),
    };
    deny.extend(others.into_iter().map(|(path, dir)| match dir {
        true => edit_rule(&path),
        false => file_rule(&path),
    }));
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
            "loopherd update",
        ]);
    }
    sandbox["excludedCommands"] = json!(excluded);
    let mut allow = vec!["WebFetch(domain:*)".to_string()];
    allow.extend(excluded.iter().map(|command| format!("Bash({command})")));
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
        if sockets_open(paths, herd)? {
            return Err(credentials_refusal(
                name,
                &herd::credentials_with_sockets(herd, "lead"),
            ));
        }
        let workers = herd::worker(paths, herd)?.sandbox;
        if workers != WorkerSandbox::Require {
            return Err(credentials_refusal(
                name,
                &herd::credentials_with_workers_unrequired(herd, workers),
            ));
        }
    }
    let mut settings = json!({
        "sandbox": sandbox,
        "permissions": {
            "denyInsteadOfPrompting": true,
            "allow": allow,
            "deny": deny,
        },
    });
    if herd::merge_guard(paths, herd)? == Some(MergeGuard::Credentials) {
        // Its broker's lock, stop file and replies, which its own loopherd reads.
        confine_credentials(
            paths,
            name,
            herd,
            &mut settings,
            [crate::broker::dir(paths)],
            machine,
        )?;
    }
    Ok(settings)
}

/// Gives credentials member `name`'s `settings` the environment and read denies
/// of [`crate::credentials::confined`], ahead of the reads it already denies,
/// and opens `readable` back to its sandbox.
pub fn confine_credentials(
    paths: &Paths,
    name: &str,
    herd: &str,
    settings: &mut Value,
    readable: impl IntoIterator<Item = PathBuf>,
    machine: &crate::credentials::Machine,
) -> Result<()> {
    let confined = crate::credentials::confined(paths, herd, machine)
        .map_err(|e| credentials_refusal(name, &format!("{e:#}")))?;
    let filesystem = &mut settings["sandbox"]["filesystem"];
    // The file tools run outside the sandbox, so each path is denied to both.
    let mut deny_read: Vec<Value> = confined
        .deny_read
        .iter()
        .map(|(path, _)| json!(path.display().to_string()))
        .collect();
    deny_read.extend(
        filesystem["denyRead"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let mut allow_read = confined.allow_read;
    allow_read.extend(readable);
    filesystem["denyRead"] = json!(deny_read);
    filesystem["allowRead"] = json!(allow_read);
    let deny = settings["permissions"]["deny"]
        .as_array_mut()
        .context("settings without permissions.deny")?;
    deny.extend(confined.deny_read.iter().map(|(path, is_dir)| {
        let under = if *is_dir { "/**" } else { "" };
        json!(format!("Read(/{}{under})", path.display()))
    }));
    settings["env"] = Value::Object(confined.env);
    Ok(())
}

/// What in `<state>/broker/` isn't lead `name`'s `requests/`, each named, as
/// the file tools' denies. An `Edit` deny is also a sandbox write deny, which
/// beats `allowWrite`, so one on all of `broker/` would close `requests/`.
/// Not on Linux, where a glob deny covers the rest.
fn others_in_broker(paths: &Paths, name: &str) -> Result<Vec<(PathBuf, bool)>> {
    let broker = paths.state_dir().join("broker");
    let mut found = entries_but(&broker, name)?;
    found.extend(entries_but(&broker.join(name), "requests")?);
    found.sort();
    Ok(found)
}

/// What in `<state>/handoffs/` isn't lead `name`'s own directory, each
/// named, as [`others_in_broker`] does for `broker/`.
fn others_in_handoffs(paths: &Paths, name: &str) -> Result<Vec<(PathBuf, bool)>> {
    entries_but(&paths.state_dir().join("handoffs"), name)
}

/// The entries of `dir` but `skip`, sorted, each marked `true` when a directory.
pub fn entries_but(dir: &Path, skip: &str) -> Result<Vec<(PathBuf, bool)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(vec![]);
        }
        Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
    };
    let mut found = vec![];
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        if entry.file_name() != skip {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            found.push((entry.path(), is_dir));
        }
    }
    found.sort();
    Ok(found)
}

/// `settings` without the denies naming paths in `<state>/broker/` and
/// `<state>/handoffs/` one by one, which change as agents and herds come and
/// go, nor the copies Claude Code leaves beside `.claude.json`, which come as
/// it runs; neither changes with the switches.
fn switch_settings(settings: &Value, paths: &Paths) -> Value {
    let dirs = ["broker", "handoffs"]
        .map(|dir| format!("Edit(/{}/", paths.state_dir().join(dir).display()));
    let named = |rule: &str| {
        dirs.iter().any(|dir| {
            rule.strip_prefix(dir.as_str())
                .is_some_and(|rest| !rest.trim_end_matches("/**)").contains('*'))
        })
    };
    let copy = |path: &str| {
        Path::new(path.trim_end_matches(')'))
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".claude.json."))
    };
    let mut settings = settings.clone();
    if let Some(deny) = settings["permissions"]["deny"].as_array_mut() {
        deny.retain(|rule| !rule.as_str().is_some_and(|rule| named(rule) || copy(rule)));
    }
    if let Some(deny_read) = settings["sandbox"]["filesystem"]["denyRead"].as_array_mut() {
        deny_read.retain(|path| !path.as_str().is_some_and(copy));
    }
    settings
}

/// A deny for the file tools; `//` marks the path absolute, not relative to the settings file.
pub fn edit_rule(dir: &Path) -> String {
    format!("Edit(/{}/**)", dir.display())
}

pub fn file_rule(file: &Path) -> String {
    format!("Edit(/{})", file.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_allowed_permission_mode_passes_in_both_forms_and_no_other_does() {
        let args = |line: &str| line.split(' ').map(String::from).collect::<Vec<_>>();
        for mode in ["default", "manual", "auto", "plan"] {
            let spaced = args(&format!("--permission-mode {mode}"));
            let inline = args(&format!("--permission-mode={mode}"));
            assert_eq!(allowed_len(&spaced), Some(2), "{mode}");
            assert_eq!(allowed_len(&inline), Some(1), "{mode}");
            assert!(outside_allowlist(&spaced).is_empty(), "{mode}");
            assert!(outside_allowlist(&inline).is_empty(), "{mode}");
        }
        for mode in ["acceptEdits", "dontAsk", "bypassPermissions", ""] {
            let spaced = args(&format!("--permission-mode {mode}"));
            let inline = args(&format!("--permission-mode={mode}"));
            assert_eq!(allowed_len(&spaced), None, "{mode}");
            assert_eq!(allowed_len(&inline), None, "{mode}");
        }
    }

    #[test]
    fn a_retire_removes_only_a_launched_leads_files_never_another_leads_by_a_dotted_name() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let dir = paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let files = ["lead-x.json", "lead-x.mcp.json", "lead-x.lead.json"];
        let guard = Guard::new(paths.clone(), None);

        for name in ["lead-x.lead", "lead-x.mcp", "lh-1"] {
            for file in files {
                std::fs::write(dir.join(file), "{}").unwrap();
            }

            assert_eq!(
                guard.forget_launch(name, true),
                Vec::<String>::new(),
                "{name}"
            );
            for file in files {
                assert!(dir.join(file).exists(), "{name} removed {file}");
            }
        }

        let removed = guard.forget_launch("lead-x", true);

        assert_eq!(
            removed,
            [
                format!("removed {}", dir.join("lead-x.mcp.json").display()),
                format!("removed {}", dir.join("lead-x.json").display()),
            ]
        );
        assert!(dir.join("lead-x.lead.json").exists());
    }

    #[test]
    fn a_retire_by_anyone_removes_a_sandboxed_workers_settings_mcp_config_and_record_once_its_claude_has_ended()
     {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let dir = paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let files = ["w1.json", "w1.mcp.json", "w1.member.json"];
        for file in files {
            std::fs::write(dir.join(file), "{}").unwrap();
        }

        let guard = Guard::new(paths.clone(), Some("lead-h".into()));
        assert_eq!(guard.forget_launch("w1", false), Vec::<String>::new());
        for file in files {
            assert!(dir.join(file).exists(), "{file}");
        }

        let removed = guard.forget_launch("w1", true);

        assert_eq!(
            removed,
            files.map(|file| format!("removed {}", dir.join(file).display()))
        );
    }

    #[test]
    fn an_edit_rule_is_confined_only_when_no_segment_climbs_and_it_lies_beneath_an_own_write() {
        let home = Path::new("/home/me");
        let worktree = Path::new("/a/wt");
        let own_writes = [worktree.to_path_buf(), home.join("own")];
        let cases = [
            ("Edit(//a/wt2/**)", false),
            ("Edit(/src/../../**)", false),
            ("Edit(src/*/../../../**)", false),
            ("Edit(src/**/..)", false),
            ("Edit(src/**)", true),
            ("Edit(~/own/**)", true),
        ];
        for (rule, confined) in cases {
            let settings = serde_json::json!({"permissions": {"allow": [rule]}});
            let found = permission_loosenings(
                &settings,
                Path::new("settings.json"),
                worktree,
                worktree,
                home,
                &own_writes,
            );
            assert_eq!(found.is_empty(), confined, "{rule}: {found:?}");
        }
    }

    #[test]
    fn an_allow_read_counts_when_it_or_a_glob_it_holds_could_match_a_denied_path() {
        let home = Path::new("/home/me");
        let own = serde_json::json!({"sandbox": {"filesystem": {"denyRead": [
            "/home/me/.local/state/loopherd/locks",
            "/home/me/.claude/secret",
        ]}}});
        let cases = [
            ("~/.local/state/loopherd/*", true),
            ("~/.local/state/loopherd/lock?", true),
            ("~/.local/state/loopherd/[l]ocks", true),
            ("~/.local/state/*/locks", true),
            ("~/.local/state/loopherd/locks/*.lock", true),
            ("~/.local/state/loopherd/other/*", false),
            ("/srv/*", false),
            // Relative entries resolve against ~/.claude, not the worktree.
            ("secret", true),
            ("notes", false),
        ];
        for (entry, counts) in cases {
            let user = serde_json::json!({"sandbox": {"filesystem": {"allowRead": [entry]}}});
            let found = sandbox_loosenings(&user, Path::new("settings.json"), home, &own, &[]);
            assert_eq!(!found.is_empty(), counts, "{entry}: {found:?}");
        }
    }

    #[test]
    fn a_workers_settings_dont_make_it_a_calling_lead() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let dir = paths.config_dir().join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w1.member.json"), "{}").unwrap();
        let settings = settings_file(&paths, "w1").display().to_string();
        let mut procs = crate::process::FakeProcs::pane();
        procs.set_cmdline(30, &["claude", "--settings", &settings]);
        procs.add(40, "/usr/bin/loopherd", 30, 400);

        assert_eq!(calling_lead(&paths, &procs, 40).unwrap(), None);
    }

    #[test]
    fn an_install_leads_own_settings_confine_an_edit_rule_beneath_local_bin() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: root.path().join("home"),
            ..Paths::under(root.path())
        };
        std::fs::create_dir_all(&paths.herds).unwrap();
        std::fs::write(
            paths.herds.join("h.toml"),
            "repo = \"/src/repo\"\nworkspace = \"w1\"\n[lead]\nprompt = \"go\"\ninstall = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(paths.home.join(".claude")).unwrap();
        std::fs::write(
            paths.home.join(".claude/settings.json"),
            r#"{"permissions": {"allow": ["Edit(~/.local/bin/**)"]}}"#,
        )
        .unwrap();

        assert_eq!(
            loosened(&paths, "lead-h", "h").unwrap(),
            Vec::<String>::new()
        );
    }

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
    fn the_kept_path_drops_every_entry_a_lead_could_plant_in() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        std::fs::create_dir_all(&paths.herds).unwrap();
        let repo = root.path().join("repo");
        std::fs::write(
            paths.herds.join("h.toml"),
            format!(
                "repo = {repo:?}\nworkspace = \"w1\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n"
            ),
        )
        .unwrap();
        let lead = Lead {
            herd: "h".into(),
            args: vec![],
            path: Some("/planted".into()),
        };
        let worktree = worktree_path(&repo, "lead-h");
        let temp = std::env::temp_dir().join("x");
        let given = [
            "/usr/bin",
            "",
            "bin",
            "/dev/shm/bin",
            temp.to_str().unwrap(),
            worktree.join("bin").to_str().unwrap(),
            paths.state_dir().join("bin").to_str().unwrap(),
            "/bin",
        ]
        .join(":");

        // `prefer` with no sandbox keeps the record before refusing any entry.
        start(
            &paths,
            "lead-h",
            &lead,
            &|| {
                Err(Unavailable {
                    why: "none".into(),
                    fix: "none".into(),
                })
            },
            ArgsFrom::Fresh,
            Some(given.as_ref()),
        )
        .unwrap();

        let kept = Lead::kept(&paths, "lead-h").unwrap().unwrap();
        assert_eq!(kept.path.as_deref(), Some("/usr/bin:/bin"));
    }

    #[test]
    fn a_plantable_path_refuses_the_start_before_any_launch_file_is_written() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        std::fs::create_dir_all(&paths.herds).unwrap();
        let repo = root.path().join("repo");
        std::fs::write(
            paths.herds.join("h.toml"),
            format!("repo = {repo:?}\nworkspace = \"w1\"\n[lead]\nprompt = \"go\"\n"),
        )
        .unwrap();
        let lead = Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };

        let err = start(
            &paths,
            "lead-h",
            &lead,
            &|| Ok(Sockets::Blocked),
            ArgsFrom::Fresh,
            Some("/usr/bin::/bin".as_ref()),
        )
        .err()
        .unwrap();

        assert!(
            format!("{err:#}").contains("PATH has an empty entry"),
            "{err:#}"
        );
        for file in ["lead-h.lead.json", "lead-h.mcp.json", "lead-h.json"] {
            let file = paths.config_dir().join("sandbox").join(file);
            assert!(!file.exists(), "{} was written", file.display());
        }
    }

    #[test]
    fn a_credentials_refusal_from_its_settings_leaves_no_launch_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        std::fs::create_dir_all(&paths.herds).unwrap();
        let repo = root.path().join("repo");
        std::fs::write(
            paths.herds.join("h.toml"),
            format!(
                "repo = {repo:?}\nworkspace = \"w1\"\n[lead]\nprompt = \"go\"\n\
                 merge_guard = \"credentials\"\ninstall = true\n"
            ),
        )
        .unwrap();
        let lead = Lead {
            herd: "h".into(),
            args: vec![],
            path: None,
        };

        let err = start(
            &paths,
            "lead-h",
            &lead,
            &|| Ok(Sockets::Blocked),
            ArgsFrom::Fresh,
            Some("/usr/bin:/bin".as_ref()),
        )
        .err()
        .unwrap();

        assert!(format!("{err:#}").contains("install = true"), "{err:#}");
        for file in ["lead-h.lead.json", "lead-h.mcp.json", "lead-h.json"] {
            let file = paths.config_dir().join("sandbox").join(file);
            assert!(!file.exists(), "{} was written", file.display());
        }
    }

    /// Lead `lead-h` of credentials herd `h`, with its key and token, its home under `root`.
    fn credentials_lead(root: &Path) -> Paths {
        let mut paths = Paths::under(root);
        paths.home = root.join("home");
        std::fs::create_dir_all(&paths.home).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        crate::worktree::testing::git(&repo, &["init", "--quiet"]).unwrap();
        let url = "https://github.com/acme/widgets.git";
        crate::worktree::testing::git(&repo, &["remote", "add", "origin", url]).unwrap();
        std::fs::create_dir_all(&paths.herds).unwrap();
        std::fs::write(
            paths.herds.join("h.toml"),
            format!(
                "repo = {repo:?}\nworkspace = \"w1\"\n[lead]\nprompt = \"go\"\n\
                 merge_guard = \"credentials\"\n[worker]\nsandbox = \"require\"\n"
            ),
        )
        .unwrap();
        let credentials = paths.config_dir().join("credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        std::fs::write(credentials.join("h.key"), "a private key\n").unwrap();
        std::fs::write(credentials.join("h.token"), "github_pat_1\n").unwrap();
        paths
    }

    #[test]
    fn a_credentials_leads_settings_hide_the_windows_drives_its_machine_mounts() {
        let root = tempfile::tempdir().unwrap();
        let paths = credentials_lead(root.path());
        let machine = crate::credentials::Machine {
            env: &|_| None,
            mounts: &|| Ok(b"Z: /mnt/zz drvfs rw 0 0\n".to_vec()),
            credential_helpers: &|_| String::new(),
        };

        let settings = settings_on(&paths, "lead-h", "h", &machine).unwrap();

        let deny_read = settings["sandbox"]["filesystem"]["denyRead"]
            .as_array()
            .unwrap();
        assert!(deny_read.contains(&json!("/mnt/zz")), "{deny_read:?}");
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(deny.contains(&json!("Read(//mnt/zz/**)")), "{deny:?}");
    }

    #[test]
    fn a_claude_json_copy_made_after_the_settings_isnt_a_switch_change() {
        let root = tempfile::tempdir().unwrap();
        let paths = credentials_lead(root.path());
        std::fs::write(paths.home.join(".claude.json.tmp.1.ab"), "{}").unwrap();
        let machine = crate::credentials::Machine {
            env: &|_| None,
            mounts: &|| Ok(vec![]),
            credential_helpers: &|_| String::new(),
        };
        let built = settings_on(&paths, "lead-h", "h", &machine).unwrap();

        std::fs::write(paths.home.join(".claude.json.tmp.2.cd"), "{}").unwrap();
        let current = settings_on(&paths, "lead-h", "h", &machine).unwrap();

        assert_ne!(built, current, "the new copy is denied");
        assert_eq!(
            switch_settings(&built, &paths),
            switch_settings(&current, &paths)
        );
    }

    #[test]
    fn this_machines_mounts_are_read_from_proc() {
        let read = |m: &crate::credentials::Machine| (m.mounts)().map_err(|e| e.kind());

        assert_eq!(
            read(&THIS_MACHINE),
            std::fs::read("/proc/mounts").map_err(|e| e.kind())
        );
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

    /// A filter `program` whose run under the probe prints `stderr` and exits `code`.
    fn fake_filter(program: &'static str, stderr: &str, code: u8) -> Filter {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            &format!("printf '%s' '{stderr}' >&2; exit {code}"),
            "sh",
        ]);
        Filter { program, command }
    }

    #[test]
    fn a_filter_that_blocks_ends_the_probe_before_the_next_is_looked_up() {
        let blocks = fake_filter(
            "claude",
            "socat[7] E connect(5, AF=1): Operation not permitted\n",
            1,
        );
        let filters = std::iter::once(blocks).chain(std::iter::once_with(|| -> Filter {
            panic!("looked up the next filter after one blocked")
        }));

        assert_eq!(socket_filter(filters), Sockets::Blocked);
    }

    #[test]
    fn a_filter_that_fails_its_setup_is_named_in_the_fix() {
        let failed = fake_filter(
            "apply-seccomp",
            "apply-seccomp: unshare(CLONE_NEWUSER): Operation not permitted\n",
            1,
        );

        let Sockets::Open(open) = socket_filter(std::iter::once(failed)) else {
            panic!("a filter that failed its setup blocked");
        };
        assert_eq!(
            open.fix,
            "let it make user namespaces, as bubblewrap does \
             (kernel.unprivileged_userns_clone, or an AppArmor profile for apply-seccomp)"
        );
    }

    #[test]
    fn a_probe_command_still_running_at_its_deadline_is_killed() {
        let started = std::time::Instant::now();

        let err = output_within(std::process::Command::new("sleep").arg("30"), 1).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "still running after 1s");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_probe_command_whose_descendant_holds_its_output_returns_by_its_deadline() {
        let started = std::time::Instant::now();

        let out = output_within(
            std::process::Command::new("sh").args(["-c", "sleep 30 & echo forked"]),
            1,
        )
        .unwrap();

        assert_eq!(String::from_utf8_lossy(&out.stdout), "forked\n");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
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
    #[test]
    fn a_unix_socket_allow_list_loosens_the_sandbox_unless_it_is_empty() {
        let home = Path::new("/home/me");
        let file = Path::new("settings.json");
        let own = serde_json::json!({});
        let opened = serde_json::json!(
            {"sandbox": {"network": {"allowUnixSockets": ["/var/run/docker.sock"]}}}
        );
        let empty = serde_json::json!({"sandbox": {"network": {"allowUnixSockets": []}}});

        assert_eq!(
            sandbox_loosenings(&opened, file, home, &own, &[]),
            ["settings.json: sandbox.network.allowUnixSockets reopens those socket paths on macOS"]
        );
        assert_eq!(
            sandbox_loosenings(&empty, file, home, &own, &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn any_enabled_wsl_interop_entry_counts_whatever_its_suffix() {
        let binfmt = tempfile::tempdir().unwrap();
        assert!(!wsl_interop_in(binfmt.path()));

        std::fs::write(
            binfmt.path().join("WSLInterop"),
            "disabled\ninterpreter /init\n",
        )
        .unwrap();
        std::fs::write(binfmt.path().join("status"), "enabled\n").unwrap();
        assert!(!wsl_interop_in(binfmt.path()));

        std::fs::write(
            binfmt.path().join("WSLInterop-late"),
            "enabled\ninterpreter /init\n",
        )
        .unwrap();
        assert!(wsl_interop_in(binfmt.path()));
    }
}
