//! `loopherd report` and `loopherd reports`: a mailbox members write and the
//! shepherd reads, kept as one JSON-lines file per member. Besides reports it
//! holds `stop` entries from the Stop hook, `start` boundaries from spawn
//! and revive, and herdr's `blocked` and `gone`, which `reports` shows only
//! with `--all`.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::ledger::{AgentRecord, Ledger, Process};
use crate::lock::Lock;
use crate::usage::Usage;

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
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub own_lineage: &'a [Process],
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

    /// Its kind and text, unless it's a `stop`, `start`, `blocked`, `gone` or
    /// `moved`.
    pub fn report(&self) -> Option<(Kind, &str)> {
        match &self.body {
            Body::Done { text } => Some((Kind::Done, text)),
            Body::Question { text } => Some((Kind::Question, text)),
            Body::Note { text } => Some((Kind::Note, text)),
            Body::Handoff { text } => Some((Kind::Handoff, text)),
            Body::Stop(_)
            | Body::Start { .. }
            | Body::Blocked {}
            | Body::Gone { .. }
            | Body::Moved {}
            | Body::Usage { .. }
            | Body::SubagentStart(_)
            | Body::SubagentStop(_) => None,
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
    /// spawn, restart or revive started the member's Claude. A revive
    /// resumes its conversation, so its context carries over.
    Start {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        resumed: bool,
    },
    /// herdr showed the member waiting on a dialog.
    Blocked {},
    /// herdr lost the member's pane; `pane`, `tab` or `workspace` when it was
    /// found by that closing, none when the pane exited or a watch saw it go.
    Gone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// herdr moved the member's pane out of the tab or workspace whose close
    /// recorded the `gone` before it, retracting that `gone`.
    Moved {},
    /// The member's tokens, recorded when it retired.
    Usage {
        models: Usage,
    },
    /// The member's Claude launched a subagent; written only by the subagent hook.
    #[serde(rename = "subagent-start")]
    SubagentStart(Subagent),
    /// One of the member's subagents finished.
    #[serde(rename = "subagent-stop")]
    SubagentStop(Subagent),
}

impl Body {
    pub fn report(kind: Kind, text: &str) -> Body {
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
    /// The context window that capped `limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::context::Window>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    pub id: String,
    #[serde(rename = "type")]
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The member's Claude session that launched it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Claude's `meta.json` for it, which holds its description; Claude writes
    /// it only after the start hook has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<PathBuf>,
}

impl Subagent {
    /// `reviewer  Review PR 325`, or the type alone without a description.
    pub fn label(&self) -> String {
        match &self.description {
            Some(description) => format!("{}  {description}", self.agent_type),
            None => self.agent_type.clone(),
        }
    }
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

/// Where `name`'s reports are appended: a directory of its own under `dir`,
/// so a sandbox can allow one agent's mailbox without the others.
pub fn mailbox_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name).join("mailbox.jsonl")
}

/// Where older releases kept `name`'s mailbox. It no longer grows, and is
/// read as the start of the mailbox, so cursor offsets into it stay valid.
pub fn flat_mailbox_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.jsonl"))
}

/// Whether `name` has a mailbox in either layout.
pub fn has_mailbox(dir: &Path, name: &str) -> bool {
    mailbox_path(dir, name).exists() || flat_mailbox_path(dir, name).exists()
}

/// The files holding `name`'s mailbox, for messages: those that exist, or
/// where its next report goes when neither does.
pub fn shown_mailbox(dir: &Path, name: &str) -> String {
    let (flat, path) = (flat_mailbox_path(dir, name), mailbox_path(dir, name));
    match (flat.exists(), path.exists()) {
        (true, true) => format!("{} and {}", flat.display(), path.display()),
        (true, false) => flat.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Opens `name`'s mailbox in `dir` for append, making its directory.
fn open_mailbox(dir: &Path, name: &str) -> Result<(PathBuf, Lock)> {
    let path = mailbox_path(dir, name);
    let parent = dir.join(name);
    fs::create_dir_all(&parent).with_context(|| format!("create {}", parent.display()))?;
    let file = lock_mailbox(&path)?;
    Ok((path, file))
}

/// Appends `entry` to `name`'s mailbox in `dir`.
pub fn append(dir: &Path, name: &str, entry: &Entry) -> Result<()> {
    let (path, mut file) = open_mailbox(dir, name)?;
    append_locked(&mut file, &path, entry)
}

/// Appends `entry` to `name`'s mailbox in `dir` unless `already` holds for
/// the entries there, read under the same lock. Returns whether it appended.
pub fn append_unless(
    dir: &Path,
    name: &str,
    entry: &Entry,
    already: impl Fn(&[Entry]) -> bool,
) -> Result<bool> {
    let (path, mut file) = open_mailbox(dir, name)?;
    let bytes = complete_lines(dir, name)?;
    let mut found = vec![];
    parse(dir, name, &bytes, &mut found, &mut vec![]);
    let entries: Vec<Entry> = found.into_iter().map(|(_, e)| e).collect();
    if already(&entries) {
        return Ok(false);
    }
    append_locked(&mut file, &path, entry)?;
    Ok(true)
}

fn append_locked(file: &mut fs::File, path: &Path, entry: &Entry) -> Result<()> {
    let mut bytes = vec![];
    // A torn last line would swallow this entry; start it on a line of its own.
    if !ends_in_newline(file).with_context(|| format!("read {}", path.display()))? {
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
    Lock::wait(file, crate::lock::BRIEF).with_context(|| format!("lock {}", path.display()))
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

/// The non-retired member of the herdr at `socket` that a process with
/// `$HERDR_PANE_ID` set to `own_pane` and `lineage` as itself and its ancestors runs in. Pane ids repeat across herdr sessions,
/// so an entry recorded without a socket matches only a caller that has none
/// either.
pub fn member_of_own_pane<'a>(
    entries: &'a BTreeMap<String, AgentRecord>,
    own_pane: &str,
    lineage: &[Process],
    socket: Option<&Path>,
) -> Option<&'a AgentRecord> {
    entries
        .values()
        .find(|e| !e.retired && e.is_own_pane(own_pane, lineage) && e.socket.as_deref() == socket)
}

/// The non-retired member of the herdr at `socket` in the pane herdr now
/// calls `pane`, which a move to another workspace changes.
pub fn member_in<'a>(
    entries: &'a BTreeMap<String, AgentRecord>,
    pane: &str,
    socket: Option<&Path>,
) -> Option<&'a AgentRecord> {
    entries
        .values()
        .find(|e| !e.retired && e.pane_id == pane && e.socket.as_deref() == socket)
}

pub fn reporter(ledger: &dyn Ledger, who: &Reporter) -> Result<String> {
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
    member_of_own_pane(&entries, pane, who.own_lineage, who.own_socket)
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
    /// With `all`, only entries at or after this time; ignored without it,
    /// since unread reports it hid would still be marked read.
    pub since: Option<SystemTime>,
    /// With `all`, only the last this many entries after the other filters;
    /// ignored without it.
    pub tail: Option<NonZeroUsize>,
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

/// `reports --since`: anything `stats --since` takes, or a span such as `30m`,
/// `2h` or `1d` back from `now`.
pub fn parse_since(value: &str, now: SystemTime) -> Result<SystemTime> {
    const ACCEPTED: &str = "a date (2026-11-01), RFC 3339 time (2026-11-01T12:00:00Z) \
                            or span back from now (30m, 2h, 1d)";
    let Some((count, unit)) = value
        .strip_suffix(['m', 'h', 'd'])
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|n| (n, &value[n.len()..]))
    else {
        return crate::stats::parse_moment(value, ACCEPTED);
    };
    let unit_secs = match unit {
        "m" => 60,
        "h" => 3600,
        _ => 86_400,
    };
    count
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit_secs))
        .and_then(|secs| now.checked_sub(Duration::from_secs(secs)))
        .ok_or_else(|| anyhow!("{value:?} is not {ACCEPTED}"))
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
            read_all(dir, &names),
            Pending::nothing(Cursor::Reports, dir),
        ),
        false => Cursor::Reports.read(dir, &names, locked, |e| e.report().is_some())?,
    };
    if filter.all {
        if let Some(since) = filter.since {
            found.retain(|(_, e)| humantime::parse_rfc3339_weak(&e.at).is_ok_and(|at| at >= since));
        }
        if let Some(tail) = filter.tail {
            found.drain(..found.len().saturating_sub(tail.get()));
        }
    } else {
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

/// Each of `names` with an unread `stop`, `blocked` or `gone` entry, with its
/// whole mailbox so a stop can be judged against what came before. Saving
/// `Pending` marks everything read on the watch cursor, which `reports` never
/// moves.
pub fn unread_wakes(dir: &Path, names: &[String]) -> Result<(Vec<Mailbox>, Pending)> {
    let names: Vec<String> = mailboxes(dir)?
        .into_iter()
        .filter(|n| names.contains(n))
        .collect();
    let mut stops = vec![];
    // `reports` warns about bad lines and unreadable mailboxes; its cursor
    // reads the same bytes.
    let (pending, _) = Cursor::Watch.peek(dir, &names, Locked::Take, |name, read, unread| {
        let mut entries = vec![];
        parse(dir, name, unread, &mut entries, &mut vec![]);
        let wakes =
            |e: &Entry| matches!(e.body, Body::Stop(_) | Body::Blocked {} | Body::Gone { .. });
        if !entries.iter().any(|(_, e)| wakes(e)) {
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
    if !has_mailbox(dir, name) {
        return Ok(SinceHandoff::None);
    }
    let bytes = complete_lines(dir, name)?;
    let mut entries = vec![];
    parse(dir, name, &bytes, &mut entries, &mut vec![]);
    let mut last = None;
    for (_, entry) in entries.into_iter().rev() {
        match entry.body {
            Body::Stop(stop) if last.is_none() => last = Some(stop),
            Body::Handoff { .. } => {
                return Ok(last.map_or(SinceHandoff::NotStopped, SinceHandoff::Stopped));
            }
            Body::Start { .. } => break,
            _ => {}
        }
    }
    Ok(SinceHandoff::None)
}

/// Every entry in `name`'s mailbox in file order, and a warning per bad line.
/// Reads no cursor and takes no lock.
pub fn history(dir: &Path, name: &str) -> Result<(Vec<Entry>, Vec<String>)> {
    if !has_mailbox(dir, name) {
        return Ok((vec![], vec![]));
    }
    let bytes = complete_lines(dir, name)?;
    let mut found = vec![];
    let mut warnings = vec![];
    parse(dir, name, &bytes, &mut found, &mut warnings);
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

/// Every entry of `names`, by time; an unreadable mailbox is a warning.
fn read_all(dir: &Path, names: &[String]) -> Found {
    let mut found = vec![];
    let mut warnings = vec![];
    for name in names {
        match complete_lines(dir, name) {
            Ok(bytes) => parse(dir, name, &bytes, &mut found, &mut warnings),
            Err(e) => warnings.push(format!("{e:#}")),
        }
    }
    found.sort_by(|(_, a), (_, b)| a.at.cmp(&b.at));
    (found, warnings)
}

/// The mailbox's complete lines, each file's in turn; a line still being
/// appended has no newline yet.
fn complete_lines(dir: &Path, name: &str) -> Result<Vec<u8>> {
    Ok(segments(dir, name)?
        .into_iter()
        .flat_map(|segment| segment.bytes)
        .collect())
}

/// One file of a mailbox, cut at its last complete line, under its own
/// cursor key, so growth in one file can't shift the other's offsets.
struct Segment {
    key: String,
    bytes: Vec<u8>,
}

/// The flat file an older release wrote, if any, then the new one; with
/// neither, the new one's read error.
fn segments(dir: &Path, name: &str) -> Result<Vec<Segment>> {
    let read = |path: &Path, key: String| {
        let mut bytes =
            fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        bytes.truncate(bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1));
        anyhow::Ok(Segment { key, bytes })
    };
    let (flat, path) = (flat_mailbox_path(dir, name), mailbox_path(dir, name));
    let mut found = vec![];
    if flat.exists() {
        found.push(read(&flat, name.to_string())?);
    }
    if !flat.exists() || path.exists() {
        // Agent names hold no `/`, so this key can't be another agent's.
        found.push(read(&path, format!("{name}/mailbox.jsonl"))?);
    }
    Ok(found)
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
                shown_mailbox(dir, name)
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
    let mut names = std::collections::BTreeSet::new();
    for entry in entries {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        match file_name.strip_suffix(".jsonl") {
            Some(name) => names.insert(name.to_string()),
            None if entry.path().join("mailbox.jsonl").exists() => names.insert(file_name),
            None => false,
        };
    }
    Ok(names.into_iter().collect())
}

fn render(name: &str, entry: &Entry) -> Vec<String> {
    let when = match entry.when() {
        cut if cut != entry.at => format!("{cut} UTC"),
        stored => stored,
    };
    let (kind, text) = match &entry.body {
        Body::Stop(stop) => match (stop.context, stop.limit) {
            (Some(context), Some(limit)) => (
                "stop",
                format!("{} {}k/{}k", describe(stop), kilo(context), kilo(limit)),
            ),
            _ => ("stop", describe(stop)),
        },
        Body::Start { .. } => return vec![format!("{name} start {when}")],
        Body::Blocked {} => return vec![format!("{name} blocked {when}")],
        Body::Gone { .. } => return vec![format!("{name} gone {when}")],
        Body::Moved {} => return vec![format!("{name} moved {when}")],
        Body::SubagentStart(subagent) => ("subagent-start", subagent.label()),
        Body::SubagentStop(subagent) => ("subagent-stop", subagent.label()),
        Body::Usage { models } => {
            let sum = crate::usage::total(models);
            let (read, wrote) = (kilo(sum.read()), kilo(sum.output));
            return vec![format!("{name} usage {when} in {read}k, out {wrote}k")];
        }
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
    tokens / 1000
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
        let mut bad_lines = vec![];
        let (pending, mut warnings) = self.peek(dir, names, locked, |name, _, unread| {
            parse(dir, name, unread, &mut found, &mut bad_lines)
        })?;
        warnings.extend(bad_lines);
        found.retain(|(_, e)| keep(e));
        Ok(((found, warnings), pending))
    }

    /// Calls `visit` with each of `names`' complete lines split at the
    /// cursor, already read then unread; returns the move past them, and a
    /// warning per unreadable mailbox, whose cursor stays put.
    fn peek(
        self,
        dir: &Path,
        names: &[String],
        locked: Locked,
        mut visit: impl FnMut(&str, &[u8], &[u8]),
    ) -> Result<(Pending, Vec<String>)> {
        let mut pending = Pending::nothing(self, dir);
        let mut unreadable = vec![];
        if names.is_empty() {
            return Ok((pending, unreadable));
        }
        let offsets = match locked {
            Locked::Held => self.load(dir)?,
            Locked::Take => {
                let _lock = self.lock(dir)?;
                self.load(dir)?
            }
        };
        for name in names {
            let segments = match segments(dir, name) {
                Ok(segments) => segments,
                Err(e) => {
                    unreadable.push(format!("{e:#}"));
                    continue;
                }
            };
            let (mut read, mut unread) = (vec![], vec![]);
            for Segment { key, bytes } in segments {
                let from = offsets.read_to(&key);
                // Past the end: the file was truncated or replaced, so all of it is new.
                let start = if from > bytes.len() { 0 } else { from };
                read.extend_from_slice(&bytes[..start]);
                unread.extend_from_slice(&bytes[start..]);
                pending.moves.push((key, from, bytes.len()));
            }
            visit(name, &read, &unread);
        }
        Ok((pending, unreadable))
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
        Lock::wait(file, crate::lock::BRIEF).with_context(|| format!("lock {}", path.display()))
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
    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};

    fn member(name: &str, pane: &str, herd: Option<&str>, retired: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
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
            outcome: None,
            released_at: None,
            herd: herd.map(Into::into),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            attended: None,
            no_slot: false,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
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
            own_lineage: &[],
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
            std::fs::read_to_string(dir.path().join("lh-2/mailbox.jsonl")).unwrap(),
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
            own_lineage: &[],
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
        assert!(dir.path().join("lh-1/mailbox.jsonl").exists());
        assert!(!has_mailbox(dir.path(), "lh-2"));
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
                own_lineage: &[],
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
            own_lineage: &[],
        };
        report(ledger, dir, &who, kind, text, at(time)).unwrap();
    }

    const UNREAD: Filter = Filter {
        names: &[],
        herd: None,
        all: false,
        since: None,
        tail: None,
    };

    const ALL: Filter = Filter {
        all: true,
        ..UNREAD
    };

    fn tail(n: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(n)
    }

    /// lh-1 in herd cards and zpr-2 in herd prs, sending alternately on the hour.
    fn two_herds_mail(dir: &Path) -> MemLedger {
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", Some("cards"), false),
            member("zpr-2", "w1:p2", Some("prs"), false),
        ]);
        for (name, text, time) in [
            ("lh-1", "a", "2026-11-01T09:00:00Z"),
            ("zpr-2", "b", "2026-11-01T10:00:00Z"),
            ("lh-1", "c", "2026-11-01T11:00:00Z"),
            ("zpr-2", "d", "2026-11-01T12:00:00Z"),
        ] {
            send(&ledger, dir, name, Kind::Note, text, time);
        }
        ledger
    }

    #[test]
    fn since_keeps_entries_at_or_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = two_herds_mail(dir.path());
        let since = Filter {
            since: Some(at("2026-11-01T11:00:00Z")),
            ..ALL
        };

        assert_eq!(
            reports(&ledger, dir.path(), &since).unwrap().lines,
            [
                "lh-1 note 2026-11-01 11:00 UTC c",
                "zpr-2 note 2026-11-01 12:00 UTC d",
            ]
        );
    }

    #[test]
    fn tail_keeps_the_last_entries_after_the_other_filters() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = two_herds_mail(dir.path());
        let lines = |filter: Filter| reports(&ledger, dir.path(), &filter).unwrap().lines;

        assert_eq!(
            lines(Filter {
                tail: tail(3),
                ..ALL
            }),
            [
                "zpr-2 note 2026-11-01 10:00 UTC b",
                "lh-1 note 2026-11-01 11:00 UTC c",
                "zpr-2 note 2026-11-01 12:00 UTC d",
            ]
        );
        assert_eq!(
            lines(Filter {
                tail: tail(1),
                since: Some(at("2026-11-01T10:00:00Z")),
                ..ALL
            }),
            ["zpr-2 note 2026-11-01 12:00 UTC d"]
        );
        assert_eq!(
            lines(Filter {
                tail: tail(5),
                since: Some(at("2026-11-01T10:00:00Z")),
                ..ALL
            }),
            [
                "zpr-2 note 2026-11-01 10:00 UTC b",
                "lh-1 note 2026-11-01 11:00 UTC c",
                "zpr-2 note 2026-11-01 12:00 UTC d",
            ]
        );
        assert_eq!(
            lines(Filter {
                tail: tail(1),
                herd: Some("cards"),
                ..ALL
            }),
            ["lh-1 note 2026-11-01 11:00 UTC c"]
        );
    }

    #[test]
    fn tail_counts_entries_and_keeps_their_continuation_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        for (text, time) in [
            ("old", "2026-11-01T09:00:00Z"),
            ("Which store?\n- sqlite\n- json", "2026-11-01T10:00:00Z"),
            ("merged", "2026-11-01T11:00:00Z"),
        ] {
            send(&ledger, dir.path(), "lh-1", Kind::Note, text, time);
        }
        let last_two = Filter {
            tail: tail(2),
            ..ALL
        };

        assert_eq!(
            reports(&ledger, dir.path(), &last_two).unwrap().lines,
            [
                "lh-1 note 2026-11-01 10:00 UTC Which store?",
                "    - sqlite",
                "    - json",
                "lh-1 note 2026-11-01 11:00 UTC merged",
            ]
        );
    }

    #[test]
    fn unread_reports_ignore_since_and_tail_so_none_is_marked_read_unseen() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = two_herds_mail(dir.path());
        let filtered = Filter {
            since: Some(at("2026-11-01T12:00:00Z")),
            tail: tail(1),
            ..UNREAD
        };

        assert_eq!(
            reports(&ledger, dir.path(), &filtered).unwrap().lines.len(),
            4
        );
    }

    #[test]
    fn since_takes_a_date_a_time_or_a_span_back_from_now() {
        let now = at("2026-11-16T15:00:00Z");
        for (value, expected) in [
            ("2026-11-01", "2026-11-01T00:00:00Z"),
            ("2026-11-01T12:00:00Z", "2026-11-01T12:00:00Z"),
            ("2026-11-01T12:00:00+05:00", "2026-11-01T07:00:00Z"),
            ("30m", "2026-11-16T14:30:00Z"),
            ("2h", "2026-11-16T13:00:00Z"),
            ("1d", "2026-11-15T15:00:00Z"),
        ] {
            assert_eq!(parse_since(value, now).unwrap(), at(expected), "{value}");
        }
    }

    #[test]
    fn a_bad_since_names_every_accepted_form() {
        let now = at("2026-11-16T15:00:00Z");
        for value in [
            "yesterday",
            "30",
            "2w",
            "m",
            "-1h",
            "213503982334602d",
            "200000000000000000m",
        ] {
            let error = parse_since(value, now).unwrap_err().to_string();
            assert_eq!(
                error,
                format!(
                    "{value:?} is not a date (2026-11-01), RFC 3339 time \
                     (2026-11-01T12:00:00Z) or span back from now (30m, 2h, 1d)"
                ),
            );
        }
    }

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
                "lh-2 question 2026-11-01 11:00 UTC which DB?",
                "lh-2 note 2026-11-01 09:00 UTC starting",
                "lh-1 done 2026-11-01 10:00 UTC PR #9",
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
            ["lh-1 note 2026-11-01 12:00 UTC merged main"]
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
                "lh-1 note 2026-11-01 09:00 UTC one",
                "lh-1 note 2026-11-01 10:00 UTC two",
            ]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            ["lh-1 note 2026-11-01 10:00 UTC two"]
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
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
        let by_herd = Filter {
            herd: Some("cards"),
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &by_herd).unwrap().lines,
            ["lh-2 note 2026-11-01 09:00 UTC hi"]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            ["zpr-3 note 2026-11-01 09:00 UTC hi"]
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
                "lh-1 question 2026-11-01 09:00 UTC Which store?",
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

    fn append(path: &Path, bytes: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes.as_bytes()).unwrap();
    }

    #[test]
    fn a_flat_mailbox_from_an_older_release_reads_as_the_start_of_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        std::fs::write(
            flat_mailbox_path(dir.path(), "lh-1"),
            concat!(
                r#"{"at":"2026-11-01T09:00:00Z","kind":"note","text":"read"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines.len(),
            1
        );
        append(
            &flat_mailbox_path(dir.path(), "lh-1"),
            concat!(
                r#"{"at":"2026-11-01T09:30:00Z","kind":"note","text":"unread"}"#,
                "\n",
            ),
        );

        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Done,
            "new",
            "2026-11-01T10:00:00Z",
        );

        let read = reports(&ledger, dir.path(), &UNREAD).unwrap();
        assert_eq!(
            read.lines,
            [
                "lh-1 note 2026-11-01 09:30 UTC unread",
                "lh-1 done 2026-11-01 10:00 UTC new",
            ]
        );
        let (history, warnings) = history(dir.path(), "lh-1").unwrap();
        let texts: Vec<_> = history
            .iter()
            .filter_map(|e| e.report())
            .map(|r| r.1)
            .collect();
        assert_eq!((texts, warnings), (vec!["read", "unread", "new"], vec![]));
        assert_eq!(
            std::fs::read_to_string(flat_mailbox_path(dir.path(), "lh-1"))
                .unwrap()
                .lines()
                .count(),
            2,
            "the flat mailbox no longer grows"
        );
    }

    fn note(at: &str, text: &str) -> String {
        format!("{{\"at\":\"{at}\",\"kind\":\"note\",\"text\":\"{text}\"}}\n")
    }

    fn unread_texts(ledger: &MemLedger, dir: &Path) -> Vec<String> {
        let read = reports(ledger, dir, &UNREAD).unwrap();
        assert_eq!(read.warnings, Vec::<String>::new());
        read.entries
            .iter()
            .filter_map(|(_, e)| e.report().map(|(_, text)| text.to_string()))
            .collect()
    }

    #[test]
    fn an_older_release_still_appending_to_the_flat_mailbox_has_each_report_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let flat = flat_mailbox_path(dir.path(), "lh-1");
        std::fs::write(&flat, note("2026-11-01T09:00:00Z", "flat-one")).unwrap();
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "new-one",
            "2026-11-01T09:10:00Z",
        );
        assert_eq!(unread_texts(&ledger, dir.path()), ["flat-one", "new-one"]);

        let flat_two = note("2026-11-01T09:20:00Z", "flat-two");
        let (torn, rest) = flat_two.split_at(20);
        append(&flat, torn);
        assert_eq!(unread_texts(&ledger, dir.path()), Vec::<String>::new());
        append(&flat, rest);
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "new-two",
            "2026-11-01T09:30:00Z",
        );

        assert_eq!(unread_texts(&ledger, dir.path()), ["flat-two", "new-two"]);
    }

    #[test]
    fn deleting_the_flat_mailbox_leaves_the_new_ones_reports_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let flat = flat_mailbox_path(dir.path(), "lh-1");
        std::fs::write(&flat, note("2026-11-01T09:00:00Z", "flat-one")).unwrap();
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "new-one",
            "2026-11-01T09:10:00Z",
        );
        assert_eq!(unread_texts(&ledger, dir.path()), ["flat-one", "new-one"]);

        std::fs::remove_file(&flat).unwrap();
        send(
            &ledger,
            dir.path(),
            "lh-1",
            Kind::Note,
            "new-two",
            "2026-11-01T09:30:00Z",
        );

        assert_eq!(unread_texts(&ledger, dir.path()), ["new-two"]);
    }

    #[test]
    fn append_unless_sees_an_entry_only_the_flat_mailbox_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            flat_mailbox_path(dir.path(), "lh-1"),
            note("2026-11-01T09:00:00Z", "once"),
        )
        .unwrap();
        let entry = Entry::new(at("2026-11-01T10:00:00Z"), Body::report(Kind::Note, "once"));
        let has_once = |entries: &[Entry]| {
            entries
                .iter()
                .any(|e| e.report() == Some((Kind::Note, "once")))
        };

        assert!(!append_unless(dir.path(), "lh-1", &entry, has_once).unwrap());
        assert_eq!(history(dir.path(), "lh-1").unwrap().0.len(), 1);
    }

    #[test]
    fn a_handoff_only_the_flat_mailbox_holds_is_found_with_the_stop_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let stop = r#"{"at":"2026-11-01T10:00:00Z","kind":"stop","session":"s1","crons":0,"background":{"shells":0,"subagents":0,"other":0}}"#;
        std::fs::write(
            flat_mailbox_path(dir.path(), "lh-1"),
            format!(
                "{}{}{stop}\n",
                r#"{"at":"2026-11-01T08:00:00Z","kind":"start"}"#.to_string() + "\n",
                r#"{"at":"2026-11-01T09:00:00Z","kind":"handoff","text":"bye"}"#.to_string() + "\n",
            ),
        )
        .unwrap();

        match since_handoff(dir.path(), "lh-1").unwrap() {
            SinceHandoff::Stopped(stop) => assert_eq!(stop.session.as_deref(), Some("s1")),
            other => panic!("{other:?}"),
        }
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
        append(
            &mailbox_path(dir.path(), "lh-1"),
            "{\"at\":\"2026-11-01T09:30",
        );
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
                "lh-1 note 2026-11-01 09:00 UTC one",
                "lh-1 done 2026-11-01 10:00 UTC two",
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
        append(&mailbox_path(dir.path(), "lh-1"), "not json\n");
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
                "lh-1 note 2026-11-01 09:00 UTC one",
                "lh-1 note 2026-11-01 10:00 UTC two",
                "lh-2 note 2026-11-01 11:00 UTC hi",
            ]
        );
        assert_eq!(read.warnings.len(), 1);
        assert!(
            read.warnings[0].contains("lh-1/mailbox.jsonl"),
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
        std::fs::remove_file(mailbox_path(dir.path(), "lh-1")).unwrap();
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
            ["lh-1 note 2026-11-01 10:00 UTC new"]
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
            window: None,
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
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:00:00Z",
            Body::Start { resumed: false },
        );
        put(
            dir.path(),
            "lh-2",
            "2026-11-01T09:01:00Z",
            Body::Start { resumed: false },
        );
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
                "lh-2 question 2026-11-01 09:04 UTC which?",
                "lh-1 note 2026-11-01 09:02 UTC hi",
            ]
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 start 2026-11-01 09:00 UTC",
                "lh-2 start 2026-11-01 09:01 UTC",
                "lh-1 note 2026-11-01 09:02 UTC hi",
                "lh-1 stop 2026-11-01 09:03 UTC paused: 1 shell",
                "lh-2 question 2026-11-01 09:04 UTC which?",
            ]
        );
    }

    #[test]
    fn all_shows_a_stops_context_use_in_whole_thousands_rounded_down_when_it_was_measured() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let measured = Stop {
            context: Some(120_400),
            limit: Some(150_000),
            ..finished_stop()
        };
        let nearly_full = Stop {
            context: Some(149_600),
            ..measured.clone()
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
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:02:00Z",
            Body::Stop(nearly_full),
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 stop 2026-11-01 09:00 UTC finished 120k/150k",
                "lh-1 stop 2026-11-01 09:01 UTC finished",
                "lh-1 stop 2026-11-01 09:02 UTC finished 149k/150k",
            ]
        );
    }

    #[test]
    fn all_shows_a_subagents_start_and_stop_with_its_type_and_any_description() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None, false)]);
        let reviewer = |description: Option<&str>| Subagent {
            id: "a1f".into(),
            agent_type: "reviewer".into(),
            description: description.map(str::to_string),
            session: Some("s1".into()),
            meta: None,
        };
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:00:00Z",
            Body::SubagentStart(reviewer(None)),
        );
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:04:00Z",
            Body::SubagentStop(reviewer(Some("Review PR 325 round 1"))),
        );
        let all = Filter {
            all: true,
            ..UNREAD
        };
        assert_eq!(
            reports(&ledger, dir.path(), &all).unwrap().lines,
            [
                "lh-1 subagent-start 2026-11-01 09:00 UTC reviewer",
                "lh-1 subagent-stop 2026-11-01 09:04 UTC reviewer  Review PR 325 round 1",
            ]
        );
        assert_eq!(
            reports(&ledger, dir.path(), &UNREAD).unwrap().lines,
            [] as [&str; 0]
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
        let (stops, pending) = unread_wakes(dir, names).unwrap();
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

        let (stops, _unsaved) = unread_wakes(dir.path(), &names).unwrap();
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
        put(
            dir.path(),
            "lh-1",
            "2026-11-01T09:00:00Z",
            Body::Start { resumed: false },
        );
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
        assert_eq!(lh1.entries[0].body, Body::Start { resumed: false });
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
                "lh-2 note 2026-11-01 09:04 UTC no stop",
                "lh-1 note 2026-11-01 09:05 UTC more"
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
                    window: None,
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
                    window: None,
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
                window: None,
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
            window: None,
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
            own_lineage: &[],
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
