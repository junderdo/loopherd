//! herdr's `blocked` and `gone` as mailbox entries. The herdr plugin's event
//! hook records them as they happen, and a watch's poll records what it sees
//! first; a watch prints them only from the mailbox, so each prints once.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::ledger::Ledger;
use crate::notify;
use crate::report::{self, Body, Entry, member_in};

/// The suffix of the mark `<member>.block-recorded`, present while the
/// member's current block has its `blocked` entry.
pub const BLOCK_RECORDED: &str = "block-recorded";

/// Records the herdr event `event`, as `HERDR_PLUGIN_EVENT` names it, with
/// `json` from `HERDR_PLUGIN_EVENT_JSON`, for the non-retired member in its
/// pane of the herdr at `socket`. Any other pane records nothing.
pub fn event_hook(
    ledger: &dyn Ledger,
    dir: &Path,
    event: &str,
    json: &str,
    socket: Option<&Path>,
    now: SystemTime,
) -> Result<()> {
    let envelope: Value = serde_json::from_str(json).context("parse HERDR_PLUGIN_EVENT_JSON")?;
    let data = &envelope["data"];
    let Some(pane) = data["pane_id"].as_str() else {
        return Ok(());
    };
    let entries = ledger.entries()?;
    let Some(member) = member_in(&entries, pane, socket) else {
        return Ok(());
    };
    match (event, data["agent_status"].as_str()) {
        ("pane.agent_status_changed", Some("blocked")) => {
            record_blocked(dir, &member.name, now)?;
        }
        ("pane.agent_status_changed", Some("working" | "idle" | "done")) => {
            notify::forget_block(dir, &member.name)?;
        }
        ("pane.exited" | "pane.closed", _) => {
            record_gone(dir, &member.name, now)?;
        }
        _ => {}
    }
    Ok(())
}

/// Appends a `blocked` entry to `name`'s mailbox unless its current block
/// already has one. Returns whether it appended.
pub fn record_blocked(dir: &Path, name: &str, now: SystemTime) -> Result<bool> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mark = dir.join(format!("{name}.{BLOCK_RECORDED}"));
    match OpenOptions::new().write(true).create_new(true).open(&mark) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("create {}", mark.display())),
    }
    if let Err(e) = report::append(dir, name, &Entry::new(now, Body::Blocked {})) {
        // Unmarked, the next blocked event or poll records it again.
        let _ = fs::remove_file(&mark);
        return Err(e);
    }
    Ok(true)
}

/// Appends a `gone` entry to `name`'s mailbox unless one is there since its
/// last start. Returns whether it appended.
pub fn record_gone(dir: &Path, name: &str, now: SystemTime) -> Result<bool> {
    report::append_unless(dir, name, &Entry::new(now, Body::Gone {}), |entries| {
        entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e.body, Body::Start { .. }))
            .any(|e| matches!(e.body, Body::Gone {}))
    })
}

/// Appends a `gone` entry to `name`'s mailbox unless one is there from
/// `seen_live` on, when a watch last saw the member live: a member back
/// in its pane without a start can go again.
pub fn record_gone_after(
    dir: &Path,
    name: &str,
    now: SystemTime,
    seen_live: SystemTime,
) -> Result<bool> {
    report::append_unless(dir, name, &Entry::new(now, Body::Gone {}), |entries| {
        entries.iter().any(|e| {
            matches!(e.body, Body::Gone {})
                && humantime::parse_rfc3339_weak(&e.at).is_ok_and(|at| at >= seen_live)
        })
    })
}
