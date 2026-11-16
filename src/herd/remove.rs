//! `loopherd herd remove`: delete a finished herd and its members' history.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Paths, expand_home, file_of, herd_files};
use crate::context::handoff_path;
use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger};
use crate::retire::open_tab;

#[derive(Debug, Default, Clone, Copy)]
pub struct Removal {
    /// Print what would go and change nothing.
    pub dry_run: bool,
    /// Keep the ledger entries and mailboxes, so `stats` still counts them.
    pub keep_history: bool,
}

/// Removes `herd`'s file, lead prompt and pause, and its members' ledger
/// entries, mailboxes and handoffs in loopherd's `state` dir, pushing a line
/// per thing removed or kept to `out`, including before an error. Refuses,
/// changing nothing, while any member is not retired or herdr shows its
/// agent or tab open, or herdr can't say. Never touches worktrees, branches
/// or tabs.
pub fn remove(
    paths: &Paths,
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    state: &Path,
    herd: &str,
    how: Removal,
    out: &mut Vec<String>,
) -> Result<()> {
    let file = file_of(&paths.herds, herd)?;
    let entries = ledger.entries()?;
    let members: Vec<&AgentRecord> = entries
        .values()
        .filter(|r| r.herd.as_deref() == Some(herd))
        .collect();
    let has_file = present(&file)?;
    if !has_file && members.is_empty() {
        bail!(
            "no herd {herd}: no {} and no ledger entries",
            file.display()
        );
    }
    refuse_open(herdr, herd, &entries, &members)?;
    for dir in ["reports", "handoffs"].map(|d| state.join(d)) {
        if dir.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            bail!("refused: {} is a symlink", dir.display());
        }
    }

    let verb = if how.dry_run {
        "would remove"
    } else {
        "removed"
    };
    let names: Vec<String> = members.iter().map(|r| r.name.clone()).collect();
    let mailboxes = !how.keep_history;
    if how.keep_history || how.dry_run || names.is_empty() {
        delete_all(&member_files(state, &names, mailboxes), verb, how, out)?;
        if mailboxes {
            out.extend(names.iter().map(|n| format!("{verb} ledger entry {n}")));
        }
    } else {
        let removed = ledger
            .remove_herd(herd, &mut |names| {
                delete_all(&member_files(state, names, true), verb, how, out)
            })
            .context("ledger")?;
        out.extend(removed.iter().map(|n| format!("{verb} ledger entry {n}")));
    }

    let mut own = vec![paths.paused.join(herd)];
    if has_file {
        match lead_prompt(paths, herd, &file) {
            Ok(Some(prompt)) => own.push(prompt),
            Ok(None) => {}
            Err(kept) => out.push(format!("kept {kept}")),
        }
        own.push(file);
    }
    delete_all(&own, verb, how, out)
}

/// Fails naming each member that is not retired or that herdr shows open,
/// with the command that closes it.
fn refuse_open(
    herdr: &dyn Herdr,
    herd: &str,
    entries: &BTreeMap<String, AgentRecord>,
    members: &[&AgentRecord],
) -> Result<()> {
    let mut open = Vec::new();
    for member in members {
        let why = match member.retired {
            false => Some("not retired".to_string()),
            true => {
                let mut others = entries.clone();
                others.remove(&member.name);
                open_tab(herdr, member, &others).with_context(|| {
                    format!(
                        "refused: can't ask herdr whether {}'s tab is closed",
                        member.name
                    )
                })?
            }
        };
        if let Some(why) = why {
            open.push(format!(
                "\n  loopherd retire --close {}  # {why}",
                member.name
            ));
        }
    }
    if !open.is_empty() {
        bail!(
            "refused: {herd} has members still open; close each first:{}",
            open.concat()
        );
    }
    Ok(())
}

/// Each member's handoffs, after its mailbox when `mailboxes`.
fn member_files(state: &Path, names: &[String], mailboxes: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for name in names {
        if mailboxes {
            files.push(state.join("reports").join(format!("{name}.jsonl")));
        }
        let handoff = handoff_path(state, name);
        let prev = handoff.with_extension("prev.md");
        files.extend([handoff, prev]);
    }
    files
}

fn delete_all(paths: &[PathBuf], verb: &str, how: Removal, out: &mut Vec<String>) -> Result<()> {
    for path in paths {
        if !present(path)? {
            continue;
        }
        if !how.dry_run {
            std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
        out.push(format!("{verb} {}", path.display()));
    }
    Ok(())
}

/// Whether `path` exists, itself and not a symlink's target; an error other
/// than not-found is passed on rather than read as absent.
fn present(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// The `[lead]` `prompt_file` of `herd`'s `file`, when it is a regular file
/// in the herds dir and no other herd file names it; `Err` says why one is kept.
fn lead_prompt(paths: &Paths, herd: &str, file: &Path) -> Result<Option<PathBuf>, String> {
    let Some(named) = prompt_files(paths, file).ok().and_then(|f| f.lead) else {
        return Ok(None);
    };
    let Ok(prompt) = named.canonicalize() else {
        return Ok(None);
    };
    if !prompt.is_file() {
        return Err(format!("{}: not a regular file", named.display()));
    }
    let herds = paths
        .herds
        .canonicalize()
        .map_err(|e| format!("{}: {e}", named.display()))?;
    if !prompt.starts_with(&herds) {
        return Err(format!(
            "{}: outside {}",
            named.display(),
            paths.herds.display()
        ));
    }
    let others = herd_files(&paths.herds).map_err(|e| format!("{}: {e:#}", named.display()))?;
    for other in others
        .iter()
        .filter(|o| o.file_stem() != Some(herd.as_ref()))
    {
        let names_it = match prompt_files(paths, other) {
            Ok(files) => files
                .all
                .iter()
                .any(|f| f.canonicalize().ok() == Some(prompt.clone())),
            Err(_) => true,
        };
        if names_it || other.canonicalize().ok() == Some(prompt.clone()) {
            return Err(format!("{}: {} names it", named.display(), other.display()));
        }
    }
    Ok(Some(named))
}

struct PromptFiles {
    lead: Option<PathBuf>,
    /// The lead's and every template's.
    all: Vec<PathBuf>,
}

/// Read leniently, so a herd file `herds` calls invalid still counts.
fn prompt_files(paths: &Paths, file: &Path) -> Result<PromptFiles> {
    let text = std::fs::read_to_string(file)?;
    let table: toml::Table = text.parse()?;
    let resolve = |t: &toml::Value| {
        t.get("prompt_file")
            .and_then(toml::Value::as_str)
            .map(|f| paths.herds.join(expand_home(f, &paths.home)))
    };
    let lead = table.get("lead").and_then(resolve);
    let templates = table.get("template").and_then(toml::Value::as_table);
    let mut all: Vec<PathBuf> = templates
        .into_iter()
        .flat_map(|t| t.values())
        .filter_map(resolve)
        .collect();
    all.extend(lead.clone());
    Ok(PromptFiles { lead, all })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;

    struct Setup {
        dir: tempfile::TempDir,
        ledger: MemLedger,
        herdr: FakeHerdr,
    }

    impl Setup {
        fn new() -> Setup {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("herds")).unwrap();
            Setup {
                dir,
                ledger: MemLedger::default(),
                herdr: FakeHerdr::new(&[("w1", "loopherd")]),
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.path(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn member(&mut self, name: &str, herd: &str, retired: bool, lead: bool) {
            let record: AgentRecord = serde_json::from_value(serde_json::json!({
                "name": name, "pane_id": "w1:p1", "session_id": null, "repo": "/r",
                "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
                "created": "2026-11-01T00:00:00Z", "herd": herd, "retired": retired,
                "role": if lead { Some("lead") } else { None },
            }))
            .unwrap();
            self.ledger.0.insert(name.into(), record);
        }

        fn remove(&mut self, herd: &str, how: Removal) -> Result<String> {
            let (out, result) = self.remove_partly(herd, how);
            result.map(|()| out)
        }

        /// The lines pushed, even before an error.
        fn remove_partly(&mut self, herd: &str, how: Removal) -> (String, Result<()>) {
            let paths = Paths::under(self.dir.path());
            let state = self.path("state");
            let mut out = vec![];
            let result = remove(
                &paths,
                &self.herdr,
                &mut self.ledger,
                &state,
                herd,
                how,
                &mut out,
            );
            (out.iter().map(|l| format!("{l}\n")).collect(), result)
        }

        fn names(&self) -> Vec<String> {
            self.ledger.0.keys().cloned().collect()
        }
    }

    #[test]
    fn refuses_naming_each_member_not_retired_lead_included_and_changes_nothing() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("lead-old", "old", false, true);
        s.member("a", "old", true, false);
        s.member("b", "old", false, false);
        s.write("state/reports/a.jsonl", "{}\n");

        let err = s.remove("old", Removal::default()).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: old has members still open; close each first:\n  \
             loopherd retire --close b  # not retired\n  \
             loopherd retire --close lead-old  # not retired"
        );
        assert!(s.path("herds/old.toml").exists());
        assert!(s.path("state/reports/a.jsonl").exists());
        assert_eq!(s.names(), ["a", "b", "lead-old"]);
    }

    const LED: &str = "repo = \"/r\"\n[lead]\nprompt_file = \"old-lead.md\"\n";

    fn finished_herd(s: &mut Setup) {
        s.write("herds/old.toml", LED);
        s.write("herds/old-lead.md", "lead the herd\n");
        s.write("paused/old", "{}");
        s.member("lead-old", "old", true, true);
        s.member("a", "old", true, false);
        for name in ["lead-old", "a"] {
            s.write(&format!("state/reports/{name}.jsonl"), "{}\n");
            s.write(&format!("state/handoffs/{name}.md"), "now\n");
            s.write(&format!("state/handoffs/{name}.prev.md"), "before\n");
        }
        s.member("other", "live", false, false);
        s.write("state/reports/other.jsonl", "{}\n");
        s.write("state/handoffs/other.md", "x\n");
        s.write("herds/live.toml", "repo = \"/r\"\n");
    }

    fn exists(s: &Setup, rels: &[&str]) -> Vec<bool> {
        rels.iter().map(|r| s.path(r).exists()).collect()
    }

    const OLD_FILES: [&str; 9] = [
        "herds/old.toml",
        "herds/old-lead.md",
        "paused/old",
        "state/reports/a.jsonl",
        "state/reports/lead-old.jsonl",
        "state/handoffs/a.md",
        "state/handoffs/a.prev.md",
        "state/handoffs/lead-old.md",
        "state/handoffs/lead-old.prev.md",
    ];

    #[test]
    fn removes_the_herd_file_lead_prompt_pause_entries_and_members_mailboxes_and_handoffs() {
        let mut s = Setup::new();
        finished_herd(&mut s);

        let out = s.remove("old", Removal::default()).unwrap();

        assert_eq!(exists(&s, &OLD_FILES), [false; 9], "{out}");
        assert_eq!(s.names(), ["other"]);
        assert_eq!(
            exists(
                &s,
                &[
                    "herds/live.toml",
                    "state/reports/other.jsonl",
                    "state/handoffs/other.md"
                ]
            ),
            [true; 3]
        );
        let root = s.dir.path().display();
        assert_eq!(
            out,
            format!(
                "removed {root}/state/reports/a.jsonl\n\
                 removed {root}/state/handoffs/a.md\n\
                 removed {root}/state/handoffs/a.prev.md\n\
                 removed {root}/state/reports/lead-old.jsonl\n\
                 removed {root}/state/handoffs/lead-old.md\n\
                 removed {root}/state/handoffs/lead-old.prev.md\n\
                 removed ledger entry a\n\
                 removed ledger entry lead-old\n\
                 removed {root}/paused/old\n\
                 removed {root}/herds/old-lead.md\n\
                 removed {root}/herds/old.toml\n"
            )
        );
    }

    #[test]
    fn keeps_a_lead_prompt_another_herd_file_names() {
        let mut s = Setup::new();
        s.write("herds/old.toml", LED);
        s.write("herds/old-lead.md", "lead\n");
        s.write(
            "herds/twin.toml",
            "repo = \"/r\"\n[template.t]\nprompt_file = \"./old-lead.md\"\n",
        );

        let out = s.remove("old", Removal::default()).unwrap();

        assert_eq!(
            exists(&s, &["herds/old.toml", "herds/old-lead.md"]),
            [false, true]
        );
        let prompt = s.path("herds/old-lead.md");
        let twin = s.path("herds/twin.toml");
        assert!(
            out.contains(&format!(
                "kept {}: {} names it",
                prompt.display(),
                twin.display()
            )),
            "{out}"
        );
    }

    #[test]
    fn keeps_a_lead_prompt_outside_the_herds_dir() {
        let mut s = Setup::new();
        let outside = s.path("prompts/lead.md");
        s.write("prompts/lead.md", "lead\n");
        let file = format!(
            "repo = \"/r\"\n[lead]\nprompt_file = \"{}\"\n",
            outside.display()
        );
        s.write("herds/old.toml", &file);

        let out = s.remove("old", Removal::default()).unwrap();

        assert!(outside.exists(), "{out}");
        assert!(!s.path("herds/old.toml").exists());
        assert!(
            out.contains(&format!("kept {}: outside", outside.display())),
            "{out}"
        );
    }

    #[test]
    fn removes_a_herd_with_ledger_entries_but_no_herd_file() {
        let mut s = Setup::new();
        s.member("zpr-1050", "zagops-prs", true, false);
        s.write("state/reports/zpr-1050.jsonl", "{}\n");

        let out = s.remove("zagops-prs", Removal::default()).unwrap();

        assert!(s.names().is_empty(), "{out}");
        assert!(!s.path("state/reports/zpr-1050.jsonl").exists());
    }

    #[test]
    fn removes_a_herd_file_with_no_ledger_entries() {
        let mut s = Setup::new();
        s.write("herds/new.toml", "repo = \"/r\"\n");
        s.member("other", "live", false, false);

        let out = s.remove("new", Removal::default()).unwrap();

        let root = s.dir.path().display();
        assert_eq!(out, format!("removed {root}/herds/new.toml\n"));
        assert_eq!(s.names(), ["other"]);
    }

    #[test]
    fn an_unknown_herd_is_an_error() {
        let mut s = Setup::new();
        s.member("other", "live", true, false);

        let err = s.remove("nope", Removal::default()).unwrap_err();

        let file = s.path("herds/nope.toml");
        assert_eq!(
            format!("{err:#}"),
            format!("no herd nope: no {} and no ledger entries", file.display())
        );
        assert_eq!(s.names(), ["other"]);
    }

    #[test]
    fn dry_run_prints_everything_it_would_remove_and_changes_nothing() {
        let mut s = Setup::new();
        finished_herd(&mut s);

        let out = s
            .remove(
                "old",
                Removal {
                    dry_run: true,
                    ..Removal::default()
                },
            )
            .unwrap();

        assert_eq!(exists(&s, &OLD_FILES), [true; 9]);
        assert_eq!(s.names(), ["a", "lead-old", "other"]);
        for rel in OLD_FILES {
            let line = format!("would remove {}\n", s.path(rel).display());
            assert!(out.contains(&line), "{rel}: {out}");
        }
        for name in ["a", "lead-old"] {
            assert!(
                out.contains(&format!("would remove ledger entry {name}\n")),
                "{out}"
            );
        }
        assert!(!out.contains("removed"), "{out}");
    }

    #[test]
    fn keep_history_keeps_the_ledger_entries_and_mailboxes_and_removes_the_rest() {
        let mut s = Setup::new();
        finished_herd(&mut s);

        let out = s
            .remove(
                "old",
                Removal {
                    keep_history: true,
                    ..Removal::default()
                },
            )
            .unwrap();

        assert_eq!(
            exists(&s, &OLD_FILES),
            [false, false, false, true, true, false, false, false, false],
            "{out}"
        );
        assert_eq!(s.names(), ["a", "lead-old", "other"]);
        assert!(!out.contains("ledger entry"), "{out}");
    }

    #[test]
    fn a_members_leftover_worktree_survives() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.write("worktrees/a/README.md", "work\n");
        s.member("a", "old", true, false);
        s.ledger.0.get_mut("a").unwrap().worktree = Some(s.path("worktrees/a"));

        s.remove("old", Removal::default()).unwrap();

        assert!(s.path("worktrees/a/README.md").exists());
        assert!(s.names().is_empty());
    }

    #[test]
    fn refuses_a_retired_member_whose_tab_herdr_still_shows_and_changes_nothing() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        let cwd = s.path("work");
        fs::create_dir(&cwd).unwrap();
        let pane = s.herdr.add_tab("w1", "a", &cwd, true);
        s.member("a", "old", true, false);
        let a = s.ledger.0.get_mut("a").unwrap();
        a.pane_id = pane;
        a.repo = cwd;

        let err = s.remove("old", Removal::default()).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: old has members still open; close each first:\n  \
             loopherd retire --close a  # tab w1:t1 is open"
        );
        assert_eq!(s.names(), ["a"]);
        assert!(s.path("herds/old.toml").exists());
        assert_eq!(s.herdr.mutations(), 0);
    }

    #[test]
    fn refuses_a_retired_lead_whose_agent_is_still_running() {
        let mut s = Setup::new();
        s.herdr = FakeHerdr::new(&[("w1", "loopherd")]).with_agent("lead-old");
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("lead-old", "old", true, true);

        let err = s.remove("old", Removal::default()).unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("loopherd retire --close lead-old  # tab "),
            "{msg}"
        );
        assert_eq!(s.names(), ["lead-old"]);
    }

    #[test]
    fn refuses_when_herdr_cant_be_reached_and_changes_nothing() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.herdr.set_unreachable(true);

        let err = s
            .remove(
                "old",
                Removal {
                    dry_run: true,
                    ..Removal::default()
                },
            )
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("refused: can't ask herdr whether a's tab is closed: "),
            "{msg}"
        );
        assert_eq!(s.names(), ["a"]);
        assert!(s.path("herds/old.toml").exists());
    }

    #[test]
    fn a_herd_file_with_no_members_needs_no_herdr() {
        let mut s = Setup::new();
        s.write("herds/new.toml", "repo = \"/r\"\n");
        s.herdr.set_unreachable(true);

        assert!(s.remove("new", Removal::default()).is_ok());
        assert!(!s.path("herds/new.toml").exists());
    }

    #[test]
    fn a_mailbox_it_cant_stat_is_an_error_and_keeps_the_ledger_entry() {
        use std::os::unix::fs::PermissionsExt;
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.write("state/reports/a.jsonl", "{}\n");
        let reports = s.path("state/reports");
        fs::set_permissions(&reports, fs::Permissions::from_mode(0o000)).unwrap();

        let (_, result) = s.remove_partly("old", Removal::default());

        fs::set_permissions(&reports, fs::Permissions::from_mode(0o755)).unwrap();
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("stat "), "{msg}");
        assert_eq!(s.names(), ["a"]);
        assert!(s.path("state/reports/a.jsonl").exists());
        assert!(s.path("herds/old.toml").exists());
    }

    #[test]
    fn a_failure_partway_still_reports_what_was_removed() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.write("state/handoffs/a.md", "now\n");
        fs::create_dir_all(s.path("paused/old")).unwrap();
        s.member("a", "old", true, false);

        let (out, result) = s.remove_partly("old", Removal::default());

        let root = s.dir.path().display();
        assert_eq!(
            out,
            format!("removed {root}/state/handoffs/a.md\nremoved ledger entry a\n")
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.starts_with(&format!("remove {root}/paused/old")),
            "{msg}"
        );
    }

    #[test]
    fn keeps_a_lead_prompt_file_that_is_not_a_regular_file() {
        let mut s = Setup::new();
        s.write("herds/old.toml", LED);
        fs::create_dir(s.path("herds/old-lead.md")).unwrap();

        let out = s.remove("old", Removal::default()).unwrap();

        let prompt = s.path("herds/old-lead.md");
        assert!(prompt.is_dir());
        assert!(!s.path("herds/old.toml").exists());
        assert!(
            out.contains(&format!("kept {}: not a regular file", prompt.display())),
            "{out}"
        );
    }

    #[test]
    fn refuses_a_symlinked_handoffs_dir_and_changes_nothing() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.write("elsewhere/a.md", "not ours\n");
        fs::create_dir(s.path("state")).unwrap();
        std::os::unix::fs::symlink(s.path("elsewhere"), s.path("state/handoffs")).unwrap();
        s.member("a", "old", true, false);

        let err = s.remove("old", Removal::default()).unwrap_err();

        let handoffs = s.path("state/handoffs");
        assert_eq!(
            format!("{err:#}"),
            format!("refused: {} is a symlink", handoffs.display())
        );
        assert!(s.path("elsewhere/a.md").exists());
        assert_eq!(s.names(), ["a"]);
    }

    #[test]
    fn keeps_the_files_of_a_name_reused_before_the_ledger_lock() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.write("state/reports/a.jsonl", "{}\n");
        let mut ledger = Reusing {
            inner: std::mem::take(&mut s.ledger),
        };
        let paths = Paths::under(s.dir.path());
        let mut out = vec![];

        remove(
            &paths,
            &s.herdr,
            &mut ledger,
            &s.path("state"),
            "old",
            Removal::default(),
            &mut out,
        )
        .unwrap();

        assert!(s.path("state/reports/a.jsonl").exists(), "{out:?}");
        assert_eq!(ledger.inner.0["a"].herd.as_deref(), Some("new"));
        assert!(!s.path("herds/old.toml").exists());
    }

    /// Between the caller's read and the lock, `a` is respawned into
    /// another herd, as a concurrent `add` could.
    struct Reusing {
        inner: MemLedger,
    }

    impl Ledger for Reusing {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            self.inner.entries()
        }
        fn record(&mut self, r: AgentRecord) -> Result<()> {
            self.inner.record(r)
        }
        fn update(
            &mut self,
            name: &str,
            change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.inner.update(name, change)
        }
        fn remove_herd(
            &mut self,
            herd: &str,
            before: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<Vec<String>> {
            let a = self.inner.0.get_mut("a").unwrap();
            a.herd = Some("new".into());
            a.retired = false;
            self.inner.remove_herd(herd, before)
        }
        fn started(&mut self, name: &str, at: std::time::SystemTime) -> Result<()> {
            self.inner.started(name, at)
        }
    }
}
