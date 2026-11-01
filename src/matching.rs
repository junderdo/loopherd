//! Which live herdr agent is a ledger entry's, by retire `--close`'s rules:
//! its name first, then its recorded pane running its own Claude session.

use crate::herdr::Agent;
use crate::ledger::AgentRecord;

/// The entry's live agent, if any.
pub fn live_agent<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    by_name(record, agents)
        .filter(|a| !session_conflict(a, record))
        .or_else(|| {
            agents
                .iter()
                .find(|a| a.pane_id == record.pane_id && same_session(a, record))
        })
}

/// The live agent holding the entry's name, whatever its session.
pub fn by_name<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    agents
        .iter()
        .find(|a| a.name.as_deref() == Some(record.name.as_str()))
}

/// Both sides know their session and they differ.
pub fn session_conflict(agent: &Agent, record: &AgentRecord) -> bool {
    agent.session_id.is_some()
        && record.session_id.is_some()
        && agent.session_id != record.session_id
}

pub fn same_session(agent: &Agent, record: &AgentRecord) -> bool {
    agent.session_id.is_some() && agent.session_id == record.session_id
}
