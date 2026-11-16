//! The repo's instructions for workers: a loopherd section in `AGENTS.md`
//! and `docs/agents/loopherd.md`, each written only when missing.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use super::tracker::read;

const SECTION: &str = include_str!("agents/section.md");
const LOOPHERD: &str = include_str!("agents/loopherd.md");

const AGENTS_MD: &str = "AGENTS.md";
const LOOPHERD_DOC: &str = "docs/agents/loopherd.md";

/// A file init writes into the repo, uncommitted.
pub struct Doc {
    pub path: &'static str,
    pub text: String,
    /// Added to the end of an existing file rather than written as a new one.
    pub append: bool,
}

impl Doc {
    pub fn new(path: &'static str, text: String) -> Doc {
        Doc {
            path,
            text,
            append: false,
        }
    }

    pub fn done(&self) -> String {
        match self.append {
            true => format!("added a loopherd section to {} (uncommitted)", self.path),
            false => format!("wrote {} (uncommitted)", self.path),
        }
    }

    pub fn planned(&self) -> String {
        match self.append {
            true => format!("a loopherd section in {}", self.path),
            false => self.path.to_string(),
        }
    }

    /// Writes the doc under `repo`. An append that fails leaves the file as it was.
    pub fn write(&self, repo: &Path) -> Result<()> {
        let path = repo.join(self.path);
        if !self.append {
            return super::write_new(&path, &self.text);
        }
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("write {}", path.display()))?;
        let len = file.metadata()?.len();
        if let Err(e) = file.write_all(self.text.as_bytes()) {
            let _ = file.set_len(len);
            return Err(e).with_context(|| format!("write {}", path.display()));
        }
        Ok(())
    }
}

/// The values `docs/agents/loopherd.md` names.
pub struct Herd<'a> {
    pub account: &'a str,
    pub branch: &'a str,
    pub checks: &'a [String],
    pub needs_info: &'a str,
}

/// What init writes into the repo and, for a linked `AGENTS.md`, the step
/// left to the human.
pub struct Plan {
    /// `AGENTS.md`'s last, so a failed append is the only write left to undo.
    pub docs: Vec<Doc>,
    pub step: Option<String>,
}

/// The docs the repo lacks.
pub fn missing(repo: &Path, herd: Herd) -> Result<Plan> {
    let mut docs = vec![];
    let mut step = None;
    if fs::symlink_metadata(repo.join(LOOPHERD_DOC)).is_err() {
        let checks: Vec<String> = herd.checks.iter().map(|c| format!("- `{c}`")).collect();
        let text = super::fill(
            LOOPHERD,
            &[
                ("ACCOUNT", herd.account),
                ("BRANCH", herd.branch),
                ("CHECKS", &checks.join("\n")),
                ("NEEDS_INFO", herd.needs_info),
            ],
        );
        docs.push(Doc::new(LOOPHERD_DOC, text));
    }
    let agents_md = repo.join(AGENTS_MD);
    let linked = fs::symlink_metadata(&agents_md).is_ok_and(|m| m.file_type().is_symlink());
    match read(&agents_md)? {
        // Appending through a link could change a file outside the repo.
        Some(agents) if linked && !has_loopherd_section(&agents) => {
            step = Some(format!(
                "{AGENTS_MD} is a symlink, so init left it alone; add a loopherd section \
                 pointing at {LOOPHERD_DOC} and docs/agents/issue-tracker.md to it by hand."
            ));
        }
        None => docs.push(Doc::new(
            AGENTS_MD,
            format!("# Agent instructions\n\n{SECTION}"),
        )),
        Some(agents) if has_loopherd_section(&agents) => {}
        Some(agents) => {
            let gap = match agents.as_str() {
                "" => "",
                text if text.ends_with("\n\n") => "",
                text if text.ends_with('\n') => "\n",
                _ => "\n\n",
            };
            docs.push(Doc {
                path: AGENTS_MD,
                text: format!("{gap}{SECTION}"),
                append: true,
            });
        }
    }
    Ok(Plan { docs, step })
}

/// Whether any ATX heading outside a fenced or indented code block names
/// loopherd, in any case.
fn has_loopherd_section(text: &str) -> bool {
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        let indent = line.len() - line.trim_start_matches(' ').len();
        let line = line.trim_start();
        if let Some(open) = fence {
            if line.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if indent >= 4 || line.starts_with('\t') {
            continue;
        }
        if line.starts_with("```") || line.starts_with("~~~") {
            fence = Some(&line[..3]);
            continue;
        }
        let title = line.trim_start_matches('#');
        let level = line.len() - title.len();
        if (1..=6).contains(&level)
            && (title.is_empty() || title.starts_with(char::is_whitespace))
            && title.to_ascii_lowercase().contains("loopherd")
        {
            return true;
        }
    }
    false
}
