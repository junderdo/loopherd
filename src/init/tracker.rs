//! The repo's tracker doc, `docs/agents/issue-tracker.md`, and its
//! `triage-labels.md`, in mattpocock-skills' format: read when present,
//! written when not.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::agents::Doc;
use super::ask::{interactive, line};

const GITHUB: &str = include_str!("tracker/github.md");
const LOCAL: &str = include_str!("tracker/local.md");
const TRIAGE_LABELS: &str = include_str!("tracker/triage-labels.md");

pub const TRACKER_DOC: &str = "docs/agents/issue-tracker.md";
pub const LABELS_DOC: &str = "docs/agents/triage-labels.md";

/// What `--tracker` and `--tracker-desc` asked for.
pub enum Choice {
    GitHub,
    Local,
    Other(Option<String>),
}

pub struct Tracker {
    /// Whether loopherd's built-in GitHub snippets fit; anything else gets a
    /// placeholder queue source.
    pub github: bool,
    pub labels: Labels,
    /// Docs init writes into the repo, relative to it.
    pub writes: Vec<(&'static str, String)>,
}

/// The tracker's label for each triage role the herd uses.
pub struct Labels {
    pub ready_for_agent: String,
    pub needs_info: String,
    pub ready_for_human: String,
}

impl Labels {
    fn roles(&self) -> [(&'static str, &str); 3] {
        [
            ("ready-for-agent", &self.ready_for_agent),
            ("needs-info", &self.needs_info),
            ("ready-for-human", &self.ready_for_human),
        ]
    }
}

impl Default for Labels {
    fn default() -> Labels {
        Labels {
            ready_for_agent: "ready-for-agent".into(),
            needs_info: "needs-info".into(),
            ready_for_human: "ready-for-human".into(),
        }
    }
}

/// The repo's tracker: from its doc when there is one, else from `choice`,
/// else asked on a terminal, else GitHub.
pub fn resolve(repo: &Path, choice: Option<Choice>) -> Result<Tracker> {
    let labels = read_labels(repo)?;
    let (github, writes) = match read(&repo.join(TRACKER_DOC))? {
        Some(_) if choice.is_some() => {
            bail!("{TRACKER_DOC} already names the tracker; drop --tracker")
        }
        Some(doc) => (names_github(&doc), vec![]),
        None => {
            let choice = match choice {
                Some(choice) => choice,
                None if interactive() => ask()?,
                None => Choice::GitHub,
            };
            let (github, doc) = match choice {
                Choice::GitHub => (true, GITHUB.to_string()),
                Choice::Local => (false, LOCAL.to_string()),
                Choice::Other(desc) => (false, other_doc(desc)?),
            };
            let mut writes = vec![(TRACKER_DOC, doc)];
            if fs::symlink_metadata(repo.join(LABELS_DOC)).is_err() {
                writes.push((LABELS_DOC, TRIAGE_LABELS.to_string()));
            }
            (github, writes)
        }
    };
    // Only GitHub's snippets put the labels in shell commands.
    if github {
        for (role, label) in labels.roles() {
            if !is_label(label) {
                bail!(
                    "{LABELS_DOC} maps {role} to {label:?}; init takes GitHub labels of \
                     letters, digits and - _ . : / only, not starting with -"
                );
            }
        }
    }
    Ok(Tracker {
        github,
        labels,
        writes,
    })
}

/// Today's built-in for each tracker doc the repo has, for `--diff`: the
/// tracker doc when its title names a built-in tracker, and the labels doc.
pub fn built_in(repo: &Path) -> Result<Vec<Doc>> {
    let mut docs = vec![];
    if let Some(doc) = read(&repo.join(TRACKER_DOC))? {
        let built_in = match title_kind(&doc).as_deref() {
            Some("github") => Some(GITHUB),
            Some("local markdown") => Some(LOCAL),
            _ => None,
        };
        docs.extend(built_in.map(|text| Doc::new(TRACKER_DOC, text.to_string())));
    }
    if read(&repo.join(LABELS_DOC))?.is_some() {
        docs.push(Doc::new(LABELS_DOC, TRIAGE_LABELS.to_string()));
    }
    Ok(docs)
}

fn other_doc(desc: Option<String>) -> Result<String> {
    let desc = match desc {
        Some(desc) if desc.trim().is_empty() => bail!("the tracker description is empty"),
        Some(desc) => desc,
        None if interactive() => loop {
            let answer = line("Describe how this repo tracks issues, in one paragraph: ")?;
            if !answer.trim().is_empty() {
                break answer;
            }
        },
        None => bail!("--tracker other needs --tracker-desc <text>"),
    };
    Ok(format!("# Issue tracker\n\n{}\n", desc.trim()))
}

/// Whether the doc's title is mattpocock-skills' GitHub one, in any case.
fn names_github(doc: &str) -> bool {
    title_kind(doc).as_deref() == Some("github")
}

/// The lowercased tracker a `# Issue tracker: <kind>` title names.
fn title_kind(doc: &str) -> Option<String> {
    let title = doc
        .trim_start_matches('\u{feff}')
        .lines()
        .find_map(|line| line.trim().strip_prefix("# "))?
        .trim()
        .to_ascii_lowercase();
    Some(title.strip_prefix("issue tracker:")?.trim().to_string())
}

fn read_labels(repo: &Path) -> Result<Labels> {
    let mut labels = Labels::default();
    let Some(doc) = read(&repo.join(LABELS_DOC))? else {
        return Ok(labels);
    };
    for line in doc.lines() {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().trim_matches('`').trim())
            .collect();
        let [role, label, ..] = cells[..] else {
            continue;
        };
        let role = role.to_ascii_lowercase();
        let slot = match role.as_str() {
            "ready-for-agent" => &mut labels.ready_for_agent,
            "needs-info" => &mut labels.needs_info,
            "ready-for-human" => &mut labels.ready_for_human,
            _ => continue,
        };
        if label.eq_ignore_ascii_case("blocked") {
            bail!("{LABELS_DOC} maps {role} to blocked, which is loopherd's own label");
        }
        *slot = label.to_string();
    }
    let roles = labels.roles();
    for (i, (role, label)) in roles.iter().enumerate() {
        if let Some((other, _)) = roles[i + 1..]
            .iter()
            .find(|(_, other)| other.eq_ignore_ascii_case(label))
        {
            bail!("{LABELS_DOC} maps {role} and {other} to one label, {label}");
        }
    }
    Ok(labels)
}

/// Safe unquoted in the herd's shell commands and inside its jq strings.
fn is_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('-')
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
}

pub(super) fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound && fs::symlink_metadata(path).is_err() =>
        {
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn ask() -> Result<Choice> {
    loop {
        let answer = line(&format!(
            "No {TRACKER_DOC}. Where does this repo track issues? \
             [github/local/other] (github): "
        ))?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "github" => return Ok(Choice::GitHub),
            "local" => return Ok(Choice::Local),
            "other" => return Ok(Choice::Other(None)),
            _ => eprintln!("answer github, local or other"),
        }
    }
}
