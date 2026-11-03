//! The loopherd skill: the shepherd's and lead's guide, printed by
//! `loopherd --skill` and installed by `install-hook` as a Claude skill.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const SKILL: &str = include_str!("skill.md");

/// Where the skill goes under a Claude skills directory.
pub fn path(skills_dir: &Path) -> PathBuf {
    skills_dir.join("loopherd").join("SKILL.md")
}

/// Writes the skill, overwriting what's there. Returns whether it changed.
pub fn install(skills_dir: &Path) -> Result<bool> {
    let path = path(skills_dir);
    match fs::read(&path) {
        Ok(bytes) if bytes == SKILL.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let dir = path.parent().expect("the skill sits in a directory");
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(&path, SKILL).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Deletes the skill and its directory if that leaves it empty. Returns
/// whether there was one.
pub fn remove(skills_dir: &Path) -> Result<bool> {
    let path = path(skills_dir);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
    }
    // Fails while the user keeps their own files there, which is the point.
    let _ = fs::remove_dir(path.parent().expect("the skill sits in a directory"));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Kind;
    use clap::ValueEnum;

    fn frontmatter() -> Vec<&'static str> {
        let rest = SKILL.strip_prefix("---\n").expect("opens with frontmatter");
        let (front, _) = rest.split_once("\n---\n").expect("frontmatter closes");
        front.lines().collect()
    }

    #[test]
    fn opens_with_name_and_description_frontmatter() {
        let front = frontmatter();
        assert!(front.contains(&"name: loopherd"), "{front:?}");
        assert!(
            front.iter().any(|l| l.starts_with("description: ")),
            "{front:?}"
        );
    }

    #[test]
    fn has_shepherd_and_lead_sections() {
        let headings: Vec<&str> = SKILL.lines().filter(|l| l.starts_with("## ")).collect();
        assert!(headings.contains(&"## Shepherd"), "{headings:?}");
        assert!(headings.contains(&"## Lead"), "{headings:?}");
    }

    #[test]
    fn names_nothing_from_this_repo() {
        for word in [
            "lh-",
            "LH-",
            "Trello",
            "trello",
            "junderdo",
            "loopherd-cards",
            "docs/",
            "AGENTS.md",
            "cargo",
        ] {
            assert!(!SKILL.contains(word), "the skill names {word:?}");
        }
    }

    /// The second word of each `| \`<member> <kind> …\` |` row in the line table.
    fn table_kinds() -> Vec<&'static str> {
        SKILL
            .lines()
            .filter_map(|l| l.trim().strip_prefix("| `"))
            .filter_map(|l| l.split_once('`').map(|(cell, _)| cell))
            .filter_map(|cell| cell.split_whitespace().nth(1))
            .collect()
    }

    #[test]
    fn the_line_table_covers_every_line_watch_prints() {
        let reports = Kind::value_variants()
            .iter()
            .map(|k| k.to_possible_value().unwrap().get_name().to_owned());
        let others = [
            "paused",
            "finished",
            "failed",
            "blocked",
            "gone",
            "merged",
            "claimable",
        ]
        .map(String::from);
        let table = table_kinds();
        for kind in reports.chain(others) {
            assert!(table.contains(&kind.as_str()), "no {kind} row in {table:?}");
        }
    }

    #[test]
    fn install_writes_the_skill_byte_equal_and_reruns_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(install(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
        assert!(!install(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
    }

    #[test]
    fn install_overwrites_an_edited_skill() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("loopherd")).unwrap();
        fs::write(path(dir.path()), "old").unwrap();
        assert!(install(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
    }

    #[test]
    fn remove_deletes_the_skill_and_its_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        assert!(remove(dir.path()).unwrap());
        assert!(!dir.path().join("loopherd").exists());
        assert!(!remove(dir.path()).unwrap());
    }

    #[test]
    fn remove_keeps_other_files_beside_the_skill() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        fs::write(dir.path().join("loopherd/notes.md"), "mine").unwrap();
        assert!(remove(dir.path()).unwrap());
        assert!(!path(dir.path()).exists());
        assert!(dir.path().join("loopherd/notes.md").exists());
    }
}
