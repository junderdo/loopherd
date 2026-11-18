//! herdr's `blocked` and `gone` as mailbox entries. The herdr plugin's event
//! hook records them as they happen, and a watch's poll records what it sees
//! first; a watch prints them only from the mailbox, so each prints once.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::ledger::{AgentRecord, Ledger};
use crate::notify;
use crate::report::{self, Body, Entry, member_in};

/// The suffix of the mark `<member>.block-recorded`, present while the
/// member's current block has its `blocked` entry.
pub const BLOCK_RECORDED: &str = "block-recorded";

/// Records the herdr event `event`, as `HERDR_PLUGIN_EVENT` names it, with
/// `json` from `HERDR_PLUGIN_EVENT_JSON`, for the non-retired members of the
/// herdr at `socket` in its pane, tab or workspace, and follows such a
/// member's pane moved to another tab or workspace in the ledger. Anything
/// else records nothing.
pub fn event_hook(
    ledger: &mut dyn Ledger,
    dir: &Path,
    event: &str,
    json: &str,
    socket: Option<&Path>,
    now: SystemTime,
) -> Result<()> {
    let envelope: Value = serde_json::from_str(json).context("parse HERDR_PLUGIN_EVENT_JSON")?;
    let data = &envelope["data"];
    let entries = ledger.entries()?;
    let (key, held_in): (_, fn(&AgentRecord) -> Option<&str>) = match event {
        "tab.closed" => ("tab_id", |m| m.tab_id.as_deref()),
        "workspace.closed" => ("workspace_id", |m| Some(&m.workspace)),
        "pane.moved" => return follow_move(ledger, &entries, data, socket),
        _ => return pane_event(&entries, dir, event, data, socket, now),
    };
    let Some(id) = data[key].as_str() else {
        return Ok(());
    };
    let members = entries
        .values()
        .filter(|m| !m.retired && m.socket.as_deref() == socket && held_in(m) == Some(id));
    let failed: Vec<String> = members
        .filter_map(|m| record_gone(dir, &m.name, now).err())
        .map(|e| format!("{e:#}"))
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(anyhow!(failed.join("; "))),
    }
}

/// Points the member at its pane's new pane, tab and workspace. A move to
/// another workspace gives the pane a new id, while the agent keeps the old
/// one in `$HERDR_PANE_ID`, so that one is kept as `env_pane_id`.
fn follow_move(
    ledger: &mut dyn Ledger,
    entries: &BTreeMap<String, AgentRecord>,
    data: &Value,
    socket: Option<&Path>,
) -> Result<()> {
    let (Some(previous_pane), Some(previous_tab), Some(pane), Some(tab)) = (
        data["previous_pane_id"].as_str(),
        data["previous_tab_id"].as_str(),
        data["pane"]["pane_id"].as_str(),
        data["pane"]["tab_id"].as_str(),
    ) else {
        return Ok(());
    };
    let workspaces = (
        data["previous_workspace_id"].as_str(),
        data["pane"]["workspace_id"].as_str(),
    );
    let workspace = match workspaces {
        _ if pane == previous_pane => None,
        (Some(previous), Some(workspace)) => Some((previous, workspace)),
        _ => return Ok(()),
    };
    let Some(member) = member_in(entries, previous_pane, socket) else {
        return Ok(());
    };
    // Hooks run unordered, so a late event for a place the member already left is skipped.
    ledger.update(&member.name, &mut |m| {
        let still_there = m.pane_id == previous_pane
            && m.tab_id.as_deref().is_none_or(|t| t == previous_tab)
            && workspace.is_none_or(|(previous, _)| m.workspace == previous);
        if m.retired || !still_there {
            return Ok(());
        }
        if let Some((_, workspace)) = workspace {
            m.env_pane_id.get_or_insert_with(|| previous_pane.into());
            m.pane_id = pane.into();
            m.workspace = workspace.into();
        }
        m.tab_id = Some(tab.into());
        Ok(())
    })?;
    Ok(())
}

fn pane_event(
    entries: &BTreeMap<String, AgentRecord>,
    dir: &Path,
    event: &str,
    data: &Value,
    socket: Option<&Path>,
    now: SystemTime,
) -> Result<()> {
    let Some(pane) = data["pane_id"].as_str() else {
        return Ok(());
    };
    let Some(member) = member_in(entries, pane, socket) else {
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

/// Appends a `gone` entry to `name`'s mailbox unless one is there with no
/// sign of life after it. Returns whether it appended.
pub fn record_gone(dir: &Path, name: &str, now: SystemTime) -> Result<bool> {
    report::append_unless(dir, name, &Entry::new(now, Body::Gone {}), |entries| {
        entries
            .iter()
            .rev()
            .find(|e| matches!(e.body, Body::Gone {}) || shows_alive(&e.body))
            .is_some_and(|e| matches!(e.body, Body::Gone {}))
    })
}

/// Whether the member itself, or a start of its Claude, wrote `body`. A
/// note may come from loopherd, as a revive's to a lead does.
fn shows_alive(body: &Body) -> bool {
    match body {
        Body::Start { .. }
        | Body::Stop(_)
        | Body::Blocked {}
        | Body::Done { .. }
        | Body::Question { .. }
        | Body::Handoff { .. } => true,
        Body::Note { .. } | Body::Gone {} => false,
    }
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
