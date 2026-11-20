//! loopherd's record of the agents it spawned.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
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
    /// The agent's `$HERDR_PANE_ID`, kept when a move to another workspace
    /// gave its pane the new id in `pane_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_pane_id: Option<String>,
    /// The pane's shell when the move happened: only its descendants carry
    /// `env_pane_id`, and no restored herdr brings it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_pane_shell: Option<Process>,
    /// The herdr tab holding `pane_id`. Missing in ledgers written before
    /// spawn, revive and restart recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    pub session_id: Option<String>,
    pub repo: PathBuf,
    pub worktree: Option<PathBuf>,
    /// The worktree's branch at spawn, empty when detached or without a
    /// worktree. Missing in ledgers written before spawn recorded it.
    #[serde(default)]
    pub branch: Option<String>,
    /// Workspace ID at spawn or the last revive; IDs can be reused after a restart.
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
    /// How the run ended (D92). Missing in ledgers written before retire
    /// recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// RFC 3339, UTC. Set on a retired entry so `feed` may spawn its name again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
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
    /// Why this lead runs without its sandbox, guarded by its tool hook
    /// alone; `None` when sandboxed, and for a worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsandboxed: Option<String>,
    /// How far `feed` has got with its herd's `after_merge` for this member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_merge: Option<AfterMerge>,
    /// Earlier runs of this name, oldest first: each retired record that a
    /// new one replaced, kept for `stats`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<AgentRecord>,
    /// Fields another loopherd version wrote, saved back unchanged (D99).
    #[serde(flatten)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

/// How a retired run ended (D92).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Feed retired it with its template's `merged` check passing.
    Merged,
    /// Feed retired it without that.
    Closed,
    /// `loopherd release` gave its task back to feed.
    Released,
    /// A plain `loopherd retire`.
    Retired,
    /// Written by another loopherd version; saved back unchanged (D99).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Merged => "merged",
            Outcome::Closed => "closed",
            Outcome::Released => "released",
            Outcome::Retired => "retired",
            Outcome::Unknown(_) => "-",
        }
    }
}

/// A member's `after_merge`, once a feed has taken it on; it never runs twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AfterMerge {
    /// Taken on by the feed `owner`, which hasn't noted its outcome yet.
    Pending { owner: Process },
    /// Its outcome is in the member's mailbox.
    Noted,
    /// Written by another loopherd version; saved back unchanged (D99).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Runs its herd's routine loop unattended (D17).
    Lead,
    /// Written by another loopherd version; saved back unchanged (D99).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl AgentRecord {
    /// Whether a process with `$HERDR_PANE_ID` set to `pane` and `lineage`
    /// as itself and its ancestors runs in this member's pane: its agent keeps
    /// the id from before a move to another workspace, and a restored herdr
    /// gives a new shell the current one.
    pub fn is_own_pane(&self, pane: &str, lineage: &[Process]) -> bool {
        self.pane_id == pane
            || (self.env_pane_id.as_deref() == Some(pane)
                && self
                    .env_pane_shell
                    .is_some_and(|shell| lineage.contains(&shell)))
    }

    pub fn is_lead(&self) -> bool {
        self.role == Some(Role::Lead)
    }

    /// A lead, or a role from another loopherd version that may be one: lead
    /// guards apply to it and feed never reaps it (D99).
    pub fn may_be_lead(&self) -> bool {
        matches!(self.role, Some(Role::Lead | Role::Unknown(_)))
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
    /// Inserts or replaces the entry for `record.name`. An empty `earlier`
    /// inherits the replaced entry's; a retired entry of a different run
    /// (another `created`) is then appended to it.
    fn record(&mut self, record: AgentRecord) -> Result<()>;
    /// Applies `change` to the current entry for `name`, read fresh under the
    /// ledger lock, so a write made since the caller's `entries` survives.
    /// Saves nothing when `change` fails. Returns the entry as saved.
    fn update(
        &mut self,
        name: &str,
        change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord>;
    /// Removes every run of `herd`, current or in an entry's `earlier`, read
    /// fresh under the ledger lock; refuses, removing nothing, while any
    /// entry of `herd` is not retired. An entry left with other runs keeps
    /// them, the newest becoming the entry. Runs `before` with the names
    /// left with no run while holding the lock, and saves nothing when it
    /// fails.
    fn remove_herd(
        &mut self,
        herd: &str,
        before: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<RemovedHerd>;
    /// Appends a `start` boundary to `name`'s mailbox; `resumed` when the
    /// conversation carried over, as on a revive.
    fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()>;
}

/// The ledger entries `remove_herd` changed, by name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemovedHerd {
    /// Entries removed: every run was the herd's.
    pub gone: Vec<String>,
    /// Entries that lost the herd's runs and kept others.
    pub trimmed: Vec<String>,
}

/// The entries holding a run of `herd`, split by whether removing those runs
/// leaves any.
pub fn herd_runs(entries: &BTreeMap<String, AgentRecord>, herd: &str) -> RemovedHerd {
    let mut runs = RemovedHerd::default();
    for (name, record) in entries {
        match without_herd(record.clone(), herd) {
            None => runs.gone.push(name.clone()),
            Some(left) if left != *record => runs.trimmed.push(name.clone()),
            Some(_) => {}
        }
    }
    runs
}

/// `record`'s runs not of `herd`: the newest as the entry, the rest as its
/// `earlier`, oldest first. `None` when every run was `herd`'s.
fn without_herd(mut record: AgentRecord, herd: &str) -> Option<AgentRecord> {
    let mut runs = std::mem::take(&mut record.earlier);
    runs.push(record);
    runs.retain(|r| r.herd.as_deref() != Some(herd));
    let mut newest = runs.pop()?;
    newest.earlier = runs;
    Some(newest)
}

fn insert(entries: &mut BTreeMap<String, AgentRecord>, mut record: AgentRecord) {
    if let Some(mut old) = entries.remove(&record.name) {
        let kept = std::mem::take(&mut old.earlier);
        if record.earlier.is_empty() {
            record.earlier = kept;
        }
        if old.retired && old.created != record.created {
            record.earlier.push(old);
        }
    }
    entries.insert(record.name.clone(), record);
}

/// The names of `herd`'s entries that are not retired.
fn live_in_herd(entries: &BTreeMap<String, AgentRecord>, herd: &str) -> Vec<String> {
    in_herd(entries, herd)
        .filter(|r| !r.retired)
        .map(|r| r.name.clone())
        .collect()
}

fn in_herd<'a>(
    entries: &'a BTreeMap<String, AgentRecord>,
    herd: &'a str,
) -> impl Iterator<Item = &'a AgentRecord> {
    entries
        .values()
        .filter(move |r| r.herd.as_deref() == Some(herd))
}

fn remove_herd_from(
    entries: &mut BTreeMap<String, AgentRecord>,
    herd: &str,
    before: &mut dyn FnMut(&[String]) -> Result<()>,
) -> Result<RemovedHerd> {
    let live = live_in_herd(entries, herd);
    if !live.is_empty() {
        anyhow::bail!("not retired: {}", live.join(", "));
    }
    let removed = herd_runs(entries, herd);
    before(&removed.gone)?;
    for name in &removed.gone {
        entries.remove(name);
    }
    for name in &removed.trimmed {
        let record = entries.remove(name).expect("trimmed entry exists");
        entries.extend(without_herd(record, herd).map(|r| (name.clone(), r)));
    }
    Ok(removed)
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

    /// Whether this process is refused writing the ledger, as one in a
    /// lead's sandbox is (D97). Takes no lock.
    pub fn read_only(&self) -> Result<bool> {
        let path = self.path.with_extension("lock");
        let opened = fs::create_dir_all(self.dir()?).and_then(|()| {
            fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
        });
        match opened {
            Ok(_) => Ok(false),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::PermissionDenied
                ) =>
            {
                Ok(true)
            }
            Err(e) => Err(e).with_context(|| format!("open {}", path.display())),
        }
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
        insert(&mut entries, record);
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

    fn remove_herd(
        &mut self,
        herd: &str,
        before: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<RemovedHerd> {
        let _lock = self.lock()?;
        let mut entries = self.load()?;
        let removed = remove_herd_from(&mut entries, herd, before)?;
        if removed != RemovedHerd::default() {
            self.save(&entries)?;
        }
        Ok(removed)
    }

    fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()> {
        report::append(
            &self.reports_dir()?,
            name,
            &Entry::new(at, Body::Start { resumed }),
        )
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
        insert(&mut self.0, record);
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

    fn remove_herd(
        &mut self,
        herd: &str,
        before: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<RemovedHerd> {
        remove_herd_from(&mut self.0, herd, before)
    }

    fn started(&mut self, name: &str, _at: SystemTime, _resumed: bool) -> Result<()> {
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

    fn remove_herd(
        &mut self,
        herd: &str,
        before: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<RemovedHerd> {
        self.0.remove_herd(herd, before)
    }

    fn started(&mut self, _: &str, _: SystemTime, _resumed: bool) -> Result<()> {
        anyhow::bail!("disk full")
    }
}

/// A [`MemLedger`] whose entries can't be updated.
#[cfg(test)]
pub struct NoUpdate(pub MemLedger);

#[cfg(test)]
impl Ledger for NoUpdate {
    fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
        self.0.entries()
    }

    fn record(&mut self, record: AgentRecord) -> Result<()> {
        self.0.record(record)
    }

    fn update(
        &mut self,
        _: &str,
        _: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
    ) -> Result<AgentRecord> {
        anyhow::bail!("disk full")
    }

    fn remove_herd(
        &mut self,
        herd: &str,
        before: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<RemovedHerd> {
        self.0.remove_herd(herd, before)
    }

    fn started(&mut self, name: &str, at: SystemTime, resumed: bool) -> Result<()> {
        self.0.started(name, at, resumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, prompt_sent: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
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
            outcome: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            unsandboxed: None,
            earlier: vec![],
            unknown: Default::default(),
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
    fn recording_over_a_released_entry_keeps_it_as_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        let first = AgentRecord {
            retired: true,
            released_at: Some("2026-11-02T00:00:00Z".into()),
            ..record("a", true)
        };
        ledger.record(first.clone()).unwrap();
        let second = AgentRecord {
            retired: true,
            released_at: Some("2026-11-04T00:00:00Z".into()),
            created: "2026-11-03T00:00:00Z".into(),
            ..record("a", true)
        };
        ledger.record(second.clone()).unwrap();

        ledger.record(record("a", false)).unwrap();

        let entry = &ledger.entries().unwrap()["a"];
        assert_eq!(entry.earlier, [first, second]);
        assert!(!entry.retired);
    }

    #[test]
    fn recording_over_an_unreleased_entry_replaces_it_and_keeps_its_earlier_runs() {
        let mut ledger = MemLedger::default();
        let earlier = AgentRecord {
            retired: true,
            released_at: Some("2026-11-02T00:00:00Z".into()),
            ..record("a", true)
        };
        ledger.record(earlier.clone()).unwrap();
        let current = AgentRecord {
            created: "2026-11-03T00:00:00Z".into(),
            ..record("a", false)
        };
        ledger.record(current.clone()).unwrap();

        ledger
            .record(AgentRecord {
                prompt_sent: true,
                ..current
            })
            .unwrap();

        let entry = &ledger.entries().unwrap()["a"];
        assert_eq!(entry.earlier, [earlier]);
        assert!(entry.prompt_sent);
    }

    fn released(name: &str, created: &str) -> AgentRecord {
        AgentRecord {
            retired: true,
            created: created.into(),
            released_at: Some("2026-11-10T00:00:00Z".into()),
            ..record(name, true)
        }
    }

    #[test]
    fn recording_a_new_run_over_a_retired_unreleased_entry_keeps_it_as_an_earlier_run() {
        let mut ledger = MemLedger::default();
        let retired = AgentRecord {
            retired: true,
            retired_at: Some("2026-11-02T00:00:00Z".into()),
            outcome: None,
            ..record("a", true)
        };
        ledger.record(retired.clone()).unwrap();

        ledger
            .record(AgentRecord {
                created: "2026-11-03T00:00:00Z".into(),
                ..record("a", false)
            })
            .unwrap();

        let entry = &ledger.entries().unwrap()["a"];
        assert_eq!(entry.earlier, [retired]);
        assert_eq!(entry.created, "2026-11-03T00:00:00Z");
    }

    #[test]
    fn re_recording_a_retired_entry_of_the_same_run_keeps_no_copy_of_it() {
        let mut ledger = MemLedger::default();
        let retired = AgentRecord {
            retired: true,
            ..record("a", true)
        };
        ledger.record(retired.clone()).unwrap();

        ledger.record(record("a", true)).unwrap();

        assert_eq!(ledger.entries().unwrap()["a"].earlier, []);
    }

    #[test]
    fn re_recording_a_released_entry_as_it_is_keeps_no_copy_of_it() {
        let mut ledger = MemLedger::default();
        let entry = released("a", "2026-11-01T00:00:00Z");
        ledger.record(entry.clone()).unwrap();

        ledger.record(entry.clone()).unwrap();

        assert_eq!(ledger.entries().unwrap()["a"], entry);
    }

    #[test]
    fn a_new_run_with_its_own_earlier_list_still_gets_the_released_one_appended() {
        let mut ledger = MemLedger::default();
        let oldest = released("a", "2026-11-01T00:00:00Z");
        let replaced = released("a", "2026-11-03T00:00:00Z");
        ledger.record(replaced.clone()).unwrap();

        ledger
            .record(AgentRecord {
                earlier: vec![oldest.clone()],
                ..record("a", false)
            })
            .unwrap();

        assert_eq!(ledger.entries().unwrap()["a"].earlier, [oldest, replaced]);
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

    fn member(name: &str, herd: &str, retired: bool) -> AgentRecord {
        AgentRecord {
            herd: Some(herd.into()),
            retired,
            ..record(name, true)
        }
    }

    #[test]
    fn remove_herd_removes_its_entries_and_keeps_a_write_made_since_the_caller_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut remover = FileLedger::new(path.clone());
        let mut other = FileLedger::new(path);
        remover.record(member("a", "old", true)).unwrap();
        remover.record(member("b", "old", true)).unwrap();
        remover.record(member("c", "live", false)).unwrap();
        let _stale = remover.entries().unwrap();

        other.record(member("d", "live", false)).unwrap();
        let mut seen = vec![];
        let removed = remover
            .remove_herd("old", &mut |names| {
                seen = names.to_vec();
                Ok(())
            })
            .unwrap();

        assert_eq!(removed.gone, ["a", "b"]);
        assert!(removed.trimmed.is_empty());
        assert_eq!(seen, ["a", "b"]);
        let left: Vec<_> = remover.entries().unwrap().into_keys().collect();
        assert_eq!(left, ["c", "d"]);
    }

    #[test]
    fn remove_herd_saves_an_entry_that_only_lost_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        ledger.record(member("a", "old", true)).unwrap();
        ledger
            .record(AgentRecord {
                created: "later".into(),
                ..member("a", "live", false)
            })
            .unwrap();
        assert_eq!(ledger.entries().unwrap()["a"].earlier.len(), 1);

        let removed = ledger.remove_herd("old", &mut |_| Ok(())).unwrap();

        assert_eq!(removed.trimmed, ["a"]);
        let a = &ledger.entries().unwrap()["a"];
        assert_eq!((a.herd.as_deref(), a.earlier.len()), (Some("live"), 0));
    }

    #[test]
    fn remove_herd_removes_nothing_while_one_of_its_entries_is_not_retired() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        ledger.record(member("a", "old", true)).unwrap();
        ledger.record(member("b", "old", false)).unwrap();
        let mut called = false;

        let err = ledger
            .remove_herd("old", &mut |_| {
                called = true;
                Ok(())
            })
            .unwrap_err();

        assert_eq!(format!("{err:#}"), "not retired: b");
        assert!(!called);
        assert_eq!(ledger.entries().unwrap().len(), 2);
    }

    #[test]
    fn remove_herd_removes_nothing_when_its_before_step_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("ledger.json"));
        ledger.record(member("a", "old", true)).unwrap();

        let err = ledger
            .remove_herd("old", &mut |_| anyhow::bail!("disk full"))
            .unwrap_err();

        assert_eq!(format!("{err:#}"), "disk full");
        assert_eq!(ledger.entries().unwrap().len(), 1);
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
            after_merge: None,
            unsandboxed: None,
            earlier: vec![],
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

    fn ledger_from_a_newer_version(dir: &Path) -> PathBuf {
        let path = dir.join("ledger.json");
        fs::write(
            &path,
            r#"{"lh-9": {"name": "lh-9", "pane_id": "w1:p1", "session_id": "abc",
                "repo": "/src/repo", "worktree": null, "workspace": "w1",
                "prompt": null, "prompt_sent": true, "created": "2026-10-31T00:00:00Z",
                "retired": true, "outcome": "abandoned", "role": "reviewer",
                "after_merge": {"failed": {"code": 2}}, "colour": "blue",
                "earlier": [{"name": "lh-9", "pane_id": "w1:p0", "session_id": null,
                    "repo": "/src/repo", "worktree": null, "workspace": "w1",
                    "prompt": null, "prompt_sent": true,
                    "created": "2026-10-30T00:00:00Z", "after_merge": "skipped",
                    "shade": 3}]}}"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn unknown_variants_load_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let entries = FileLedger::new(ledger_from_a_newer_version(dir.path()))
            .entries()
            .unwrap();
        let entry = &entries["lh-9"];
        assert_eq!(entry.outcome, Some(Outcome::Unknown("abandoned".into())));
        assert_eq!(entry.role, Some(Role::Unknown("reviewer".into())));
        assert_eq!(
            entry.after_merge,
            Some(AfterMerge::Unknown(
                serde_json::json!({"failed": {"code": 2}})
            ))
        );
        assert_eq!(
            entry.earlier[0].after_merge,
            Some(AfterMerge::Unknown("skipped".into()))
        );
        assert!(!entry.is_lead());
    }

    #[test]
    fn unknown_fields_and_variants_survive_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = ledger_from_a_newer_version(dir.path());
        FileLedger::new(path.clone())
            .update("lh-9", &mut |r| {
                r.released_at = Some("2026-11-01T00:00:00Z".into());
                Ok(())
            })
            .unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let entry = &saved["lh-9"];
        assert_eq!(entry["released_at"], "2026-11-01T00:00:00Z");
        assert_eq!(entry["colour"], "blue");
        assert_eq!(entry["outcome"], "abandoned");
        assert_eq!(entry["role"], "reviewer");
        assert_eq!(
            entry["after_merge"],
            serde_json::json!({"failed": {"code": 2}})
        );
        assert_eq!(entry["earlier"][0]["shade"], 3);
        assert_eq!(entry["earlier"][0]["after_merge"], "skipped");
    }

    #[test]
    fn started_appends_a_start_entry_marking_a_resume_to_the_mailbox_beside_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = FileLedger::new(dir.path().join("loopherd/ledger.json"));
        let at = humantime::parse_rfc3339("2026-11-02T09:00:00Z").unwrap();
        ledger.started("lh-1", at, false).unwrap();
        ledger.started("lh-1", at, true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("loopherd/reports/lh-1.jsonl")).unwrap(),
            "{\"at\":\"2026-11-02T09:00:00.000000000Z\",\"kind\":\"start\"}\n\
             {\"at\":\"2026-11-02T09:00:00.000000000Z\",\"kind\":\"start\",\"resumed\":true}\n"
        );
    }
}
