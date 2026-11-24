//! A member parked on its own background work: its last stop left work in
//! flight, nothing has come from it since, and herdr shows it idle.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::herdr::AgentStatus;
use crate::report::{Body, history};

/// How long a member may sit parked before it counts as stalled, unless its
/// herd file sets `stalled_after`.
pub const DEFAULT_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parked {
    /// The paused stop's time as its mailbox entry wrote it.
    pub stop_at: String,
    /// What the stop left in flight.
    pub pending: String,
    /// How long it has sat since the stop, or since it was last seen busy.
    pub idle: Duration,
}

impl Parked {
    pub fn minutes(&self) -> u64 {
        self.idle.as_secs() / 60
    }
}

/// `name`'s parked state at `now`, from its mailbox in `dir`. `busy_at`, when
/// a watch last saw it working or blocked, restarts the clock.
pub fn parked(
    dir: &Path,
    name: &str,
    status: AgentStatus,
    busy_at: Option<SystemTime>,
    now: SystemTime,
) -> Result<Option<Parked>> {
    if !matches!(status, AgentStatus::Idle | AgentStatus::Done) {
        return Ok(None);
    }
    let (entries, _) = history(dir, name)?;
    let mut last = None;
    for entry in entries {
        match &entry.body {
            Body::Stop(stop) if stop.error.is_none() => {
                last = stop.pending().map(|pending| (entry.at.clone(), pending));
            }
            Body::Usage { .. } | Body::SubagentStart(_) | Body::SubagentStop(_) => {}
            _ => last = None,
        }
    }
    let Some((stop_at, pending)) = last else {
        return Ok(None);
    };
    let Ok(stopped) = humantime::parse_rfc3339_weak(&stop_at) else {
        return Ok(None);
    };
    let since = busy_at.map_or(stopped, |busy| busy.max(stopped));
    let idle = now.duration_since(since).unwrap_or(Duration::ZERO);
    Ok(Some(Parked {
        stop_at,
        pending,
        idle,
    }))
}
