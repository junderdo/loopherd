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

/// The docs the repo lacks, `AGENTS.md`'s last so a failed append is the
/// only write left to undo.
pub fn missing(repo: &Path, herd: Herd) -> Result<Vec<Doc>> {
    let mut docs = vec![];
    if read(&repo.join(LOOPHERD_DOC))?.is_none() {
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
    match read(&repo.join(AGENTS_MD))? {
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
    Ok(docs)
}

/// Whether any Markdown heading names loopherd, in any case.
fn has_loopherd_section(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start_matches('\u{feff}').trim_start();
        let title = line.trim_start_matches('#');
        title.len() < line.len()
            && (title.is_empty() || title.starts_with(char::is_whitespace))
            && title.to_ascii_lowercase().contains("loopherd")
    })
}
