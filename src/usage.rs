//! Tokens a member's Claude spent, summed from its sessions' transcripts.
//! Claude deletes transcripts after `cleanupPeriodDays`, so retiring a worker
//! keeps its totals in its mailbox as a `usage` entry (D92).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::continuation::transcript_of;
use crate::ledger::AgentRecord;
use crate::report::{self, Body, Entry};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// Everything the model read: input plus cache write and cache read.
    pub fn read(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
    }

    fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
    }
}

/// Tokens per model.
pub type Usage = BTreeMap<String, Tokens>;

pub fn total(usage: &Usage) -> Tokens {
    let mut sum = Tokens::default();
    usage.values().for_each(|t| sum.add(t));
    sum
}

/// Appends a worker's tokens to its mailbox in `mailboxes` as a `usage`
/// entry, unless its run has one; a warning line when that fails. Leads, and
/// workers with no transcript left, get none.
pub fn record_at_retire(
    mailboxes: &Path,
    claude_dir: Option<&Path>,
    member: &AgentRecord,
    now: SystemTime,
) -> Option<String> {
    if member.is_lead() {
        return None;
    }
    let error = record(mailboxes, claude_dir?, member, now).err()?;
    Some(format!("warning: tokens not recorded: {error:#}"))
}

fn record(
    mailboxes: &Path,
    claude_dir: &Path,
    member: &AgentRecord,
    now: SystemTime,
) -> Result<()> {
    let created = parse_time(&member.created);
    // A reused name's mailbox still holds its earlier run's entries.
    let in_run = |e: &&Entry| created.is_none_or(|c| parse_time(&e.at).is_some_and(|at| at >= c));
    let (entries, _) = report::history(mailboxes, &member.name)?;
    let run = sessions(entries.iter().filter(in_run), member.session_id.as_deref());
    let Some(models) = of_sessions(claude_dir, &run, &Window::default()) else {
        return Ok(());
    };
    // At the retirement, where stats looks, even when a rerun records it.
    let at = member
        .retired_at
        .as_deref()
        .and_then(parse_time)
        .unwrap_or(now);
    let entry = Entry::new(at, Body::Usage { models });
    report::append_unless(mailboxes, &member.name, &entry, |entries| {
        entries
            .iter()
            .filter(in_run)
            .any(|e| matches!(e.body, Body::Usage { .. }))
    })?;
    Ok(())
}

/// The sessions named by `entries`' `stop`s, then `current` (the ledger's,
/// which a first turn has before any `stop`), each once.
pub fn sessions<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    current: Option<&str>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter_map(|e| match &e.body {
            Body::Stop(stop) => stop.session.clone(),
            _ => None,
        })
        .chain(current.map(str::to_string))
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// From `since` on and before `until`; a missing bound is open.
#[derive(Debug, Default, Clone, Copy)]
pub struct Window {
    pub since: Option<SystemTime>,
    pub until: Option<SystemTime>,
}

impl Window {
    pub fn contains(&self, at: SystemTime) -> bool {
        self.since.is_none_or(|since| at >= since) && self.until.is_none_or(|until| at < until)
    }

    pub fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }
}

/// The tokens of every assistant message in `sessions`' transcripts under
/// `claude_dir` and their `subagents/`, each message id once, keeping
/// messages stamped within `window`. `None` when no transcript is found.
pub fn of_sessions(claude_dir: &Path, sessions: &[String], window: &Window) -> Option<Usage> {
    let files: Vec<PathBuf> = sessions
        .iter()
        .filter_map(|s| transcript_of(claude_dir, s))
        .flat_map(|t| {
            let subagents = t.with_extension("").join("subagents");
            std::iter::once(t).chain(jsonl_in(&subagents))
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    let mut usage = Usage::new();
    let mut seen = HashSet::new();
    for file in files {
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if record["type"] != "assistant" {
                continue;
            }
            if !window.is_open()
                && record["timestamp"]
                    .as_str()
                    .and_then(parse_time)
                    .is_none_or(|at| !window.contains(at))
            {
                continue;
            }
            let message = &record["message"];
            if let Some(id) = message["id"].as_str()
                && !seen.insert(id.to_string())
            {
                continue;
            }
            // Claude's own notices, with zero usage.
            let Some(model) = message["model"].as_str().filter(|m| *m != "<synthetic>") else {
                continue;
            };
            let count = |key: &str| message["usage"][key].as_u64().unwrap_or(0);
            usage.entry(model.to_string()).or_default().add(&Tokens {
                input: count("input_tokens"),
                cache_write: count("cache_creation_input_tokens"),
                cache_read: count("cache_read_input_tokens"),
                output: count("output_tokens"),
            });
        }
    }
    Some(usage)
}

fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    files
}

fn parse_time(at: &str) -> Option<SystemTime> {
    humantime::parse_rfc3339_weak(at).ok()
}
