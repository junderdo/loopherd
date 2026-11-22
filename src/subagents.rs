//! Which of a member's subagents are running, from the starts and stops the
//! subagent hook records in its mailbox.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::herd_view::{Member, RunningSubagent, View};
use crate::ledger::AgentRecord;
use crate::report::{Body, Entry, Subagent, Tally, history};
use crate::subagent_hook::description;

/// Where members' running subagents are read, and the time their ages run to.
pub struct Subagents<'a> {
    pub mailboxes: &'a Path,
    pub now: SystemTime,
}

/// Fills each open member's running subagents from its mailbox. A gone
/// member, or one whose mailbox can't be read, lists none.
pub fn fill(view: &mut View, ledger: &BTreeMap<String, AgentRecord>, from: &Subagents) {
    let members = view.herds.iter_mut().flat_map(|h| h.members.iter_mut());
    for member in members.chain(view.ungrouped.iter_mut()) {
        if !member.gone {
            fill_member(member, ledger, from);
        }
    }
}

fn fill_member(member: &mut Member, ledger: &BTreeMap<String, AgentRecord>, from: &Subagents) {
    let Ok((entries, _)) = history(from.mailboxes, &member.name) else {
        return;
    };
    let session = ledger
        .get(&member.name)
        .and_then(|r| r.session_id.as_deref());
    member.subagents = running(&entries, session)
        .into_iter()
        .map(|(at, subagent)| RunningSubagent {
            id: subagent.id.clone(),
            agent_type: subagent.agent_type.clone(),
            description: subagent
                .description
                .clone()
                .or_else(|| subagent.meta.as_deref().and_then(description)),
            started: at.to_string(),
            age: humantime::parse_rfc3339_weak(at)
                .ok()
                .and_then(|started| from.now.duration_since(started).ok())
                .unwrap_or(Duration::ZERO),
        })
        .collect();
}

/// Subagents started and not stopped, oldest first, with when each started.
/// A stop missed when the member was killed, revived or cleared is covered
/// by dropping them all at a start of its Claude or a stop with no subagent
/// running, and any started in a session other than `session`.
fn running<'a>(entries: &'a [Entry], session: Option<&str>) -> Vec<(&'a str, &'a Subagent)> {
    let mut running: Vec<(&str, &Subagent)> = vec![];
    for entry in entries {
        match &entry.body {
            Body::Start { .. } => running.clear(),
            Body::Stop(stop) if matches!(stop.background, Tally::Known(b) if b.subagents == 0) => {
                running.clear()
            }
            Body::SubagentStart(subagent) => {
                running.retain(|(_, s)| s.id != subagent.id);
                running.push((&entry.at, subagent));
            }
            Body::SubagentStop(subagent) => running.retain(|(_, s)| s.id != subagent.id),
            _ => {}
        }
    }
    running.retain(|(_, s)| match (session, &s.session) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => true,
    });
    running
}
