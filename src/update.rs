//! `loopherd update`: brings an install up to date by the path recorded in
//! `install.toml` (D122): a release runs the target release's own
//! `install.sh`; a source checkout fast-forwards and runs `scripts/install --update`.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::herd::Paths;

const RELEASES: &str = "https://github.com/junderdo/loopherd/releases";
const RESTART_VIEW: &str = "restart any open `loopherd view`: a running view keeps the old binary";

/// What `loopherd update` was asked for.
pub enum Want {
    Latest,
    Check,
    Version(String),
}

/// Where an update reads and writes, gathered by the caller.
pub struct Places {
    /// The config dir and the herds whose upgrades `--check` counts.
    pub paths: Paths,
    /// `~/.local/bin/loopherd`, which every install replaces.
    pub installed: PathBuf,
    pub running: PathBuf,
    /// `$LOOPHERD_RELEASE_BASE`, when set, in place of GitHub's releases.
    pub base: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Record {
    Release {
        tag: String,
    },
    Source {
        checkout: PathBuf,
        /// The commit `scripts/install` last installed, which a record
        /// written before it was recorded lacks.
        commit: Option<String>,
    },
}

/// Why a missing `install.toml` stops `update`, and its fix, as doctor
/// warns about it too.
pub fn no_record(path: &Path) -> (String, String) {
    (
        format!(
            "no install record at {}, so loopherd update can't tell how loopherd was installed",
            path.display()
        ),
        "install it again with `curl -fsSL https://junderdo.dev/loopherd/install.sh | sh`, \
         or `scripts/install` from a checkout"
            .to_string(),
    )
}

pub fn record_path(config_dir: &Path) -> PathBuf {
    config_dir.join("install.toml")
}

/// Runs the update, returning the exit code: with [`Want::Check`], 0 up to
/// date, 1 behind and 2 when it couldn't check, then the herds' pending
/// upgrades; otherwise 0 once installed or up to date, then the installed
/// binary's `herd upgrade`. `on_path` says whether a program is on PATH.
pub fn update(
    places: &Places,
    want: Want,
    on_path: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> Result<i32> {
    if !matches!(want, Want::Check) {
        let code = run(places, want, on_path, out)?;
        upgrade_herds(&places.installed, out)?;
        return Ok(code);
    }
    let code = run(places, want, on_path, out).unwrap_or_else(|e| {
        eprintln!("loopherd update: {e:#}");
        2
    });
    match crate::herd::pending(&places.paths) {
        Ok(pending) if pending.is_empty() => {}
        Ok(pending) => {
            let features = pending.iter().map(|(_, ids)| ids.len()).sum();
            let herds = match pending.len() {
                1 => "1 herd has".to_string(),
                n => format!("{n} herds have"),
            };
            writeln!(
                out,
                "{herds} {}; run loopherd herd upgrade",
                crate::herd::new_features(features)
            )?;
        }
        Err(e) => eprintln!("loopherd update: can't count herd upgrades: {e:#}"),
    }
    Ok(code)
}

/// Runs `installed`'s own `herd upgrade`, which alone knows the migrations
/// it ships; it offers them on a terminal and lists them otherwise.
fn upgrade_herds(installed: &Path, out: &mut dyn Write) -> Result<()> {
    out.flush()?;
    let failed = match Command::new(installed).args(["herd", "upgrade"]).status() {
        Ok(status) if status.success() => return Ok(()),
        Ok(status) => status.to_string(),
        Err(e) => e.to_string(),
    };
    writeln!(
        out,
        "herd upgrade failed ({failed}); run loopherd herd upgrade to try again"
    )?;
    Ok(())
}

fn run(
    places: &Places,
    want: Want,
    on_path: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> Result<i32> {
    match read_record(places.paths.config_dir())? {
        Record::Release { tag } => update_release(places, tag, want, on_path, out),
        Record::Source { checkout, commit } => update_source(places, &checkout, commit, want, out),
    }
}

fn update_release(
    places: &Places,
    current: String,
    want: Want,
    on_path: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let base = places.base.as_deref().unwrap_or(RELEASES);
    let fetch = Fetch::find(on_path)?;
    let target = match want {
        Want::Version(version) => {
            let tag = parse_tag(&version)
                .map(|_| format!("v{}", version.trim_start_matches('v')))
                .ok_or_else(|| anyhow!("--version takes a release tag vX.Y.Z, not {version:?}"))?;
            match tag == current {
                true => writeln!(out, "{current} → {tag} (reinstall)")?,
                false => writeln!(out, "{current} → {tag}")?,
            }
            tag
        }
        want => {
            let latest = fetch.latest_tag(base)?;
            if !newer(&latest, &current)? {
                writeln!(out, "loopherd is up to date ({current})")?;
                return Ok(0);
            }
            if matches!(want, Want::Check) {
                writeln!(out, "{current} → {latest} available; run loopherd update")?;
                return Ok(1);
            }
            writeln!(out, "{current} → {latest}")?;
            latest
        }
    };
    say_which_loopherd(places, out)?;
    install(&fetch, base, &target)?;
    // A release whose installer predates the record wouldn't write it.
    record_release(places.paths.config_dir(), &target)?;
    after_install(places, out)?;
    Ok(0)
}

fn update_source(
    places: &Places,
    checkout: &Path,
    installed: Option<String>,
    want: Want,
    out: &mut dyn Write,
) -> Result<i32> {
    let refuse = |why: String| {
        anyhow!(
            "refused: {why}; update by hand with `git pull && scripts/install` in {}",
            checkout.display()
        )
    };
    if matches!(want, Want::Version(_)) {
        return Err(refuse(
            "--version picks a release, and this is a source install".to_string(),
        ));
    }
    if !checkout.is_dir() {
        return Err(refuse(format!(
            "the recorded checkout {} doesn't exist",
            checkout.display()
        )));
    }
    if !checkout.join(".git").exists() {
        return Err(refuse(format!(
            "the recorded checkout {} isn't a git repository",
            checkout.display()
        )));
    }
    let git = Git(checkout);
    let default = default_branch(&git).map_err(|e| refuse(format!("{e:#}")))?;
    let local = format!("refs/heads/{default}");
    let upstream = format!("refs/remotes/origin/{default}");
    let blocker = {
        let branch = git
            .output(&["symbolic-ref", "-q", "HEAD"])
            .unwrap_or_default();
        if branch != local {
            let what = branch
                .strip_prefix("refs/heads/")
                .unwrap_or("a detached HEAD");
            Some(format!("{what} is checked out, not {default}"))
        } else if !git
            .output(&["status", "--porcelain", "--untracked-files=no"])?
            .is_empty()
        {
            Some("the checkout has uncommitted changes".to_string())
        } else {
            None
        }
    };
    let checking = matches!(want, Want::Check);
    if let Some(why) = &blocker
        && !checking
    {
        return Err(refuse(why.clone()));
    }
    git.output(&["fetch", "-q", "origin"])?;
    let target = git.output(&["rev-parse", "--verify", &format!("{upstream}^{{commit}}")])?;
    // A failed install leaves the record on the commit before the pull.
    let installed = match installed {
        Some(commit) if git.succeeds(&["cat-file", "-e", &format!("{commit}^{{commit}}")])? => {
            commit
        }
        _ => git.output(&["rev-parse", "--verify", &format!("{local}^{{commit}}")])?,
    };
    let short = |sha: &str| git.output(&["rev-parse", "--short", sha]);
    let behind: usize = git
        .output(&["rev-list", "--count", &format!("{installed}..{target}")])?
        .parse()
        .context("count the commits behind")?;
    if behind == 0 {
        writeln!(out, "loopherd is up to date ({})", short(&installed)?)?;
        return Ok(0);
    }
    let commits = match behind {
        1 => "1 commit".to_string(),
        n => format!("{n} commits"),
    };
    let (old, new) = (short(&installed)?, short(&target)?);
    if checking {
        let next = match &blocker {
            Some(why) => format!("loopherd update would refuse: {why}"),
            None => "run loopherd update".to_string(),
        };
        writeln!(out, "{old} → {new}: {commits} behind; {next}")?;
        return Ok(1);
    }
    if !git.succeeds(&["merge-base", "--is-ancestor", &local, &target])? {
        return Err(refuse(format!(
            "{default} has commits origin/{default} doesn't, so the pull isn't a fast-forward"
        )));
    }
    writeln!(out, "{old} → {new}, {commits}")?;
    say_which_loopherd(places, out)?;
    // Not a refusal: a pull can fail partway, after rewriting some files.
    git.output(&["pull", "-q", "--ff-only", "origin", &default])
        .map_err(|e| {
            anyhow!(
                "{e:#}; the checkout {} may be partly updated: check `git status` there, \
                 and `git checkout -- .` restores it",
                checkout.display()
            )
        })?;
    let status = Command::new(checkout.join("scripts/install"))
        .arg("--update")
        .current_dir(checkout)
        .status()
        .context("run scripts/install --update")?;
    if !status.success() {
        bail!(
            "scripts/install --update failed ({status}) in {}; fix it and run loopherd update again",
            checkout.display()
        );
    }
    after_install(places, out)?;
    Ok(0)
}

/// origin's default branch: from `origin/HEAD`, else by asking origin.
fn default_branch(git: &Git) -> Result<String> {
    if let Ok(head) = git.output(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        && let Some(branch) = head.strip_prefix("refs/remotes/origin/")
    {
        return Ok(branch.to_string());
    }
    git.output(&["ls-remote", "--symref", "origin", "HEAD"])?
        .lines()
        .find_map(|line| {
            line.strip_prefix("ref: refs/heads/")?
                .strip_suffix("\tHEAD")
        })
        .map(str::to_string)
        .ok_or_else(|| anyhow!("can't tell origin's default branch"))
}

/// Runs git in one checkout.
struct Git<'a>(&'a Path);

impl Git<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(self.0).args(args);
        command
    }

    /// Trimmed stdout, or an error naming the command and git's stderr.
    fn output(&self, args: &[&str]) -> Result<String> {
        let output = self
            .command(args)
            .output()
            .with_context(|| format!("run git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn succeeds(&self, args: &[&str]) -> Result<bool> {
        let status = self
            .command(args)
            .status()
            .with_context(|| format!("run git {}", args.join(" ")))?;
        Ok(status.success())
    }
}

fn say_which_loopherd(places: &Places, out: &mut dyn Write) -> Result<()> {
    if !same_file(&places.running, &places.installed) {
        writeln!(
            out,
            "this updates {}, not the loopherd running ({})",
            places.installed.display(),
            places.running.display()
        )?;
    }
    out.flush()?;
    Ok(())
}

fn after_install(places: &Places, out: &mut dyn Write) -> Result<()> {
    if herdr_plugin_older(&places.installed) {
        writeln!(
            out,
            "herdr's loopherd plugin is older than loopherd; update it with: herdr plugin update loopherd"
        )?;
    }
    writeln!(out, "{RESTART_VIEW}")?;
    Ok(())
}

fn read_record(config_dir: &Path) -> Result<Record> {
    let path = record_path(config_dir);
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let (why, fix) = no_record(&path);
            bail!("refused: {why}; {fix}")
        }
        text => text.with_context(|| format!("read {}", path.display()))?,
    };
    toml::from_str(&text).with_context(|| format!("read {}", path.display()))
}

fn record_release(config_dir: &Path, tag: &str) -> Result<()> {
    let path = record_path(config_dir);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("kind = \"release\"\ntag = \"{tag}\"\n"))
        .and_then(|()| std::fs::rename(&tmp, &path))
        .with_context(|| format!("record {tag} in {}", path.display()))
}

/// Downloads `tag`'s own installer and runs it pinned to `tag`.
fn install(fetch: &Fetch, base: &str, tag: &str) -> Result<()> {
    let dir = tempfile::tempdir().context("make a temp dir for the installer")?;
    let script = dir.path().join("install.sh");
    fetch.download(&format!("{base}/download/{tag}/install.sh"), &script)?;
    let status = Command::new("sh")
        .arg(&script)
        .env("LOOPHERD_VERSION", tag)
        // The splash belongs to a first install.
        .env("LOOPHERD_NO_SPLASH", "1")
        .status()
        .context("run the installer with sh")?;
    if !status.success() {
        bail!("the installer for {tag} failed ({status})");
    }
    Ok(())
}

/// Whether the new binary's doctor names `herdr plugin update` as the fix.
fn herdr_plugin_older(installed: &Path) -> bool {
    let Ok(output) = Command::new(installed).args(["doctor", "--json"]).output() else {
        return false;
    };
    let Ok(doctor) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };
    doctor["checks"].as_array().is_some_and(|checks| {
        checks.iter().any(|check| {
            check["name"] == "herdr plugin" && check["fix"] == "herdr plugin update loopherd"
        })
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn parse_tag(tag: &str) -> Option<[u64; 3]> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        *number = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

fn newer(latest: &str, current: &str) -> Result<bool> {
    let parse = |tag| parse_tag(tag).ok_or_else(|| anyhow!("{tag:?} isn't a release tag vX.Y.Z"));
    Ok(parse(latest)? > parse(current)?)
}

enum Fetch {
    Curl,
    Wget,
}

impl Fetch {
    fn find(on_path: &dyn Fn(&str) -> bool) -> Result<Fetch> {
        if on_path("curl") {
            Ok(Fetch::Curl)
        } else if on_path("wget") {
            Ok(Fetch::Wget)
        } else {
            bail!("needs curl or wget to reach the releases; install one first")
        }
    }

    /// The tag `<base>/latest` redirects to, without following it.
    fn latest_tag(&self, base: &str) -> Result<String> {
        let url = format!("{base}/latest");
        let output = match self {
            // -q, first, skips a ~/.curlrc whose `location` would follow the redirect.
            Fetch::Curl => Command::new("curl")
                .args([
                    "-q",
                    "-fsS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{redirect_url}",
                    &url,
                ])
                .output(),
            // wget exits nonzero on the redirect it was told not to follow.
            Fetch::Wget => Command::new("wget")
                .args(["-S", "--spider", "--max-redirect=0", &url])
                .output(),
        }
        .with_context(|| format!("ask {url} for the latest release"))?;
        let location = match self {
            Fetch::Curl => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            Fetch::Wget => String::from_utf8_lossy(&output.stderr)
                .lines()
                .find_map(|line| line.trim().strip_prefix("Location:"))
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_default()
                .to_string(),
        };
        location
            .rsplit_once("/tag/")
            .map(|(_, tag)| tag.to_string())
            .filter(|tag| parse_tag(tag).is_some())
            .ok_or_else(|| {
                anyhow!(
                    "couldn't read the latest release from {url}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            })
    }

    fn download(&self, url: &str, to: &Path) -> Result<()> {
        let output = match self {
            Fetch::Curl => Command::new("curl")
                .args(["-q", "-fsSL", "-o"])
                .arg(to)
                .arg(url)
                .output(),
            Fetch::Wget => Command::new("wget")
                .args(["-q", "-O"])
                .arg(to)
                .arg(url)
                .output(),
        }
        .with_context(|| format!("download {url}"))?;
        if !output.status.success() {
            bail!(
                "could not download {url}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}
