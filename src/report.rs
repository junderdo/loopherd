//! `loopherd report` and `loopherd reports`: a mailbox members write and the
//! shepherd reads, kept as one JSON-lines file per member.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::ledger::Ledger;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Done,
    Question,
    Note,
}

pub struct Reporter<'a> {
    pub own_pane: Option<&'a str>,
    pub as_name: Option<&'a str>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Line {
    at: String,
    kind: Kind,
    text: String,
}

/// Appends a report from the member `who` resolves to; returns its name.
pub fn report(
    ledger: &dyn Ledger,
    dir: &Path,
    who: &Reporter,
    kind: Kind,
    text: &str,
    now: SystemTime,
) -> Result<String> {
    let name = reporter(ledger, who)?;
    let line = Line {
        at: humantime::format_rfc3339_nanos(now).to_string(),
        kind,
        text: text.into(),
    };
    let mut bytes = serde_json::to_vec(&line)?;
    bytes.push(b'\n');
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // One write, so concurrent reporters' lines never interleave.
    let written = file
        .write(&bytes)
        .with_context(|| format!("append to {}", path.display()))?;
    if written != bytes.len() {
        bail!("short write to {}", path.display());
    }
    Ok(name)
}

fn reporter(ledger: &dyn Ledger, who: &Reporter) -> Result<String> {
    let entries = ledger.entries()?;
    if let Some(name) = who.as_name {
        let entry = entries
            .get(name)
            .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
        if entry.retired {
            bail!("{name} is retired");
        }
        return Ok(entry.name.clone());
    }
    let pane = who.own_pane.context("no HERDR_PANE_ID; pass --as <name>")?;
    entries
        .values()
        .find(|e| !e.retired && e.pane_id == pane)
        .map(|e| e.name.clone())
        .ok_or_else(|| anyhow!("pane {pane} is not a member"))
}

pub struct Filter<'a> {
    pub names: &'a [String],
    pub herd: Option<&'a str>,
    /// Full history, leaving the cursor alone.
    pub all: bool,
}

/// The reports `filter` selects, one rendered line each, unread only unless
/// `filter.all`; marks those read.
pub fn reports(ledger: &dyn Ledger, dir: &Path, filter: &Filter) -> Result<Vec<String>> {
    let mut names = mailboxes(dir)?;
    if names.is_empty() {
        return Ok(vec![]);
    }
    if !filter.names.is_empty() {
        names.retain(|n| filter.names.contains(n));
    }
    if let Some(herd) = filter.herd {
        let entries = ledger.entries()?;
        names.retain(|n| {
            entries
                .get(n)
                .is_some_and(|e| e.herd.as_deref() == Some(herd))
        });
    }
    let mut cursor = Cursor::load(dir)?;
    let mut found = vec![];
    for name in names {
        let path = dir.join(format!("{name}.jsonl"));
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        // A line still being appended has no newline yet; leave it for next time.
        let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let start = match filter.all {
            true => 0,
            false => cursor.read_to(&name).min(end),
        };
        for raw in bytes[start..end]
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let line: Line = serde_json::from_slice(raw)
                .with_context(|| format!("parse a report in {}", path.display()))?;
            found.push((name.clone(), line));
        }
        cursor.0.insert(name, end);
    }
    found.sort_by(|(_, a), (_, b)| {
        (a.kind != Kind::Question, &a.at).cmp(&(b.kind != Kind::Question, &b.at))
    });
    if !filter.all {
        cursor.save(dir)?;
    }
    Ok(found
        .iter()
        .flat_map(|(name, line)| render(name, line))
        .collect())
}

fn mailboxes(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut names = vec![];
    for entry in entries {
        let file_name = entry?.file_name();
        if let Some(name) = file_name.to_str().and_then(|n| n.strip_suffix(".jsonl")) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn render(name: &str, line: &Line) -> Vec<String> {
    let kind = match line.kind {
        Kind::Done => "done",
        Kind::Question => "question",
        Kind::Note => "note",
    };
    let when = match (line.at.get(..10), line.at.get(11..16)) {
        (Some(day), Some(minute)) => format!("{day} {minute}"),
        _ => line.at.clone(),
    };
    let mut body = line.text.lines();
    let first = body.next().unwrap_or_default();
    let mut out = vec![format!("{name} {kind} {when} {first}")];
    out.extend(body.map(|l| format!("    {l}")));
    out
}

/// How far into each member's file the reader has read, in bytes.
#[derive(Default, Serialize, Deserialize)]
struct Cursor(BTreeMap<String, usize>);

impl Cursor {
    fn path(dir: &Path) -> PathBuf {
        dir.join(".cursor")
    }

    fn load(dir: &Path) -> Result<Cursor> {
        let path = Cursor::path(dir);
        match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursor::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn read_to(&self, name: &str) -> usize {
        self.0.get(name).copied().unwrap_or(0)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let path = Cursor::path(dir);
        let tmp = dir.join(format!(".cursor.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};

    fn member(name: &str, pane: &str, herd: Option<&str>, retired: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired,
            retired_at: None,
            herd: herd.map(Into::into),
            template: None,
            params: None,
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(members.into_iter().map(|m| (m.name.clone(), m)).collect())
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    fn from_pane(pane: &str) -> Reporter<'_> {
        Reporter {
            own_pane: Some(pane),
            as_name: None,
        }
    }

    #[test]
    fn a_member_reports_from_its_own_pane_as_a_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);

        let name = report(
            &ledger,
            dir.path(),
            &from_pane("w1:p2"),
            Kind::Done,
            "opened PR #9",
            at("2026-11-01T12:34:56Z"),
        )
        .unwrap();

        assert_eq!(name, "lh-2");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lh-2.jsonl")).unwrap(),
            "{\"at\":\"2026-11-01T12:34:56.000000000Z\",\"kind\":\"done\",\"text\":\"opened PR #9\"}\n"
        );
    }

    #[test]
    fn as_names_the_reporter_over_the_pane() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);
        let who = Reporter {
            own_pane: Some("w1:p2"),
            as_name: Some("lh-1"),
        };

        let name = report(
            &ledger,
            dir.path(),
            &who,
            Kind::Note,
            "hi",
            at("2026-11-01T12:00:00Z"),
        );

        assert_eq!(name.unwrap(), "lh-1");
        assert!(dir.path().join("lh-1.jsonl").exists());
        assert!(!dir.path().join("lh-2.jsonl").exists());
    }

    #[test]
    fn strays_retired_entries_and_unknown_names_are_refused_without_writing() {
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("old", "w1:p9", None, true),
        ]);
        let refused = [
            (
                "stray pane",
                Some("w1:p5"),
                None,
                "pane w1:p5 is not a member",
            ),
            (
                "retired pane",
                Some("w1:p9"),
                None,
                "pane w1:p9 is not a member",
            ),
            ("no pane", None, None, "no HERDR_PANE_ID; pass --as <name>"),
            ("retired name", Some("w1:p1"), Some("old"), "old is retired"),
            (
                "unknown name",
                Some("w1:p1"),
                Some("ghost"),
                "ghost is not in the ledger",
            ),
        ];
        for (case, own_pane, as_name, error) in refused {
            let dir = tempfile::tempdir().unwrap();
            let who = Reporter { own_pane, as_name };
            let err = report(
                &ledger,
                dir.path(),
                &who,
                Kind::Note,
                "hi",
                at("2026-11-01T12:00:00Z"),
            )
            .unwrap_err();
            assert_eq!(err.to_string(), error, "{case}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "{case}");
        }
    }

    fn send(ledger: &MemLedger, dir: &Path, name: &str, kind: Kind, text: &str, time: &str) {
        let who = Reporter {
            own_pane: None,
            as_name: Some(name),
        };
        report(ledger, dir, &who, kind, text, at(time)).unwrap();
    }

    const UNREAD: Filter = Filter {
        names: &[],
        herd: None,
        all: false,
    };

    #[test]
    fn reports_shows_unread_questions_first_then_by_time_and_marks_them_read() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);
        send(
            &ledger,
            dir.path(),
            "lh-2",
            Kind::Note,
            "starting",
            "2026-11-01T09:00:00Z",
        );
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Done,
            "PR #9",
            "2026-11-01T10:00:00Z",
        );
        send(
            &ledger,
            dir.path(),
            "lh-2",
            Kind::Question,
            "which DB?",
            "2026-11-01T11:00:00Z",
        );

        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap(),
            [
                "lh-2 question 2026-11-01 11:00 which DB?",
                "lh-2 note 2026-11-01 09:00 starting",
                "lh-1 done 2026-11-01 10:00 PR #9",
            ]
        );
        assert!(reports(&ledger, dir.path(), &UNREAD).unwrap().is_empty());

        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "merged main",
            "2026-11-01T12:00:00Z",
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap(),
            ["lh-1 note 2026-11-01 12:00 merged main"]
        );
    }

    #[test]
    fn all_shows_history_and_leaves_the_cursor_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "one",
            "2026-11-01T09:00:00Z",
        );
        reports(&ledger, dir.path(), &UNREAD).unwrap();
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "two",
            "2026-11-01T10:00:00Z",
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };

        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap(),
            [
                "lh-1 note 2026-11-01 09:00 one",
                "lh-1 note 2026-11-01 10:00 two",
            ]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap(),
            ["lh-1 note 2026-11-01 10:00 two"]
        );
    }

    #[test]
    fn names_and_herd_filter_and_leave_the_rest_unread() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(vec![
            member("lh-1", "w1:p1", Some("cards"), false),
            member("lh-2", "w1:p2", Some("cards"), false),
            member("zpr-3", "w1:p3", Some("prs"), false),
        ]);
        for name in ["lh-1", "lh-2", "zpr-3"] {
            send(
                &ledger,
                dir.path(),
                name,
                Kind::Note,
                "hi",
                "2026-11-01T09:00:00Z",
            );
        }
        ledger.0.get_mut("lh-1").unwrap().retired = true;

        let names = ["lh-1".to_string()];
        let by_name = Filter {
            names: &names,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &by_name).unwrap(),
            ["lh-1 note 2026-11-01 09:00 hi"]
        );
        let by_herd = Filter {
            herd: Some("cards"),
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &by_herd).unwrap(),
            ["lh-2 note 2026-11-01 09:00 hi"]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap(),
            ["zpr-3 note 2026-11-01 09:00 hi"]
        );
    }

    #[test]
    fn further_body_lines_are_indented_beneath_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Question,
            "Which store?\n- sqlite\n- json",
            "2026-11-01T09:00:00Z",
        );

        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap(),
            [
                "lh-1 question 2026-11-01 09:00 Which store?",
                "    - sqlite",
                "    - json",
            ]
        );
    }

    #[test]
    fn no_mailbox_yet_reads_as_no_reports() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("reports");
        assert!(
            reports(&MemLedger::default(), &missing, &UNREAD)
                .unwrap()
                .is_empty()
        );
        assert!(!missing.exists());
    }
}
