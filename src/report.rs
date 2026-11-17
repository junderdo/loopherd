//! `loopherd report` and `loopherd reports`: a mailbox members write and the
//! shepherd reads, kept as one JSON-lines file per member. Besides reports it
//! holds `stop` entries from the Stop hook and `start` boundaries from spawn
//! and revive, which `reports` shows only with `--all`.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::ledger::{AgentRecord, Ledger};
use crate::lock::Lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Done,
    Question,
    Note,
    /// The member wrote its handoff and waits for `loopherd restart`.
    Handoff,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Done => "done",
            Kind::Question => "question",
            Kind::Note => "note",
            Kind::Handoff => "handoff",
        }
    }
}

pub struct Reporter<'a> {
    pub own_pane: Option<&'a str>,
    /// `$HERDR_SOCKET_PATH`: which herdr the pane belongs to.
    pub own_socket: Option<&'a Path>,
    pub as_name: Option<&'a str>,
}

/// One line of a mailbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at: String,
    #[serde(flatten)]
    pub body: Body,
}

impl Entry {
    pub fn new(at: SystemTime, body: Body) -> Entry {
        Entry {
            at: humantime::format_rfc3339_nanos(at).to_string(),
            body,
        }
    }

    /// `2026-11-16 10:05`, in UTC like `at`; `at` itself when it won't parse.
    pub fn when(&self) -> String {
        match (self.at.get(..10), self.at.get(11..16)) {
            (Some(day), Some(minute)) => format!("{day} {minute}"),
            _ => self.at.clone(),
        }
    }

    /// Its kind and text, unless it's a `stop` or `start`.
    pub fn report(&self) -> Option<(Kind, &str)> {
        match &self.body {
            Body::Done { text } => Some((Kind::Done, text)),
            Body::Question { text } => Some((Kind::Question, text)),
            Body::Note { text } => Some((Kind::Note, text)),
            Body::Handoff { text } => Some((Kind::Handoff, text)),
            Body::Stop(_) | Body::Start => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Body {
    Done {
        text: String,
    },
    Question {
        text: String,
    },
    Note {
        text: String,
    },
    Handoff {
        text: String,
    },
    /// Claude ended a turn; written only by the Stop hook.
    Stop(Stop),
    /// spawn or revive started the member's Claude.
    Start,
}

impl Body {
    fn report(kind: Kind, text: &str) -> Body {
        let text = text.to_string();
        match kind {
            Kind::Done => Body::Done { text },
            Kind::Question => Body::Question { text },
            Kind::Note => Body::Note { text },
            Kind::Handoff => Body::Handoff { text },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    /// Work still in flight that can wake Claude again.
    pub background: Tally<Background>,
    pub crons: Tally<u32>,
    /// Set for a StopFailure: the API error that ended the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Claude session that stopped; missing in entries written before LH-70.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Context tokens in use when Claude stopped, when the hook measured them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// The member's context limit, set alongside `context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Background {
    pub shells: u32,
    pub subagents: u32,
    pub other: u32,
}

/// A count, or `"unknown"` when Claude didn't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Tally<T> {
    Known(T),
    Unknown(Unknown),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unknown {
    Unknown,
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
    append(dir, &name, &Entry::new(now, Body::report(kind, text)))?;
    Ok(name)
}

/// Appends `entry` to `name`'s mailbox in `dir`.
pub fn append(dir: &Path, name: &str, entry: &Entry) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.jsonl"));
    let mut file = lock_mailbox(&path)?;
    let mut bytes = vec![];
    // A torn last line would swallow this entry; start it on a line of its own.
    if !ends_in_newline(&mut file).with_context(|| format!("read {}", path.display()))? {
        bytes.push(b'\n');
    }
    serde_json::to_writer(&mut bytes, entry)?;
    bytes.push(b'\n');
    // One write, so concurrent writers' lines never interleave.
    let written = file
        .write(&bytes)
        .with_context(|| format!("append to {}", path.display()))?;
    if written != bytes.len() {
        bail!("short write to {}", path.display());
    }
    Ok(())
}

/// Opens `path` for append, locked so no other writer is mid-copy while we
/// read the last byte.
fn lock_mailbox(path: &Path) -> Result<Lock> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Lock::wait(file).with_context(|| format!("lock {}", path.display()))
}

/// True for an empty file too.
fn ends_in_newline(file: &mut fs::File) -> std::io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    let mut last = [0];
    file.read_exact_at(&mut last, len - 1)?;
    Ok(last[0] == b'\n')
}

/// The non-retired member running in `pane` of the herdr at `socket`. Pane
/// ids repeat across herdr sessions, so an entry recorded without a socket
/// matches only a caller that has none either.
pub fn member_in<'a>(
    entries: &'a BTreeMap<String, AgentRecord>,
    pane: &str,
    socket: Option<&Path>,
) -> Option<&'a AgentRecord> {
    entries
        .values()
        .find(|e| !e.retired && e.pane_id == pane && e.socket.as_deref() == socket)
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
    member_in(&entries, pane, who.own_socket)
        .map(|e| e.name.clone())
        .ok_or_else(|| match who.own_socket {
            Some(socket) => anyhow!("pane {pane} of {} is not a member", socket.display()),
            None => anyhow!("pane {pane} is not a member"),
        })
}

#[derive(Default)]
pub struct Read {
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
    /// The entries behind `lines`, each with its member's name.
    pub entries: Vec<(String, Entry)>,
}

pub struct Filter<'a> {
    pub names: &'a [String],
    pub herd: Option<&'a str>,
    /// Full history, `stop` and `start` entries included, leaving the cursor alone.
    pub all: bool,
}

/// The reports `filter` selects, one rendered line each, unread only unless
/// `filter.all`; marks those read. Unread, questions come first.
pub fn reports(ledger: &dyn Ledger, dir: &Path, filter: &Filter) -> Result<Read> {
    // Held from read to save, so concurrent readers print each report once.
    if !dir.is_dir() {
        return Ok(Read::default());
    }
    let _lock = match filter.all {
        true => None,
        false => Some(Cursor::Reports.lock(dir)?),
    };
    let (read, pending) = reports_with(ledger, dir, filter, Locked::Held)?;
    pending.save_with(Locked::Held)?;
    Ok(read)
}

/// [`reports`], leaving the cursor where it was until the caller saves
/// `Pending`, once what was read is safely out.
pub fn peek_reports(ledger: &dyn Ledger, dir: &Path, filter: &Filter) -> Result<(Read, Pending)> {
    reports_with(ledger, dir, filter, Locked::Take)
}

/// Whether the caller already holds the cursor's lock.
#[derive(Clone, Copy)]
enum Locked {
    Held,
    Take,
}

fn reports_with(
    ledger: &dyn Ledger,
    dir: &Path,
    filter: &Filter,
    locked: Locked,
) -> Result<(Read, Pending)> {
    let mut names = mailboxes(dir)?;
    if !filter.names.is_empty() {
        names.retain(|n| filter.names.contains(n));
    }
    if let (Some(herd), false) = (filter.herd, names.is_empty()) {
        let entries = ledger.entries()?;
        names.retain(|n| {
            entries
                .get(n)
                .is_some_and(|e| e.herd.as_deref() == Some(herd))
        });
    }
    let ((mut found, warnings), pending) = match filter.all {
        true => (
            read_all(dir, &names)?,
            Pending::nothing(Cursor::Reports, dir),
        ),
        false => Cursor::Reports.read(dir, &names, locked, |e| e.report().is_some())?,
    };
    if !filter.all {
        found.sort_by(|(_, a), (_, b)| {
            let question = |e: &Entry| e.report().is_some_and(|(k, _)| k == Kind::Question);
            (!question(a), &a.at).cmp(&(!question(b), &b.at))
        });
    }
    let read = Read {
        lines: found
            .iter()
            .flat_map(|(name, entry)| render(name, entry))
            .collect(),
        warnings,
        entries: found,
    };
    Ok((read, pending))
}

/// Each of `names` with an unread `stop` entry, with its whole mailbox so a
/// stop can be judged against what came before. Saving `Pending` marks
/// everything read on the watch cursor, which `reports` never moves.
pub fn unread_stops(dir: &Path, names: &[String]) -> Result<(Vec<Mailbox>, Pending)> {
    let names: Vec<String> = mailboxes(dir)?
        .into_iter()
        .filter(|n| names.contains(n))
        .collect();
    let mut stops = vec![];
    let pending = Cursor::Watch.peek(dir, &names, Locked::Take, |name, read, unread| {
        let mut entries = vec![];
        // `reports` warns about bad lines; its cursor reads the same bytes.
        parse(dir, name, unread, &mut entries, &mut vec![]);
        if !entries.iter().any(|(_, e)| matches!(e.body, Body::Stop(_))) {
            return;
        }
        // Already warned about when it was unread.
        let mut history = vec![];
        parse(dir, name, read, &mut history, &mut vec![]);
        let unread = history.len();
        history.extend(entries);
        stops.push(Mailbox {
            name: name.to_string(),
            entries: history.into_iter().map(|(_, e)| e).collect(),
            unread,
        });
    })?;
    Ok((stops, pending))
}

/// Where a member stands relative to its last handoff report.
#[derive(Debug, PartialEq)]
pub enum SinceHandoff {
    /// No handoff since its last start.
    None,
    /// Handed off, and hasn't stopped since.
    NotStopped,
    /// Its last stop after the handoff.
    Stopped(Stop),
}

/// Reads `name`'s mailbox back to its last handoff or start.
pub fn since_handoff(dir: &Path, name: &str) -> Result<SinceHandoff> {
    if !dir.join(format!("{name}.jsonl")).is_file() {
        return Ok(SinceHandoff::None);
    }
    let (bytes, end) = complete_lines(dir, name)?;
    let mut entries = vec![];
    parse(dir, name, &bytes[..end], &mut entries, &mut vec![]);
    let mut last = None;
    for (_, entry) in entries.into_iter().rev() {
        match entry.body {
            Body::Stop(stop) if last.is_none() => last = Some(stop),
            Body::Handoff { .. } => {
                return Ok(last.map_or(SinceHandoff::NotStopped, SinceHandoff::Stopped));
            }
            Body::Start => break,
            _ => {}
        }
    }
    Ok(SinceHandoff::None)
}

/// Every entry in `name`'s mailbox in file order, and a warning per bad line.
/// Reads no cursor and takes no lock.
pub fn history(dir: &Path, name: &str) -> Result<(Vec<Entry>, Vec<String>)> {
    if !dir.join(format!("{name}.jsonl")).is_file() {
        return Ok((vec![], vec![]));
    }
    let (bytes, end) = complete_lines(dir, name)?;
    let mut found = vec![];
    let mut warnings = vec![];
    parse(dir, name, &bytes[..end], &mut found, &mut warnings);
    Ok((found.into_iter().map(|(_, e)| e).collect(), warnings))
}

/// Cursor moves read but not yet saved.
#[must_use = "the cursor moves only once saved"]
pub struct Pending {
    cursor: Cursor,
    dir: PathBuf,
    /// Each name's offset as read, and where it moves to.
    moves: Vec<(String, usize, usize)>,
}

impl Pending {
    fn nothing(cursor: Cursor, dir: &Path) -> Pending {
        Pending {
            cursor,
            dir: dir.to_path_buf(),
            moves: vec![],
        }
    }

    /// Moves the cursor, except for a name another reader moved meanwhile.
    pub fn save(self) -> Result<()> {
        self.save_with(Locked::Take)
    }

    fn save_with(self, locked: Locked) -> Result<()> {
        if self.moves.iter().all(|(_, from, to)| from == to) {
            return Ok(());
        }
        let _lock = match locked {
            Locked::Held => None,
            Locked::Take => Some(self.cursor.lock(&self.dir)?),
        };
        let mut offsets = self.cursor.load(&self.dir)?;
        let mut moved = false;
        for (name, from, to) in self.moves {
            if from != to && offsets.read_to(&name) == from {
                offsets.0.insert(name, to);
                moved = true;
            }
        }
        if moved {
            self.cursor.save(&self.dir, &offsets)?;
        }
        Ok(())
    }
}

/// A member's mailbox in file order; `entries[unread..]` are new.
#[derive(Debug)]
pub struct Mailbox {
    pub name: String,
    pub entries: Vec<Entry>,
    pub unread: usize,
}

type Found = (Vec<(String, Entry)>, Vec<String>);

/// Every entry of `names`, by time.
fn read_all(dir: &Path, names: &[String]) -> Result<Found> {
    let mut found = vec![];
    let mut warnings = vec![];
    for name in names {
        let (bytes, end) = complete_lines(dir, name)?;
        parse(dir, name, &bytes[..end], &mut found, &mut warnings);
    }
    found.sort_by(|(_, a), (_, b)| a.at.cmp(&b.at));
    Ok((found, warnings))
}

/// The mailbox's bytes and where its last complete line ends; a line still
/// being appended has no newline yet.
fn complete_lines(dir: &Path, name: &str) -> Result<(Vec<u8>, usize)> {
    let path = dir.join(format!("{name}.jsonl"));
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    Ok((bytes, end))
}

fn parse(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    found: &mut Vec<(String, Entry)>,
    warnings: &mut Vec<String>,
) {
    for raw in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        match serde_json::from_slice::<Entry>(raw) {
            Ok(entry) => found.push((name.to_string(), entry)),
            Err(e) => warnings.push(format!(
                "skipped a bad line in {}: {e}",
                dir.join(format!("{name}.jsonl")).display()
            )),
        }
    }
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

fn render(name: &str, entry: &Entry) -> Vec<String> {
    let when = entry.when();
    let (kind, text) = match &entry.body {
        Body::Stop(stop) => match (stop.context, stop.limit) {
            (Some(context), Some(limit)) => (
                "stop",
                format!("{} {}k/{}k", describe(stop), kilo(context), kilo(limit)),
            ),
            _ => ("stop", describe(stop)),
        },
        Body::Start => return vec![format!("{name} start {when}")],
        _ => {
            let (kind, text) = entry.report().expect("a report");
            (kind.as_str(), text.to_string())
        }
    };
    let mut body = text.lines();
    let first = body.next().unwrap_or_default();
    let mut out = vec![format!("{name} {kind} {when} {first}")];
    out.extend(body.map(|l| format!("    {l}")));
    out
}

fn kilo(tokens: u64) -> u64 {
    (tokens + 500) / 1000
}

/// `finished`, `paused: 1 shell, 2 crons`, or what's unknown, after any error.
fn describe(stop: &Stop) -> String {
    let mut parts: Vec<String> = stop.error.iter().map(|e| format!("error {e}")).collect();
    match stop.in_flight() {
        (None, None) => parts.push("finished".into()),
        (pending, unknown) => {
            parts.extend(pending.map(|p| format!("paused: {p}")));
            parts.extend(unknown);
        }
    }
    parts.join("; ")
}

impl Stop {
    /// `finished`, `paused: 1 shell`, or what's unknown, after any error.
    pub fn describe(&self) -> String {
        describe(self)
    }

    /// What may still wake Claude, `1 shell, 2 crons` or `background
    /// unknown`; `None` when nothing is in flight.
    pub fn pending(&self) -> Option<String> {
        let (pending, unknown) = self.in_flight();
        let parts: Vec<String> = pending.into_iter().chain(unknown).collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// The counted work in flight, and what Claude didn't say.
    pub fn in_flight(&self) -> (Option<String>, Option<String>) {
        let mut pending = vec![];
        let mut unknown = vec![];
        match self.background {
            Tally::Known(b) => {
                pending.extend(counted(b.shells, "shell"));
                pending.extend(counted(b.subagents, "subagent"));
                pending.extend(counted(b.other, "other task"));
            }
            Tally::Unknown(_) => unknown.push("background"),
        }
        match self.crons {
            Tally::Known(n) => pending.extend(counted(n, "cron")),
            Tally::Unknown(_) => unknown.push("crons"),
        }
        (
            (!pending.is_empty()).then(|| pending.join(", ")),
            (!unknown.is_empty()).then(|| format!("{} unknown", unknown.join(" and "))),
        )
    }
}

fn counted(n: u32, what: &str) -> Option<String> {
    match n {
        0 => None,
        1 => Some(format!("1 {what}")),
        n => Some(format!("{n} {what}s")),
    }
}

/// How far into each member's file a reader has read, in bytes. `reports`
/// and `watch` each keep their own.
#[derive(Clone, Copy)]
enum Cursor {
    Reports,
    Watch,
}

#[derive(Default, Serialize, Deserialize)]
struct Offsets(BTreeMap<String, usize>);

impl Cursor {
    fn file(self) -> &'static str {
        match self {
            Cursor::Reports => ".cursor",
            Cursor::Watch => ".watch-cursor",
        }
    }

    /// Entries of `names` past the cursor that `keep` selects, in file
    /// order; saving `Pending` moves the cursor past every complete line.
    fn read(
        self,
        dir: &Path,
        names: &[String],
        locked: Locked,
        keep: impl Fn(&Entry) -> bool,
    ) -> Result<(Found, Pending)> {
        let mut found = vec![];
        let mut warnings = vec![];
        let pending = self.peek(dir, names, locked, |name, _, unread| {
            parse(dir, name, unread, &mut found, &mut warnings)
        })?;
        found.retain(|(_, e)| keep(e));
        Ok(((found, warnings), pending))
    }

    /// Calls `visit` with each of `names`' complete lines split at the
    /// cursor, already read then unread; returns the move past them.
    fn peek(
        self,
        dir: &Path,
        names: &[String],
        locked: Locked,
        mut visit: impl FnMut(&str, &[u8], &[u8]),
    ) -> Result<Pending> {
        let mut pending = Pending::nothing(self, dir);
        if names.is_empty() {
            return Ok(pending);
        }
        let offsets = match locked {
            Locked::Held => self.load(dir)?,
            Locked::Take => {
                let _lock = self.lock(dir)?;
                self.load(dir)?
            }
        };
        for name in names {
            let (bytes, end) = complete_lines(dir, name)?;
            let from = offsets.read_to(name);
            // Past the end: the file was truncated or replaced, so all of it is new.
            let start = if from > end { 0 } else { from };
            visit(name, &bytes[..start], &bytes[start..end]);
            pending.moves.push((name.clone(), from, end));
        }
        Ok(pending)
    }

    /// Held across load and save, so concurrent readers never move it back.
    fn lock(self, dir: &Path) -> Result<Lock> {
        let path = dir.join(format!("{}.lock", self.file()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Lock::wait(file).with_context(|| format!("lock {}", path.display()))
    }

    fn load(self, dir: &Path) -> Result<Offsets> {
        let path = dir.join(self.file());
        match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Offsets::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(self, dir: &Path, offsets: &Offsets) -> Result<()> {
        let path = dir.join(self.file());
        let tmp = dir.join(format!("{}.{}.tmp", self.file(), std::process::id()));
        fs::write(&tmp, serde_json::to_vec(offsets)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
    }
}

impl Offsets {
    fn read_to(&self, name: &str) -> usize {
        self.0.get(name).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
            socket: None,
            process: None,
            role: None,
            after_merge: None,
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    fn from_pane(pane: &str) -> Reporter<'_> {
        Reporter {
            own_pane: Some(pane),
            own_socket: None,
            as_name: None,
        }
    }

    fn is_free(path: &Path) -> bool {
        let file = fs::File::options().write(true).open(path).unwrap();
        Lock::try_take(file).unwrap().is_some()
    }

    #[test]
    fn dropping_the_mailbox_lock_frees_it_while_a_spawning_thread_holds_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zpr-1.jsonl");
        let held = lock_mailbox(&path).unwrap();
        assert!(!is_free(&path));
        // A child forked by another test thread holds this copy until it execs.
        let _inherited = held.try_clone().unwrap();
        drop(held);
        assert!(is_free(&path));
    }

    #[test]
    fn dropping_the_cursor_lock_frees_it_while_a_spawning_thread_holds_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let held = Cursor::Reports.lock(dir.path()).unwrap();
        let path = dir.path().join(".cursor.lock");
        assert!(!is_free(&path));
        let _inherited = held.try_clone().unwrap();
        drop(held);
        assert!(is_free(&path));
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
            own_socket: None,
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
            let who = Reporter {
                own_pane,
                own_socket: None,
                as_name,
            };
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
            own_socket: None,
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
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            [
                "lh-2 question 2026-11-01 11:00 which DB?",
                "lh-2 note 2026-11-01 09:00 starting",
                "lh-1 done 2026-11-01 10:00 PR #9",
            ]
        );
        assert!(
            reports(&ledger, dir.path(), &UNREAD)
                .unwrap()
                .lines
                .is_empty()
        );

        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "merged main",
            "2026-11-01T12:00:00Z",
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
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
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 note 2026-11-01 09:00 one",
                "lh-1 note 2026-11-01 10:00 two",
            ]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
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
            reports(&ledger, dir.path(), &by_name).unwrap().lines,
            ["lh-1 note 2026-11-01 09:00 hi"]
        );
        let by_herd = Filter {
            herd: Some("cards"),
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &by_herd).unwrap().lines,
            ["lh-2 note 2026-11-01 09:00 hi"]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
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
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
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
                .lines
                .is_empty()
        );
        assert!(!missing.exists());
    }

    fn mailbox(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.jsonl"))
    }

    fn append(path: &Path, bytes: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes.as_bytes()).unwrap();
    }

    #[test]
    fn a_torn_line_does_not_swallow_the_next_report() {
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
        append(&mailbox(dir.path(), "lh-1"), "{\"at\":\"2026-11-01T09:30");
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Done,
            "two",
            "2026-11-01T10:00:00Z",
        );

        let read = reports(&ledger, dir.path(), &UNREAD).unwrap();

        assert_eq!(
            read.lines,
            [
                "lh-1 note 2026-11-01 09:00 one",
                "lh-1 done 2026-11-01 10:00 two",
            ]
        );
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn a_bad_line_is_skipped_with_a_warning_and_the_cursor_still_advances() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "one",
            "2026-11-01T09:00:00Z",
        );
        append(&mailbox(dir.path(), "lh-1"), "not json\n");
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "two",
            "2026-11-01T10:00:00Z",
        );
        send(
            &ledger,
            dir.path(),
            "lh-2",
            Kind::Note,
            "hi",
            "2026-11-01T11:00:00Z",
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };

        let read = reports(&ledger, dir.path(), &UNREAD).unwrap();
        assert_eq!(
            read.lines,
            [
                "lh-1 note 2026-11-01 09:00 one",
                "lh-1 note 2026-11-01 10:00 two",
                "lh-2 note 2026-11-01 11:00 hi",
            ]
        );
        assert_eq!(read.warnings.len(), 1);
        assert!(
            read.warnings[0].contains("lh-1.jsonl"),
            "{:?}",
            read.warnings
        );

        let again = reports(&ledger, dir.path(), &UNREAD).unwrap();
        assert!(again.lines.is_empty());
        assert!(again.warnings.is_empty());
        assert_eq!(reports(&ledger, dir.path(), &all).unwrap().lines.len(), 3);
    }

    #[test]
    fn a_file_shorter_than_the_cursor_was_replaced_and_is_read_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "a long first report",
            "2026-11-01T09:00:00Z",
        );
        reports(&ledger, dir.path(), &UNREAD).unwrap();
        std::fs::remove_file(mailbox(dir.path(), "lh-1")).unwrap();
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "new",
            "2026-11-01T10:00:00Z",
        );

        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            ["lh-1 note 2026-11-01 10:00 new"]
        );
    }

    #[test]
    fn concurrent_readers_print_each_report_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let sent = 200;
        let printed = std::thread::scope(|s| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        let mut printed = 0;
                        for _ in 0..sent {
                            printed += reports(&ledger, dir.path(), &UNREAD).unwrap().lines.len();
                        }
                        printed
                    })
                })
                .collect();
            for n in 0..sent {
                send(
                    &ledger,
                    dir.path(),
                    "lh-1",
                    Kind::Note,
                    &n.to_string(),
                    "2026-11-01T09:00:00Z",
                );
            }
            readers
                .into_iter()
                .map(|r| r.join().unwrap())
                .sum::<usize>()
        });
        let rest = reports(&ledger, dir.path(), &UNREAD).unwrap().lines.len();

        assert_eq!(printed + rest, sent);
    }

    #[test]
    fn a_read_with_nothing_new_leaves_the_cursor_file_alone() {
        use std::os::unix::fs::MetadataExt;
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
        let inode = || std::fs::metadata(dir.path().join(".cursor")).unwrap().ino();
        let before = inode();

        assert!(
            reports(&ledger, dir.path(), &UNREAD)
                .unwrap()
                .lines
                .is_empty()
        );

        assert_eq!(inode(), before);
    }

    #[test]
    fn a_stop_line_without_a_session_still_parses() {
        let entry: Entry = serde_json::from_str(
            r#"{"at":"2026-11-02T09:00:00.000000000Z","kind":"stop","background":"unknown","crons":0}"#,
        )
        .unwrap();
        let Body::Stop(stop) = entry.body else {
            panic!("not a stop")
        };
        assert_eq!(stop.session, None);
    }

    fn stop(shells: u32, crons: Tally<u32>, error: Option<&str>) -> Body {
        Body::Stop(Stop {
            session: None,
            background: Tally::Known(Background {
                shells,
                ..Background::default()
            }),
            crons,
            error: error.map(Into::into),
            context: None,
            limit: None,
        })
    }

    fn put(dir: &Path, name: &str, time: &str, body: Body) {
        super::append(dir, name, &Entry::new(at(time), body)).unwrap();
    }

    #[test]
    fn reports_skips_stop_and_start_entries_and_all_shows_the_timeline_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);
        put(dir.path(), "lh-1", "2026-11-01T09:00:00Z", Body::Start);
        put(dir.path(), "lh-2", "2026-11-01T09:01:00Z", Body::Start);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "hi",
            "2026-11-01T09:02:00Z",
        );
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:03:00Z",
            stop(1, Tally::Known(0), None),
        );
        send(
            &ledger,
            dir.path(),
            "lh-2",
            Kind::Question,
            "which?",
            "2026-11-01T09:04:00Z",
        );

        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            [
                "lh-2 question 2026-11-01 09:04 which?",
                "lh-1 note 2026-11-01 09:02 hi",
            ]
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 start 2026-11-01 09:00",
                "lh-2 start 2026-11-01 09:01",
                "lh-1 note 2026-11-01 09:02 hi",
                "lh-1 stop 2026-11-01 09:03 paused: 1 shell",
                "lh-2 question 2026-11-01 09:04 which?",
            ]
        );
    }

    #[test]
    fn all_shows_a_stops_context_use_in_thousands_when_it_was_measured() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let measured = Stop {
            context: Some(120_400),
            limit: Some(150_000),
            ..finished_stop()
        };
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:00:00Z",
            Body::Stop(measured),
        );
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:01:00Z",
            Body::Stop(finished_stop()),
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 stop 2026-11-01 09:00 finished 120k/150k",
                "lh-1 stop 2026-11-01 09:01 finished",
            ]
        );
    }

    #[test]
    fn a_stop_after_the_last_report_does_not_count_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Done,
            "PR",
            "2026-11-01T09:00:00Z",
        );
        reports(&ledger, dir.path(), &UNREAD).unwrap();
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:01:00Z",
            stop(0, Tally::Known(0), None),
        );

        let read = reports(&ledger, dir.path(), &UNREAD).unwrap();

        assert!(read.lines.is_empty());
    }

    fn read_stops(dir: &Path, names: &[String]) -> Vec<Mailbox> {
        let (stops, pending) = unread_stops(dir, names).unwrap();
        pending.save().unwrap();
        stops
    }

    #[test]
    fn unsaved_reads_leave_the_cursor_and_a_concurrent_readers_move_stands() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let names = ["lh-1".to_string()];
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:00:00Z",
            stop(0, Tally::Known(0), None),
        );
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "hi",
            "2026-11-01T09:01:00Z",
        );

        let (stops, _unsaved) = unread_stops(dir.path(), &names).unwrap();
        assert_eq!(stops.len(), 1);
        let (_, pending) = peek_reports(&ledger, dir.path(), &UNREAD).unwrap();
        drop(pending);
        assert_eq!(read_stops(dir.path(), &names).len(), 1);

        let (read, stale) = peek_reports(&ledger, dir.path(), &UNREAD).unwrap();
        assert_eq!(read.lines.len(), 1);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "more",
            "2026-11-01T09:02:00Z",
        );
        reports(&ledger, dir.path(), &UNREAD).unwrap();
        stale.save().unwrap();
        assert!(
            reports(&ledger, dir.path(), &UNREAD)
                .unwrap()
                .lines
                .is_empty()
        );
    }

    #[test]
    fn the_watch_cursor_reads_stops_apart_from_the_reports_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", None, false),
            member("lh-2", "w1:p2", None, false),
        ]);
        let names = ["lh-1".to_string(), "lh-2".to_string()];
        put(dir.path(), "lh-1", "2026-11-01T09:00:00Z", Body::Start);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Done,
            "PR",
            "2026-11-01T09:01:00Z",
        );
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:02:00Z",
            stop(1, Tally::Known(0), None),
        );
        put(
            dir.path(),
            "lh-2",
            "2026-11-01T09:03:00Z",
            stop(0, Tally::Known(0), Some("rate_limit")),
        );

        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines.len(),
            1
        );
        let read = read_stops(dir.path(), &names);
        let unread: Vec<(&str, usize, usize)> = read
            .iter()
            .map(|m| (m.name.as_str(), m.entries.len(), m.unread))
            .collect();
        assert_eq!(unread, [("lh-1", 3, 0), ("lh-2", 1, 0)]);

        assert!(read_stops(dir.path(), &names).is_empty());
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:04:00Z",
            stop(0, Tally::Known(0), None),
        );
        send(
            &ledger,
            dir.path(),
            "lh-2",
            Kind::Note,
            "no stop",
            "2026-11-01T09:04:30Z",
        );
        let read = read_stops(dir.path(), &names);
        assert_eq!(read.len(), 1);
        let lh1 = &read[0];
        assert_eq!(
            (lh1.name.as_str(), lh1.entries.len(), lh1.unread),
            ("lh-1", 4, 3)
        );
        assert_eq!(lh1.entries[0].body, Body::Start);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "more",
            "2026-11-01T09:05:00Z",
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            [
                "lh-2 note 2026-11-01 09:04 no stop",
                "lh-1 note 2026-11-01 09:05 more"
            ]
        );
    }

    #[test]
    fn a_stop_says_what_is_pending_unknown_or_failed() {
        let unknown = Tally::Unknown(Unknown::Unknown);
        let cases = [
            (stop(0, Tally::Known(0), None), "finished"),
            (stop(2, Tally::Known(1), None), "paused: 2 shells, 1 cron"),
            (stop(0, unknown, None), "crons unknown"),
            (stop(1, unknown, None), "paused: 1 shell; crons unknown"),
            (
                Body::Stop(Stop {
                    session: None,
                    background: Tally::Unknown(Unknown::Unknown),
                    crons: unknown,
                    error: Some("rate_limit".into()),
                    context: None,
                    limit: None,
                }),
                "error rate_limit; background and crons unknown",
            ),
        ];
        for (body, want) in cases {
            let Body::Stop(stop) = body else {
                unreachable!()
            };
            assert_eq!(describe(&stop), want);
        }
    }

    #[test]
    fn pending_names_what_may_still_wake_claude() {
        let unknown = Tally::Unknown(Unknown::Unknown);
        let cases = [
            (stop(0, Tally::Known(0), None), None),
            (stop(0, Tally::Known(0), Some("rate_limit")), None),
            (stop(1, Tally::Known(1), None), Some("1 shell, 1 cron")),
            (stop(1, unknown, None), Some("1 shell; crons unknown")),
            (
                Body::Stop(Stop {
                    session: None,
                    background: Tally::Unknown(Unknown::Unknown),
                    crons: Tally::Known(0),
                    error: None,
                    context: None,
                    limit: None,
                }),
                Some("background unknown"),
            ),
        ];
        for (body, want) in cases {
            let Body::Stop(stop) = body else {
                unreachable!()
            };
            assert_eq!(stop.pending().as_deref(), want);
        }
    }

    #[test]
    fn a_stop_entry_round_trips_through_its_json_line() {
        let entry = Entry::new(
            at("2026-11-01T09:00:00Z"),
            Body::Stop(Stop {
                session: None,
                background: Tally::Known(Background {
                    shells: 1,
                    subagents: 2,
                    other: 3,
                }),
                crons: Tally::Unknown(Unknown::Unknown),
                error: None,
                context: None,
                limit: None,
            }),
        );
        let line = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            line,
            "{\"at\":\"2026-11-01T09:00:00.000000000Z\",\"kind\":\"stop\",\
             \"background\":{\"shells\":1,\"subagents\":2,\"other\":3},\"crons\":\"unknown\"}"
        );
        assert_eq!(serde_json::from_str::<Entry>(&line).unwrap(), entry);
    }

    #[test]
    fn a_stops_context_use_round_trips_and_an_old_stop_reads_without_it() {
        let entry = Entry::new(
            at("2026-11-01T09:00:00Z"),
            Body::Stop(Stop {
                context: Some(120_000),
                limit: Some(150_000),
                ..finished_stop()
            }),
        );
        let line = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            line,
            "{\"at\":\"2026-11-01T09:00:00.000000000Z\",\"kind\":\"stop\",\
             \"background\":{\"shells\":0,\"subagents\":0,\"other\":0},\"crons\":0,\
             \"context\":120000,\"limit\":150000}"
        );
        assert_eq!(serde_json::from_str::<Entry>(&line).unwrap(), entry);

        let old: Entry = serde_json::from_str(
            r#"{"at":"2026-11-01T09:00:00.000000000Z","kind":"stop","background":{"shells":0,"subagents":0,"other":0},"crons":0}"#,
        )
        .unwrap();
        let Body::Stop(old) = old.body else {
            panic!("not a stop")
        };
        assert_eq!((old.context, old.limit), (None, None));
    }

    fn finished_stop() -> Stop {
        Stop {
            background: Tally::Known(Background::default()),
            crons: Tally::Known(0),
            error: None,
            session: None,
            context: None,
            limit: None,
        }
    }

    #[test]
    fn a_report_goes_to_the_exact_socket_match_over_a_socketless_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = member("zzz-live", "w1:p3", None, false);
        live.socket = Some("/run/live.sock".into());
        let ledger = ledger(vec![member("aaa-old", "w1:p3", None, false), live]);
        let who = Reporter {
            own_pane: Some("w1:p3"),
            own_socket: Some(Path::new("/run/live.sock")),
            as_name: None,
        };
        let name = report(
            &ledger,
            dir.path(),
            &who,
            Kind::Note,
            "hi",
            at("2026-11-01T09:00:00Z"),
        );
        assert_eq!(name.unwrap(), "zzz-live");
    }
}
