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

    /// Appends the doc to its file under `repo`, leaving the file as it was on failure.
    pub fn append_to(&self, repo: &Path) -> Result<()> {
        let path = repo.join(self.path);
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

/// What init writes into the repo, and the steps left to the human where
/// writing could reach outside it.
pub struct Plan {
    /// `AGENTS.md`'s last, so a failed append is the only write left to undo.
    pub docs: Vec<Doc>,
    pub steps: Vec<String>,
}

/// The docs the repo lacks. Refuses an `AGENTS.md` init would append to but
/// can't write, so that fails before any change.
pub fn missing(repo: &Path, herd: Herd) -> Result<Plan> {
    let mut docs = vec![];
    let mut steps = vec![];
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
        match lands_inside(repo, LOOPHERD_DOC)? {
            true => docs.push(Doc::new(LOOPHERD_DOC, text)),
            false => steps.push(format!(
                "{LOOPHERD_DOC} would land outside the repo through a symlink, so init \
                 didn't write it; write it in the repo by hand."
            )),
        }
    }
    let agents_md = repo.join(AGENTS_MD);
    let linked = fs::symlink_metadata(&agents_md).is_ok_and(|m| m.file_type().is_symlink());
    if linked {
        // Appending through a link could change a file outside the repo.
        let has_section = fs::read(&agents_md)
            .is_ok_and(|bytes| has_loopherd_section(&String::from_utf8_lossy(&bytes)));
        if !has_section {
            steps.push(format!(
                "{AGENTS_MD} is a symlink, so init left it alone; add a loopherd section \
                 pointing at {LOOPHERD_DOC} and docs/agents/issue-tracker.md to it by hand."
            ));
        }
        return Ok(Plan { docs, steps });
    }
    match read(&agents_md)? {
        None => docs.push(Doc::new(
            AGENTS_MD,
            format!("# Agent instructions\n\n{SECTION}"),
        )),
        Some(agents) if has_loopherd_section(&agents) => {}
        Some(agents) => {
            fs::OpenOptions::new()
                .append(true)
                .open(&agents_md)
                .with_context(|| format!("write {}", agents_md.display()))?;
            docs.push(Doc {
                path: AGENTS_MD,
                text: appended(&agents),
                append: true,
            });
        }
    }
    Ok(Plan { docs, steps })
}

/// The section to append to `existing`, a blank line apart and in its line endings.
fn appended(existing: &str) -> String {
    let nl = match existing.contains("\r\n") {
        true => "\r\n",
        false => "\n",
    };
    let gap = match existing {
        "" => "",
        text if text.ends_with("\n\n") || text.ends_with("\n\r\n") => "",
        text if text.ends_with('\n') => nl,
        _ => &format!("{nl}{nl}"),
    };
    format!("{gap}{}", SECTION.replace('\n', nl))
}

/// Whether `rel`'s directory, or its nearest existing ancestor, resolves
/// inside the repo.
fn lands_inside(repo: &Path, rel: &str) -> Result<bool> {
    let root = repo
        .canonicalize()
        .with_context(|| format!("read {}", repo.display()))?;
    let mut dir = repo.join(rel);
    while dir.pop() {
        match dir.canonicalize() {
            Ok(dir) => return Ok(dir.starts_with(&root)),
            // A dangling link may yet be created outside.
            Err(_) if fs::symlink_metadata(&dir).is_ok() => return Ok(false),
            Err(_) => {}
        }
    }
    Ok(false)
}

/// Whether any heading outside a code block names loopherd, in any case:
/// an ATX `#` heading or a setext one underlined with `=` or `-`.
fn has_loopherd_section(text: &str) -> bool {
    let mut fence: Option<(char, usize)> = None;
    let mut paragraph = String::new();
    // In a list item or blockquote, where an underline can't make a heading.
    let mut container = false;
    let names = |title: &str| title.to_ascii_lowercase().contains("loopherd");
    for line in text.trim_start_matches('\u{feff}').lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let code = indent >= 4 || line.starts_with('\t');
        let line = line.trim();
        let run = |c: char| line.len() - line.trim_start_matches(c).len();
        if let Some((c, len)) = fence {
            if !code && run(c) >= len && line.trim_start_matches(c).is_empty() {
                fence = None;
            }
            continue;
        }
        if code {
            if !paragraph.is_empty() && !container {
                paragraph.push_str(line);
                paragraph.push('\n');
            }
            continue;
        }
        let opener = ['`', '~']
            .into_iter()
            .find(|&c| run(c) >= 3 && (c == '~' || !line.trim_start_matches(c).contains('`')));
        if let Some(c) = opener {
            fence = Some((c, run(c)));
            paragraph.clear();
            container = false;
            continue;
        }
        let underline = ['=', '-']
            .into_iter()
            .any(|c| run(c) > 0 && line.trim_start_matches(c).is_empty());
        if underline {
            if names(&paragraph) {
                return true;
            }
            paragraph.clear();
            continue;
        }
        let title = line.trim_start_matches('#');
        let level = line.len() - title.len();
        if (1..=6).contains(&level) && (title.is_empty() || title.starts_with(char::is_whitespace))
        {
            if names(title) {
                return true;
            }
            paragraph.clear();
            container = false;
            continue;
        }
        if line.is_empty() {
            paragraph.clear();
            container = false;
        } else if container || opens_container(line) {
            paragraph.clear();
            container = true;
        } else {
            paragraph.push_str(line);
            paragraph.push('\n');
        }
    }
    false
}

/// Whether `line` starts a blockquote or a list item.
fn opens_container(line: &str) -> bool {
    let marker_then_space = |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t']);
    if line.starts_with('>') {
        return true;
    }
    if let Some(rest) = line.strip_prefix(['-', '*', '+']) {
        return marker_then_space(rest);
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (1..=9).contains(&digits)
        && line[digits..]
            .strip_prefix(['.', ')'])
            .is_some_and(marker_then_space)
}

/// Refuses a new doc whose nearest existing directory init can't write in.
pub fn check_writable(repo: &Path, docs: &[Doc]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    for doc in docs.iter().filter(|doc| !doc.append) {
        let path = repo.join(doc.path);
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
