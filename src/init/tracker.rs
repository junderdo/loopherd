//! The repo's tracker doc, `docs/agents/issue-tracker.md`, and its
//! `triage-labels.md`, in mattpocock-skills' format: read when present,
//! written when not.

use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
    if let Some(doc) = read(&repo.join(TRACKER_DOC))? {
        if choice.is_some() {
            bail!("{TRACKER_DOC} already names the tracker; drop --tracker");
        }
        return Ok(Tracker {
            github: names_github(&doc),
            labels,
            writes: vec![],
        });
    }
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
    Ok(Tracker {
        github,
        labels,
        writes,
    })
}

fn other_doc(desc: Option<String>) -> Result<String> {
    let desc = match desc {
        Some(desc) => desc,
        None if interactive() => {
            prompt("Describe how this repo tracks issues, in one paragraph: ")?
        }
        None => bail!("--tracker other needs --tracker-desc <text>"),
    };
    let desc = desc.trim();
    if desc.is_empty() {
        bail!("the tracker description is empty");
    }
    Ok(format!("# Issue tracker\n\n{desc}\n"))
}

/// Whether the doc's title is mattpocock-skills' GitHub one.
fn names_github(doc: &str) -> bool {
    doc.lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .and_then(|title| title.trim().strip_prefix("Issue tracker:"))
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("github"))
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
        let slot = match role {
            "ready-for-agent" => &mut labels.ready_for_agent,
            "needs-info" => &mut labels.needs_info,
            "ready-for-human" => &mut labels.ready_for_human,
            _ => continue,
        };
        if !is_label(label) {
            bail!(
                "{LABELS_DOC} maps {role} to {label:?}; init takes labels of letters, digits \
                 and - _ . : / only, not starting with -"
            );
        }
        if label.eq_ignore_ascii_case("blocked") {
            bail!("{LABELS_DOC} maps {role} to blocked, which is loopherd's own label");
        }
        *slot = label.to_string();
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

fn read(path: &PathBuf) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn ask() -> Result<Choice> {
    loop {
        let answer = prompt(&format!(
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

fn prompt(question: &str) -> Result<String> {
    eprint!("{question}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("no answer on stdin");
    }
    Ok(line)
}
