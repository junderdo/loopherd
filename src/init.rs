//! `loopherd init`: a working herd for the GitHub repo around a directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::herd::{self, Paths};
use crate::worktree::{checkout_root, same_dir};

mod agents;
mod ask;
mod tracker;

pub use tracker::Choice as Tracker;

const HERD: &str = include_str!("init/herd.toml");
const LEAD_TABLE: &str = include_str!("init/lead-table.toml");
const GITHUB_SOURCE: &str = include_str!("init/github-source.toml");
const PLACEHOLDER_SOURCE: &str = include_str!("init/placeholder-source.toml");
const LEAD: &str = include_str!("init/lead.md");

pub struct Request<'a> {
    pub dir: &'a Path,
    pub herd: Option<&'a str>,
    /// Empty to take the ones suggested by the repo's build files.
    pub checks: &'a [String],
    pub gh_account: Option<&'a str>,
    /// `<owner>/<name>`, for an origin init can't read as GitHub.
    pub github: Option<&'a str>,
    /// The tracker to record when the repo has no tracker doc.
    pub tracker: Option<Tracker>,
    pub max: Option<u64>,
    pub lead: Option<bool>,
    pub context_limit: Option<u64>,
}

/// What init writes, settled from flags, inferred values and, on a
/// terminal, the human's answers.
struct Herd {
    repo: PathBuf,
    name: String,
    slug: String,
    tracker: tracker::Tracker,
    account: String,
    token: String,
    branch: String,
    checks: Vec<String>,
    max: u64,
    lead: bool,
    /// The lead's switch lines, from `[lead_defaults]`.
    switches: String,
    context_limit: Option<u64>,
    /// The limit that applies when `context_limit` is unset.
    config_limit: String,
    docs: Vec<agents::Doc>,
    /// What the human writes in the repo that init wouldn't.
    agents_steps: Vec<String>,
}

/// Writes the herd file and its lead prompt and creates missing labels,
/// returning what it did and the next steps. Refuses, changing nothing,
/// when either file exists. On a terminal it asks for each value, showing
/// what it inferred, and confirms before writing.
pub fn init(paths: &Paths, req: Request) -> Result<Vec<String>> {
    let ask = ask::Asker {
        terminal: ask::interactive(),
    };
    let herd = settle(paths, req, &ask)?;
    if ask.terminal {
        eprint!("{}", summary(&herd));
        if !ask.yes("Write this herd?", None, true)? {
            bail!("nothing written");
        }
    }
    write(paths, herd)
}

fn settle(paths: &Paths, req: Request, ask: &ask::Asker) -> Result<Herd> {
    let here = checkout_root(req.dir).with_context(|| {
        format!(
            "can't read a git repo at {}; run init inside one",
            req.dir.display()
        )
    })?;
    let repo = ask.value("Repo", None, Some(&here.to_string_lossy()), "", |answer| {
        checkout_root(Path::new(answer))
            .with_context(|| format!("can't read a git repo at {answer}"))
    })?;
    // No herd name can get past these, so on a terminal they end init before it asks for one.
    if ask.terminal {
        refuse_second_herd(paths, &repo)?;
    }
    let inferred = repo.file_name().map(|n| n.to_string_lossy().into_owned());
    let name = ask.value(
        "Herd name",
        req.herd,
        inferred.as_deref(),
        "the repo has no directory name; pass --herd",
        |name| {
            let name = herd_name(name)?;
            refuse_taken_name(paths, &name, &repo)?;
            Ok(name)
        },
    )?;
    if !ask.terminal {
        refuse_second_herd(paths, &repo)?;
    }
    let origin = github_repo(&repo).map(|(owner, name)| format!("{owner}/{name}"));
    let (owner, repo_name) = ask.value(
        "GitHub repo (owner/name)",
        req.github,
        origin.as_ref().ok().map(String::as_str),
        &origin
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default(),
        |slug| match slug.split_once('/') {
            Some((owner, name)) if is_github_name(owner) && is_github_name(name) => {
                Ok((owner.to_string(), name.to_string()))
            }
            _ => bail!("--repo {slug:?} is not <owner>/<name>"),
        },
    )?;
    let slug = format!("{owner}/{repo_name}");
    let mut tracker = tracker::resolve(&repo, req.tracker)?;
    for check in req.checks {
        prompt_text(check)?;
    }
    let config = paths.herds.with_file_name("config.toml");
    let switches = crate::config::lead_defaults(&config)?
        .unwrap_or_default()
        .over(&Default::default())
        .named()
        .map(|(name, on)| format!("{name} = {on}"))
        .join("\n");

    let checks = checks(&repo, req.checks, ask)?;
    let max = ask.value(
        "max, how many workers run at once",
        req.max.map(|m| m.to_string()).as_deref(),
        Some("3"),
        "",
        |max| match max.parse::<i64>() {
            Ok(max) if max > 0 => Ok(max as u64),
            Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => {
                bail!("max {max} is too big; at most {}", i64::MAX)
            }
            _ => bail!("max must be a positive integer, got {max:?}"),
        },
    )?;
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let lead = loop {
        let lead = ask.yes(
            "Give the herd a lead, a member that feeds, watches and verifies its workers?",
            req.lead,
            true,
        )?;
        if !lead || fs::symlink_metadata(&lead_file).is_err() {
            break lead;
        }
        let refusal = format!(
            "{} already exists; init never replaces a lead prompt",
            lead_file.display()
        );
        match ask.terminal {
            true => eprintln!("{refusal}; answer n or remove it"),
            false => bail!("{refusal}; pass --no-lead to write the herd without a lead"),
        }
    };
    let config_file = crate::config::read(&config)?;
    let cap = crate::context::limit_cap(&config)?;
    let unset = config_file
        .default_context_limit
        .unwrap_or(crate::context::DEFAULT_LIMIT);
    let window = match config_file.context_window {
        Some(_) => "config.toml's",
        None => "the default",
    };
    let config_limit = match config_file.default_context_limit {
        None if config_file.context_window.is_none() => {
            format!("the default {}", cap.limit.min(unset))
        }
        _ if unset > cap.limit => format!(
            "the cap of {} (80% of {window} {}-token context window)",
            cap.limit, cap.window
        ),
        Some(limit) => format!("config.toml's {limit}"),
        None => format!("the default {unset}"),
    };
    let context_limit = ask.value(
        &format!(
            "Workers' handoff limit in tokens (context_limit); Enter leaves it unset, \
             so {config_limit} applies"
        ),
        req.context_limit.map(|l| l.to_string()).as_deref(),
        Some(""),
        "",
        |limit| {
            if limit.is_empty() {
                return Ok(None);
            }
            let Ok(limit) = limit.parse::<u64>() else {
                bail!("context_limit must be a whole number of tokens, got {limit:?}");
            };
            crate::context::check_tokens("context_limit", Some(limit))?;
            if limit > cap.limit {
                bail!(
                    "context_limit {limit} is over {}, 80% of the {}-token context window",
                    cap.limit,
                    cap.window
                );
            }
            Ok(Some(limit))
        },
    )?;
    let account = account(&owner, req.gh_account, ask)?;
    let token = gh(None, &["auth", "token", "--user", &account])?
        .trim()
        .to_string();
    let branch = gh(
        Some(&token),
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
        branch => ask.value(
            "Default branch",
            None,
            Some(branch),
            "",
            |branch| match branch.contains(char::is_whitespace) {
                true => bail!("{branch:?} can't name a branch"),
                false => prompt_text(branch).map(|_| branch.to_string()),
            },
        )?,
    };
    let mut docs: Vec<agents::Doc> = tracker
        .writes
        .drain(..)
        .map(|(path, text)| agents::Doc::new(path, text))
        .collect();
    let plan = agents::missing(
        &repo,
        agents::Herd {
            account: &account,
            branch: &branch,
            checks: &checks,
            needs_info: &tracker.labels.needs_info,
        },
    )?;
    docs.extend(plan.docs);
    let new_docs = docs.iter().filter(|doc| !doc.append);
    refuse_unwritable(
        [paths.herds.join(format!("{name}.toml"))]
            .into_iter()
            .chain(lead.then(|| lead_file.clone()))
            .chain(new_docs.map(|doc| repo.join(doc.path))),
    )?;
    Ok(Herd {
        repo,
        name,
        slug,
        tracker,
        account,
        token,
        branch,
        checks,
        max,
        lead,
        switches,
        context_limit,
        config_limit,
        docs,
        agents_steps: plan.steps,
    })
}

/// The lead and members are lead-<herd> and <herd>-<n>, both [a-z][a-z0-9_-]{0,31}.
fn herd_name(name: &str) -> Result<String> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 27
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !valid {
        bail!(
            "{name:?} can't name a herd: it needs a lowercase letter then at most 26 of \
             a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>"
        );
    }
    Ok(name.to_string())
}

/// `given`, else the checks the repo's build files suggest: on a terminal
/// kept, or each kept, dropped or replaced, then others added.
fn checks(repo: &Path, given: &[String], ask: &ask::Asker) -> Result<Vec<String>> {
    if !given.is_empty() {
        return Ok(given.to_vec());
    }
    let suggested = suggested_checks(repo);
    if !ask.terminal {
        if suggested.is_empty() {
            bail!("no checks found in Cargo.toml or package.json; pass --check <cmd>");
        }
        return Ok(suggested);
    }
    if !suggested.is_empty() {
        eprintln!("Checks workers run before calling work done:");
        for check in &suggested {
            eprintln!("  {check}");
        }
        if ask.yes("Use these checks?", None, true)? {
            return Ok(suggested);
        }
        eprintln!("Enter keeps a check, - drops it, anything else replaces it.");
    }
    let mut checks = vec![];
    for check in &suggested {
        let kept = ask.value("Check", None, Some(check), "", |answer| match answer {
            "-" => Ok(None),
            answer => prompt_text(answer).map(|_| Some(answer.to_string())),
        })?;
        checks.extend(kept);
    }
    loop {
        let check = ask::line(match checks.is_empty() {
            true => "A check workers run before calling work done: ",
            false => "Another check (Enter when done): ",
        })?;
        match check.trim() {
            "" if checks.is_empty() => continue,
            "" => return Ok(checks),
            check => match prompt_text(check) {
                Ok(_) => checks.push(check.to_string()),
                Err(e) => eprintln!("{e:#}"),
            },
        }
    }
}

/// Cargo's checks for a Cargo.toml, and package.json's `lint` and `test` scripts.
fn suggested_checks(repo: &Path) -> Vec<String> {
    let mut checks = vec![];
    if repo.join("Cargo.toml").is_file() {
        checks.extend(
            [
                "cargo fmt --check",
                "cargo clippy -- -D warnings",
                "cargo test",
            ]
            .map(String::from),
        );
    }
    let package = repo.join("package.json");
    let json = match fs::read_to_string(&package) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
            .map_err(anyhow::Error::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return checks,
        Err(e) => Err(e.into()),
    };
    let json = match json {
        Ok(json) => json,
        Err(e) => {
            eprintln!(
                "warning: can't read {}, so it suggests no checks: {e}",
                package.display()
            );
            return checks;
        }
    };
    if let Some(scripts) = json.get("scripts") {
        let script = |name| scripts.get(name).and_then(serde_json::Value::as_str);
        if script("lint").is_some() {
            checks.push("npm run lint".into());
        }
        // npm init's placeholder test script always fails.
        if script("test").is_some_and(|test| !test.contains("no test specified")) {
            checks.push("npm test".into());
        }
    }
    checks
}

fn summary(herd: &Herd) -> String {
    let mut also = vec![];
    if herd.tracker.github {
        let names = wanted_labels(&herd.tracker.labels).map(|(name, ..)| name);
        let (last, rest) = names.split_last().expect("four labels");
        also.push(format!(
            "creates the labels {} and {last} in {} where missing",
            rest.join(", "),
            herd.slug
        ));
    }
    if !herd.docs.is_empty() {
        let docs: Vec<String> = herd.docs.iter().map(agents::Doc::planned).collect();
        also.push(format!(
            "writes {} in the repo, uncommitted",
            and_list(&docs)
        ));
    }
    let also = match also.is_empty() {
        true => String::new(),
        false => format!("Writing it also {}.\n", also.join(", and ")),
    };
    let limit = match herd.context_limit {
        Some(limit) => limit.to_string(),
        None => format!("unset, {} applies", herd.config_limit),
    };
    format!(
        "\nHerd {name}:\n  repo    {repo}\n  GitHub  {slug} as {account}, branch {branch}\n  \
         max     {max} workers at once\n  lead    {lead}\n  limit   {limit}\n  checks  {checks}\n\
         {also}",
        name = herd.name,
        repo = herd.repo.display(),
        slug = herd.slug,
        account = herd.account,
        branch = herd.branch,
        max = herd.max,
        lead = if herd.lead { "yes" } else { "no" },
        checks = herd.checks.join("; "),
    )
}

fn write(paths: &Paths, herd: Herd) -> Result<Vec<String>> {
    let Herd {
        repo,
        name,
        slug,
        tracker,
        account,
        token,
        branch,
        ..
    } = &herd;
    let herd_file = paths.herds.join(format!("{name}.toml"));
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let checks = herd
        .checks
        .iter()
        .map(|check| Ok(format!("   - `{}`", prompt_text(check)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let branch = prompt_text(branch)?;
    let repo_path = toml_string(&repo.to_string_lossy());
    let lead_prompt = toml_string(&format!("{name}-lead.md"));
    let max = herd.max.to_string();
    let context_limit = herd
        .context_limit
        .map(|limit| format!("context_limit = {limit}\n"))
        .unwrap_or_default();
    let labels = &tracker.labels;
    let needs_info = prompt_text(&labels.needs_info)?;
    let (needs_info_lc, ready_for_human_lc) = (
        labels.needs_info.to_ascii_lowercase(),
        labels.ready_for_human.to_ascii_lowercase(),
    );
    let mut values = vec![
        ("OWNER_REPO", slug.as_str()),
        ("ACCOUNT", account),
        ("HERD", name),
        ("REPO", &repo_path),
        ("BRANCH", &branch),
        ("CHECKS", &checks),
        ("MAX", &max),
        ("CONTEXT_LIMIT", &context_limit),
        ("LEAD_PROMPT", &lead_prompt),
        ("SWITCHES", &herd.switches),
        ("READY_FOR_AGENT", &labels.ready_for_agent),
        ("NEEDS_INFO", &needs_info),
        ("READY_FOR_HUMAN", &labels.ready_for_human),
        ("NEEDS_INFO_LC", &needs_info_lc),
        ("READY_FOR_HUMAN_LC", &ready_for_human_lc),
    ];
    // Filled before they join the herd file, whose fill leaves inserted text literal.
    let source = fill(
        match tracker.github {
            true => GITHUB_SOURCE,
            false => PLACEHOLDER_SOURCE,
        },
        &values,
    );
    let source = source.trim_end();
    let lead_table = match herd.lead {
        true => format!("\n{}", fill(LEAD_TABLE, &values)),
        false => String::new(),
    };
    values.push(("SOURCE", source));
    values.push(("LEAD_TABLE", &lead_table));
    let mut lines = match tracker.github {
        true => create_labels(token, slug, labels)?,
        false => vec![],
    };
    let mut written = Written::default();
    let lead = herd.lead.then(|| (lead_file.clone(), fill(LEAD, &values)));
    let herd_files = lead
        .into_iter()
        .chain([(herd_file.clone(), fill(HERD, &values))]);
    for (path, text) in herd_files {
        if let Err(e) = written.new_file(&path, &text) {
            written.undo();
            return Err(e);
        }
    }
    for doc in &herd.docs {
        let done = match doc.append {
            true => doc.append_to(repo),
            false => written.new_file(&repo.join(doc.path), &doc.text),
        };
        if let Err(e) = done {
            written.undo();
            return Err(e);
        }
    }
    lines.push(format!("wrote {}", herd_file.display()));
    if herd.lead {
        lines.push(format!("wrote {}", lead_file.display()));
    }
    lines.extend(herd.docs.iter().map(agents::Doc::done));
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
            "Fill [queue] source in {} from {}, and adapt its issue template and \
             docs/agents/loopherd.md to the same tracker.",
            herd_file.display(),
            tracker::TRACKER_DOC
        ));
    }
    steps.extend(herd.agents_steps.iter().cloned());
    if !herd.docs.is_empty() {
        let paths: Vec<String> = herd.docs.iter().map(|doc| doc.path.to_string()).collect();
        steps.push(format!(
            "Review and commit what init wrote in the repo: {}.",
            and_list(&paths)
        ));
    }
    steps.extend([
        "loopherd herds".into(),
        format!("loopherd feed {name} --dry-run"),
    ]);
    if herd.lead {
        steps.push(format!("loopherd lead {name}"));
    }
    lines.push("Next:".into());
    for (n, step) in steps.iter().enumerate() {
        lines.push(format!("  {}. {step}", n + 1));
    }
    Ok(lines)
}

/// Refuses a new file whose nearest existing directory init can't write in.
fn refuse_unwritable(new_files: impl IntoIterator<Item = PathBuf>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    for path in new_files {
        let Some(dir) = path.ancestors().skip(1).find(|dir| dir.exists()) else {
            continue;
        };
        let c_dir = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: c_dir is a valid NUL-terminated path for the call's duration.
        if unsafe { libc::access(c_dir.as_ptr(), libc::W_OK) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(e).with_context(|| format!("write {}", path.display()));
        }
    }
    Ok(())
}

/// The files and directories init created, to undo when a later write fails.
#[derive(Default)]
struct Written {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl Written {
    fn new_file(&mut self, path: &Path, text: &str) -> Result<()> {
        if let Some(dir) = path.parent() {
            let missing: Vec<&Path> = dir
                .ancestors()
                .take_while(|dir| fs::symlink_metadata(dir).is_err())
                .collect();
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            self.dirs
                .extend(missing.into_iter().rev().map(Path::to_path_buf));
        }
        write_new(path, text)?;
        self.files.push(path.to_path_buf());
        Ok(())
    }

    fn undo(&self) {
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        for dir in self.dirs.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => items.join(""),
    }
}

fn refuse_taken_name(paths: &Paths, herd: &str, repo: &Path) -> Result<()> {
    let herd_file = &paths.herds.join(format!("{herd}.toml"));
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
    Ok(())
}

/// Refuses a repo another herd runs, or one a herd file init can't read might run.
fn refuse_second_herd(paths: &Paths, repo: &Path) -> Result<()> {
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

/// `wanted` if gh is logged in as it, else the logged-in account named
/// `owner`, asked on a terminal from the logged-in ones.
fn account(owner: &str, wanted: Option<&str>, ask: &ask::Asker) -> Result<String> {
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
    let logged_in = |name: &str| logins.iter().find(|l| l.eq_ignore_ascii_case(name));
    ask.value(
        &format!("gh account (logged in: {found})"),
        wanted,
        logged_in(owner).copied(),
        &format!(
            "no logged-in gh account matches the remote owner {owner} \
             (found: {found}); pass --gh-account <login>"
        ),
        |wanted| match logged_in(wanted) {
            Some(login) => Ok(login.to_string()),
            None => bail!("gh has no logged-in account {wanted} (found: {found})"),
        },
    )
}

/// The labels the herd's queue and workers use: name, description and color.
fn wanted_labels(labels: &tracker::Labels) -> [(&str, &str, &str); 4] {
    [
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
    ]
}

/// Creates each label the herd's queue and workers use that the repo lacks,
/// one line per label created.
fn create_labels(token: &str, slug: &str, labels: &tracker::Labels) -> Result<Vec<String>> {
    let wanted = wanted_labels(labels);
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
