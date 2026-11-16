//! `loopherd herd remove`: delete a finished herd and its members' history.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Paths, expand_home, file_of, herd_files};
use crate::context::handoff_path;
use crate::ledger::{Ledger, live_in_herd};

#[derive(Debug, Default, Clone, Copy)]
pub struct Removal {
    /// Print what would go and change nothing.
    pub dry_run: bool,
    /// Keep the ledger entries and mailboxes, so `stats` still counts them.
    pub keep_history: bool,
}

/// Removes `herd`'s file, lead prompt and pause, and its retired members'
/// ledger entries, mailboxes and handoffs in loopherd's `state` dir; returns
/// a line per thing removed or kept. Refuses, changing nothing, while any of
/// its members is not retired. Never touches worktrees, branches or tabs.
pub fn remove(
    paths: &Paths,
    ledger: &mut dyn Ledger,
    state: &Path,
    herd: &str,
    how: Removal,
) -> Result<String> {
    let file = file_of(&paths.herds, herd)?;
    let entries = ledger.entries()?;
    let live = live_in_herd(&entries, herd);
    if !live.is_empty() {
        let mut msg = format!("refused: {herd} has members not retired; retire each first:");
        for name in live {
            write!(msg, "\n  loopherd retire --close {name}")?;
        }
        bail!(msg);
    }
    let members: Vec<&str> = entries
        .values()
        .filter(|r| r.herd.as_deref() == Some(herd))
        .map(|r| r.name.as_str())
        .collect();
    let has_file = file.symlink_metadata().is_ok();
    if !has_file && members.is_empty() {
        bail!(
            "no herd {herd}: no {} and no ledger entries",
            file.display()
        );
    }

    let mut out = String::new();
    let mut member_files = Vec::new();
    for name in &members {
        if !how.keep_history {
            member_files.push(state.join("reports").join(format!("{name}.jsonl")));
        }
        let handoff = handoff_path(state, name);
        let prev = handoff.with_extension("prev.md");
        member_files.extend([handoff, prev]);
    }
    let mut herd_files = vec![paths.paused.join(herd)];
    if has_file {
        match lead_prompt(paths, herd, &file) {
            Ok(Some(prompt)) => herd_files.push(prompt),
            Ok(None) => {}
            Err(kept) => writeln!(out, "kept {kept}")?,
        }
        herd_files.push(file);
    }

    let verb = if how.dry_run {
        "would remove"
    } else {
        "removed"
    };
    for path in member_files.iter().filter(|p| p.exists()) {
        delete(path, how)?;
        writeln!(out, "{verb} {}", path.display())?;
    }
    if !how.keep_history && !members.is_empty() {
        let removed = match how.dry_run {
            true => members.iter().map(|m| m.to_string()).collect(),
            false => ledger.remove_herd(herd).context("ledger")?,
        };
        for name in removed {
            writeln!(out, "{verb} ledger entry {name}")?;
        }
    }
    for path in herd_files.iter().filter(|p| p.symlink_metadata().is_ok()) {
        delete(path, how)?;
        writeln!(out, "{verb} {}", path.display())?;
    }
    Ok(out)
}

fn delete(path: &Path, how: Removal) -> Result<()> {
    if how.dry_run {
        return Ok(());
    }
    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
}

/// The `[lead]` `prompt_file` of `herd`'s `file`, when it lies in the herds
/// dir and no other herd file names it; `Err` says why one is kept.
fn lead_prompt(paths: &Paths, herd: &str, file: &Path) -> Result<Option<PathBuf>, String> {
    let Some(named) = prompt_files(paths, file).ok().and_then(|f| f.lead) else {
        return Ok(None);
    };
    let Ok(prompt) = named.canonicalize() else {
        return Ok(None);
    };
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
    use crate::ledger::{AgentRecord, MemLedger};

    struct Setup {
        dir: tempfile::TempDir,
        ledger: MemLedger,
    }

    impl Setup {
        fn new() -> Setup {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("herds")).unwrap();
            Setup {
                dir,
                ledger: MemLedger::default(),
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
            let paths = Paths::under(self.dir.path());
            let state = self.path("state");
            remove(&paths, &mut self.ledger, &state, herd, how)
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
            "refused: old has members not retired; retire each first:\n  \
             loopherd retire --close b\n  loopherd retire --close lead-old"
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
}
