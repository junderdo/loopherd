//! loopherd's record of the agents it spawned.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::Lock;
use crate::report::{self, Body, Entry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub name: String,
    pub pane_id: String,
    pub session_id: Option<String>,
    pub repo: PathBuf,
    pub worktree: Option<PathBuf>,
    /// The worktree's branch at spawn, empty when detached or without a
    /// worktree. Missing in ledgers written before spawn recorded it.
    #[serde(default)]
    pub branch: Option<String>,
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
    /// Set by `loopherd add`; spawn leaves it empty.
    #[serde(default)]
    pub herd: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    /// Each param's value by name, for members of a herd.
    #[serde(default)]
    pub params: Option<BTreeMap<String, String>>,
    /// `$HERDR_SOCKET_PATH` at spawn or revive: which herdr the pane is in.
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// The member's own Claude process, claimed by a hook (LH-71).
    #[serde(default)]
    pub process: Option<Process>,
    /// `None` is a worker.
    #[serde(default)]
    pub role: Option<Role>,
    /// `feed` ran its herd's `after_merge` for this member.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_merge_ran: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Runs its herd's routine loop unattended (D17).
    Lead,
}

impl AgentRecord {
    pub fn is_lead(&self) -> bool {
        self.role == Some(Role::Lead)
    }
}

/// A process by PID and start time, so a reused PID isn't mistaken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub start: u64,
}

pub trait Ledger {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>>;
    /// Inserts or replaces the entry for `record.name`.
    fn record(&mut self, record: AgentRecord) -> Result<()>;
    /// Applies `change` to the current entry for `name`, read fresh under the
    /// ledger lock, so a write made since the caller's `entries` survives.
    /// Saves nothing when `change` fails. Returns the entry as saved.
    fn update(
        &mut self,
        name: &str,
        change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord>;
    /// Appends a `start` boundary to `name`'s mailbox.
    fn started(&mut self, name: &str, at: SystemTime) -> Result<()>;
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

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// loopherd's state dir, which holds the ledger.
    pub fn dir(&self) -> Result<PathBuf> {
        Ok(self
            .path
            .parent()
            .context("ledger path has no parent")?
            .to_path_buf())
    }

    /// The members' mailboxes, beside the ledger.
    pub fn reports_dir(&self) -> Result<PathBuf> {
        Ok(self.dir()?.join("reports"))
    }

    /// Held by every writer for its read-modify-write; readers see whole
    /// files, since `save` renames into place.
    fn lock(&self) -> Result<Lock> {
        let dir = self.path.parent().context("ledger path has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = self.path.with_extension("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Lock::wait(file).with_context(|| format!("lock {}", path.display()))
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
        let _lock = self.lock()?;
        let mut entries = self.load()?;
        entries.insert(record.name.clone(), record);
        self.save(&entries)
    }

    fn update(
        &mut self,
        name: &str,
        change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord> {
        let _lock = self.lock()?;
        let mut entries = self.load()?;
        let record = entries
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("{name} is not in the ledger"))?;
        change(record)?;
        let saved = record.clone();
        self.save(&entries)?;
        Ok(saved)
    }

    fn started(&mut self, name: &str, at: SystemTime) -> Result<()> {
        report::append(&self.reports_dir()?, name, &Entry::new(at, Body::Start))
    }
}

/// Entries, and the names `started` was called with, in order.
#[cfg(test)]
#[derive(Default, Clone)]
pub struct MemLedger(pub BTreeMap<String, AgentRecord>, pub Vec<String>);

#[cfg(test)]
impl Ledger for MemLedger {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
        Ok(self.0.clone())
    }

    fn record(&mut self, record: AgentRecord) -> Result<()> {
        self.0.insert(record.name.clone(), record);
        Ok(())
    }

    fn update(
        &mut self,
        name: &str,
        change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord> {
        let record = self
            .0
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("{name} is not in the ledger"))?;
        change(record)?;
        Ok(record.clone())
    }

    fn started(&mut self, name: &str, _at: SystemTime) -> Result<()> {
        self.1.push(name.into());
        Ok(())
    }
}

/// A [`MemLedger`] whose mailbox can't be written.
#[cfg(test)]
#[derive(Default)]
pub struct NoMailbox(pub MemLedger);

#[cfg(test)]
impl Ledger for NoMailbox {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
        self.0.entries()
    }

    fn record(&mut self, record: AgentRecord) -> Result<()> {
        self.0.record(record)
    }

    fn update(
        &mut self,
        name: &str,
        change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord> {
        self.0.update(name, change)
    }

    fn started(&mut self, _: &str, _: SystemTime) -> Result<()> {
        anyhow::bail!("disk full")
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
            branch: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec!["--model".into(), "opus".into()],
            prompt: Some("go".into()),
            prompt_sent,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge_ran: false,
        }
    }

    #[test]
    fn a_ledger_without_the_process_field_parses() {
        let mut json = serde_json::to_value(record("a", true)).unwrap();
        json.as_object_mut().unwrap().remove("process");
        let parsed: AgentRecord = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.process, None);
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
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec!["ledger.json", "ledger.lock"]);
    }

    #[test]
    fn update_changes_the_current_entry_and_keeps_a_write_made_since_the_caller_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut hook = FileLedger::new(path.clone());
        let mut retire = FileLedger::new(path);
        hook.record(record("a", true)).unwrap();
        let stale = hook.entries().unwrap()["a"].clone();

        retire
            .update("a", &mut |r| {
                r.retired = true;
                Ok(())
            })
            .unwrap();
        let saved = hook
            .update("a", &mut |r| {
                r.session_id = Some("new".into());
                Ok(())
            })
            .unwrap();

        assert!(!stale.retired);
        assert!(saved.retired);
        let now = hook.entries().unwrap()["a"].clone();
        assert!(now.retired);
        assert_eq!(now.session_id.as_deref(), Some("new"));
    }

    #[test]
    fn update_saves_nothing_when_the_change_fails_or_the_entry_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        ledger.record(record("a", true)).unwrap();
        let failed = ledger.update("a", &mut |r| {
            r.retired = true;
            anyhow::bail!("no")
        });
        assert!(failed.is_err());
        assert!(!ledger.entries().unwrap()["a"].retired);
        assert!(ledger.update("zz", &mut |_| Ok(())).is_err());
    }

    #[test]
    fn writers_in_parallel_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    FileLedger::new(path)
                        .record(record(&format!("m{i}"), true))
                        .unwrap();
                })
            })
            .collect();
        writers.into_iter().for_each(|w| w.join().unwrap());
        assert_eq!(FileLedger::new(path).entries().unwrap().len(), 8);
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
        assert_eq!(entry.herd, None);
        assert_eq!(entry.template, None);
        assert_eq!(entry.params, None);
        assert_eq!(entry.branch, None);
        assert_eq!(entry.socket, None);
        assert_eq!(entry.role, None);
    }

    #[test]
    fn a_lead_role_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let lead = AgentRecord {
            role: Some(Role::Lead),
            after_merge_ran: false,
            ..record("lead-h", true)
        };
        FileLedger::new(path.clone()).record(lead).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"role\": \"lead\"")
        );
        let entries = FileLedger::new(path).entries().unwrap();
        assert_eq!(entries["lead-h"].role, Some(Role::Lead));
    }

    #[test]
    fn started_appends_a_start_entry_to_the_mailbox_beside_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("loopherd/ledger.json"));
        ledger
            .started(
                "lh-1",
                humantime::parse_rfc3339("2026-11-02T09:00:00Z").unwrap(),
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("loopherd/reports/lh-1.jsonl")).unwrap(),
            "{\"at\":\"2026-11-02T09:00:00.000000000Z\",\"kind\":\"start\"}\n"
        );
    }
}
