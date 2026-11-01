//! loopherd's record of the agents it spawned.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub name: String,
    pub pane_id: String,
    pub session_id: Option<String>,
    pub repo: PathBuf,
    pub worktree: Option<PathBuf>,
    /// Workspace ID at spawn time; IDs can be reused after a restart.
    pub workspace: String,
    /// Missing in ledgers written before revive existed.
    #[serde(default)]
    pub workspace_label: Option<String>,
    /// Passed to claude at spawn, and again on resume.
    #[serde(default)]
    pub claude_args: Vec<String>,
    pub prompt: Option<String>,
    pub prompt_sent: bool,
    /// RFC 3339, UTC.
    pub created: String,
    /// Finished; revive leaves it alone.
    #[serde(default)]
    pub retired: bool,
    /// RFC 3339, UTC.
    #[serde(default)]
    pub retired_at: Option<String>,
}

pub trait Ledger {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>>;
    /// Inserts or replaces the entry for `record.name`.
    fn record(&mut self, record: AgentRecord) -> Result<()>;
}

/// A JSON file of entries keyed by agent name.
pub struct FileLedger {
    path: PathBuf,
}

impl FileLedger {
    pub fn new(path: PathBuf) -> Self {
        FileLedger { path }
    }

    /// `$XDG_STATE_HOME/loopherd/ledger.json`, defaulting to `~/.local/state`.
    pub fn default_path() -> Result<PathBuf> {
        let state = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".local/state"),
        };
        Ok(state.join("loopherd/ledger.json"))
    }

    fn load(&self) -> Result<BTreeMap<String, AgentRecord>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse ledger {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("read ledger {}", self.path.display())),
        }
    }

    fn save(&self, entries: &BTreeMap<String, AgentRecord>) -> Result<()> {
        let dir = self.path.parent().context("ledger path has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = temp_path(&self.path);
        let mut file = fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, entries)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replace ledger {}", self.path.display()))
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

impl Ledger for FileLedger {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
        self.load()
    }

    fn record(&mut self, record: AgentRecord) -> Result<()> {
        let mut entries = self.load()?;
        entries.insert(record.name.clone(), record);
        self.save(&entries)
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct MemLedger(pub BTreeMap<String, AgentRecord>);

#[cfg(test)]
impl Ledger for MemLedger {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
        Ok(self.0.clone())
    }

    fn record(&mut self, record: AgentRecord) -> Result<()> {
        self.0.insert(record.name.clone(), record);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, prompt_sent: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("abc".into()),
            repo: "/src/repo".into(),
            worktree: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec!["--model".into(), "opus".into()],
            prompt: Some("go".into()),
            prompt_sent,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
        }
    }

    #[test]
    fn file_ledger_keeps_entries_by_name_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ledger.json");
        let mut ledger = FileLedger::new(path.clone());
        ledger.record(record("a", false)).unwrap();
        ledger.record(record("b", false)).unwrap();
        ledger.record(record("a", true)).unwrap();

        let entries = FileLedger::new(path).load().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries["a"], record("a", true));
        assert_eq!(entries["b"], record("b", false));
    }

    #[test]
    fn file_ledger_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        ledger.record(record("a", true)).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["ledger.json"]);
    }

    #[test]
    fn a_ledger_written_before_retire_and_revive_loads_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        fs::write(
            &path,
            r#"{"lh-8": {"name": "lh-8", "pane_id": "w1:p1", "session_id": "abc",
                "repo": "/src/repo", "worktree": null, "workspace": "w1",
                "prompt": null, "prompt_sent": true, "created": "2026-10-31T00:00:00Z"}}"#,
        )
        .unwrap();
        let entries = FileLedger::new(path).entries().unwrap();
        let entry = &entries["lh-8"];
        assert!(!entry.retired);
        assert_eq!(entry.retired_at, None);
        assert_eq!(entry.workspace_label, None);
        assert!(entry.claude_args.is_empty());
    }
}
