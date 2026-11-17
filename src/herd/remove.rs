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
/// changing nothing, while any member is not retired, ran on another herdr
/// session, or herdr may still show its agent or pane, or herdr can't say.
/// Stats every file before deleting any. Never touches worktrees, branches
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

    let mut own = vec![paths.paused.join(herd)];
    if has_file {
        match lead_prompt(paths, herd, &file) {
            Ok(Some(prompt)) => own.push(prompt),
            Ok(None) => {}
            Err(kept) => out.push(format!("kept {kept}")),
        }
        own.push(file);
    }
    let names: Vec<String> = members.iter().map(|r| r.name.clone()).collect();
    let mailboxes = !how.keep_history;
    for path in member_files(state, &names, mailboxes).iter().chain(&own) {
        present(path)?;
    }

    let verb = if how.dry_run {
        "would remove"
    } else {
        "removed"
    };
    if how.dry_run || names.is_empty() {
        delete_all(&member_files(state, &names, mailboxes), verb, how, out)?;
        if mailboxes {
            out.extend(names.iter().map(|n| format!("{verb} ledger entry {n}")));
        }
    } else if how.keep_history {
        for name in &names {
            ledger
                .update(name, &mut |record| {
                    if record.herd.as_deref() != Some(herd) || !record.retired {
                        return Ok(());
                    }
                    let files = member_files(state, std::slice::from_ref(name), false);
                    delete_all(&files, verb, how, out)
                })
                .context("ledger")?;
        }
    } else {
        let removed = ledger
            .remove_herd(herd, &mut |names| {
                delete_all(&member_files(state, names, true), verb, how, out)
            })
            .context("ledger")?;
        out.extend(removed.iter().map(|n| format!("{verb} ledger entry {n}")));
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
    let mut here = None;
    for member in members {
        let fix = match (member.retired, &member.socket) {
            (false, _) => Some(format!(
                "loopherd retire --close {}  # not retired",
                member.name
            )),
            (true, Some(socket)) => match here.get_or_insert_with(|| herdr.socket()) {
                None => bail!(
                    "refused: can't ask herdr whether {}'s tab is closed: \
                     herdr didn't say which session it runs",
                    member.name
                ),
                Some(here) if here != socket => Some(format!(
                    "loopherd retire --close {}  # on herdr session {}, not this one; run it there",
                    member.name,
                    socket.display()
                )),
                Some(_) => open_tab_of(herdr, member, entries)?,
            },
            (true, None) => open_tab_of(herdr, member, entries)?,
        };
        if let Some(line) = fix {
            open.push(format!("\n  {line}"));
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

fn open_tab_of(
    herdr: &dyn Herdr,
    member: &AgentRecord,
    entries: &BTreeMap<String, AgentRecord>,
) -> Result<Option<String>> {
    let mut others = entries.clone();
    others.remove(&member.name);
    open_tab(herdr, member, &others).with_context(|| {
        format!(
            "refused: can't ask herdr whether {}'s tab is closed",
            member.name
        )
    })
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

/// Stats every path before deleting any.
fn delete_all(paths: &[PathBuf], verb: &str, how: Removal, out: &mut Vec<String>) -> Result<()> {
    let mut found = vec![];
    for path in paths {
        if present(path)? {
            found.push(path);
        }
    }
    for path in found {
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
            msg.contains(
                "herdr tab close w9:t9  # lead-old: tab w9:t9 is open: not in workspace w1"
            ),
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
    fn refuses_a_socketed_member_as_unaskable_when_herdr_cant_be_reached() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.ledger.0.get_mut("a").unwrap().socket = Some("/run/other/herdr.sock".into());
        s.herdr.set_unreachable(true);
        s.herdr.socket = None;

        let err = s.remove("old", Removal::default()).unwrap_err();

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
    fn keep_history_keeps_the_handoffs_of_a_name_reused_before_the_ledger_lock() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.write("state/handoffs/a.md", "the new a's\n");
        let mut ledger = Racing {
            inner: std::mem::take(&mut s.ledger),
            race: respawn_a,
        };
        let paths = Paths::under(s.dir.path());
        let mut out = vec![];
        let how = Removal {
            keep_history: true,
            ..Removal::default()
        };

        remove(
            &paths,
            &s.herdr,
            &mut ledger,
            &s.path("state"),
            "old",
            how,
            &mut out,
        )
        .unwrap();

        assert!(s.path("state/handoffs/a.md").exists(), "{out:?}");
        assert!(!s.path("herds/old.toml").exists());
    }

    #[test]
    fn keep_history_fails_keeping_the_herd_file_when_an_entry_vanishes_before_the_ledger_lock() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        let mut ledger = Racing {
            inner: std::mem::take(&mut s.ledger),
            race: drop_a,
        };
        let paths = Paths::under(s.dir.path());
        let mut out = vec![];
        let how = Removal {
            keep_history: true,
            ..Removal::default()
        };

        let err = remove(
            &paths,
            &s.herdr,
            &mut ledger,
            &s.path("state"),
            "old",
            how,
            &mut out,
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("a is not in the ledger"),
            "{err:#}"
        );
        assert!(s.path("herds/old.toml").exists());
        s.ledger = ledger.inner;
        s.remove("old", how).unwrap();
        assert!(!s.path("herds/old.toml").exists());
    }

    #[test]
    fn keeps_the_files_of_a_name_reused_before_the_ledger_lock() {
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.write("state/reports/a.jsonl", "{}\n");
        let mut ledger = Racing {
            inner: std::mem::take(&mut s.ledger),
            race: respawn_a,
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

    /// A retired member `a` of herd `old` in workspace `w1`, recorded at
    /// `pane` with its repo at `work/`.
    fn retired_in(s: &mut Setup, pane: &str) {
        s.write("herds/old.toml", "repo = \"/r\"\n");
        fs::create_dir_all(s.path("work")).unwrap();
        s.member("a", "old", true, false);
        let work = s.path("work");
        let a = s.ledger.0.get_mut("a").unwrap();
        a.pane_id = pane.into();
        a.repo = work;
    }

    fn refusal(s: &mut Setup) -> String {
        let err = s.remove("old", Removal::default()).unwrap_err();
        assert_eq!(s.names(), ["a"]);
        assert!(s.path("herds/old.toml").exists());
        format!("{err:#}")
    }

    #[test]
    fn refuses_a_member_recorded_on_another_herdr_session() {
        let mut s = Setup::new();
        retired_in(&mut s, "w1:p1");
        s.ledger.0.get_mut("a").unwrap().socket = Some("/run/other/herdr.sock".into());

        assert_eq!(
            refusal(&mut s),
            "refused: old has members still open; close each first:\n  \
             loopherd retire --close a  # on herdr session /run/other/herdr.sock, \
             not this one; run it there"
        );
    }

    #[test]
    fn removes_a_member_recorded_on_this_herdr_session_whose_tab_is_gone() {
        let mut s = Setup::new();
        retired_in(&mut s, "w1:p1");
        s.ledger.0.get_mut("a").unwrap().socket = s.herdr.socket();

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn refuses_a_pane_at_its_prompt_whose_cwd_moved() {
        let mut s = Setup::new();
        let moved = s.path("moved");
        fs::create_dir(&moved).unwrap();
        let pane = s.herdr.add_tab("w1", "a", &moved, true);
        retired_in(&mut s, &pane);

        let msg = refusal(&mut s);

        assert!(
            msg.ends_with(&format!(
                "herdr tab close w1:t1  # a: not confirmed closed: \
                 no matching tab (pane {pane} is in {}); \
                 if that tab isn't a's, label it otherwise: herdr tab rename w1:t1 <label>",
                moved.display()
            )),
            "{msg}"
        );
    }

    #[test]
    fn refuses_a_pane_running_a_command() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "a", &s.path("work"), false);
        retired_in(&mut s, &pane);

        let msg = refusal(&mut s);

        assert!(
            msg.ends_with(
                "herdr tab close w1:t1  # a: tab w1:t1 is open: pane is running a command; \
                 if that tab isn't a's, label it otherwise: herdr tab rename w1:t1 <label>"
            ),
            "{msg}"
        );
    }

    #[test]
    fn refuses_a_pane_still_open_in_a_relabeled_workspace() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "a", &s.path("work"), true);
        retired_in(&mut s, &pane);
        s.ledger.0.get_mut("a").unwrap().workspace_label = Some("before".into());

        let msg = refusal(&mut s);

        assert!(
            msg.ends_with(&format!(
                "herdr tab close w1:t1  # a: not confirmed closed: no matching tab: \
                 workspace before not found, but pane {pane} is in workspace loopherd; \
                 if that tab isn't a's, label it otherwise: herdr tab rename w1:t1 <label>"
            )),
            "{msg}"
        );
    }

    #[test]
    fn removes_a_member_whose_pane_id_a_restart_gave_another_workspaces_tab() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "1", &s.path("work"), true);
        retired_in(&mut s, &pane);
        s.ledger.0.get_mut("a").unwrap().workspace_label = Some("zagops".into());

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn removes_a_member_whose_pane_id_another_tab_reused_at_its_cwd() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "1", &s.path("work"), true);
        retired_in(&mut s, &pane);

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn removes_a_member_whose_pane_id_another_tab_reused_to_run_a_command() {
        let mut s = Setup::new();
        let elsewhere = s.path("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let pane = s.herdr.add_tab("w1", "1", &elsewhere, false);
        retired_in(&mut s, &pane);

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn refuses_a_renamed_tab_still_running_the_members_own_session() {
        let mut s = Setup::new();
        let moved = s.path("moved");
        fs::create_dir(&moved).unwrap();
        let pane = s.herdr.add_tab("w1", "a-renamed", &moved, false);
        s.herdr = std::mem::replace(&mut s.herdr, FakeHerdr::new(&[])).with_live_agent(
            None,
            Some("ours"),
            &pane,
        );
        retired_in(&mut s, &pane);
        s.ledger.0.get_mut("a").unwrap().session_id = Some("ours".into());

        let msg = refusal(&mut s);

        assert!(
            msg.contains("herdr tab close w1:t1  # a: not confirmed closed"),
            "{msg}"
        );
    }

    #[test]
    fn refuses_a_members_tab_split_with_another_pane_naming_a_tab_close() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "a", &s.path("work"), true);
        s.herdr.split("w1:t1", &s.path("work"));
        retired_in(&mut s, &pane);

        assert!(
            refusal(&mut s)
                .ends_with("herdr tab close w1:t1  # a: tab w1:t1 is open: it has 2 panes")
        );
    }

    #[test]
    fn refuses_a_name_held_by_another_session_naming_how_to_unname_it() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "a", &s.path("work"), false);
        s.herdr = std::mem::replace(&mut s.herdr, FakeHerdr::new(&[])).with_live_agent(
            Some("a"),
            Some("theirs"),
            &pane,
        );
        retired_in(&mut s, &pane);
        s.ledger.0.get_mut("a").unwrap().session_id = Some("ours".into());

        let msg = refusal(&mut s);

        assert!(
            msg.ends_with(
                "herdr agent rename a --clear  # a: tab w1:t1 is open: the name is held by \
                 another session; if that agent is a, close its tab instead: herdr tab close w1:t1"
            ),
            "{msg}"
        );
    }

    #[test]
    fn removes_a_member_whose_pane_id_is_reused_in_its_workspace_by_another_tab() {
        let mut s = Setup::new();
        let elsewhere = s.path("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let pane = s.herdr.add_tab("w1", "notes", &elsewhere, true);
        retired_in(&mut s, &pane);

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn removes_a_member_whose_pane_id_now_runs_an_agent_from_another_session() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "a", &s.path("work"), true);
        s.herdr = std::mem::replace(&mut s.herdr, FakeHerdr::new(&[])).with_live_agent(
            None,
            Some("theirs"),
            &pane,
        );
        retired_in(&mut s, &pane);
        let a = s.ledger.0.get_mut("a").unwrap();
        a.session_id = Some("ours".into());
        a.workspace_label = Some("zagops".into());

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn removes_a_member_whose_workspace_and_pane_are_gone() {
        let mut s = Setup::new();
        retired_in(&mut s, "w9:p1");
        s.ledger.0.get_mut("a").unwrap().workspace_label = Some("before".into());

        s.remove("old", Removal::default()).unwrap();

        assert!(s.names().is_empty());
    }

    #[test]
    fn removes_a_member_whose_recorded_pane_another_entry_now_holds() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "b", &s.path("work"), true);
        retired_in(&mut s, &pane);
        s.member("b", "live", false, false);
        let b = s.ledger.0.get_mut("b").unwrap();
        b.pane_id = pane;
        b.socket = s.herdr.socket();

        s.remove("old", Removal::default()).unwrap();

        assert_eq!(s.names(), ["b"]);
    }

    #[test]
    fn stats_every_file_before_deleting_any() {
        use std::os::unix::fs::PermissionsExt;
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.member("a", "old", true, false);
        s.write("state/reports/a.jsonl", "{}\n");
        s.write("state/handoffs/a.md", "now\n");
        let handoffs = s.path("state/handoffs");
        fs::set_permissions(&handoffs, fs::Permissions::from_mode(0o000)).unwrap();

        let (out, result) = s.remove_partly("old", Removal::default());

        fs::set_permissions(&handoffs, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(format!("{:#}", result.unwrap_err()).contains("stat "));
        assert_eq!(out, "");
        assert!(s.path("state/reports/a.jsonl").exists());
        assert_eq!(s.names(), ["a"]);
    }

    #[test]
    fn a_pause_it_cant_stat_keeps_the_members_files_and_entries() {
        use std::os::unix::fs::PermissionsExt;
        let mut s = Setup::new();
        s.write("herds/old.toml", "repo = \"/r\"\n");
        s.write("paused/old", "{}");
        s.member("a", "old", true, false);
        s.write("state/reports/a.jsonl", "{}\n");
        let paused = s.path("paused");
        fs::set_permissions(&paused, fs::Permissions::from_mode(0o000)).unwrap();

        let (out, result) = s.remove_partly("old", Removal::default());

        fs::set_permissions(&paused, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(format!("{:#}", result.unwrap_err()).contains("stat "));
        assert_eq!(out, "");
        assert!(s.path("state/reports/a.jsonl").exists());
        assert_eq!(s.names(), ["a"]);
    }

    /// Runs `race` on the ledger as each locked change starts, standing in
    /// for another process that got there between the snapshot and the lock.
    struct Racing {
        inner: MemLedger,
        race: fn(&mut MemLedger),
    }

    fn respawn_a(ledger: &mut MemLedger) {
        let a = ledger.0.get_mut("a").unwrap();
        a.herd = Some("new".into());
        a.retired = false;
    }

    fn drop_a(ledger: &mut MemLedger) {
        ledger.0.remove("a");
    }

    impl Ledger for Racing {
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
            (self.race)(&mut self.inner);
            self.inner.update(name, change)
        }
        fn remove_herd(
            &mut self,
            herd: &str,
            before: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<Vec<String>> {
            (self.race)(&mut self.inner);
            self.inner.remove_herd(herd, before)
        }
        fn started(&mut self, name: &str, at: std::time::SystemTime) -> Result<()> {
            self.inner.started(name, at)
        }
    }
}
