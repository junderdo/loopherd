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
        "pane.moved" => return follow_move(ledger, &entries, dir, data, socket, now),
        _ => return pane_event(&entries, dir, event, data, socket, now),
    };
    let Some(id) = data[key].as_str() else {
        return Ok(());
    };
    let closed = match event {
        "tab.closed" => Closed::Tab(id),
        _ => Closed::Workspace(id),
    };
    let failed: Vec<String> = entries
        .values()
        .filter(|m| !m.retired && m.socket.as_deref() == socket && held_in(m) == Some(id))
        .filter_map(|m| record_closed_gone(&*ledger, dir, &m.name, closed, now).err())
        .map(|e| format!("{e:#}"))
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(anyhow!(failed.join("; "))),
    }
}

/// Points the member at its pane's new pane, tab and workspace. A move to
/// another workspace gives the pane a new id, while the agent keeps the old
/// one in `$HERDR_PANE_ID`, so that one is kept as `env_pane_id`. Retracts a
/// `gone` that closing the tab or workspace it left recorded first.
fn follow_move(
    ledger: &mut dyn Ledger,
    entries: &BTreeMap<String, AgentRecord>,
    dir: &Path,
    data: &Value,
    socket: Option<&Path>,
    now: SystemTime,
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
    let previous_workspace = data["previous_workspace_id"].as_str();
    let mut followed = false;
    // Hooks run unordered, so a late event for a place the member already left is skipped.
    ledger.update(&member.name, &mut |m| {
        let still_there = m.pane_id == previous_pane
            && m.tab_id.as_deref().is_none_or(|t| t == previous_tab)
            && workspace.is_none_or(|(previous, _)| m.workspace == previous);
        followed = !m.retired && still_there;
        if !followed {
            return Ok(());
        }
        if let Some((_, workspace)) = workspace {
            m.env_pane_id.get_or_insert_with(|| previous_pane.into());
            m.pane_id = pane.into();
            m.workspace = workspace.into();
            // The payload has no label, and the old one would send a revive back.
            m.workspace_label = None;
        }
        m.tab_id = Some(tab.into());
        Ok(())
    })?;
    if !followed {
        return Ok(());
    }
    let left = |body: &Body| match body {
        Body::Gone { tab: Some(t), .. } => t == previous_tab,
        Body::Gone {
            workspace: Some(w), ..
        } => Some(w.as_str()) == previous_workspace,
        _ => false,
    };
    report::append_unless(
        dir,
        &member.name,
        &Entry::new(now, Body::Moved {}),
        |entries| {
            // A gone for a place this live pane left is false whatever came after it.
            let last = entries
                .iter()
                .rev()
                .find(|e| matches!(e.body, Body::Gone { .. } | Body::Moved {}));
            !last.is_some_and(|e| left(&e.body))
        },
    )?;
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
    let gone = Entry::new(
        now,
        Body::Gone {
            tab: None,
            workspace: None,
        },
    );
    report::append_unless(dir, name, &gone, already_gone)
}

#[derive(Clone, Copy)]
enum Closed<'a> {
    Tab(&'a str),
    Workspace(&'a str),
}

/// [`record_gone`] for `name` found in the `closed` tab or workspace, unless
/// the ledger, read under the mailbox lock, no longer holds it there.
fn record_closed_gone(
    ledger: &dyn Ledger,
    dir: &Path,
    name: &str,
    closed: Closed,
    now: SystemTime,
) -> Result<bool> {
    let (body, still_in): (_, fn(&AgentRecord, &str) -> bool) = match closed {
        Closed::Tab(tab) => (
            Body::Gone {
                tab: Some(tab.into()),
                workspace: None,
            },
            |m, tab| m.tab_id.as_deref() == Some(tab),
        ),
        Closed::Workspace(workspace) => (
            Body::Gone {
                tab: None,
                workspace: Some(workspace.into()),
            },
            |m, workspace| m.workspace == workspace,
        ),
    };
    let (Closed::Tab(id) | Closed::Workspace(id)) = closed;
    report::append_unless(dir, name, &Entry::new(now, body), |entries| {
        let moved_out = ledger
            .entries()
            .is_ok_and(|e| e.get(name).is_some_and(|m| !still_in(m, id)));
        moved_out || already_gone(entries)
    })
}

fn already_gone(entries: &[Entry]) -> bool {
    entries
        .iter()
        .rev()
        .find(|e| gone_or_alive(&e.body))
        .is_some_and(|e| matches!(e.body, Body::Gone { .. }))
}

/// Whether `body` is a `gone` or a sign of life: written by the member
/// itself, a start of its Claude, or a move of its pane. A note may come
/// from loopherd, as a revive's to a lead does.
fn gone_or_alive(body: &Body) -> bool {
    match body {
        Body::Start { .. }
        | Body::Stop(_)
        | Body::Blocked {}
        | Body::Done { .. }
        | Body::Question { .. }
        | Body::Handoff { .. }
        | Body::Moved {}
        | Body::Gone { .. } => true,
        Body::Note { .. } => false,
    }
}

/// Whether a `moved` in `later`, the entries after a `gone`, retracts it.
pub fn retracted(later: &[Entry]) -> bool {
    later
        .iter()
        .find(|e| matches!(e.body, Body::Gone { .. } | Body::Moved {}))
        .is_some_and(|e| matches!(e.body, Body::Moved {}))
}

/// Appends a `gone` entry to `name`'s mailbox unless one is there from
/// `seen_live` on, when a watch last saw the member live: a member back
/// in its pane without a start can go again. A retracted `gone` doesn't count.
pub fn record_gone_after(
    dir: &Path,
    name: &str,
    now: SystemTime,
    seen_live: SystemTime,
) -> Result<bool> {
    let gone = Entry::new(
        now,
        Body::Gone {
            tab: None,
            workspace: None,
        },
    );
    report::append_unless(dir, name, &gone, |entries| {
        entries.iter().enumerate().any(|(i, e)| {
            matches!(e.body, Body::Gone { .. })
                && !retracted(&entries[i + 1..])
                && humantime::parse_rfc3339_weak(&e.at).is_ok_and(|at| at >= seen_live)
        })
    })
}
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use super::*;
    use crate::ledger::{MemLedger, RemovedHerd};

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn lh_1_in(tab: &str) -> MemLedger {
        let record: AgentRecord = serde_json::from_value(serde_json::json!({
            "name": "lh-1", "pane_id": "w1:p1", "tab_id": tab, "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z", "socket": SOCKET,
        }))
        .unwrap();
        MemLedger(BTreeMap::from([("lh-1".into(), record)]), vec![])
    }

    /// Answers its first read from `stale`, and later ones from `now`, as
    /// when a move hook lands between a close hook's reads.
    struct Moving {
        stale: MemLedger,
        now: MemLedger,
        read: Cell<bool>,
    }

    impl Ledger for Moving {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            match self.read.replace(true) {
                false => self.stale.entries(),
                true => self.now.entries(),
            }
        }
        fn record(&mut self, _: AgentRecord) -> Result<()> {
            unreachable!()
        }
        fn update(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            unreachable!()
        }
        fn remove_herd(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&[String]) -> Result<()>,
        ) -> Result<RemovedHerd> {
            unreachable!()
        }
        fn started(&mut self, _: &str, _: SystemTime, _: bool) -> Result<()> {
            unreachable!()
        }
    }

    fn kinds(dir: &Path) -> Vec<String> {
        let path = dir.join("lh-1.jsonl");
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["kind"].to_string())
            .collect()
    }

    #[test]
    fn a_tab_close_whose_move_lands_after_its_first_read_records_no_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Moving {
            stale: lh_1_in("w1:t1"),
            now: lh_1_in("w1:t4"),
            read: Cell::new(false),
        };
        let closed = r#"{"event":"tab_closed","data":{"tab_id":"w1:t1","workspace_id":"w1"}}"#;

        let socket = Some(Path::new(SOCKET));
        event_hook(
            &mut ledger,
            dir.path(),
            "tab.closed",
            closed,
            socket,
            SystemTime::now(),
        )
        .unwrap();

        assert!(kinds(dir.path()).is_empty());
    }

    #[test]
    fn a_retracted_gone_does_not_stop_a_watch_recording_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = lh_1_in("w1:t1");
        let socket = Some(Path::new(SOCKET));
        let seen_live = SystemTime::now();
        let then = seen_live + Duration::from_secs(1);
        let closed = r#"{"event":"tab_closed","data":{"tab_id":"w1:t1","workspace_id":"w1"}}"#;
        let moved = r#"{"event":"pane_moved","data":{"pane":{"pane_id":"w1:p1",
            "tab_id":"w1:t4","workspace_id":"w1"},"previous_pane_id":"w1:p1",
            "previous_tab_id":"w1:t1","previous_workspace_id":"w1"}}"#;
        event_hook(&mut ledger, dir.path(), "tab.closed", closed, socket, then).unwrap();
        event_hook(&mut ledger, dir.path(), "pane.moved", moved, socket, then).unwrap();

        let recorded = record_gone_after(dir.path(), "lh-1", then, seen_live).unwrap();

        assert!(recorded);
        assert_eq!(kinds(dir.path()), [r#""gone""#, r#""moved""#, r#""gone""#]);
    }
}
