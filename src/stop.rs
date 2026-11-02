//! Members' stop records, written by a Claude Stop hook loopherd installs, so
//! `watch` can tell a member paused on background work from a finished one.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ledger::{Ledger, temp_path};
use crate::lock::Lock;
use crate::report::{Reporter, reporter};

/// A member's latest stop. A `None` array is unknown, not empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub at: String,
    pub background_tasks: Option<Vec<Value>>,
    pub session_crons: Option<Vec<Value>>,
}

/// What a stop says about the member's background work.
#[derive(Debug, PartialEq)]
pub enum State {
    /// Work still in flight, e.g. `1 shell, 1 cron`.
    Paused(String),
    Finished,
    /// The hook input lacked an array.
    Unknown,
}

impl Stop {
    pub fn state(&self) -> State {
        let mut counts: Vec<(&str, usize)> = vec![];
        for task in self.background_tasks.iter().flatten() {
            let kind = task.get("type").and_then(Value::as_str).unwrap_or("task");
            match counts.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((kind, 1)),
            }
        }
        let crons = self.session_crons.as_ref().map_or(0, Vec::len);
        if crons > 0 {
            counts.push(("cron", crons));
        }
        if !counts.is_empty() {
            let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
            return State::Paused(parts.join(", "));
        }
        match self.background_tasks.is_some() && self.session_crons.is_some() {
            true => State::Finished,
            false => State::Unknown,
        }
    }
}

/// The latest stop `watch` has judged for a member, kept across runs so a
/// re-armed watch still sees what changed since.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub at: Option<String>,
    pub paused: bool,
    /// Its last stop woke watch before the hook recorded it, so the next
    /// record is that stop's.
    #[serde(default)]
    pub unrecorded: bool,
}

impl Mark {
    pub fn of(stop: Option<&Stop>) -> Mark {
        Mark {
            at: stop.map(|s| s.at.clone()),
            paused: stop.is_some_and(|s| matches!(s.state(), State::Paused(_))),
            unrecorded: false,
        }
    }
}

/// Runs `f` on the marks kept in `dir`, locked so concurrent watches don't
/// lose each other's, and saves them if `f` changed any.
pub fn with_marks<T>(dir: &Path, f: impl FnOnce(&mut BTreeMap<String, Mark>) -> T) -> Result<T> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let lock = dir.join(".marks.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .with_context(|| format!("open {}", lock.display()))?;
    let _lock = Lock::wait(file).with_context(|| format!("lock {}", lock.display()))?;
    let path = dir.join(".marks");
    let mut marks: BTreeMap<String, Mark> = match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let before = marks.clone();
    let out = f(&mut marks);
    if marks != before {
        replace(&path, &serde_json::to_vec(&marks)?)?;
    }
    Ok(out)
}

/// Records the Stop hook `input` as the latest stop of the member `who`
/// resolves to; returns its name.
pub fn record(
    ledger: &dyn Ledger,
    dir: &Path,
    who: &Reporter,
    input: &[u8],
    now: SystemTime,
) -> Result<String> {
    let name = reporter(ledger, who)?;
    let input: Value = serde_json::from_slice(input).context("parse the Stop hook input")?;
    let array = |key: &str| input.get(key).and_then(Value::as_array).cloned();
    let stop = Stop {
        at: humantime::format_rfc3339_nanos(now).to_string(),
        background_tasks: array("background_tasks"),
        session_crons: array("session_crons"),
    };
    replace(&path(dir, &name), &serde_json::to_vec(&stop)?)?;
    Ok(name)
}

/// Writes `bytes` aside and renames them in, so a reader never sees half.
pub fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = temp_path(path);
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

/// The member's latest stop; `None` when it has none yet.
pub fn latest(dir: &Path, name: &str) -> Result<Option<Stop>> {
    let path = path(dir, name);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};

    pub(crate) fn member(name: &str, pane: &str) -> AgentRecord {
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
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
            params: None,
        }
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    fn pane(pane: &str) -> Reporter<'_> {
        Reporter {
            own_pane: Some(pane),
            as_name: None,
        }
    }

    #[test]
    fn the_hook_records_the_latest_stop_of_the_member_in_its_pane() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger(
            [member("lh-1", "w1:p1"), member("lh-2", "w1:p2")]
                .into_iter()
                .map(|m| (m.name.clone(), m))
                .collect(),
        );
        let paused = br#"{"session_id":"s","hook_event_name":"Stop",
            "background_tasks":[{"id":"b1","type":"shell","status":"running","command":"sleep 240"}],
            "session_crons":[]}"#;
        let finished = br#"{"background_tasks":[],"session_crons":[]}"#;

        let name = record(
            &ledger,
            dir.path(),
            &pane("w1:p2"),
            paused,
            at("2026-11-02T08:05:40Z"),
        );
        assert_eq!(name.unwrap(), "lh-2");
        record(
            &ledger,
            dir.path(),
            &pane("w1:p2"),
            finished,
            at("2026-11-02T08:09:52Z"),
        )
        .unwrap();

        assert_eq!(
            latest(dir.path(), "lh-2").unwrap(),
            Some(Stop {
                at: "2026-11-02T08:09:52.000000000Z".into(),
                background_tasks: Some(vec![]),
                session_crons: Some(vec![]),
            })
        );
        assert_eq!(latest(dir.path(), "lh-1").unwrap(), None);
    }

    #[test]
    fn a_missing_array_is_recorded_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger([("lh-1".into(), member("lh-1", "w1:p1"))].into());

        record(
            &ledger,
            dir.path(),
            &pane("w1:p1"),
            br#"{"session_crons":[]}"#,
            at("2026-11-02T08:05:40Z"),
        )
        .unwrap();

        let stop = latest(dir.path(), "lh-1").unwrap().unwrap();
        assert_eq!(stop.background_tasks, None);
        assert_eq!(stop.session_crons, Some(vec![]));
    }

    #[test]
    fn a_pane_that_is_no_member_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger([("lh-1".into(), member("lh-1", "w1:p1"))].into());

        let err = record(
            &ledger,
            dir.path(),
            &pane("w1:p9"),
            b"{}",
            SystemTime::now(),
        );

        assert!(err.unwrap_err().to_string().contains("not a member"));
        assert_eq!(latest(dir.path(), "lh-1").unwrap(), None);
    }
}
