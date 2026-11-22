//! The loopherd skill: the shepherd's and lead's guide, printed by
//! `loopherd --skill` and installed by `install-hook` as a Claude skill.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

pub const SKILL: &str = include_str!("skill.md");

/// The `## <name>` section, matched case-insensitively, up to the next `## `.
pub fn section(name: &str) -> Option<&'static str> {
    let starts = section_starts();
    let i = starts
        .iter()
        .position(|&(_, title)| title.eq_ignore_ascii_case(name))?;
    let end = starts.get(i + 1).map_or(SKILL.len(), |&(at, _)| at);
    Some(&SKILL[starts[i].0..end])
}

/// Each section's name, lowercased, as `section` takes it.
pub fn section_names() -> Vec<String> {
    section_starts()
        .into_iter()
        .map(|(_, title)| title.to_lowercase())
        .collect()
}

fn section_starts() -> Vec<(usize, &'static str)> {
    let mut at = 0;
    let mut fenced = false;
    let mut starts = Vec::new();
    for line in SKILL.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if let Some(title) = line.strip_prefix("## ").filter(|_| !fenced) {
            starts.push((at, title.trim_end()));
        }
        at += line.len();
    }
    starts
}

/// Where `install-hook` puts the skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// `<dir>/loopherd/SKILL.md`, written in place: `--skills-dir`.
    Direct(PathBuf),
    /// The skill in `<real>/loopherd`, and `<link>/loopherd` a relative
    /// symlink to it, the way the `skills` CLI installs a skill.
    Linked { real: PathBuf, link: PathBuf },
}

impl Layout {
    /// `--skills-dir` wins; `--settings` keeps both dirs in its file's directory,
    /// unless that is the claude dir; otherwise `$HOME/.agents/skills` linked from
    /// `<claude dir>/skills`. `None` when that needs a `HOME` that isn't set.
    pub fn resolve(
        skills_dir: Option<PathBuf>,
        settings: Option<&Path>,
        home: Option<PathBuf>,
        claude_dir: Option<PathBuf>,
    ) -> Result<Option<Layout>> {
        if let Some(dir) = skills_dir {
            return Ok(Some(Layout::Direct(dir)));
        }
        if let Some(settings) = settings {
            let settings = std::path::absolute(settings)
                .with_context(|| format!("resolve {}", settings.display()))?;
            let dir = settings.parent().expect("a file has a parent");
            if claude_dir
                .as_deref()
                .is_none_or(|claude| !same_dir(dir, claude))
            {
                return Ok(Some(Layout::Linked {
                    real: dir.join(".agents/skills"),
                    link: dir.join("skills"),
                }));
            }
        }
        Ok(home.zip(claude_dir).map(|(home, claude)| Layout::Linked {
            real: home.join(".agents/skills"),
            link: claude.join("skills"),
        }))
    }

    /// The skill file loopherd writes.
    pub fn skill(&self) -> PathBuf {
        match self {
            Layout::Direct(dir) | Layout::Linked { real: dir, .. } => path(dir),
        }
    }
}

/// What sits at `<link dir>/loopherd`.
#[derive(Debug, PartialEq, Eq)]
pub enum Link {
    Ours,
    Missing,
    /// A real directory holding only loopherd's SKILL.md, from before the symlinked layout.
    OldCopy,
    /// Anything else: the human's, never touched.
    Foreign(String),
}

/// Classifies `<link dir>/loopherd` against `<real dir>/loopherd`.
pub fn link_state(real: &Path, link: &Path) -> Link {
    let (real, link) = (real.join(NAME), link.join(NAME));
    let meta = match fs::symlink_metadata(&link) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Link::Missing,
        Err(e) => return Link::Foreign(format!("can't be read: {e}")),
    };
    if meta.is_symlink() {
        let target = fs::read_link(&link).unwrap_or_default();
        let same = match (fs::canonicalize(&link), fs::canonicalize(&real)) {
            (Ok(a), Ok(b)) => a == b,
            // The skill is gone, so judge the link by the target install-hook writes.
            _ if fs::symlink_metadata(&real).is_err() => {
                target == relative(&resolved(link.parent().unwrap()), &resolved(&real))
            }
            _ => false,
        };
        return match same {
            true => Link::Ours,
            false => Link::Foreign(format!("points to {}", target.display())),
        };
    }
    if !meta.is_dir() {
        return Link::Foreign("isn't a directory or symlink".into());
    }
    match holds_only_our_skill(&link) {
        true => Link::OldCopy,
        false => Link::Foreign("holds files that aren't loopherd's SKILL.md".into()),
    }
}

/// What `install` and `remove` did.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The skill file was written, or removed.
    pub skill: bool,
    /// The link was made, replaced or removed.
    pub link: bool,
    /// Why `<link dir>/loopherd` was left alone.
    pub left_alone: Option<String>,
}

pub fn install(layout: &Layout) -> Result<Outcome> {
    let (real, link) = match layout {
        Layout::Direct(dir) => return Ok(direct(write(dir)?)),
        Layout::Linked { real, link } => (real, link),
    };
    refuse_a_symlinked_skill_dir(real)?;
    fs::create_dir_all(link).with_context(|| format!("create {}", link.display()))?;
    // Concurrent installs would each see the other's half-made link.
    let _lock = fs::File::open(link)
        .and_then(|dir| crate::lock::Lock::wait(dir, crate::lock::BRIEF))
        .with_context(|| format!("lock {}", link.display()))?;
    let mut outcome = direct(write(real)?);
    let state = link_state(real, link);
    match &state {
        Link::Ours => return Ok(outcome),
        Link::Foreign(why) => {
            outcome.left_alone = Some(why.clone());
            return Ok(outcome);
        }
        Link::Missing | Link::OldCopy => {}
    }
    let at = link.join(NAME);
    // The kernel resolves a relative link from its physical dir, so both sides
    // must be free of symlinks.
    let (from, to) = (canonical(link)?, canonical(&real.join(NAME))?);
    let target = relative(&from, &to);
    let placed = match state {
        Link::OldCopy => replace_old_copy(&target, &at),
        _ => std::os::unix::fs::symlink(&target, &at)
            .with_context(|| format!("link {}", at.display())),
    };
    match placed {
        Ok(()) => outcome.link = true,
        // Something else made a link there first; ours is as good as made.
        Err(_) if link_state(real, link) == Link::Ours => {}
        Err(e) => return Err(e),
    }
    Ok(outcome)
}

/// Makes the link under a temp name and renames it over the old copy, moved
/// aside first so a failed rename can put it back.
fn replace_old_copy(target: &Path, at: &Path) -> Result<()> {
    let tmp = at.with_file_name(format!(".{NAME}.{}.tmp", unique()));
    std::os::unix::fs::symlink(target, &tmp).with_context(|| format!("link {}", tmp.display()))?;
    let aside = at.with_file_name(format!(".{NAME}.{}.old", unique()));
    let swapped = fs::rename(at, &aside)
        .with_context(|| format!("move {} aside", at.display()))
        .and_then(|()| {
            fs::rename(&tmp, at).or_else(|e| {
                let _ = fs::rename(&aside, at);
                Err(e).with_context(|| format!("link {}", at.display()))
            })
        });
    let _ = fs::remove_file(&tmp);
    swapped?;
    let _ = fs::remove_file(aside.join("SKILL.md"));
    let _ = fs::remove_dir(&aside);
    Ok(())
}

pub fn uninstall(layout: &Layout) -> Result<Outcome> {
    let (real, link) = match layout {
        Layout::Direct(dir) => return Ok(direct(remove(dir)?)),
        Layout::Linked { real, link } => (real, link),
    };
    refuse_a_symlinked_skill_dir(real)?;
    let at = link.join(NAME);
    let mut outcome = direct(false);
    match link_state(real, link) {
        Link::Ours => {
            fs::remove_file(&at).with_context(|| format!("remove {}", at.display()))?;
            outcome.link = true;
        }
        Link::OldCopy => outcome.link = remove(link)?,
        Link::Missing => {}
        Link::Foreign(why) => outcome.left_alone = Some(why),
    }
    outcome.skill = remove(real)?;
    Ok(outcome)
}

/// `<real>/loopherd` as a symlink is the human's; writing or deleting through it
/// would change whatever it points to.
fn refuse_a_symlinked_skill_dir(real: &Path) -> Result<()> {
    let dir = real.join(NAME);
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.is_symlink()) {
        anyhow::bail!(
            "{} is a symlink, so loopherd won't write or delete through it; move it aside and rerun",
            dir.display()
        );
    }
    Ok(())
}

/// A name no other install, in this process or another, picks.
fn unique() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// `path` with its longest existing prefix canonicalized, so it compares
/// equal to the same place reached through symlinks even if its end is missing.
fn resolved(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut base = path.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = fs::canonicalize(base) {
            return rest.iter().rev().fold(canonical, |p, name| p.join(name));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return path,
        }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    resolved(a) == resolved(b)
}

fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))
}

fn direct(skill: bool) -> Outcome {
    Outcome {
        skill,
        link: false,
        left_alone: None,
    }
}

const NAME: &str = "loopherd";

fn holds_only_our_skill(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    names == ["SKILL.md"]
        && fs::symlink_metadata(dir.join("SKILL.md")).is_ok_and(|m| m.is_file())
        && fs::read_to_string(dir.join("SKILL.md"))
            .is_ok_and(|text| text.starts_with("---\nname: loopherd\n"))
}

/// `to` relative to the directory `from`, both absolute, by their components.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n(Component::ParentDir, from.len() - common)
        .chain(to[common..].iter().copied())
        .collect()
}

/// Where the skill goes under a skills directory.
pub fn path(skills_dir: &Path) -> PathBuf {
    skills_dir.join(NAME).join("SKILL.md")
}

/// Writes the skill, overwriting what's there. Returns whether it changed.
fn write(skills_dir: &Path) -> Result<bool> {
    let path = path(skills_dir);
    match fs::read(&path) {
        Ok(bytes) if bytes == SKILL.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let dir = path.parent().expect("the skill sits in a directory");
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // A rename replaces a symlinked SKILL.md rather than writing through to its target.
    let tmp = dir.join(format!(".SKILL.md.{}.tmp", unique()));
    fs::write(&tmp, SKILL).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(true)
}

/// Deletes the skill and its directory if that leaves it empty. Returns
/// whether there was one.
fn remove(skills_dir: &Path) -> Result<bool> {
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
    fn the_shepherd_has_a_setting_up_a_project_section() {
        let section = SKILL
            .split_once("\n### Setting up a project\n")
            .map(|(_, rest)| rest.split_once("\n#").map_or(rest, |(s, _)| s))
            .expect("a Setting up a project section")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for step in [
            "loopherd init",
            "loopherd herds",
            "loopherd feed <herd> --dry-run",
            "read, claim, escalate a question and close the task",
            "review and commit",
            "trust dialog",
            "loopherd lead <herd>",
        ] {
            assert!(section.contains(step), "the section misses {step:?}");
        }
    }

    fn section(start: &str, end: &str) -> String {
        let (_, rest) = SKILL.split_once(start).expect("section start");
        let (body, _) = rest.split_once(end).expect("section end");
        body.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn the_leads_restart_and_wake_advice_reads_its_section_and_recent_reports() {
        let lead = super::section("lead").unwrap();
        let bullet = |name: &str| {
            let start = lead.find(&format!("- **{name}**")).unwrap();
            let rest = &lead[start + 2..];
            rest[..rest.find("\n- **").unwrap_or(rest.len())].replace("\n  ", " ")
        };
        for name in ["After a restart", "On each wake"] {
            let advice = bullet(name);
            assert!(advice.contains("`loopherd status`"), "{advice}");
            assert!(
                advice.contains("`loopherd reports --all --tail 40`"),
                "{advice}"
            );
            assert!(
                advice.contains("`loopherd --skill --section lead`"),
                "{advice}"
            );
        }
    }

    #[test]
    fn the_shepherds_wake_step_reads_recent_reports_not_the_whole_day() {
        let step = section("3. **On wake,**", "| Line | Means |");
        for text in [
            "`loopherd reports --all --tail 40`",
            "`--since <time>`",
            "plain `--all`",
            "`--all <name>` for one member",
        ] {
            assert!(step.contains(text), "missing {text}: {step}");
        }
    }

    #[test]
    fn the_lead_section_carries_the_task_life_a_restarted_lead_needs() {
        let lead = super::section("lead")
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for step in [
            "**Task life, in brief**",
            "`loopherd feed <herd> --dry-run`",
            "read-only reviewer subagent",
            "`gh pr view <n> --json state`",
            "close out the task in the tracker",
            "`git worktree remove --force`",
            "`$XDG_STATE_HOME/loopherd/handoffs/<your name>/handoff.md`",
            "`## State`",
            "`<scratch>/pr<n>/review-<round>.md`",
            "`pass <head sha>: <m> minor, <k> nit`",
            "`fail <head sha>: <b> blocking, <m> minor, <k> nit`",
            "`gh pr comment <n> --body-file <scratch>/pr<n>/review-<round>-blocking.md`",
            "until the PR merges or closes",
            "current review file",
            "`review-<round - 1>.md`",
            "`git diff <its head sha>..<new head>`",
            "later rounds pass `model: sonnet`",
        ] {
            assert!(lead.contains(step), "missing {step}:\n{lead}");
        }
    }

    #[test]
    fn reviewers_write_findings_to_a_file_forwarded_as_a_pr_comment() {
        let verify = section("4. **Verify**", "5. **After merge**");
        for rule in [
            "<scratch>/pr<n>/review-<round>.md",
            "`pass <head sha>: <m> minor, <k> nit`",
            "`fail <head sha>: <b> blocking, <m> minor, <k> nit`",
            "review-<round>-<axis>.md",
            "`<scratch>/pr<n>/review-<round>-blocking.md`",
            "`gh pr comment <n> --body-file`",
            "can't read your scratch dir",
        ] {
            assert!(verify.contains(rule), "Verify misses {rule:?}");
        }
        let handoffs = section("\n### Handoffs\n", "\n### Your own handoff\n");
        assert!(
            handoffs.contains("current review file"),
            "Handoffs misses the review file"
        );
    }

    #[test]
    fn later_review_rounds_run_the_reviewer_on_sonnet() {
        let verify = section("4. **Verify**", "5. **After merge**");
        assert!(
            verify.contains(
                "A first review uses the reviewer as defined; later rounds pass `model: sonnet`"
            ),
            "{verify}"
        );
    }

    #[test]
    fn a_wake_re_arms_with_its_first_action_and_reads_the_output_once() {
        let watch = section("2. **Watch.**", "3. **On wake,**");
        let heartbeat = section("- **Heartbeat.**", "- **After a restart**");
        for (name, text) in [("Watch", watch), ("Heartbeat", heartbeat)] {
            for rule in [
                "run `loopherd status` and re-arm in the same message as your first action",
                "unless the wake needs a handoff",
                "Read the watch's output once",
            ] {
                assert!(text.contains(rule), "{name} misses {rule:?}");
            }
        }
    }

    #[test]
    fn names_nothing_from_this_repo() {
        // init writes these into any repo, so the skill may name them.
        let skill = ["docs/agents/issue-tracker.md", "docs/agents/loopherd.md"]
            .iter()
            .fold(SKILL.to_owned(), |skill, path| skill.replace(path, ""));
        for word in [
            "lh-",
            "LH-",
            "Trello",
            "trello",
            "junderdo",
            "loopherd-cards",
            "docs/",
            "cargo",
        ] {
            assert!(!skill.contains(word), "the skill names {word:?}");
        }
    }

    /// What follows the first word of each `| \`<member> <kind> …\` |` row in the line table.
    fn table_kinds() -> Vec<&'static str> {
        SKILL
            .lines()
            .filter_map(|l| l.trim().strip_prefix("| `"))
            .filter_map(|l| l.split_once('`').map(|(cell, _)| cell))
            .filter_map(|cell| cell.split_once(' ').map(|(_, kind)| kind))
            .collect()
    }

    #[test]
    fn the_line_table_covers_every_line_watch_prints() {
        let reports = Kind::value_variants()
            .iter()
            .map(|k| k.to_possible_value().unwrap().get_name().to_owned());
        let others = crate::watch::line::ALL.map(String::from);
        let table = table_kinds();
        for kind in reports.chain(others) {
            let row = |cell: &&str| {
                cell.strip_prefix(kind.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
            };
            assert!(table.iter().any(row), "no {kind} row in {table:?}");
        }
    }

    #[test]
    fn install_writes_the_skill_byte_equal_and_reruns_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
        assert!(!write(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
    }

    #[test]
    fn install_overwrites_an_edited_skill() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("loopherd")).unwrap();
        fs::write(path(dir.path()), "old").unwrap();
        assert!(write(dir.path()).unwrap());
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
    }

    #[test]
    fn install_replaces_a_symlinked_skill_and_leaves_its_target_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-skill.md");
        fs::write(&target, "mine").unwrap();
        fs::create_dir_all(dir.path().join("loopherd")).unwrap();
        std::os::unix::fs::symlink(&target, path(dir.path())).unwrap();

        assert!(write(dir.path()).unwrap());

        assert_eq!(fs::read_to_string(&target).unwrap(), "mine");
        assert_eq!(fs::read(path(dir.path())).unwrap(), SKILL.as_bytes());
        assert!(!fs::symlink_metadata(path(dir.path())).unwrap().is_symlink());
    }

    #[test]
    fn remove_deletes_the_skill_and_its_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path()).unwrap();
        assert!(remove(dir.path()).unwrap());
        assert!(!dir.path().join("loopherd").exists());
        assert!(!remove(dir.path()).unwrap());
    }

    #[test]
    fn remove_keeps_other_files_beside_the_skill() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path()).unwrap();
        fs::write(dir.path().join("loopherd/notes.md"), "mine").unwrap();
        assert!(remove(dir.path()).unwrap());
        assert!(!path(dir.path()).exists());
        assert!(dir.path().join("loopherd/notes.md").exists());
    }

    fn linked(dir: &Path) -> Layout {
        Layout::Linked {
            real: dir.join(".agents/skills"),
            link: dir.join(".claude/skills"),
        }
    }

    #[test]
    fn a_skill_md_that_isnt_loopherds_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join(".claude/skills/loopherd");
        fs::create_dir_all(&at).unwrap();
        fs::write(at.join("SKILL.md"), "---\nname: other\n---\n").unwrap();

        let outcome = install(&linked(dir.path())).unwrap();

        assert!(outcome.left_alone.is_some(), "{outcome:?}");
        assert_eq!(
            fs::read_to_string(at.join("SKILL.md")).unwrap(),
            "---\nname: other\n---\n"
        );
    }

    #[test]
    fn a_file_where_the_link_goes_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();
        fs::write(dir.path().join(".claude/skills/loopherd"), "mine").unwrap();

        let outcome = install(&linked(dir.path())).unwrap();

        assert_eq!(
            outcome.left_alone.as_deref(),
            Some("isn't a directory or symlink")
        );
    }

    #[test]
    fn an_absolute_link_to_the_skill_counts_as_ours() {
        let dir = tempfile::tempdir().unwrap();
        let layout = linked(dir.path());
        install(&layout).unwrap();
        let at = dir.path().join(".claude/skills/loopherd");
        fs::remove_file(&at).unwrap();
        std::os::unix::fs::symlink(dir.path().join(".agents/skills/loopherd"), &at).unwrap();

        assert_eq!(
            link_state(
                &dir.path().join(".agents/skills"),
                &dir.path().join(".claude/skills")
            ),
            Link::Ours
        );
        assert!(uninstall(&layout).unwrap().link);
        assert!(fs::symlink_metadata(&at).is_err());
    }
    fn old_copy(dir: &Path) {
        let at = dir.join(".claude/skills/loopherd");
        fs::create_dir_all(&at).unwrap();
        fs::write(at.join("SKILL.md"), "---\nname: loopherd\n---\nold\n").unwrap();
    }

    fn installs_at_once(layout: &Layout) -> Vec<Outcome> {
        std::thread::scope(|s| {
            let runs: Vec<_> = (0..8).map(|_| s.spawn(|| install(layout))).collect();
            runs.into_iter()
                .map(|r| r.join().unwrap().unwrap())
                .collect()
        })
    }

    #[test]
    fn concurrent_installs_all_succeed_and_leave_one_link() {
        for migrate in [false, true] {
            for _ in 0..20 {
                let dir = tempfile::tempdir().unwrap();
                if migrate {
                    old_copy(dir.path());
                }
                let layout = linked(dir.path());

                let outcomes = installs_at_once(&layout);

                assert!(outcomes.iter().any(|o| o.link), "{outcomes:?}");
                assert!(outcomes.iter().all(|o| o.left_alone.is_none()));
                let (real, link) = (
                    dir.path().join(".agents/skills"),
                    dir.path().join(".claude/skills"),
                );
                assert_eq!(link_state(&real, &link), Link::Ours);
                let names: Vec<_> = fs::read_dir(&link)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect();
                assert_eq!(names, ["loopherd"]);
                let names: Vec<_> = fs::read_dir(real.join("loopherd"))
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect();
                assert_eq!(names, ["SKILL.md"]);
            }
        }
    }

    #[test]
    fn install_and_remove_refuse_a_symlinked_skill_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("dotfiles/loopherd");
        fs::create_dir_all(&mine).unwrap();
        fs::write(mine.join("SKILL.md"), "mine").unwrap();
        fs::create_dir_all(dir.path().join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(&mine, dir.path().join(".agents/skills/loopherd")).unwrap();
        let layout = linked(dir.path());

        let installed = install(&layout).unwrap_err().to_string();
        let removed = uninstall(&layout).unwrap_err().to_string();

        for err in [installed, removed] {
            assert!(err.contains("is a symlink"), "{err}");
        }
        assert_eq!(fs::read_to_string(mine.join("SKILL.md")).unwrap(), "mine");
        assert!(fs::symlink_metadata(dir.path().join(".claude/skills/loopherd")).is_err());
    }

    #[test]
    fn our_link_dangling_after_the_skill_is_deleted_is_still_ours_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = linked(dir.path());
        install(&layout).unwrap();
        fs::remove_dir_all(dir.path().join(".agents/skills/loopherd")).unwrap();
        let (real, link) = (
            dir.path().join(".agents/skills"),
            dir.path().join(".claude/skills"),
        );

        assert_eq!(link_state(&real, &link), Link::Ours);
        assert!(uninstall(&layout).unwrap().link);
        assert!(fs::symlink_metadata(link.join("loopherd")).is_err());
    }

    #[test]
    fn another_dangling_link_is_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join(".claude/skills");
        fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink("../../gone/loopherd", link.join("loopherd")).unwrap();

        let state = link_state(&dir.path().join(".agents/skills"), &link);

        assert_eq!(state, Link::Foreign("points to ../../gone/loopherd".into()));
    }

    fn resolve(
        settings: Option<&Path>,
        home: Option<&Path>,
        claude: Option<&Path>,
    ) -> Option<Layout> {
        Layout::resolve(None, settings, home.map(Into::into), claude.map(Into::into)).unwrap()
    }

    #[test]
    fn settings_in_the_claude_dir_take_the_home_layout() {
        let dir = tempfile::tempdir().unwrap();
        let (home, claude) = (dir.path().join("home"), dir.path().join("home/.claude"));
        fs::create_dir_all(&claude).unwrap();

        let layout = resolve(
            Some(&claude.join("settings.json")),
            Some(&home),
            Some(&claude),
        );

        assert_eq!(
            layout,
            Some(Layout::Linked {
                real: home.join(".agents/skills"),
                link: claude.join("skills")
            })
        );
    }

    #[test]
    fn settings_elsewhere_keep_both_dirs_beside_them_without_home() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");

        let layout = resolve(Some(&settings), None, Some(&dir.path().join("claude")));

        assert_eq!(
            layout,
            Some(Layout::Linked {
                real: dir.path().join(".agents/skills"),
                link: dir.path().join("skills")
            })
        );
    }

    #[test]
    fn without_home_the_home_layout_is_none() {
        let claude = Path::new("/cfg/claude");
        assert_eq!(resolve(None, None, Some(claude)), None);
        assert_eq!(
            resolve(Some(&claude.join("settings.json")), None, Some(claude)),
            None
        );
    }
}
