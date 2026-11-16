//! `loopherd init`: a working herd for the GitHub repo around a directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::herd::{self, Paths};
use crate::worktree::{repo_root, same_dir};

mod tracker;

pub use tracker::Choice as Tracker;

const HERD: &str = include_str!("init/herd.toml");
const GITHUB_SOURCE: &str = include_str!("init/github-source.toml");
const PLACEHOLDER_SOURCE: &str = include_str!("init/placeholder-source.toml");
const LEAD: &str = include_str!("init/lead.md");

pub struct Request<'a> {
    pub dir: &'a Path,
    pub herd: Option<&'a str>,
    pub checks: &'a [String],
    pub gh_account: Option<&'a str>,
    /// `<owner>/<name>`, for an origin init can't read as GitHub.
    pub github: Option<&'a str>,
    /// The tracker to record when the repo has no tracker doc.
    pub tracker: Option<Tracker>,
}

/// Writes the herd file and its lead prompt and creates missing labels,
/// returning what it did and the next steps. Refuses, changing nothing,
/// when either file exists.
pub fn init(paths: &Paths, req: Request) -> Result<Vec<String>> {
    let repo = repo_root(req.dir)?;
    let herd = match req.herd {
        Some(herd) => herd.to_string(),
        None => repo
            .file_name()
            .context("the repo has no directory name; pass --herd")?
            .to_string_lossy()
            .into_owned(),
    };
    // The lead is lead-<herd> and members <herd>-<n>, both [a-z][a-z0-9_-]{0,31}.
    let mut chars = herd.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && herd.len() <= 27
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !valid {
        bail!(
            "{herd:?} can't name a herd: it needs a lowercase letter then at most 26 of \
             a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>"
        );
    }
    let (owner, name) = match req.github {
        Some(slug) => match slug.split_once('/') {
            Some((owner, name)) if is_github_name(owner) && is_github_name(name) => {
                (owner.to_string(), name.to_string())
            }
            _ => bail!("--repo {slug:?} is not <owner>/<name>"),
        },
        None => github_repo(&repo)?,
    };
    let slug = format!("{owner}/{name}");
    let herd_file = paths.herds.join(format!("{herd}.toml"));
    let lead_file = paths.herds.join(format!("{herd}-lead.md"));
    refuse_existing(paths, &herd, &repo, &herd_file, &lead_file)?;
    let tracker = tracker::resolve(&repo, req.tracker)?;
    let checks = req
        .checks
        .iter()
        .map(|check| Ok(format!("   - `{}`", prompt_text(check)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let defaults = crate::config::lead_defaults(&paths.herds.with_file_name("config.toml"))?;
    let switches = defaults.unwrap_or_default().over(&Default::default());

    let account = account(&owner, req.gh_account)?;
    let token = gh(None, &["auth", "token", "--user", &account])?;
    let token = token.trim();
    let branch = gh(
        Some(token),
        &[
            "repo",
            "view",
            &slug,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name // \"\"",
        ],
    )?;
    let branch = match branch.trim() {
        "" => bail!("{slug} has no default branch yet; push a first commit"),
        branch => prompt_text(branch)?,
    };
    let switches = switches
        .named()
        .map(|(name, on)| format!("{name} = {on}"))
        .join("\n");
    let repo_path = toml_string(&repo.to_string_lossy());
    let lead_prompt = toml_string(&format!("{herd}-lead.md"));
    let labels = &tracker.labels;
    let needs_info = prompt_text(&labels.needs_info)?;
    let (needs_info_lc, ready_for_human_lc) = (
        labels.needs_info.to_ascii_lowercase(),
        labels.ready_for_human.to_ascii_lowercase(),
    );
    let mut values = vec![
        ("OWNER_REPO", slug.as_str()),
        ("ACCOUNT", &account),
        ("HERD", &herd),
        ("REPO", &repo_path),
        ("BRANCH", &branch),
        ("CHECKS", &checks),
        ("LEAD_PROMPT", &lead_prompt),
        ("SWITCHES", &switches),
        ("READY_FOR_AGENT", &labels.ready_for_agent),
        ("NEEDS_INFO", &needs_info),
        ("READY_FOR_HUMAN", &labels.ready_for_human),
        ("NEEDS_INFO_LC", &needs_info_lc),
        ("READY_FOR_HUMAN_LC", &ready_for_human_lc),
    ];
    // Filled before it joins the herd file, whose fill leaves inserted text literal.
    let source = fill(
        match tracker.github {
            true => GITHUB_SOURCE,
            false => PLACEHOLDER_SOURCE,
        },
        &values,
    );
    let source = source.trim_end();
    values.push(("SOURCE", source));
    fs::create_dir_all(&paths.herds)
        .with_context(|| format!("create {}", paths.herds.display()))?;
    let mut lines = match tracker.github {
        true => create_labels(token, &slug, labels)?,
        false => vec![],
    };
    let mut written: Vec<PathBuf> = vec![];
    let files = [
        (lead_file.clone(), fill(LEAD, &values)),
        (herd_file.clone(), fill(HERD, &values)),
    ]
    .into_iter()
    .chain(
        tracker
            .writes
            .iter()
            .map(|(path, text)| (repo.join(path), text.clone())),
    );
    for (path, text) in files {
        if let Err(e) = write_new(&path, &text) {
            for path in &written {
                let _ = fs::remove_file(path);
            }
            return Err(e);
        }
        written.push(path);
    }
    lines.push(format!("wrote {}", herd_file.display()));
    lines.push(format!("wrote {}", lead_file.display()));
    for (path, _) in &tracker.writes {
        lines.push(format!("wrote {path} (uncommitted)"));
    }
    let mut steps = vec![format!(
        "Open Claude in {} once and accept its trust dialog; members can't start past \
         it, and init never accepts it for you.",
        repo.display()
    )];
    if !tracker.github {
        lines.push(
            "The tracker isn't GitHub, so the herd's queue source is a placeholder that \
             `loopherd herds` reports invalid until it's filled."
                .into(),
        );
        steps.push(format!(
            "Fill [queue] source in {} from {}, and adapt its issue template to the same \
             tracker.",
            herd_file.display(),
            tracker::TRACKER_DOC
        ));
    }
    steps.extend([
        "loopherd herds".into(),
        format!("loopherd feed {herd} --dry-run"),
        format!("loopherd lead {herd}"),
    ]);
    lines.push("Next:".into());
    for (n, step) in steps.iter().enumerate() {
        lines.push(format!("  {}. {step}", n + 1));
    }
    Ok(lines)
}

fn refuse_existing(
    paths: &Paths,
    herd: &str,
    repo: &Path,
    herd_file: &Path,
    lead_file: &Path,
) -> Result<()> {
    if fs::symlink_metadata(herd_file).is_ok() {
        if let Ok(other) = herd::repo(paths, herd)
            && !same_dir(&other, repo)
        {
            bail!(
                "herd {herd} already points at {}; pass --herd <name> to name this repo's herd",
                other.display()
            );
        }
        bail!(
            "{} already exists; init never replaces a herd file",
            herd_file.display()
        );
    }
    if fs::symlink_metadata(lead_file).is_ok() {
        bail!(
            "{} already exists; init never replaces a lead prompt",
            lead_file.display()
        );
    }
    for other in herd::names(&paths.herds)? {
        let file = paths.herds.join(format!("{other}.toml"));
        if !fs::metadata(&file).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let other_repo = herd::repo(paths, &other).with_context(|| {
            format!(
                "can't tell which repo {} runs; fix or remove it",
                file.display()
            )
        })?;
        if same_dir(&other_repo, repo) {
            bail!(
                "herd {other} already runs {}; init writes one herd per repo",
                repo.display()
            );
        }
    }
    Ok(())
}

/// `origin`'s GitHub owner and repo name.
fn github_repo(repo: &Path) -> Result<(String, String)> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "{} has no origin remote; pass --repo <owner>/<name>",
            repo.display()
        );
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts = github_path(&url).map(|path| {
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        path.split('/').map(String::from).collect::<Vec<_>>()
    });
    match parts.as_deref() {
        Some([owner, name]) if is_github_name(owner) && is_github_name(name) => {
            Ok((owner.clone(), name.clone()))
        }
        _ => bail!("origin {url} is not a GitHub repo; pass --repo <owner>/<name>"),
    }
}

/// The path of a `github.com` remote URL, whatever the host's case, scp-like
/// (`git@github.com:o/r`) or with a scheme, a user and a port
/// (`ssh://git@ssh.github.com:443/o/r`).
fn github_path(url: &str) -> Option<&str> {
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = authority.rsplit('@').next()?.split(':').next()?;
    let host = host.to_ascii_lowercase();
    matches!(
        host.as_str(),
        "github.com" | "www.github.com" | "ssh.github.com"
    )
    .then_some(path)
}

/// Safe to put unquoted in the herd's shell commands and prompts, and not
/// an option, a path step or a `.git` suffix.
fn is_github_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && !name.ends_with(".git")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `wanted` if gh is logged in as it, else the logged-in account named `owner`.
fn account(owner: &str, wanted: Option<&str>) -> Result<String> {
    let logins = gh(
        None,
        &[
            "auth",
            "status",
            "--json",
            "hosts",
            "--jq",
            r#".hosts["github.com"][]?.login"#,
        ],
    )?;
    let logins: Vec<&str> = logins.lines().filter(|l| is_github_name(l)).collect();
    if logins.is_empty() {
        bail!("gh has no logged-in account; run `gh auth login`");
    }
    let found = logins.join(", ");
    let login = logins
        .iter()
        .find(|l| l.eq_ignore_ascii_case(wanted.unwrap_or(owner)));
    match (login, wanted) {
        (Some(login), _) => Ok(login.to_string()),
        (None, Some(wanted)) => bail!("gh has no logged-in account {wanted} (found: {found})"),
        (None, None) => bail!(
            "no logged-in gh account matches the remote owner {owner} \
             (found: {found}); pass --gh-account <login>"
        ),
    }
}

/// Creates each label the herd's queue and workers use that the repo lacks,
/// one line per label created.
fn create_labels(token: &str, slug: &str, labels: &tracker::Labels) -> Result<Vec<String>> {
    let wanted = [
        (
            labels.ready_for_agent.as_str(),
            "Fully specified; an agent may claim it",
            "0E8A16",
        ),
        (
            &labels.needs_info,
            "Stopped on a question for the human",
            "D876E3",
        ),
        (
            &labels.ready_for_human,
            "Needs a human to work it",
            "FBCA04",
        ),
        ("blocked", "Waiting on other issues", "B60205"),
    ];
    let existing = gh(
        Some(token),
        &[
            "label", "list", "--repo", slug, "--limit", "1000", "--json", "name", "--jq",
            ".[].name",
        ],
    )?;
    let existing: Vec<&str> = existing.lines().collect();
    let mut created = vec![];
    for (name, description, color) in wanted {
        if existing
            .iter()
            .any(|label| label.eq_ignore_ascii_case(name))
        {
            continue;
        }
        gh(
            Some(token),
            &[
                "label",
                "create",
                name,
                "--repo",
                slug,
                "--description",
                description,
                "--color",
                color,
            ],
        )?;
        created.push(format!("created label {name} in {slug}"));
    }
    Ok(created)
}

/// gh's stdout; `token` runs it as that account, else as gh's own login.
fn gh(token: Option<&str>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("gh");
    command.args(args);
    match token {
        Some(token) => command.env("GH_TOKEN", token),
        None => command.env_remove("GH_TOKEN"),
    };
    let output = command
        .output()
        .context("run gh; is the GitHub CLI installed?")?;
    if !output.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `value` as literal text inside the worker prompt, a `'''` TOML string
/// whose `{key}` placeholders loopherd fills.
fn prompt_text(value: &str) -> Result<String> {
    if value.contains("'''") || value.chars().any(|c| c.is_control()) {
        bail!("{value:?} can't go in the worker prompt: it has ''' or a control character");
    }
    Ok(value.replace('{', "{{").replace('}', "}}"))
}

/// `text` with each `@KEY@` of `values` replaced in one pass, so a value
/// naming another key stays literal.
fn fill(text: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let value = after.split_once('@').and_then(|(key, _)| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (key.len(), *value))
        });
        match value {
            Some((len, value)) => {
                out.push_str(value);
                rest = &after[len + 1..];
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn toml_string(value: &str) -> String {
    toml_edit::Value::from(value).to_string().trim().to_string()
}

/// Writes `text` to a new file at `path`, leaving no file behind on failure.
fn write_new(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    if let Err(e) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}
