//! The Claude Code sandbox a lead runs in, written from its switches (D83, D89).

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
    /// be where its herd file puts members: its repo the herd's, its worktree
    /// beside the herd's other worktrees. No entry may name another lead's
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
        let repo = herd::repo(&self.paths, herd).with_context(|| {
            format!("herd {herd}'s file names no repo to check {name}'s ledger entry against")
        })?;
        if !same_dir(&record.repo, &repo) {
            return Ok(Some(format!(
                "{name}'s ledger entry names repo {}, not herd {herd}'s {}",
                record.repo.display(),
                repo.display()
            )));
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
        /// What the human's own settings loosen, under `prefer`.
        loosened: Vec<String>,
        mcp: PathBuf,
    },
    /// `prefer`, with why the sandbox can't start.
    HookOnly { why: String, mcp: PathBuf },
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

    /// The warnings the start carries: why it's hook-only, or what loosens its sandbox.
    pub fn warnings(&self, name: &str) -> Vec<String> {
        match self {
            Launch::Sandboxed { loosened, .. } => loosened
                .iter()
                .map(|line| loosened_warning(name, line))
                .collect(),
            Launch::HookOnly { why, .. } => vec![hook_only_warning(name, why)],
        }
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
    if let Err(Unavailable { why, fix }) = probe() {
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
            SandboxMode::Prefer => Ok(Launch::HookOnly { why, mcp }),
            SandboxMode::Require => Err(refusal(
                name,
                &lead.herd,
                &format!("its sandbox can't start: {why}; {fix}"),
            )),
        };
    }
    let own = settings(paths, name, &lead.herd)?;
    let loosened = loosenings(&paths.home, &own);
    if let Some(refusal) = loosened_refusal(paths, name, &lead.herd, &loosened)? {
        return Err(refusal);
    }
    create_state(paths.state_dir())?;
    let file = settings_file(paths, name);
    let text = serde_json::to_string_pretty(&own)? + "\n";
    write_private(&file, &text).with_context(|| format!("write {}", file.display()))?;
    Ok(Launch::Sandboxed {
        settings: file,
        loosened,
        mcp,
    })
}

/// The only MCP servers lead `name` gets, by `--strict-mcp-config`: those its
/// herd's `[lead] mcp_servers` names, as the human's `~/.claude.json`
/// configures them for the lead's worktree (local scope) or for the human
/// (user scope). Never from the worktree's `.mcp.json`, which the lead can write.
fn mcp_config(paths: &Paths, name: &str, herd: &str) -> Result<Value> {
    let wanted = herd::lead_mcp_servers(paths, herd)?;
    let mut servers = serde_json::Map::new();
    if wanted.is_empty() {
        return Ok(json!({"mcpServers": servers}));
    }
    let file = paths.home.join(".claude.json");
    let config: Value = match std::fs::read_to_string(&file) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let worktree = worktree_path(&herd::repo(paths, herd)?, name);
    let local = &config["projects"][worktree.display().to_string()]["mcpServers"];
    for server in wanted {
        let Some(found) = local.get(&server).or(config["mcpServers"].get(&server)) else {
            bail!(
                "{name} can't start: herd {herd}'s [lead] mcp_servers names {server:?}, which \
                 {} doesn't configure for you or for the lead's worktree; add it with claude \
                 mcp add --scope user, or drop it from mcp_servers (claude.ai connectors and \
                 plugins' servers can't be given to a lead)",
                file.display()
            );
        };
        servers.insert(server, found.clone());
    }
    Ok(json!({"mcpServers": servers}))
}

/// What the human's own Claude settings loosen in lead `name`'s sandbox, one
/// line per key, each naming its file.
pub fn loosened(paths: &Paths, name: &str, herd: &str) -> Result<Vec<String>> {
    Ok(loosenings(&paths.home, &settings(paths, name, herd)?))
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

/// Boolean keys that loosen a sandbox when set to anything but `false`.
const LOOSENING_SWITCHES: [(&str, &str); 5] = [
    (
        "/sandbox/filesystem/disabled",
        "turns its filesystem rules off",
    ),
    ("/sandbox/enableWeakerNestedSandbox", "weakens it"),
    ("/sandbox/allowNestedUserNamespaces", "weakens it"),
    ("/sandbox/enableWeakerNetworkIsolation", "weakens it"),
    ("/sandbox/allowAppleEvents", "lets it drive other apps"),
];

/// What the human's `~/.claude/settings.json` adds to a lead's sandbox, whose
/// own settings are `own`: one line per loosening entry, naming the file.
/// D100 says which keys reach a sandbox locked by `--settings`, and why.
fn loosenings(home: &Path, own: &Value) -> Vec<String> {
    let file = home.join(".claude/settings.json");
    let shown = file.display();
    let user: Value = match std::fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(user) => user,
            Err(e) => return vec![format!("{shown}: can't parse it: {e}")],
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => return vec![format!("{shown}: can't read it: {e}")],
    };
    let mut found = vec![];
    let own_commands = entries(&own["sandbox"]["excludedCommands"]);
    for command in entries(&user["sandbox"]["excludedCommands"]) {
        if !own_commands.contains(&command) {
            found.push(format!(
                "{shown}: sandbox.excludedCommands runs {command:?} outside it"
            ));
        }
    }
    let own_writes: Vec<PathBuf> = entries(&own["sandbox"]["filesystem"]["allowWrite"])
        .into_iter()
        .map(PathBuf::from)
        .collect();
    // Only an exact match counts: beneath one, the lead could plant a symlink before it exists.
    let opens = |path: Option<PathBuf>| !path.is_some_and(|path| own_writes.contains(&path));
    for (key, list) in [
        (
            "sandbox.filesystem.allowWrite",
            &user["sandbox"]["filesystem"]["allowWrite"],
        ),
        (
            "permissions.additionalDirectories",
            &user["permissions"]["additionalDirectories"],
        ),
    ] {
        for path in entries(list) {
            if opens(absolute(&path, home)) {
                found.push(format!("{shown}: {key} opens {path} to writes"));
            }
        }
    }
    for rule in entries(&user["permissions"]["allow"]) {
        let edit = match rule.strip_prefix("Edit") {
            Some("") => None,
            Some(rest) => match rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
                Some(spec) => Some(edit_rule_path(spec, home)),
                None => continue,
            },
            None => continue,
        };
        if opens(edit.flatten()) {
            found.push(format!("{shown}: permissions.allow opens {rule} to writes"));
        }
    }
    // allowRead only carves into denyRead, which a lead has only with credentials to hide.
    if !own["sandbox"]["filesystem"]["denyRead"].is_null() {
        let own_reads = entries(&own["sandbox"]["filesystem"]["allowRead"]);
        for path in entries(&user["sandbox"]["filesystem"]["allowRead"]) {
            if !own_reads.contains(&path) {
                found.push(format!(
                    "{shown}: sandbox.filesystem.allowRead opens {path} to reads"
                ));
            }
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

/// An `Edit(…)` rule's directory in user settings: `//` absolute, `~/` home,
/// `/` relative to `~/.claude`. `None` for a relative path or a glob before its end.
fn edit_rule_path(spec: &str, home: &Path) -> Option<PathBuf> {
    let spec = spec.trim_end_matches("/**").trim_end_matches("/*");
    if spec.contains('*') {
        return None;
    }
    let path = if let Some(rest) = spec.strip_prefix("//") {
        format!("/{rest}")
    } else if spec.starts_with("~/") {
        spec.to_string()
    } else {
        let rest = spec.strip_prefix('/')?;
        home.join(".claude").join(rest).display().to_string()
    };
    absolute(&path, home)
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
    /// Started without its settings, under `prefer`.
    HookOnly(String),
}

/// Checks a running lead was launched with its settings, as `claude_args`
/// record, that they match its herd's current switches, and what the human's
/// own settings loosen. Under `require` one launched without them, or loosened,
/// is refused.
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
    // Judged against what the running Claude started with, which may predate the switches.
    let loosened = loosenings(&paths.home, on_disk.as_ref().unwrap_or(&current));
    if let Some(refusal) = loosened_refusal(paths, name, herd, &loosened)? {
        return Err(refusal);
    }
    let mut warnings: Vec<String> = loosened
        .iter()
        .map(|line| loosened_warning(name, line))
        .collect();
    if on_disk.as_ref() != Some(&current) {
        warnings.push(format!(
            "{name}'s sandbox was set before herd {herd}'s current switches, \
             which reach it only when its Claude starts again; {resume}"
        ));
    }
    if !claude_args.iter().any(|a| a == "--strict-mcp-config") {
        warnings.push(format!(
            "{name} was started with every MCP server you configure, not only its herd's \
             [lead] mcp_servers; {resume}"
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

/// The loopherd commands a lead runs that write what its sandbox denies: its
/// members' worktrees, beside its own, or the ledger.
const UNSANDBOXED_COMMANDS: [&str; 7] = [
    "loopherd feed",
    "loopherd feed *",
    "loopherd add *",
    "loopherd retire *",
    "loopherd release *",
    "loopherd restart *",
    "loopherd prompt *",
];

/// The state dir's directories a lead writes from its sandbox: `reports/`
/// for its mailbox and its watch's files, `handoffs/` for its handoff. The
/// rest stays read-only to it as a directory, since a deny on one file holds
/// only until something outside the sandbox renames over that file.
const WRITABLE_STATE: [&str; 2] = ["reports", "handoffs"];

/// The shepherd's watch files in `reports/`. Written in place, never
/// replaced, so a deny on each file holds.
const SHEPHERDS_WATCH: [&str; 2] = ["reports/watch.lock", "reports/watch.fired"];

/// What in the state dir is the human's or the shepherd's, denied by name to
/// the file tools, which the sandbox doesn't cover. `true` marks a directory.
const KEPT_FROM_FILE_TOOLS: [(&str, bool); 5] = [
    ("paused", true),
    ("shepherd.json", false),
    ("shepherd.json.lock", false),
    ("ledger.json", false),
    ("ledger.lock", false),
];

/// Makes what the lead's sandbox names in the state dir. An `allowWrite`
/// entry for a missing path has no effect on Linux, and a `denyWrite` entry
/// gets a placeholder that would sit in the way of the first real write.
fn create_state(state: &Path) -> Result<()> {
    for dir in WRITABLE_STATE {
        let dir = state.join(dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    for file in SHEPHERDS_WATCH {
        let file = state.join(file);
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&file)
            .with_context(|| format!("create {}", file.display()))?;
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
    let mut sandbox = json!({
        "enabled": true,
        "failIfUnavailable": true,
        "allowUnsandboxedCommands": false,
        // Linux can't narrow this to herdr's socket: its seccomp filter can't see socket paths.
        "network": {"allowAllUnixSockets": true},
        "filesystem": {
            "allowWrite": allow_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "denyWrite": deny_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        },
    });
    // `restart *` takes in `restart --self`, whose waiter the sandbox would end with the call.
    let mut excluded = UNSANDBOXED_COMMANDS.to_vec();
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
        sandbox["filesystem"]["denyRead"] = human
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
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
