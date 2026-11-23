//! `loopherd update`: brings a release install up to date by running the
//! target release's own `install.sh`, as recorded in `install.toml` (D122).

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

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
    pub config_dir: PathBuf,
    /// `~/.local/bin/loopherd`, which every install replaces.
    pub installed: PathBuf,
    pub running: PathBuf,
    /// `$LOOPHERD_RELEASE_BASE`, when set, in place of GitHub's releases.
    pub base: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Record {
    Release { tag: String },
    Source,
}

/// The refusal both `update` and doctor give when `install.toml` is missing.
pub fn no_record(path: &Path) -> String {
    format!(
        "no install record at {}, so loopherd can't tell how it was installed; install it \
         again with `curl -fsSL {RELEASES}/latest/download/install.sh | sh`, or \
         `scripts/install` from a checkout",
        path.display()
    )
}

pub fn record_path(config_dir: &Path) -> PathBuf {
    config_dir.join("install.toml")
}

/// Runs the update, returning the exit code: with [`Want::Check`], 0 up to
/// date, 1 behind and 2 when it couldn't check; otherwise 0 once installed
/// or up to date. `on_path` says whether a program is on PATH.
pub fn update(
    places: &Places,
    want: Want,
    on_path: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let checking = matches!(want, Want::Check);
    match run(places, want, on_path, out) {
        Err(e) if checking => {
            eprintln!("loopherd update: {e:#}");
            Ok(2)
        }
        result => result,
    }
}

fn run(
    places: &Places,
    want: Want,
    on_path: &dyn Fn(&str) -> bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let current = recorded_tag(&places.config_dir)?;
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
    if !same_file(&places.running, &places.installed) {
        writeln!(
            out,
            "this updates {}, not the loopherd running ({})",
            places.installed.display(),
            places.running.display()
        )?;
    }
    out.flush()?;
    install(&fetch, base, &target)?;
    if herdr_plugin_older(&places.installed) {
        writeln!(
            out,
            "herdr's loopherd plugin is older than loopherd; update it with: herdr plugin update loopherd"
        )?;
    }
    writeln!(out, "{RESTART_VIEW}")?;
    Ok(0)
}

fn recorded_tag(config_dir: &Path) -> Result<String> {
    let path = record_path(config_dir);
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("refused: {}", no_record(&path)),
        text => text.with_context(|| format!("read {}", path.display()))?,
    };
    match toml::from_str(&text).with_context(|| format!("read {}", path.display()))? {
        Record::Release { tag } => Ok(tag),
        Record::Source => bail!(
            "refused: {} records a source install, and source updates aren't built yet; \
             run `git pull && scripts/install` in its checkout",
            path.display()
        ),
    }
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
            Fetch::Curl => Command::new("curl")
                .args(["-fsS", "-o", "/dev/null", "-w", "%{redirect_url}", &url])
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
                .args(["-fsSL", "-o"])
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
